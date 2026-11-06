#![forbid(unsafe_code)]

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread,
    time::Duration,
};

use ores_otel_sidecar::{
    http::handle_connection, probe::NoopProbe, SidecarConfig, SidecarIdentity,
};

fn listener_for(requests: usize) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback");
    let addr = listener.local_addr().unwrap();
    let config =
        SidecarConfig::from_bind(SidecarIdentity::ORES_OTEL, &addr.to_string(), false).unwrap();
    let worker = thread::spawn(move || {
        for incoming in listener.incoming().take(requests) {
            handle_connection(incoming.unwrap(), &config, &NoopProbe);
        }
    });
    (addr, worker)
}

fn exchange(addr: SocketAddr, raw: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn request(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
}

#[test]
fn malformed_host_framing_and_duplicate_lengths_never_poison_followup_client() {
    let (addr, worker) = listener_for(6);
    let candidates = [
        ("GET /readyz HTTP/1.1\r\n\r\n".to_owned(), "400"),
        (
            "GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n".to_owned(),
            "400",
        ),
        (
            "GET /readyz HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
            "400",
        ),
        (
            format!("GET /healthz HTTP/1.1\r\nHost: localhost\r\nX-Large: {}\r\n\r\n", "z".repeat(1100)),
            "431",
        ),
        ("GET /../private HTTP/1.1\r\nHost: localhost\r\n\r\n".to_owned(), "404"),
    ];
    for (raw, status) in candidates {
        let response = exchange(addr, &raw);
        assert!(response.starts_with(&format!("HTTP/1.1 {status}")), "{response}");
        assert!(response.to_ascii_lowercase().contains("cache-control: no-store"));
    }
    let healthy = exchange(addr, &request("/readyz"));
    assert!(healthy.starts_with("HTTP/1.1 200"), "{healthy}");
    assert!(healthy.contains(r#""ok":true"#));
    worker.join().unwrap();
}

#[test]
fn head_metrics_is_bodyless_and_http10_readiness_has_explicit_security_headers() {
    let (addr, worker) = listener_for(3);
    let head = exchange(addr, "HEAD /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(head.split_once("\r\n\r\n").unwrap().1, "");
    assert!(head.contains("x-content-type-options: nosniff"));

    let metrics = exchange(addr, &request("/metrics"));
    assert!(metrics.starts_with("HTTP/1.1 200"), "{metrics}");
    assert!(metrics.contains("ores_otel_sidecar_up"));
    assert!(metrics.contains("content-security-policy: default-src 'none'"));

    let http10 = exchange(addr, "GET /ready HTTP/1.0\r\n\r\n");
    assert!(http10.starts_with("HTTP/1.1 200"), "{http10}");
    assert!(http10.contains(r#""ok":true"#));
    worker.join().unwrap();
}
