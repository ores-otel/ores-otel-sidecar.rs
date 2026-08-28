#![forbid(unsafe_code)]

#[path = "../generated/rust/env.rs"]
mod env;

pub use env::{
    ALLOW_NON_LOOPBACK, ALLOW_NON_LOOPBACK_DEFAULT, BIND, BIND_DEFAULT, SERVICE, SidecarEnv,
};

/// Product identity for a sidecar that inherits this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarIdentity {
    pub service: &'static str,
    pub bind_env: &'static str,
    pub default_bind: &'static str,
}

impl SidecarIdentity {
    pub const DEFAULT_BIND: &'static str = env::BIND_DEFAULT;

    pub const ORES_OTEL: Self = Self::new(env::SERVICE, env::BIND);

    pub const fn new(service: &'static str, bind_env: &'static str) -> Self {
        Self {
            service,
            bind_env,
            default_bind: Self::DEFAULT_BIND,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ores_otel_identity_uses_generated_env_keys() {
        assert_eq!(SidecarIdentity::ORES_OTEL.service, SERVICE);
        assert_eq!(SidecarIdentity::ORES_OTEL.bind_env, BIND);
        assert_eq!(SidecarIdentity::ORES_OTEL.default_bind, BIND_DEFAULT);
        assert_eq!(SidecarIdentity::DEFAULT_BIND, "127.0.0.1:9090");
        assert_eq!(SidecarEnv::KEYS.bind, BIND);
        assert_eq!(SidecarEnv::KEYS.allow_non_loopback, ALLOW_NON_LOOPBACK);
        assert_eq!(ALLOW_NON_LOOPBACK_DEFAULT, "false");
    }
}
