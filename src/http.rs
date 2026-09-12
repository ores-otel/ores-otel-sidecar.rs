#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use crate::apm;
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
    if line.as_bytes().contains(&0) || line.contains('\r') {
        return Request::Reject(Reject::Invalid);
    }
    let parts = line.split(' ').collect::<Vec<_>>();
    let method = match parts.first().copied().unwrap_or("") {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "" => return Request::Reject(Reject::Invalid),
        _ => return Request::Reject(Reject::MethodNotAllowed),
    };
    // Exactly three tokens: anything else (missing path or version, extra
    // tokens) is malformed.
    let [_, path, version] = parts.as_slice() else {
        return Request::Reject(Reject::Invalid);
    };
    let (path, version) = (*path, *version);
    if path.is_empty() || (version != "HTTP/1.1" && version != "HTTP/1.0") {
        return Request::Reject(Reject::Invalid);
    }
    if path.contains('%')
        || path.contains("..")
        || path.contains('\\')
        || path.contains('\t')
        || !path.starts_with('/')
    {
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
    probe: &(impl ProductProbe + ?Sized),
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
                let mut body = format!(
                    "# HELP ores_otel_sidecar_up Whether the sidecar probe listener is serving.\n\
                     # TYPE ores_otel_sidecar_up gauge\n\
                     ores_otel_sidecar_up{{service=\"{}\"}} 1\n",
                    identity.service
                );
                body.push_str(&apm::prometheus_text(identity.service));
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
    let content_length = if head_only { 0 } else { body.len() };
    let payload = if head_only { "" } else { body };
    let out = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\nx-frame-options: DENY\r\ncontent-security-policy: default-src 'none'; connect-src 'self'\r\n\r\n{}",
        status_line(code),
        content_type,
        content_length,
        payload
    );
    stream.write_all(out.as_bytes())
}

/// What the header block has established so far. Parsing the headers is a
/// `try_fold` over this value: [`Headers::with`] consumes one header line and
/// returns the next state or the rejection.
#[derive(Default)]
struct Headers {
    count: usize,
    content_length: Option<u64>,
    saw_host: bool,
}

impl Headers {
    fn with(self, line: &str) -> Result<Self, Reject> {
        if line.as_bytes().contains(&0) || !line.contains(':') {
            return Err(Reject::Invalid);
        }
        let count = self.count + 1;
        if count > MAX_HEADERS {
            return Err(Reject::HeaderTooLarge);
        }
        let lower = line.to_ascii_lowercase();
        let saw_host = if lower.starts_with("host:") {
            let value = line
                .split_once(':')
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            if value.is_empty() {
                return Err(Reject::Invalid);
            }
            true
        } else {
            self.saw_host
        };
        if lower.starts_with("expect:") || lower.starts_with("transfer-encoding:") {
            return Err(Reject::BodyNotAllowed);
        }
        let content_length = match lower.strip_prefix("content-length:") {
            Some(rest) => {
                let parsed = rest.trim().parse().unwrap_or(u64::MAX);
                if self
                    .content_length
                    .is_some_and(|previous| previous != parsed)
                {
                    return Err(Reject::Invalid);
                }
                Some(parsed)
            }
            None => self.content_length,
        };
        Ok(Self {
            count,
            content_length,
            saw_host,
        })
    }
}

/// The header lines as the stream yields them, terminators included. The
/// iterator ends at EOF or at the blank line that closes the header block, and
/// yields the rejection for an unreadable or oversized line.
fn header_lines<R: BufRead>(reader: &mut R) -> impl Iterator<Item = Result<String, Reject>> + '_ {
    std::iter::from_fn(move || {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) if line.len() > MAX_HEADER_LINE => Some(Err(Reject::HeaderTooLarge)),
            Ok(_) if line == "\r\n" || line == "\n" => None,
            Ok(_) => Some(Ok(line)),
            Err(_) => Some(Err(Reject::Invalid)),
        }
    })
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
    let http11 = line.contains("HTTP/1.1");
    let classified = classify_request_line(&line);
    let headers = match header_lines(&mut reader)
        .try_fold(Headers::default(), |headers, line| headers.with(&line?))
    {
        Ok(headers) => headers,
        Err(reject) => return Request::Reject(reject),
    };
    if matches!(classified, Request::Ok { .. }) && http11 && !headers.saw_host {
        return Request::Reject(Reject::Invalid);
    }
    let content_length = headers.content_length.unwrap_or(0);
    if content_length > 0 {
        let mut sink = vec![0_u8; content_length.min(64) as usize];
        let _ = reader.read(&mut sink);
        return Request::Reject(Reject::BodyNotAllowed);
    }
    classified
}

pub fn handle_connection(
    mut stream: TcpStream,
    config: &SidecarConfig,
    probe: &(impl ProductProbe + ?Sized),
) {
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
    probe: &(impl ProductProbe + ?Sized),
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

/// One-shot GET used by in-container kubelet `exec` probes. Stdout stays unused.
pub fn probe_get(addr: SocketAddr, path: &str) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    let code = buf
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse().ok())
        .unwrap_or(0);
    Ok(code)
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
    fn metrics_response_includes_process_and_filesystem_collector_state() {
        let (code, ctype, body) = response_for(
            Request::Ok {
                method: Method::Get,
                route: Route::Metrics,
            },
            SidecarIdentity::ORES_OTEL,
            &NoopProbe,
        );
        assert_eq!(code, 200);
        assert_eq!(ctype, "text/plain; version=0.0.4");
        assert!(body.contains("ores_otel_sidecar_up"));
        assert!(body.contains("ores_otel_resource_collector_supported"));
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
        assert_eq!(
            classify_request_line("GET /healthz HTTP/1.0"),
            Request::Ok {
                method: Method::Get,
                route: Route::Healthz
            }
        );
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
        assert_eq!(
            classify_request_line("GET /healthz%2e%2e HTTP/1.1"),
            Request::Reject(Reject::NotFound)
        );
    }

    #[test]
    fn http2_and_extra_tokens_are_invalid() {
        assert_eq!(
            classify_request_line("GET /healthz HTTP/2.0"),
            Request::Reject(Reject::Invalid)
        );
        assert_eq!(
            classify_request_line("GET /healthz HTTP/1.1 extra"),
            Request::Reject(Reject::Invalid)
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
