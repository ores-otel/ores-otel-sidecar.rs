#![forbid(unsafe_code)]

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub listen: String,
}

impl SidecarConfig {
    pub fn from_env() -> Self {
        Self {
            listen: std::env::var("ORES_OTEL_SIDECAR_BIND")
                .unwrap_or_else(|_| "127.0.0.1:9090".into()),
        }
    }
}

