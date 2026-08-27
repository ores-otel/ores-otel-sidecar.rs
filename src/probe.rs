#![forbid(unsafe_code)]

use serde_json::Value;

/// Optional product-specific health payload.
pub trait ProductProbe {
    fn extra_health(&self) -> Option<Value> {
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopProbe;

impl ProductProbe for NoopProbe {}
