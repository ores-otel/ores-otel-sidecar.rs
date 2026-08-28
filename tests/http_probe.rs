#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ores_otel_sidecar::http::handle_connection;
use ores_otel_sidecar::probe::{NoopProbe, ProductProbe};
use ores_otel_sidecar::{SidecarConfig, SidecarIdentity};

fn exchange(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf
}

fn serve_n(n: usize, probe: impl ProductProbe + Send + 'static) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config =
        SidecarConfig::from_bind(SidecarIdentity::ORES_OTEL, &addr.to_string(), false).unwrap();
    thread::spawn(move || {
        for incoming in listener.incoming().take(n) {
            handle_connection(incoming.unwrap(), &config, &probe);
        }
    });
    addr
}

fn headers_of(response: &str) -> &str {
    response.split("\r\n\r\n").next().unwrap_or(response)
}

#[test]
fn loopback_listener_serves_health_metrics_and_rejects_post() {
    let addr = serve_n(4, NoopProbe);
    let health = exchange(addr, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(health.starts_with("HTTP/1.1 200 OK"));
    assert!(health.contains("application/json"));
    assert!(health.contains("\"ok\":true"));
    assert!(health.contains("ores-otel-sidecar"));

    let metrics = exchange(addr, "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(metrics.contains("ores_otel_sidecar_up"));
    assert!(metrics.contains("service=\"ores-otel-sidecar\""));

    let posted = exchange(
        addr,
        "POST /healthz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert!(posted.starts_with("HTTP/1.1 400") || posted.starts_with("HTTP/1.1 405"));

    let missing = exchange(addr, "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 404"));
}

#[test]
fn responses_set_hardening_headers_and_head_omits_body() {
    let addr = serve_n(2, NoopProbe);
    let get = exchange(addr, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let headers = headers_of(&get).to_ascii_lowercase();
    assert!(headers.contains("connection: close"));
    assert!(headers.contains("cache-control: no-store"));
    assert!(headers.contains("x-content-type-options: nosniff"));
    assert!(headers.contains("x-frame-options: deny"));
    assert!(headers.contains("content-security-policy: default-src 'none'"));
    assert!(headers.contains("content-length:"));

    let head = exchange(addr, "HEAD /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    let parts: Vec<&str> = head.splitn(2, "\r\n\r\n").collect();
    assert_eq!(parts.get(1).copied().unwrap_or("x").trim(), "");
}

struct Down;

impl ProductProbe for Down {
    fn ready(&self) -> bool {
        false
    }
}

#[test]
fn readyz_fails_closed_when_probe_is_not_ready() {
    let addr = serve_n(1, Down);
    let body = exchange(addr, "GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(body.starts_with("HTTP/1.1 503"));
    assert!(body.contains("\"ok\":false"));
}

#[test]
fn chunked_and_query_and_alias_paths() {
    let addr = serve_n(3, NoopProbe);
    let alias = exchange(addr, "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(alias.starts_with("HTTP/1.1 200 OK"));

    let queried = exchange(
        addr,
        "GET /readyz?foo=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(queried.starts_with("HTTP/1.1 200 OK"));

    let chunked = exchange(
        addr,
        "POST /healthz HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    );
    assert!(chunked.starts_with("HTTP/1.1 400") || chunked.starts_with("HTTP/1.1 405"));
}

#[test]
fn http11_missing_host_expect_and_duplicate_length_fail_closed() {
    let addr = serve_n(3, NoopProbe);
    let no_host = exchange(addr, "GET /healthz HTTP/1.1\r\n\r\n");
    assert!(no_host.starts_with("HTTP/1.1 400"), "{no_host}");

    let expect = exchange(
        addr,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\n\r\n",
    );
    assert!(expect.starts_with("HTTP/1.1 400"), "{expect}");

    let duplicate = exchange(
        addr,
        "GET /healthz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nContent-Length: 8\r\n\r\n",
    );
    assert!(duplicate.starts_with("HTTP/1.1 400"), "{duplicate}");
}

#[test]
fn http10_without_host_is_ok_and_http2_is_invalid() {
    let addr = serve_n(3, NoopProbe);
    let http10 = exchange(addr, "GET /healthz HTTP/1.0\r\n\r\n");
    assert!(http10.starts_with("HTTP/1.1 200 OK"), "{http10}");

    let encoded = exchange(
        addr,
        "GET /healthz%2e%2e HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(encoded.starts_with("HTTP/1.1 404"), "{encoded}");

    let http2 = exchange(addr, "GET /healthz HTTP/2.0\r\nHost: localhost\r\n\r\n");
    assert!(http2.starts_with("HTTP/1.1 400"), "{http2}");
}
