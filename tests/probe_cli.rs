#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread;
use std::time::Duration;

use ores_otel_sidecar::http::probe_get;

fn serve_once(response: &'static [u8], delay: Duration) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let address = listener.local_addr().expect("test listener address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept probe connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set fixture read timeout");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        let _ = stream.write_all(response);
    });
    (address, server)
}

#[test]
fn bounded_probe_accepts_only_a_success_status() {
    let (ok_address, ok_server) = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        Duration::ZERO,
    );
    assert_eq!(
        probe_get(ok_address, "/healthz").expect("read healthy status"),
        200
    );
    ok_server.join().expect("join success server");

    let (unhealthy_address, unhealthy_server) = serve_once(
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        Duration::ZERO,
    );
    assert_eq!(
        probe_get(unhealthy_address, "/healthz").expect("read unhealthy status"),
        503
    );
    unhealthy_server.join().expect("join unhealthy server");
}

#[test]
fn malformed_refused_and_unsupported_probe_targets_fail_closed() {
    let (malformed_address, malformed_server) =
        serve_once(b"not-http\r\n\r\n", Duration::ZERO);
    assert!(probe_get(malformed_address, "/healthz").is_err());
    malformed_server.join().expect("join malformed server");

    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve refused port");
    let refused_address = listener.local_addr().expect("reserved address");
    drop(listener);
    assert!(probe_get(refused_address, "/healthz").is_err());
    assert!(probe_get(refused_address, "/not-a-probe").is_err());
}

#[test]
fn probe_read_is_bounded_by_the_runtime_timeout() {
    let (address, server) = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
        Duration::from_millis(2_200),
    );
    assert!(probe_get(address, "/healthz").is_err());
    server.join().expect("join delayed server");
}
