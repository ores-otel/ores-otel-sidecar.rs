#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ores_otel_sidecar::http::{bind, handle_connection};
use ores_otel_sidecar::probe::NoopProbe;
use ores_otel_sidecar::{SidecarConfig, SidecarIdentity};

fn exchange(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf
}

#[test]
fn loopback_listener_serves_health_metrics_and_rejects_post() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config =
        SidecarConfig::from_bind(SidecarIdentity::ORES_OTEL, &addr.to_string(), false).unwrap();
    thread::spawn(move || {
        for incoming in listener.incoming().take(4) {
            handle_connection(incoming.unwrap(), &config, &NoopProbe);
        }
    });

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
fn bind_helper_uses_ephemeral_loopback() {
    let listener = bind("127.0.0.1:0".parse().unwrap()).unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());
}
