#![forbid(unsafe_code)]

use crate::identity::SidecarIdentity;

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub identity: SidecarIdentity,
    pub listen: String,
}

impl SidecarConfig {
    pub fn from_env(identity: SidecarIdentity) -> Self {
        Self {
            listen: std::env::var(identity.bind_env)
                .unwrap_or_else(|_| identity.default_bind.to_string()),
            identity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_bind_uses_default() {
        let cfg = SidecarConfig::from_env(SidecarIdentity::ORES_OTEL);
        assert_eq!(cfg.listen, SidecarIdentity::DEFAULT_BIND);
        assert_eq!(cfg.identity.service, "ores-otel-sidecar");
    }
}
