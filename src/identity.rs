#![forbid(unsafe_code)]

/// Product identity for a sidecar that inherits this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarIdentity {
    pub service: &'static str,
    pub bind_env: &'static str,
    pub default_bind: &'static str,
}

impl SidecarIdentity {
    pub const DEFAULT_BIND: &'static str = "127.0.0.1:9090";

    pub const ORES_OTEL: Self = Self::new("ores-otel-sidecar", "ORES_OTEL_SIDECAR_BIND");

    pub const fn new(service: &'static str, bind_env: &'static str) -> Self {
        Self {
            service,
            bind_env,
            default_bind: Self::DEFAULT_BIND,
        }
    }
}
