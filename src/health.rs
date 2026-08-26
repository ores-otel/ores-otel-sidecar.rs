#![forbid(unsafe_code)]

use serde::Serialize;

#[derive(Serialize)]
pub struct Health {
    pub ok: bool,
    pub service: &'static str,
}

pub fn current() -> Health {
    Health { ok: true, service: "ores-otel-sidecar" }
}

