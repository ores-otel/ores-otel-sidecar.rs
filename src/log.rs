#![forbid(unsafe_code)]

use serde::Serialize;

#[derive(Serialize, Clone, Debug)]
pub struct LogLine {
    pub ok: bool,
    pub service: &'static str,
    pub event: &'static str,
    pub message: String,
}

pub fn write_stderr(
    service: &'static str,
    event: &'static str,
    message: impl Into<String>,
    ok: bool,
) {
    let line = LogLine {
        ok,
        service,
        event,
        message: message.into(),
    };
    let encoded = serde_json::to_string(&line).unwrap_or_else(|_| {
        format!(r#"{{"ok":false,"service":"{service}","event":"log_encode_failed"}}"#)
    });
    eprintln!("{encoded}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_is_json_without_secrets() {
        let encoded = serde_json::to_string(&LogLine {
            ok: true,
            service: "ores-otel-sidecar",
            event: "listen",
            message: "http://127.0.0.1:9090/healthz".into(),
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["service"], "ores-otel-sidecar");
        assert_eq!(value["event"], "listen");
        for secret in ["token", "password", "authorization"] {
            assert!(!encoded.contains(secret), "{encoded}");
        }
    }
}
