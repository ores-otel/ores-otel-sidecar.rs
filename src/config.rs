#![forbid(unsafe_code)]

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use crate::bind::{allow_non_loopback_from_env, parse_bind};
use crate::error::SidecarError;
use crate::file_config::{OresSidecarFile, RuntimeUpdatePolicy};
use crate::hooks::{DefaultOverrides, SidecarOverrides};
use crate::identity::SidecarIdentity;
use crate::log::{Operation, Outcome, Severity};
use crate::runtime_values::RuntimeValues;

#[derive(Clone)]
pub struct SidecarConfig {
    pub identity: SidecarIdentity,
    pub listen: SocketAddr,
    overrides: Arc<dyn SidecarOverrides>,
    runtime_values: RuntimeValues,
    runtime_updates: Option<RuntimeUpdatePolicy>,
}

impl fmt::Debug for SidecarConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SidecarConfig")
            .field("identity", &self.identity)
            .field("listen", &self.listen)
            .field("runtime_updates", &self.runtime_updates)
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
            Err(_error) => {
                crate::log::write_stderr(
                    identity.service,
                    Severity::Fatal,
                    Operation::SidecarConfigure,
                    Outcome::Rejected,
                    false,
                );
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
            runtime_values: RuntimeValues::default(),
            runtime_updates: None,
        })
    }

    /// Attach the entry matching this sidecar identity from one single- or
    /// multi-sidecar `.ores-sidecar.toml` file.
    pub fn with_sidecar_file(self, path: impl AsRef<Path>) -> Result<Self, SidecarError> {
        self.with_loaded_sidecar_file(OresSidecarFile::load(path)?)
    }

    /// Attach the repository-root sidecar file when present. A present but
    /// malformed file still fails closed; only `NotFound` is treated as absent.
    pub fn with_optional_sidecar_file(
        self,
        path: impl AsRef<Path>,
    ) -> Result<Self, SidecarError> {
        match OresSidecarFile::load_optional(path)? {
            Some(config) => self.with_loaded_sidecar_file(config),
            None => Ok(self),
        }
    }

    pub fn runtime_values(&self) -> RuntimeValues {
        self.runtime_values.clone()
    }

    pub fn runtime_update_policy(&self) -> Option<&RuntimeUpdatePolicy> {
        self.runtime_updates.as_ref()
    }

    pub fn overrides(&self) -> &dyn SidecarOverrides {
        self.overrides.as_ref()
    }

    fn with_loaded_sidecar_file(mut self, file: OresSidecarFile) -> Result<Self, SidecarError> {
        let resolved = file.resolve(self.identity.service)?;
        self.runtime_updates = Some(resolved.definition.runtime_updates);
        self.runtime_values = resolved.runtime_values;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_config::DEFAULT_CONFIG_PATH;
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
    fn root_sidecar_file_attaches_runtime_policy() {
        let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), DEFAULT_CONFIG_PATH);
        let cfg = SidecarConfig::from_bind(
            SidecarIdentity::ORES_OTEL,
            SidecarIdentity::DEFAULT_BIND,
            false,
        )
        .unwrap()
        .with_sidecar_file(path)
        .unwrap();
        assert!(cfg.runtime_values().is_mutable("LOG_FILTER"));
        assert_eq!(cfg.runtime_update_policy().unwrap().poll_seconds, 180);
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
