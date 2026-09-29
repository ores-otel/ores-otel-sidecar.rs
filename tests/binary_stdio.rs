#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HEALTH_REQUEST: &[u8] = b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
const READY_DEADLINE: Duration = Duration::from_secs(5);
const READ_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(500);
const RETRY_DELAY: Duration = Duration::from_millis(20);

fn reserve_loopback_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    return listener.local_addr().expect("reserved address").port();
}

fn health_response_when_ready(port: u16) -> String {
    let deadline = Instant::now() + READY_DEADLINE;
    let mut last_response = String::new();

    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(READ_ATTEMPT_TIMEOUT));
            if stream.write_all(HEALTH_REQUEST).is_ok() {
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                if response.starts_with("HTTP/1.1 200 OK") {
                    return response;
                }
                last_response = response;
            }
        }
        thread::sleep(RETRY_DELAY);
    }

    panic!(
        "sidecar never completed a health request on reserved loopback port {port}; last response {last_response:?}"
    );
}

fn assert_closed_diagnostic(stderr: &[u8], operation: &str) {
    let text = String::from_utf8_lossy(stderr);
    let value: serde_json::Value = serde_json::from_str(text.trim()).expect("one JSON diagnostic");
    assert_eq!(value["schema"], "ores.otel.log/internal-diagnostic/v1");
    assert_eq!(value["component"], "sidecar");
    assert_eq!(value["operation"], operation);
    for forbidden in [
        "message",
        "error",
        "stack",
        "url",
        "authorization",
        "token",
        "password",
    ] {
        assert!(
            value.get(forbidden).is_none(),
            "forbidden {forbidden}: {text}"
        );
    }
}

#[test]
fn binary_listens_on_loopback_and_leaves_stdout_quiet() {
    let port = reserve_loopback_port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .env("ORES_OTEL_SIDECAR_BIND", &bind)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sidecar");

    let response = health_response_when_ready(port);
    assert!(response.contains("ores-otel-sidecar"));

    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.stdout.is_empty(),
        "stdout must stay protocol-free: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn kubelet_exec_probe_hits_loopback_and_stays_off_stdout() {
    let port = reserve_loopback_port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .env("ORES_OTEL_SIDECAR_BIND", &bind)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sidecar");

    // A successful TCP connect only proves that the kernel published the listen
    // socket. Require a complete 200 response before launching the one-shot
    // probe. Slow ARM Docker runners can accept a connection while the process
    // is not yet ready to complete the first request, so readiness must be an
    // HTTP-level bounded retry rather than a one-connect assertion.
    let warmup_response = health_response_when_ready(port);
    assert!(warmup_response.starts_with("HTTP/1.1 200 OK"));

    let probe = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .arg("probe")
        .env("ORES_OTEL_SIDECAR_BIND", &bind)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("exec probe");
    assert!(
        probe.status.success(),
        "probe stderr {:?}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(
        probe.stdout.is_empty(),
        "probe must not write a stdio protocol: {:?}",
        String::from_utf8_lossy(&probe.stdout)
    );

    let missing = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .arg("probe")
        .env("ORES_OTEL_SIDECAR_BIND", "127.0.0.1:1")
        .stdin(Stdio::null())
        .output()
        .expect("exec probe against a closed port");
    assert!(!missing.status.success());
    assert_closed_diagnostic(&missing.stderr, "sidecar_probe");

    let _ = child.kill();
    let _ = child.wait();
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
    assert!(
        !output.status.success(),
        "unspecified bind must fail closed"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout must stay protocol-free: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("0.0.0.0"), "{stderr}");
    assert!(!stderr.contains("non-loopback"), "{stderr}");
    assert_closed_diagnostic(&output.stderr, "sidecar_configure");
}

#[test]
fn hostile_argument_and_bind_values_never_reach_diagnostics() {
    let argument_secret = "Authorization=Bearer-synthetic-argv-secret";
    let unknown = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .arg(argument_secret)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sidecar with hostile argument");
    assert_eq!(unknown.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&unknown.stderr).contains(argument_secret));
    assert_closed_diagnostic(&unknown.stderr, "sidecar_configure");

    let bind_secret = "not-a-bind-Bearer-synthetic-env-secret";
    let invalid_bind = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"))
        .env("ORES_OTEL_SIDECAR_BIND", bind_secret)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sidecar with hostile bind");
    assert_eq!(invalid_bind.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&invalid_bind.stderr).contains(bind_secret));
    assert_closed_diagnostic(&invalid_bind.stderr, "sidecar_configure");
}
