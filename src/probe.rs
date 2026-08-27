#![forbid(unsafe_code)]

use serde_json::Value;

/// Optional product-specific health payload and readiness.
pub trait ProductProbe {
    fn extra_health(&self) -> Option<Value> {
        None
    }

    fn ready(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopProbe;

impl ProductProbe for NoopProbe {}
