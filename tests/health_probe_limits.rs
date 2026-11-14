use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use ores_otel_sidecar::http::probe_get;

fn run_probe(reply: Vec<u8>) -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0u8; 128];
        let count = stream.read(&mut request).unwrap();
        assert!(request[..count].starts_with(b"GET /ready HTTP/1.1\r\n"));
        stream.write_all(&reply).unwrap();
    });
    let result = probe_get(address, "/ready");
    worker.join().unwrap();
    result
}

#[test]
fn probe_enforces_status_line_limit_before_accepting_results() {
    let mut long = b"HTTP/1.1 200 ".to_vec();
    long.extend(std::iter::repeat_n(b'A', 8192));
    long.extend_from_slice(b"\r\n");
    assert_eq!(
        run_probe(long).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn probe_requires_newline_and_recognized_status() {
    assert_eq!(
        run_probe(b"HTTP/1.1 200 OK".to_vec()).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(run_probe(b"HTTP/1.1 200 OK\r\n".to_vec()).unwrap(), 200);
}
