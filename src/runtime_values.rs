#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::error::SidecarError;

pub const MAX_RUNTIME_KEYS: usize = 64;
pub const MAX_RUNTIME_KEY_BYTES: usize = 128;
pub const MAX_RUNTIME_VALUE_BYTES: usize = 16 * 1024;

/// One atomic runtime update. `None` removes the current value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeValueUpdate {
    pub key: String,
    pub value: Option<String>,
}

impl RuntimeValueUpdate {
    pub fn set(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: Some(value.into()),
        }
    }

    pub fn remove(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: None,
        }
    }
}

/// Shared, cloneable runtime-value handle intended for adapters such as
/// `ores-redis-lru-cache`. It never mutates the process environment.
#[derive(Clone, Debug)]
pub struct RuntimeValues {
    mutable: Arc<BTreeSet<String>>,
    values: Arc<RwLock<BTreeMap<String, String>>>,
}

impl Default for RuntimeValues {
    fn default() -> Self {
        Self {
            mutable: Arc::new(BTreeSet::new()),
            values: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
}

impl RuntimeValues {
    pub(crate) fn new(
        mutable: impl IntoIterator<Item = String>,
        initial: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, SidecarError> {
        let mut allowed = BTreeSet::new();
        for key in mutable {
            validate_runtime_key(&key)?;
            if allowed.len() >= MAX_RUNTIME_KEYS || !allowed.insert(key) {
                return Err(SidecarError::InvalidConfig {
                    reason: "runtime mutable keys must be unique and bounded",
                });
            }
        }

        let mut values = BTreeMap::new();
        for (key, value) in initial {
            validate_update(&allowed, &key, Some(&value))?;
            if values.len() >= MAX_RUNTIME_KEYS || values.insert(key, value).is_some() {
                return Err(SidecarError::InvalidConfig {
                    reason: "runtime values must be unique and bounded",
                });
            }
        }

        Ok(Self {
            mutable: Arc::new(allowed),
            values: Arc::new(RwLock::new(values)),
        })
    }

    pub fn is_mutable(&self, key: &str) -> bool {
        self.mutable.contains(key)
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, SidecarError> {
        let values = self
            .values
            .read()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        Ok(values.get(key).cloned())
    }

    pub fn snapshot(&self) -> Result<BTreeMap<String, String>, SidecarError> {
        self.values
            .read()
            .map(|values| values.clone())
            .map_err(|_| SidecarError::RuntimeStateUnavailable)
    }

    /// Apply a bounded patch atomically. Every key/value is validated before the
    /// write lock is taken, so a rejected update cannot partially mutate state.
    pub fn apply_patch(&self, updates: &[RuntimeValueUpdate]) -> Result<(), SidecarError> {
        if updates.len() > MAX_RUNTIME_KEYS {
            return Err(SidecarError::InvalidConfig {
                reason: "runtime patch exceeds the bounded key count",
            });
        }

        let mut seen = BTreeSet::new();
        for update in updates {
            if !seen.insert(update.key.as_str()) {
                return Err(SidecarError::RuntimeUpdateRejected {
                    key: update.key.clone(),
                });
            }
            validate_update(&self.mutable, &update.key, update.value.as_deref())?;
        }

        let mut values = self
            .values
            .write()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        for update in updates {
            match &update.value {
                Some(value) => {
                    values.insert(update.key.clone(), value.clone());
                }
                None => {
                    values.remove(&update.key);
                }
            }
        }
        Ok(())
    }
}

fn validate_update(
    mutable: &BTreeSet<String>,
    key: &str,
    value: Option<&str>,
) -> Result<(), SidecarError> {
    validate_runtime_key(key)?;
    if !mutable.contains(key) {
        return Err(SidecarError::RuntimeUpdateRejected {
            key: key.to_owned(),
        });
    }
    if value.is_some_and(|value| value.len() > MAX_RUNTIME_VALUE_BYTES) {
        return Err(SidecarError::RuntimeUpdateRejected {
            key: key.to_owned(),
        });
    }
    Ok(())
}

fn validate_runtime_key(key: &str) -> Result<(), SidecarError> {
    let bytes = key.as_bytes();
    let valid_shape = !bytes.is_empty()
        && bytes.len() <= MAX_RUNTIME_KEY_BYTES
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_');
    if !valid_shape || is_sensitive_runtime_key(key) {
        return Err(SidecarError::RuntimeUpdateRejected {
            key: key.to_owned(),
        });
    }
    Ok(())
}

pub fn is_sensitive_runtime_key(key: &str) -> bool {
    if matches!(key, "REDIS_URL" | "DATABASE_URL")
        || key.ends_with("_DSN")
        || key.ends_with("_KEY")
    {
        return true;
    }

    key.split('_').any(|part| {
        matches!(
            part,
            "PASSWORD"
                | "PASS"
                | "TOKEN"
                | "SECRET"
                | "CREDENTIAL"
                | "CREDENTIALS"
                | "COOKIE"
                | "PRIVATE"
                | "AUTHORIZATION"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_is_allowlisted_and_atomic() {
        let values = RuntimeValues::new(
            ["LOG_FILTER".to_owned(), "REQUEST_TIMEOUT_MS".to_owned()],
            [("LOG_FILTER".to_owned(), "info".to_owned())],
        )
        .unwrap();

        values
            .apply_patch(&[
                RuntimeValueUpdate::set("LOG_FILTER", "debug"),
                RuntimeValueUpdate::set("REQUEST_TIMEOUT_MS", "5000"),
            ])
            .unwrap();
        assert_eq!(values.get("LOG_FILTER").unwrap().as_deref(), Some("debug"));

        assert!(values
            .apply_patch(&[
                RuntimeValueUpdate::set("LOG_FILTER", "trace"),
                RuntimeValueUpdate::set("NOT_ALLOWED", "1"),
            ])
            .is_err());
        assert_eq!(values.get("LOG_FILTER").unwrap().as_deref(), Some("debug"));
    }

    #[test]
    fn secret_like_keys_are_never_runtime_mutable() {
        for key in [
            "DATABASE_URL",
            "REDIS_URL",
            "GITHUB_TOKEN",
            "API_KEY",
            "SESSION_COOKIE",
        ] {
            assert!(is_sensitive_runtime_key(key), "{key}");
        }
        assert!(!is_sensitive_runtime_key("LOG_FILTER"));
    }
}
