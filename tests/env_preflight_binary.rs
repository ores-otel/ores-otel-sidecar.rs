#![forbid(unsafe_code)]

use std::process::{Command, Stdio};

fn run(args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ores-otel-sidecar"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run sidecar preflight")
}

#[test]
fn preflight_accepts_materialized_defaults_without_starting_listener() {
    let output = run(&["preflight"], &[]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn raw_environment_boolean_aliases_fail_closed() {
    for value in ["TRUE", "yes", "1", "0", " true", "false "] {
        let output = run(
            &["preflight"],
            &[("ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK", value)],
        );
        assert_eq!(output.status.code(), Some(1), "value={value:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains(value), "raw value leaked: {stderr}");
        assert!(stderr.contains("sidecar_configure"), "{stderr}");
    }
}

#[test]
fn argv_boolean_alias_is_normalized_before_preflight() {
    let output = run(&["preflight", "--allow-non-loopback=yes"], &[]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn malformed_bind_fails_during_startup_semantic_validation() {
    let marker = "not-a-bind-synthetic-secret-never-reflect";
    let output = run(&["preflight"], &[("ORES_OTEL_SIDECAR_BIND", marker)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(marker));
    assert!(stderr.contains("sidecar_configure"));
}
