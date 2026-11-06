#![forbid(unsafe_code)]

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread,
    time::Duration,
};

use ores_otel_sidecar::{
    http::{classify_request_line, handle_connection, Reject, Request},
    probe::NoopProbe,
    SidecarConfig, SidecarIdentity,
};

fn serve(count: usize) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config =
        SidecarConfig::from_bind(SidecarIdentity::ORES_OTEL, &addr.to_string(), false).unwrap();
    let worker = thread::spawn(move || {
        for request in listener.incoming().take(count) {
            handle_connection(request.unwrap(), &config, &NoopProbe);
        }
    });
    (addr, worker)
}

fn exchange(addr: SocketAddr, raw: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn status(response: &str, expected: u16) {
    assert!(
        response.starts_with(&format!("HTTP/1.1 {expected} ")),
        "unexpected HTTP response: {response}"
    );
    assert!(response.contains("cache-control: no-store"));
    assert!(response.contains("x-content-type-options: nosniff"));
}

#[test]
fn exactly_thirty_two_headers_are_admitted_but_thirty_three_fail_closed() {
    let (addr, worker) = serve(3);
    let build = |count: usize| {
        let mut request = "GET /healthz HTTP/1.1\r\nHost: localhost\r\n".to_owned();
        for index in 1..count {
            request.push_str(&format!("X-Request-{index}: ok\r\n"));
        }
        request.push_str("\r\n");
        request
    };
    status(&exchange(addr, &build(32)), 200);
    status(&exchange(addr, &build(33)), 431);
    status(
        &exchange(addr, "GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        200,
    );
    worker.join().unwrap();
}

#[test]
fn hostile_paths_and_request_line_boundaries_never_dispatch_to_health() {
    let invalid = [
        "GET /healthz%2fadmin HTTP/1.1",
        "GET /healthz/../admin HTTP/1.1",
        "GET \\healthz HTTP/1.1",
        "GET /healthz HTTP/1.1 extra",
        "GET /healthz HTTP/2.0",
        "GET /healthz HTTP/1.1\rX-Evil: yes",
    ];
    for raw in invalid {
        assert!(
            matches!(
                classify_request_line(raw),
                Request::Reject(Reject::NotFound | Reject::Invalid)
            ),
            "hostile request admitted: {raw:?}"
        );
    }
    assert_eq!(
        classify_request_line(&format!("GET /{} HTTP/1.1", "a".repeat(2049))),
        Request::Reject(Reject::LineTooLong)
    );
}

#[test]
fn request_bodies_and_expect_continue_are_rejected_without_poisoning_next_client() {
    let (addr, worker) = serve(5);
    let invalid = [
        ("GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\nx", 400),
        ("GET /readyz HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\n\r\n", 400),
        ("HEAD /readyz HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
        ("GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n", 200),
    ];
    for (raw, expected) in invalid {
        status(&exchange(addr, raw), expected);
    }
    status(
        &exchange(addr, "GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        200,
    );
    worker.join().unwrap();
}
