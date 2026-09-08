#![forbid(unsafe_code)]

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub use crate::http_impl::{
    bind, classify_request_line, handle_connection, response_for, serve_listener, Method, Reject,
    Request, Route,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_STATUS_LINE: usize = 256;

/// One-shot bounded GET used by in-container kubelet `exec` probes.
///
/// Only the sidecar's fixed health endpoints are accepted. Connection, write,
/// timeout, malformed status-line, and unsupported-status failures are returned
/// as typed `io::Error` values; stdout remains unused.
pub fn probe_get(addr: SocketAddr, path: &str) -> io::Result<u16> {
    if !matches!(path, "/healthz" | "/health" | "/readyz" | "/ready") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported sidecar probe path",
        ));
    }

    let mut stream = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT))?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    let bytes = reader.read_line(&mut status_line)?;
    if bytes == 0 || bytes > MAX_STATUS_LINE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sidecar probe status line",
        ));
    }

    let line = status_line.trim_end_matches(['\r', '\n']);
    if line.as_bytes().contains(&0) || line.contains('\r') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sidecar probe status line",
        ));
    }

    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || status.len() != 3
        || !status.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sidecar probe status line",
        ));
    }

    status.parse::<u16>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid sidecar probe status code",
        )
    })
}
