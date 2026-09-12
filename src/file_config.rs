#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::error::SidecarError;
use crate::runtime_updates::valid_keyspace_segment;
use crate::runtime_values::RuntimeValues;

pub const DEFAULT_CONFIG_PATH: &str = ".ores-sidecar.toml";
pub const CONFIG_PROTOCOL: &str = "ores.sidecar/config/v1";
pub const DEFAULT_STARTUP_CONFIG: &str = ".cli-flags.toml";
pub const MAX_CONFIG_FILE_BYTES: usize = 256 * 1024;
pub const MAX_SIDECARS: usize = 64;
pub const MAX_RECONCILE_SECONDS: u32 = 180;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum RuntimeUpdateProvider {
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "ores-redis-lru-cache")]
    OresRedisLruCache,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum RuntimeUpdateMode {
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "receive-only")]
    ReceiveOnly,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeUpdatePolicy {
    pub provider: RuntimeUpdateProvider,
    pub mode: RuntimeUpdateMode,
    pub poll_seconds: u32,
    pub namespace: String,
    pub cache: String,
}

impl RuntimeUpdatePolicy {
    pub fn accepts_keyspace(&self, namespace: &str, cache: &str) -> bool {
        self.namespace == namespace && self.cache == cache
    }

    pub fn is_enabled(&self) -> bool {
        matches!(
            (self.provider, self.mode),
            (
                RuntimeUpdateProvider::OresRedisLruCache,
                RuntimeUpdateMode::ReceiveOnly
            )
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeValueDefinition {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SidecarDefinition {
    pub id: String,
    pub path: Option<String>,
    pub startup_config: Option<String>,
    pub runtime_updates: RuntimeUpdatePolicy,
    #[serde(default)]
    pub runtime_mutable: Vec<String>,
    #[serde(default)]
    pub values: Vec<RuntimeValueDefinition>,
}

impl SidecarDefinition {
    pub fn startup_config(&self) -> &str {
        self.startup_config
            .as_deref()
            .unwrap_or(DEFAULT_STARTUP_CONFIG)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OresSidecarFile {
    pub schema: String,
    pub sidecars: Vec<SidecarDefinition>,
}

#[derive(Clone, Debug)]
pub struct ResolvedSidecarFile {
    pub definition: SidecarDefinition,
    pub runtime_values: RuntimeValues,
}

impl OresSidecarFile {
    pub fn parse(input: &str) -> Result<Self, SidecarError> {
        if input.len() > MAX_CONFIG_FILE_BYTES {
            return Err(SidecarError::InvalidConfig {
                reason: "sidecar config exceeds the size limit",
            });
        }
        let parsed: Self = basic_toml::from_str(input).map_err(|_| SidecarError::InvalidConfig {
            reason: "sidecar config is not valid TOML",
        })?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, SidecarError> {
        let bytes = fs::read(path)?;
        if bytes.len() > MAX_CONFIG_FILE_BYTES {
            return Err(SidecarError::InvalidConfig {
                reason: "sidecar config exceeds the size limit",
            });
        }
        let input = std::str::from_utf8(&bytes).map_err(|_| SidecarError::InvalidConfig {
            reason: "sidecar config must be UTF-8",
        })?;
        Self::parse(input)
    }

    pub fn load_optional(path: impl AsRef<Path>) -> Result<Option<Self>, SidecarError> {
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.len() > MAX_CONFIG_FILE_BYTES {
                    return Err(SidecarError::InvalidConfig {
                        reason: "sidecar config exceeds the size limit",
                    });
                }
                let input =
                    std::str::from_utf8(&bytes).map_err(|_| SidecarError::InvalidConfig {
                        reason: "sidecar config must be UTF-8",
                    })?;
                Self::parse(input).map(Some)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn resolve(&self, service: &str) -> Result<ResolvedSidecarFile, SidecarError> {
        let definition = self
            .sidecars
            .iter()
            .find(|sidecar| sidecar.id == service)
            .cloned()
            .ok_or_else(|| SidecarError::MissingSidecar {
                service: service.to_owned(),
            })?;
        let runtime_values = RuntimeValues::new(
            definition.runtime_mutable.iter().cloned(),
            definition
                .values
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone())),
        )?;
        Ok(ResolvedSidecarFile {
            definition,
            runtime_values,
        })
    }

    fn validate(&self) -> Result<(), SidecarError> {
        if self.schema != CONFIG_PROTOCOL {
            return Err(SidecarError::InvalidConfig {
                reason: "unsupported sidecar config schema",
            });
        }
        if self.sidecars.is_empty() || self.sidecars.len() > MAX_SIDECARS {
            return Err(SidecarError::InvalidConfig {
                reason: "sidecar list must be non-empty and bounded",
            });
        }

        // Every sidecar is checked in order; the fold carries the identities seen
        // so far as a value and stops at the first invalid entry.
        self.sidecars
            .iter()
            .try_fold(BTreeSet::<&str>::new(), |ids, sidecar| {
                validate_identity(&sidecar.id)?;
                if ids.contains(sidecar.id.as_str()) {
                    return Err(SidecarError::InvalidConfig {
                        reason: "sidecar identities must be unique",
                    });
                }
                validate_sidecar_shape(sidecar)?;
                Ok(ids
                    .into_iter()
                    .chain(std::iter::once(sidecar.id.as_str()))
                    .collect())
            })
            .map(|_| ())
    }
}

/// The per-sidecar checks that do not depend on the other entries.
fn validate_sidecar_shape(sidecar: &SidecarDefinition) -> Result<(), SidecarError> {
    if sidecar.path.as_deref().is_some_and(str::is_empty)
        || sidecar.startup_config.as_deref().is_some_and(str::is_empty)
    {
        return Err(SidecarError::InvalidConfig {
            reason: "sidecar path fields cannot be empty",
        });
    }
    validate_runtime_policy(&sidecar.runtime_updates)?;
    RuntimeValues::new(
        sidecar.runtime_mutable.iter().cloned(),
        sidecar
            .values
            .iter()
            .map(|entry| (entry.key.clone(), entry.value.clone())),
    )?;
    Ok(())
}

fn validate_identity(id: &str) -> Result<(), SidecarError> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(SidecarError::InvalidConfig {
            reason: "sidecar identity has an invalid shape",
        })
    }
}

fn validate_runtime_policy(policy: &RuntimeUpdatePolicy) -> Result<(), SidecarError> {
    if !(60..=MAX_RECONCILE_SECONDS).contains(&policy.poll_seconds)
        || !valid_keyspace_segment(&policy.namespace)
        || !valid_keyspace_segment(&policy.cache)
    {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime update policy is outside Redis-LRU-compatible bounds",
        });
    }

    let compatible = matches!(
        (policy.provider, policy.mode),
        (RuntimeUpdateProvider::Disabled, RuntimeUpdateMode::Disabled)
            | (
                RuntimeUpdateProvider::OresRedisLruCache,
                RuntimeUpdateMode::ReceiveOnly
            )
    );
    if !compatible {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime update provider and mode are incompatible",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_config_is_valid_and_resolves_shared_runtime() {
        let config = OresSidecarFile::parse(include_str!("../.ores-sidecar.toml")).unwrap();
        let resolved = config.resolve("ores-otel-sidecar").unwrap();
        assert_eq!(resolved.definition.path.as_deref(), Some("."));
        assert_eq!(resolved.definition.startup_config(), ".cli-flags.toml");
        assert!(resolved.runtime_values.is_mutable("LOG_FILTER"));
        assert!(resolved.definition.runtime_updates.is_enabled());
        assert!(resolved
            .definition
            .runtime_updates
            .accepts_keyspace("ores-otel-sidecar", "runtime-env"));
    }

    #[test]
    fn one_file_can_address_multiple_sidecars_independently() {
        let config = OresSidecarFile::parse(include_str!(
            "../tests/fixtures/sidecar/multi.toml"
        ))
        .unwrap();
        assert_eq!(config.sidecars.len(), 2);
        let first = config.resolve("first-sidecar").unwrap();
        assert!(!first.definition.runtime_updates.is_enabled());
        let second = config.resolve("second-sidecar").unwrap();
        assert_eq!(second.definition.path.as_deref(), Some("../second"));
        assert!(second.runtime_values.is_mutable("FEATURE_FLAGS_JSON"));
        assert!(second.definition.runtime_updates.is_enabled());
        assert!(second
            .definition
            .runtime_updates
            .accepts_keyspace("second-sidecar", "runtime-env"));
    }

    #[test]
    fn duplicate_sidecar_identity_fails_closed() {
        assert!(OresSidecarFile::parse(include_str!(
            "../tests/fixtures/sidecar/duplicate-id.toml"
        ))
        .is_err());
    }

    #[test]
    fn secret_like_runtime_key_fails_closed() {
        assert!(OresSidecarFile::parse(include_str!(
            "../tests/fixtures/sidecar/forbidden-runtime-key.toml"
        ))
        .is_err());
    }

    #[test]
    fn redis_keyspace_and_poll_limits_fail_closed() {
        for input in [
            include_str!("../tests/fixtures/sidecar/invalid-keyspace.toml"),
            include_str!("../tests/fixtures/sidecar/poll-too-slow.toml"),
        ] {
            assert!(OresSidecarFile::parse(input).is_err());
        }
    }

    #[test]
    fn unknown_field_and_malformed_toml_fail_closed() {
        for input in [
            include_str!("../tests/fixtures/sidecar/unknown-field.toml"),
            include_str!("../tests/fixtures/sidecar/malformed.toml"),
        ] {
            assert!(OresSidecarFile::parse(input).is_err());
        }
    }
}
