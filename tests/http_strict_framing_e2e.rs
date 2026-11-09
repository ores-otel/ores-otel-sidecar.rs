#![forbid(unsafe_code)]

// Actual TCP tests through the production sidecar parser, not a mock parser.
// Keep each listener single-purpose so every rejected request is followed by
// a healthy request and we can detect accidental connection poisoning.
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    thread,
    time::Duration,
};

use ores_otel_sidecar::{
    http::handle_connection, probe::NoopProbe, SidecarConfig, SidecarIdentity,
};

fn serve(connections: usize) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("listener address");
    let config =
        SidecarConfig::from_bind(SidecarIdentity::ORES_OTEL, &addr.to_string(), false).unwrap();
    let worker = thread::spawn(move || {
        for accepted in listener.incoming().take(connections) {
            handle_connection(accepted.unwrap(), &config, &NoopProbe);
        }
    });
    return (addr, worker);
}

fn exchange(addr: SocketAddr, raw: &[u8]) -> String {
    let mut socket = TcpStream::connect(addr).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket.write_all(raw).expect("send bytes");
    // Half-close matters: a truncated header must be rejected when the
    // peer signals EOF, not treated as a complete request.
    socket.shutdown(Shutdown::Write).unwrap();
    let mut reply = Vec::new();
    socket.read_to_end(&mut reply).expect("read response");
    return String::from_utf8(reply).expect("ASCII HTTP response");
}

fn status(reply: &str, code: u16) {
    assert!(
        reply.starts_with(&format!("HTTP/1.1 {code} ")),
        "expected {code}, got: {reply:?}"
    );
    assert!(
        reply.contains("cache-control: no-store"),
        "rejection must preserve cache safety"
    );
    assert!(
        reply.contains("x-content-type-options: nosniff"),
        "rejection must preserve content-type safety"
    );
}

fn healthy(addr: SocketAddr) {
    let reply = exchange(
        addr,
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    status(&reply, 200);
    assert!(reply.contains(r#""ok":true"#));
}

#[test]
fn duplicate_and_ambiguous_host_headers_fail_closed_over_tcp() {
    let bad: &[&[u8]] = &[
        b"GET /readyz HTTP/1.1\r\nHost: alpha\r\nhOSt: alpha\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: alpha\r\nHost: beta\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost:\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\n Host: alpha\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost : alpha\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: alpha\r\nX-Tag: a\rz\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: alpha\r\nX-Tag: a\x00b\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: alpha\r\nX-Tag: a\x7fb\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: alpha\r\n\tcontinued: nope\r\n\r\n",
    ];
    let (addr, worker) = serve(bad.len() + 1);
    for raw in bad {
        status(&exchange(addr, raw), 400);
    }
    healthy(addr);
    worker.join().unwrap();
}

#[test]
fn duplicate_framing_headers_and_invalid_lengths_fail_closed_over_tcp() {
    let bad: &[&[u8]] = &[
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\ncontent-length: 1\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: -1\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: +0\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 18446744073709551616\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\n\r\n",
    ];
    let (addr, worker) = serve(bad.len() + 2);
    for (index, raw) in bad.iter().enumerate() {
        // Transfer-Encoding and Expect are deliberately classified as body
        // refusals (400); every other entry is a malformed header (400).
        let reply = exchange(addr, raw);
        status(&reply, 400);
        assert!(
            !reply.contains("200 OK"),
            "invalid frame {index} unexpectedly admitted"
        );
    }
    status(
        &exchange(
            addr,
            b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
        ),
        200,
    );
    healthy(addr);
    worker.join().unwrap();
}

#[test]
fn truncated_lf_only_and_invalid_utf8_requests_are_not_completed() {
    let bad: &[&[u8]] = &[
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n", // No terminating CRLF.
        b"GET /readyz HTTP/1.1\r\nHost: loc", // EOF inside field.
        b"GET /readyz HTTP/1.1\nHost: localhost\n\n", // LF-only framing.
        b"GET /readyz HTTP/1.1\r\nHost: localhost\n\r\n", // Mixed framing.
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nX-Tag: \xff\r\n\r\n",
        b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nBad field\r\n\r\n",
    ];
    let (addr, worker) = serve(bad.len() + 2);
    for raw in bad {
        status(&exchange(addr, raw), 400);
    }
    // A standards-compliant HTTP/1.0 probe may omit Host.
    status(&exchange(addr, b"GET /ready HTTP/1.0\r\n\r\n"), 200);
    healthy(addr);
    worker.join().unwrap();
}

#[test]
fn oversized_lines_reject_early_and_next_request_recovers() {
    let (addr, worker) = serve(4);
    let large_header = format!(
        "GET /readyz HTTP/1.1\r\nHost: localhost\r\nX-Fill: {}\r\n\r\n",
        "x".repeat(4 * 1024)
    );
    status(&exchange(addr, large_header.as_bytes()), 431);
    let large_request_line = format!(
        "GET /{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "a".repeat(4 * 1024)
    );
    status(&exchange(addr, large_request_line.as_bytes()), 431);
    status(
        &exchange(
            addr,
            b"HEAD /healthz?probe=1 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        ),
        200,
    );
    healthy(addr);
    worker.join().unwrap();
}
