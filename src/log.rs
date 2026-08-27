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
