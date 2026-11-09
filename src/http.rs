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
        let line = line.strip_suffix("\r\n").ok_or(Reject::Invalid)?;
        let (name, raw_value) = line.split_once(':').ok_or(Reject::Invalid)?;
        // RFC 9110 field names use a nonempty sequence of permitted token bytes.
        // Reject leading whitespace, control bytes and ambiguous delimiters.
        if name.is_empty()
            || !name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_|~".contains(&byte) || byte == 0x60
            })
        {
            return Err(Reject::Invalid);
        }
        if raw_value
            .bytes()
            .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
        {
            return Err(Reject::Invalid);
        }
        let value = raw_value.trim();
        let count = self.count + 1;
        if count > MAX_HEADERS {
            return Err(Reject::HeaderTooLarge);
        }

        let mut saw_host = self.saw_host;
        let mut content_length = self.content_length;
        if name.eq_ignore_ascii_case("host") {
            if saw_host || value.is_empty() {
                return Err(Reject::Invalid);
            }
            saw_host = true;
        } else if name.eq_ignore_ascii_case("expect")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return Err(Reject::BodyNotAllowed);
        } else if name.eq_ignore_ascii_case("content-length") {
            // Refuse even identical duplicate lengths: intermediaries can disagree
            // about how duplicate framing fields should be interpreted.
            if content_length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(Reject::Invalid);
            }
            content_length = Some(value.parse().map_err(|_| Reject::Invalid)?);
        }
        return Ok(Self {
            count,
            content_length,
            saw_host,
        });
    }
}

/// Read at most limit bytes on the wire. BufReader::read_line would allocate an
/// unbounded String before a caller could enforce its length limit.
fn bounded_line<R: BufRead>(
    reader: &mut R,
    limit: usize,
    overflow: Reject,
) -> Result<Option<String>, Reject> {
    let mut bytes = Vec::with_capacity(limit.min(256));
    loop {
        let (consumed, ended) = {
            let available = reader.fill_buf().map_err(|_| Reject::Invalid)?;
            if available.is_empty() {
                return if bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(Reject::Invalid)
                };
            }
            let consumed = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |index| index + 1);
            if bytes.len() + consumed > limit {
                return Err(overflow);
            }
            bytes.extend_from_slice(&available[..consumed]);
            (consumed, available[consumed - 1] == b'\n')
        };
        reader.consume(consumed);
        if ended {
            break;
        }
    }
    let line = String::from_utf8(bytes).map_err(|_| Reject::Invalid)?;
    if !line.ends_with("\r\n") {
        return Err(Reject::Invalid);
    }
    return Ok(Some(line));
}

/// EOF without the mandatory empty CRLF header terminator is a truncated
/// message, never a successfully completed health/metrics request.
fn read_headers<R: BufRead>(reader: &mut R) -> Result<Headers, Reject> {
    // At most 32 recursive folds: Headers::with rejects the next field.
    // Keeping the accumulator immutable also preserves our FP lint budget.
    fn fold<R: BufRead>(reader: &mut R, headers: Headers) -> Result<Headers, Reject> {
        match bounded_line(reader, MAX_HEADER_LINE, Reject::HeaderTooLarge)? {
            Some(line) if line == "\r\n" => return Ok(headers),
            Some(line) => return fold(reader, headers.with(&line)?),
            None => return Err(Reject::Invalid),
        }
    }
    return fold(reader, Headers::default());
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let line = match bounded_line(&mut reader, MAX_REQUEST_LINE, Reject::LineTooLong) {
        Ok(Some(line)) => line,
        Ok(None) | Err(Reject::Invalid) => return Request::Reject(Reject::Invalid),
        Err(reject) => return Request::Reject(reject),
    };
    let classified = classify_request_line(&line);
    if matches!(classified, Request::Reject(_)) {
        return classified;
    }
    let http11 = line.ends_with("HTTP/1.1\r\n");
    let headers = match read_headers(&mut reader) {
        Ok(headers) => headers,
        Err(reject) => return Request::Reject(reject),
    };
    if http11 && !headers.saw_host {
        return Request::Reject(Reject::Invalid);
    }
    if headers.content_length.unwrap_or(0) > 0 {
        return Request::Reject(Reject::BodyNotAllowed);
    }
    return classified;
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
