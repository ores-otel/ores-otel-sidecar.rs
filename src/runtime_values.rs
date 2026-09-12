#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use crate::error::SidecarError;

pub const MAX_RUNTIME_KEYS: usize = 64;
pub const MAX_RUNTIME_KEY_BYTES: usize = 128;
pub const MAX_RUNTIME_VALUE_BYTES: usize = 16 * 1024;

/// One atomic runtime override. `None` removes the backend override and restores
/// the immutable configured baseline value, when one exists.
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
/// `ores-redis-lru-cache`. The baseline is immutable; backend updates live in a
/// separate override map and never mutate the process environment.
#[derive(Clone, Debug)]
pub struct RuntimeValues {
    mutable: Arc<BTreeSet<String>>,
    baseline: Arc<BTreeMap<String, String>>,
    overrides: Arc<RwLock<BTreeMap<String, String>>>,
}

impl Default for RuntimeValues {
    fn default() -> Self {
        Self {
            mutable: Arc::new(BTreeSet::new()),
            baseline: Arc::new(BTreeMap::new()),
            overrides: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }
}

impl RuntimeValues {
    pub(crate) fn new(
        mutable: impl IntoIterator<Item = String>,
        initial: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, SidecarError> {
        // Both tables are folds that return a new collection per admitted entry;
        // the first rejected key stops the fold with its error.
        let allowed =
            mutable
                .into_iter()
                .try_fold(BTreeSet::new(), |allowed: BTreeSet<String>, key| {
                    validate_runtime_key(&key)?;
                    if allowed.len() >= MAX_RUNTIME_KEYS || allowed.contains(&key) {
                        return Err(SidecarError::InvalidConfig {
                            reason: "runtime mutable keys must be unique and bounded",
                        });
                    }
                    Ok(allowed.into_iter().chain(std::iter::once(key)).collect())
                })?;

        let baseline = initial.into_iter().try_fold(
            BTreeMap::new(),
            |baseline: BTreeMap<String, String>, (key, value)| {
                validate_update(&allowed, &key, Some(&value))?;
                if baseline.len() >= MAX_RUNTIME_KEYS || baseline.contains_key(&key) {
                    return Err(SidecarError::InvalidConfig {
                        reason: "runtime baseline values must be unique and bounded",
                    });
                }
                Ok(baseline
                    .into_iter()
                    .chain(std::iter::once((key, value)))
                    .collect())
            },
        )?;

        Ok(Self {
            mutable: Arc::new(allowed),
            baseline: Arc::new(baseline),
            overrides: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    pub fn is_mutable(&self, key: &str) -> bool {
        self.mutable.contains(key)
    }

    pub fn mutable_keys(&self) -> BTreeSet<String> {
        self.mutable.as_ref().clone()
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, SidecarError> {
        let overrides = self
            .overrides
            .read()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        Ok(overrides
            .get(key)
            .cloned()
            .or_else(|| self.baseline.get(key).cloned()))
    }

    pub fn snapshot(&self) -> Result<BTreeMap<String, String>, SidecarError> {
        let overrides = self
            .overrides
            .read()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        Ok(layered(&self.baseline, &overrides))
    }

    pub(crate) fn validate_patch(&self, updates: &[RuntimeValueUpdate]) -> Result<(), SidecarError> {
        if updates.len() > MAX_RUNTIME_KEYS {
            return Err(SidecarError::InvalidConfig {
                reason: "runtime patch exceeds the bounded key count",
            });
        }

        updates
            .iter()
            .try_fold(BTreeSet::<&str>::new(), |seen, update| {
                if seen.contains(update.key.as_str()) {
                    return Err(SidecarError::RuntimeUpdateRejected {
                        key: update.key.clone(),
                    });
                }
                validate_update(&self.mutable, &update.key, update.value.as_deref())?;
                Ok(seen
                    .into_iter()
                    .chain(std::iter::once(update.key.as_str()))
                    .collect())
            })
            .map(|_| ())
    }

    /// Apply a bounded patch atomically. Every key/value is validated before the
    /// write lock is taken, so a rejected update cannot partially mutate state.
    pub fn apply_patch(&self, updates: &[RuntimeValueUpdate]) -> Result<(), SidecarError> {
        self.validate_patch(updates)?;
        let mut overrides = self
            .overrides
            .write()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        // The RwLock is the one stateful holder: the patched map is computed as a
        // new value from the current one and swapped in with a single assignment.
        let next = patched(&overrides, updates);
        *overrides = next;
        Ok(())
    }

    /// Atomically replace the entire backend override map. This is the snapshot
    /// and `replace` primitive, so replacing 64 old keys with 64 new keys stays
    /// within the configured 64-key state bound instead of being miscounted as
    /// a 128-operation patch.
    pub(crate) fn replace_overrides(
        &self,
        next: &BTreeMap<String, String>,
    ) -> Result<(), SidecarError> {
        if next.len() > MAX_RUNTIME_KEYS {
            return Err(SidecarError::InvalidConfig {
                reason: "runtime override set exceeds the bounded key count",
            });
        }
        for (key, value) in next {
            validate_update(&self.mutable, key, Some(value))?;
        }
        let mut overrides = self
            .overrides
            .write()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        *overrides = next.clone();
        Ok(())
    }
}

/// `base` with `overlay` applied on top: overlay entries win, shadowed base
/// entries are dropped rather than overwritten, and neither input is touched.
fn layered(
    base: &BTreeMap<String, String>,
    overlay: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    base.iter()
        .filter(|(key, _)| !overlay.contains_key(*key))
        .chain(overlay.iter())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// `current` with a validated patch applied: every patched key is dropped from
/// the current map, then the keys the patch sets are added back with their new
/// values (a `None` therefore removes the override).
fn patched(
    current: &BTreeMap<String, String>,
    updates: &[RuntimeValueUpdate],
) -> BTreeMap<String, String> {
    current
        .iter()
        .filter(|(key, _)| !updates.iter().any(|update| update.key == **key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .chain(updates.iter().filter_map(|update| {
            update
                .value
                .as_ref()
                .map(|value| (update.key.clone(), value.clone()))
        }))
        .collect()
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
    fn removing_backend_override_restores_baseline() {
        let values = RuntimeValues::new(
            ["LOG_FILTER".to_owned()],
            [("LOG_FILTER".to_owned(), "info".to_owned())],
        )
        .unwrap();
        values
            .apply_patch(&[RuntimeValueUpdate::set("LOG_FILTER", "debug")])
            .unwrap();
        assert_eq!(values.get("LOG_FILTER").unwrap().as_deref(), Some("debug"));
        values
            .apply_patch(&[RuntimeValueUpdate::remove("LOG_FILTER")])
            .unwrap();
        assert_eq!(values.get("LOG_FILTER").unwrap().as_deref(), Some("info"));
    }

    #[test]
    fn full_override_replacement_is_bounded_by_state_not_operation_count() {
        let keys = (0..MAX_RUNTIME_KEYS)
            .map(|index| format!("FEATURE_{index}"))
            .collect::<Vec<_>>();
        let values = RuntimeValues::new(keys.clone(), std::iter::empty::<(String, String)>()).unwrap();
        let first = keys
            .iter()
            .map(|key| (key.clone(), "one".to_owned()))
            .collect::<BTreeMap<_, _>>();
        values.replace_overrides(&first).unwrap();
        let second = keys
            .iter()
            .map(|key| (key.clone(), "two".to_owned()))
            .collect::<BTreeMap<_, _>>();
        values.replace_overrides(&second).unwrap();
        assert_eq!(values.get("FEATURE_63").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn clones_share_overrides_but_not_process_environment() {
        let values = RuntimeValues::new(
            ["LOG_FILTER".to_owned()],
            std::iter::empty::<(String, String)>(),
        )
        .unwrap();
        let clone = values.clone();
        values
            .apply_patch(&[RuntimeValueUpdate::set("LOG_FILTER", "trace")])
            .unwrap();
        assert_eq!(clone.get("LOG_FILTER").unwrap().as_deref(), Some("trace"));
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
