#![forbid(unsafe_code)]

use std::io::{self, Write};

use serde::Serialize;
use time::OffsetDateTime;

const SCHEMA: &str = "ores.otel.log/internal-diagnostic/v1";
const FALLBACK_SERVICE: &str = "ores-otel-sidecar";

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warn,
    Error,
    Fatal,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    SidecarConfigure,
    SidecarListen,
    SidecarProbe,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Failed,
    Rejected,
}

#[derive(Serialize, Clone, Debug)]
struct InternalDiagnostic {
    schema: &'static str,
    timestamp: String,
    service: &'static str,
    severity: Severity,
    component: &'static str,
    operation: Operation,
    outcome: Outcome,
    retryable: bool,
    attempt: u8,
    dropped: u16,
    suppressed: u16,
    #[serde(rename = "suppressionSaturated")]
    suppression_saturated: bool,
}

fn valid_service(service: &str) -> bool {
    !service.is_empty()
        && service.len() <= 128
        && service.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && b"._-".contains(&byte))
        })
}

fn timestamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

fn write_to_at(
    writer: &mut impl Write,
    service: &'static str,
    severity: Severity,
    operation: Operation,
    outcome: Outcome,
    retryable: bool,
    now: OffsetDateTime,
) -> io::Result<()> {
    let diagnostic = InternalDiagnostic {
        schema: SCHEMA,
        timestamp: timestamp(now),
        service: if valid_service(service) {
            service
        } else {
            FALLBACK_SERVICE
        },
        severity,
        component: "sidecar",
        operation,
        outcome,
        retryable,
        attempt: 0,
        dropped: 0,
        suppressed: 0,
        suppression_saturated: false,
    };
    serde_json::to_writer(&mut *writer, &diagnostic).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Emit one closed, payload-free diagnostic directly to process stderr.
///
/// Writer and clock failures are intentionally swallowed: an observability
/// failure must not recurse, panic, or change the sidecar's intended exit code.
pub fn write_stderr(
    service: &'static str,
    severity: Severity,
    operation: Operation,
    outcome: Outcome,
    retryable: bool,
) {
    let mut stderr = io::stderr().lock();
    let _ = write_to_at(
        &mut stderr,
        service,
        severity,
        operation,
        outcome,
        retryable,
        OffsetDateTime::now_utc(),
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn fixed_now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_788_027_200).unwrap()
    }

    #[test]
    fn diagnostic_has_the_exact_closed_schema() {
        let mut output = Vec::new();
        write_to_at(
            &mut output,
            "ores-otel-sidecar",
            Severity::Fatal,
            Operation::SidecarListen,
            Outcome::Failed,
            true,
            fixed_now(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            BTreeSet::from([
                "attempt",
                "component",
                "dropped",
                "operation",
                "outcome",
                "retryable",
                "schema",
                "service",
                "severity",
                "suppressed",
                "suppressionSaturated",
                "timestamp",
            ])
        );
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["component"], "sidecar");
        assert_eq!(value["operation"], "sidecar_listen");
        assert_eq!(value["severity"], "fatal");
        assert_eq!(value["outcome"], "failed");
        assert_eq!(value["attempt"], 0);
        assert!(output.len() < 512, "diagnostic must remain bounded");
    }

    #[test]
    fn invalid_service_is_replaced_without_serializing_credential_text() {
        let mut output = Vec::new();
        write_to_at(
            &mut output,
            "Authorization:Bearer-secret",
            Severity::Error,
            Operation::SidecarConfigure,
            Outcome::Rejected,
            false,
            fixed_now(),
        )
        .unwrap();
        let encoded = String::from_utf8(output).unwrap();
        assert!(encoded.contains(FALLBACK_SERVICE), "{encoded}");
        assert!(!encoded.contains("Authorization"), "{encoded}");
        assert!(!encoded.contains("Bearer-secret"), "{encoded}");
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic writer failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic writer failure"))
        }
    }

    #[test]
    fn writer_failure_is_returned_without_recursion_or_panic() {
        let result = write_to_at(
            &mut FailingWriter,
            "ores-otel-sidecar",
            Severity::Fatal,
            Operation::SidecarProbe,
            Outcome::Failed,
            false,
            fixed_now(),
        );
        assert!(result.is_err());
    }
}
