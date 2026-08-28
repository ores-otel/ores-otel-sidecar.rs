#![forbid(unsafe_code)]

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::bind::{allow_non_loopback_from_env, parse_bind};
use crate::error::SidecarError;
use crate::hooks::{DefaultOverrides, SidecarOverrides};
use crate::identity::SidecarIdentity;

#[derive(Clone)]
pub struct SidecarConfig {
    pub identity: SidecarIdentity,
    pub listen: SocketAddr,
    overrides: Arc<dyn SidecarOverrides>,
}

impl fmt::Debug for SidecarConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SidecarConfig")
            .field("identity", &self.identity)
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}

impl SidecarConfig {
    pub fn from_env(identity: SidecarIdentity) -> Self {
        Self::from_env_with(identity, DefaultOverrides)
    }

    /// Build config from env, applying product overrides for bind / loopback policy.
    ///
    /// `runtime::run` then uses the same overrides as the default [`crate::ProductProbe`].
    pub fn from_env_with(
        identity: SidecarIdentity,
        overrides: impl SidecarOverrides + 'static,
    ) -> Self {
        match Self::try_from_env_with(identity, overrides) {
            Ok(config) => config,
            Err(err) => {
                crate::log::write_stderr(identity.service, "fatal", err.to_string(), false);
                std::process::exit(1);
            }
        }
    }

    pub fn try_from_env(identity: SidecarIdentity) -> Result<Self, SidecarError> {
        Self::try_from_env_with(identity, DefaultOverrides)
    }

    pub fn try_from_env_with(
        identity: SidecarIdentity,
        overrides: impl SidecarOverrides + 'static,
    ) -> Result<Self, SidecarError> {
        let raw =
            std::env::var(identity.bind_env).unwrap_or_else(|_| identity.default_bind.to_string());
        let bind = overrides.bind_raw(&raw);
        let allow = overrides.allow_non_loopback(allow_non_loopback_from_env());
        Self::from_bind_with(identity, &bind, allow, overrides)
    }

    pub fn from_bind(
        identity: SidecarIdentity,
        raw: &str,
        allow_non_loopback: bool,
    ) -> Result<Self, SidecarError> {
        Self::from_bind_with(identity, raw, allow_non_loopback, DefaultOverrides)
    }

    pub fn from_bind_with(
        identity: SidecarIdentity,
        raw: &str,
        allow_non_loopback: bool,
        overrides: impl SidecarOverrides + 'static,
    ) -> Result<Self, SidecarError> {
        Ok(Self {
            listen: parse_bind(raw, allow_non_loopback)?,
            identity,
            overrides: Arc::new(overrides),
        })
    }

    pub fn overrides(&self) -> &dyn SidecarOverrides {
        self.overrides.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::SidecarHooks;
    use crate::probe::ProductProbe;

    #[test]
    fn default_bind_is_loopback() {
        let cfg = SidecarConfig::from_bind(
            SidecarIdentity::ORES_OTEL,
            SidecarIdentity::DEFAULT_BIND,
            false,
        )
        .unwrap();
        assert!(cfg.listen.ip().is_loopback());
        assert_eq!(cfg.listen.port(), 9090);
    }

    #[test]
    fn try_from_env_reads_identity_bind_key() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let identity = SidecarIdentity::new("ores-otel-sidecar", "ORES_OTEL_SIDECAR_TEST_BIND");
        std::env::set_var(identity.bind_env, "127.0.0.1:19191");
        let cfg = SidecarConfig::try_from_env(identity).unwrap();
        assert_eq!(cfg.listen.port(), 19191);
        assert!(cfg.listen.ip().is_loopback());
        std::env::remove_var(identity.bind_env);
    }

    #[test]
    fn try_from_env_rejects_unspecified_even_when_override_is_off() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let identity = SidecarIdentity::new("ores-otel-sidecar", "ORES_OTEL_SIDECAR_TEST_BIND");
        let previous_allow = std::env::var(crate::identity::ALLOW_NON_LOOPBACK).ok();
        std::env::remove_var(crate::identity::ALLOW_NON_LOOPBACK);
        std::env::set_var(identity.bind_env, "0.0.0.0:9090");
        assert!(matches!(
            SidecarConfig::try_from_env(identity),
            Err(SidecarError::NonLoopbackBind { .. })
        ));
        std::env::remove_var(identity.bind_env);
        match previous_allow {
            Some(value) => std::env::set_var(crate::identity::ALLOW_NON_LOOPBACK, value),
            None => std::env::remove_var(crate::identity::ALLOW_NON_LOOPBACK),
        }
    }

    #[test]
    fn hooks_rewrite_bind_and_can_force_loopback_policy() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let identity = SidecarIdentity::new("ores-otel-sidecar", "ORES_OTEL_SIDECAR_TEST_BIND");
        std::env::set_var(identity.bind_env, "0.0.0.0:9090");
        std::env::set_var(crate::identity::ALLOW_NON_LOOPBACK, "1");
        let cfg = SidecarConfig::try_from_env_with(
            identity,
            SidecarHooks::new()
                .bind_raw(|_| "127.0.0.1:19192".into())
                .allow_non_loopback(|_| false),
        )
        .unwrap();
        assert_eq!(cfg.listen.port(), 19192);
        assert!(cfg.listen.ip().is_loopback());
        std::env::remove_var(identity.bind_env);
        std::env::remove_var(crate::identity::ALLOW_NON_LOOPBACK);
    }

    #[test]
    fn hooks_ready_is_the_default_probe() {
        let cfg = SidecarConfig::from_bind_with(
            SidecarIdentity::ORES_OTEL,
            "127.0.0.1:9090",
            false,
            SidecarHooks::new().ready(|| false),
        )
        .unwrap();
        assert!(!ProductProbe::ready(cfg.overrides()));
    }
}
