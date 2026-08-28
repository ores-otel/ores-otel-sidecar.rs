#![forbid(unsafe_code)]

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn wait_listen_port(stderr: &mut impl Read) -> u16 {
    let mut reader = std::io::BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if value["event"] == "listen" {
                let message = value["message"].as_str().unwrap_or("");
                let hostport = message
                    .trim_start_matches("http://")
                    .trim_end_matches("/healthz");
                if let Some(port) = hostport.rsplit(':').next() {
                    return port.parse().expect("listen port");
                }
            }
        }
    }
    panic!("sidecar never logged a listen event");
}

#[test]
fn binary_listens_on_loopback_and_leaves_stdout_quiet() {
    let bind = "127.0.0.1:0";
    let mut child = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .env("ORES_OTEL_SIDECAR_BIND", bind)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sidecar");
    let mut stderr = child.stderr.take().expect("stderr");
    let port = wait_listen_port(&mut stderr);

    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    assert!(buf.starts_with("HTTP/1.1 200 OK"), "{buf}");
    assert!(buf.contains("ores-otel-sidecar"));

    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.stdout.is_empty(),
        "stdout must stay protocol-free: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn unspecified_bind_exits_fatal_and_keeps_stdout_quiet() {
    let output = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .env("ORES_OTEL_SIDECAR_BIND", "0.0.0.0:19092")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn sidecar");
    assert!(!output.status.success(), "unspecified bind must fail closed");
    assert!(
        output.stdout.is_empty(),
        "stdout must stay protocol-free: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fatal"), "{stderr}");
    assert!(stderr.contains("0.0.0.0") || stderr.contains("non-loopback"), "{stderr}");
}
