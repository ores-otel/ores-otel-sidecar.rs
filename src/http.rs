#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use crate::config::SidecarConfig;
use crate::health;
use crate::identity::SidecarIdentity;
use crate::probe::ProductProbe;

const MAX_REQUEST_LINE: usize = 2048;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_LINE: usize = 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Healthz,
    Readyz,
    Metrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    MethodNotAllowed,
    NotFound,
    LineTooLong,
    HeaderTooLarge,
    BodyNotAllowed,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Ok { method: Method, route: Route },
    Reject(Reject),
}

pub fn classify_request_line(line: &str) -> Request {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.len() > MAX_REQUEST_LINE {
        return Request::Reject(Reject::LineTooLong);
    }
    let mut parts = line.split(' ');
    let method = match parts.next().unwrap_or("") {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "" => return Request::Reject(Reject::Invalid),
        _ => return Request::Reject(Reject::MethodNotAllowed),
    };
    let path = parts.next().unwrap_or("");
    if path.is_empty() || parts.next().is_none() {
        return Request::Reject(Reject::Invalid);
    }
    if path.contains("..") || path.contains('\\') || !path.starts_with('/') {
        return Request::Reject(Reject::NotFound);
    }
    let path = path.split('?').next().unwrap_or(path);
    let route = match path {
        "/healthz" | "/health" => Route::Healthz,
        "/readyz" | "/ready" => Route::Readyz,
        "/metrics" => Route::Metrics,
        _ => return Request::Reject(Reject::NotFound),
    };
    Request::Ok { method, route }
}

fn status_line(code: u16) -> &'static str {
    match code {
        200 => "200 OK",
        405 => "405 Method Not Allowed",
        404 => "404 Not Found",
        400 => "400 Bad Request",
        413 => "413 Payload Too Large",
        431 => "431 Request Header Fields Too Large",
        503 => "503 Service Unavailable",
        _ => "500 Internal Server Error",
    }
}

pub fn response_for(
    request: Request,
    identity: SidecarIdentity,
    probe: &impl ProductProbe,
) -> (u16, &'static str, String) {
    match request {
        Request::Reject(Reject::MethodNotAllowed) => (
            405,
            "text/plain; charset=utf-8",
            "method not allowed\n".into(),
        ),
        Request::Reject(Reject::NotFound) => {
            (404, "text/plain; charset=utf-8", "not found\n".into())
        }
        Request::Reject(Reject::LineTooLong | Reject::HeaderTooLarge) => (
            431,
            "text/plain; charset=utf-8",
            "request too large\n".into(),
        ),
        Request::Reject(Reject::BodyNotAllowed) => (
            400,
            "text/plain; charset=utf-8",
            "request body not allowed\n".into(),
        ),
        Request::Reject(Reject::Invalid) => {
            (400, "text/plain; charset=utf-8", "bad request\n".into())
        }
        Request::Ok { route, .. } => match route {
            Route::Healthz => {
                let body = serde_json::to_string(&health::current(identity, probe.extra_health()))
                    .unwrap_or_else(|_| r#"{"ok":false}"#.into());
                (200, "application/json", format!("{body}\n"))
            }
            Route::Readyz => {
                let extra = probe.extra_health();
                let ready = probe.ready();
                let payload = serde_json::json!({
                    "ok": ready,
                    "service": identity.service,
                    "product": extra,
                });
                let body =
                    serde_json::to_string(&payload).unwrap_or_else(|_| r#"{"ok":false}"#.into());
                let code = if ready { 200 } else { 503 };
                (code, "application/json", format!("{body}\n"))
            }
            Route::Metrics => {
                let body = format!(
                    "# HELP ores_otel_sidecar_up Whether the sidecar probe listener is serving.\n\
                     # TYPE ores_otel_sidecar_up gauge\n\
                     ores_otel_sidecar_up{{service=\"{}\"}} 1\n",
                    identity.service
                );
                (200, "text/plain; version=0.0.4", body)
            }
        },
    }
}

fn write_http(
    stream: &mut TcpStream,
    code: u16,
    content_type: &str,
    body: &str,
    head_only: bool,
) -> std::io::Result<()> {
    let mut out = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\n\r\n",
        status_line(code),
        content_type,
        body.len()
    );
    if !head_only {
        out.push_str(body);
    }
    stream.write_all(out.as_bytes())
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Request::Reject(Reject::Invalid),
        Ok(_) if line.len() > MAX_REQUEST_LINE => return Request::Reject(Reject::LineTooLong),
        Ok(_) => {}
        Err(_) => return Request::Reject(Reject::Invalid),
    }
    let classified = classify_request_line(&line);
    let mut headers = 0;
    let mut content_length = 0_u64;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.len() > MAX_HEADER_LINE => {
                return Request::Reject(Reject::HeaderTooLarge)
            }
            Ok(_) => {}
            Err(_) => return Request::Reject(Reject::Invalid),
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers += 1;
        if headers > MAX_HEADERS {
            return Request::Reject(Reject::HeaderTooLarge);
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(usize::MAX as u64);
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            return Request::Reject(Reject::BodyNotAllowed);
        }
    }
    if content_length > 0 {
        let mut sink = vec![0_u8; content_length.min(64) as usize];
        let _ = reader.read(&mut sink);
        return Request::Reject(Reject::BodyNotAllowed);
    }
    classified
}

pub fn handle_connection(mut stream: TcpStream, config: &SidecarConfig, probe: &impl ProductProbe) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let request = read_request(&mut stream);
    let head_only = matches!(
        request,
        Request::Ok {
            method: Method::Head,
            ..
        }
    );
    let (code, content_type, body) = response_for(request, config.identity, probe);
    let _ = write_http(&mut stream, code, content_type, &body, head_only);
}

pub fn serve_listener(
    listener: TcpListener,
    config: &SidecarConfig,
    probe: &impl ProductProbe,
) -> std::io::Result<()> {
    listener.set_nonblocking(false)?;
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => handle_connection(stream, config, probe),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::NoopProbe;

    #[test]
    fn get_healthz_is_classified() {
        assert_eq!(
            classify_request_line("GET /healthz HTTP/1.1"),
            Request::Ok {
                method: Method::Get,
                route: Route::Healthz
            }
        );
    }

    #[test]
    fn post_is_rejected() {
        assert_eq!(
            classify_request_line("POST /healthz HTTP/1.1"),
            Request::Reject(Reject::MethodNotAllowed)
        );
    }

    #[test]
    fn traversal_and_unknown_paths_are_404() {
        assert_eq!(
            classify_request_line("GET /healthz/../secret HTTP/1.1"),
            Request::Reject(Reject::NotFound)
        );
        assert_eq!(
            classify_request_line("GET /admin HTTP/1.1"),
            Request::Reject(Reject::NotFound)
        );
    }

    #[test]
    fn health_response_is_json_ok() {
        let (code, ctype, body) = response_for(
            Request::Ok {
                method: Method::Get,
                route: Route::Healthz,
            },
            SidecarIdentity::ORES_OTEL,
            &NoopProbe,
        );
        assert_eq!(code, 200);
        assert_eq!(ctype, "application/json");
        assert!(body.contains("\"ok\":true"));
        assert!(body.contains("ores-otel-sidecar"));
    }

    #[test]
    fn aliases_and_query_strings_map_to_probes() {
        assert_eq!(
            classify_request_line("GET /health HTTP/1.1"),
            Request::Ok {
                method: Method::Get,
                route: Route::Healthz
            }
        );
        assert_eq!(
            classify_request_line("HEAD /readyz?verbose=1 HTTP/1.1"),
            Request::Ok {
                method: Method::Head,
                route: Route::Readyz
            }
        );
        assert_eq!(
            classify_request_line("GET /ready HTTP/1.1"),
            Request::Ok {
                method: Method::Get,
                route: Route::Readyz
            }
        );
    }

    #[test]
    fn options_trace_and_missing_version_fail_closed() {
        assert_eq!(
            classify_request_line("OPTIONS /healthz HTTP/1.1"),
            Request::Reject(Reject::MethodNotAllowed)
        );
        assert_eq!(
            classify_request_line("TRACE /healthz HTTP/1.1"),
            Request::Reject(Reject::MethodNotAllowed)
        );
        assert_eq!(
            classify_request_line("GET /healthz"),
            Request::Reject(Reject::Invalid)
        );
        assert_eq!(classify_request_line(""), Request::Reject(Reject::Invalid));
    }

    #[test]
    fn encoded_dots_and_backslash_are_not_found() {
        assert_eq!(
            classify_request_line("GET /healthz\\..\\etc HTTP/1.1"),
            Request::Reject(Reject::NotFound)
        );
        assert_eq!(
            classify_request_line("GET healthz HTTP/1.1"),
            Request::Reject(Reject::NotFound)
        );
    }

    struct NotReady;

    impl ProductProbe for NotReady {
        fn ready(&self) -> bool {
            false
        }
    }

    #[test]
    fn unreadiness_is_503() {
        let (code, _, body) = response_for(
            Request::Ok {
                method: Method::Get,
                route: Route::Readyz,
            },
            SidecarIdentity::ORES_OTEL,
            &NotReady,
        );
        assert_eq!(code, 503);
        assert!(body.contains("\"ok\":false"));
    }

    #[test]
    fn oversized_request_line_is_431() {
        let line = format!("GET /{} HTTP/1.1", "a".repeat(3000));
        assert_eq!(
            classify_request_line(&line),
            Request::Reject(Reject::LineTooLong)
        );
    }
}
