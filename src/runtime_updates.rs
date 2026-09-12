#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::error::SidecarError;
use crate::runtime_values::{RuntimeValueUpdate, RuntimeValues, MAX_RUNTIME_KEYS};

/// Largest integer represented exactly by every supported runtime, including JavaScript.
pub const MAX_SAFE_RUNTIME_REVISION: u64 = 9_007_199_254_740_991;
pub const MAX_RUNTIME_KEYSPACE_SEGMENT_BYTES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeUpdateTarget {
    pub namespace: String,
    pub cache: String,
}

impl RuntimeUpdateTarget {
    pub fn new(
        namespace: impl Into<String>,
        cache: impl Into<String>,
    ) -> Result<Self, SidecarError> {
        let target = Self {
            namespace: namespace.into(),
            cache: cache.into(),
        };
        if !valid_keyspace_segment(&target.namespace) || !valid_keyspace_segment(&target.cache) {
            return Err(SidecarError::InvalidConfig {
                reason: "runtime update keyspace is invalid",
            });
        }
        Ok(target)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeUpdateOperation {
    Upsert,
    Delete,
    Replace,
    Invalidate,
    Resync,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeUpdateBatch {
    pub namespace: String,
    pub cache: String,
    pub revision: u64,
    pub operation: RuntimeUpdateOperation,
    pub entries: BTreeMap<String, String>,
    pub keys: Vec<String>,
}

impl RuntimeUpdateBatch {
    pub fn new(
        namespace: impl Into<String>,
        cache: impl Into<String>,
        revision: u64,
        operation: RuntimeUpdateOperation,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            cache: cache.into(),
            revision,
            operation,
            entries: BTreeMap::new(),
            keys: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeUpdateOutcome {
    Applied { revision: u64 },
    Duplicate { revision: u64 },
    StaleSnapshotIgnored { current: u64, incoming: u64 },
    ReconcileRequired { current: u64, incoming: u64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeUpdateState {
    pub revision: u64,
    pub stale: bool,
    pub backend_key_count: usize,
}

/// The controller's state as an immutable value. Reductions compute the next
/// state with the methods below and the `Mutex` holder swaps it in; no field is
/// ever assigned through the lock guard.
#[derive(Clone, Debug, Default)]
struct RuntimeUpdateInner {
    revision: u64,
    stale: bool,
    backend_keys: BTreeSet<String>,
}

/// What an ordered event does to the controller, decided without touching any
/// state or value.
enum Admission {
    /// The event is not applied; `next` is the state to keep (possibly marked stale).
    Rejected {
        next: RuntimeUpdateInner,
        outcome: RuntimeUpdateOutcome,
    },
    /// The event is in sequence and may be applied to values.
    Admitted,
}

impl RuntimeUpdateInner {
    fn marked_stale(&self) -> Self {
        Self {
            stale: true,
            ..self.clone()
        }
    }

    /// The state after an authoritative revision is installed with `backend_keys`.
    fn advanced(revision: u64, backend_keys: BTreeSet<String>) -> Self {
        Self {
            revision,
            stale: false,
            backend_keys,
        }
    }

    fn admit(&self, batch: &RuntimeUpdateBatch) -> Admission {
        let reconcile_required = RuntimeUpdateOutcome::ReconcileRequired {
            current: self.revision,
            incoming: batch.revision,
        };
        if batch.revision <= self.revision {
            return Admission::Rejected {
                next: self.clone(),
                outcome: RuntimeUpdateOutcome::Duplicate {
                    revision: self.revision,
                },
            };
        }
        if self.stale {
            return Admission::Rejected {
                next: self.clone(),
                outcome: reconcile_required,
            };
        }
        if batch.revision != self.revision.saturating_add(1)
            || batch.operation == RuntimeUpdateOperation::Resync
        {
            return Admission::Rejected {
                next: self.marked_stale(),
                outcome: reconcile_required,
            };
        }
        Admission::Admitted
    }

    /// The backend key set after an admitted event is applied.
    fn backend_keys_after(&self, batch: &RuntimeUpdateBatch) -> BTreeSet<String> {
        match batch.operation {
            RuntimeUpdateOperation::Upsert => self
                .backend_keys
                .iter()
                .chain(batch.entries.keys())
                .cloned()
                .collect(),
            RuntimeUpdateOperation::Delete => self
                .backend_keys
                .iter()
                .filter(|key| !batch.keys.contains(*key))
                .cloned()
                .collect(),
            RuntimeUpdateOperation::Replace => batch.entries.keys().cloned().collect(),
            RuntimeUpdateOperation::Invalidate => BTreeSet::new(),
            RuntimeUpdateOperation::Resync => unreachable!("resync is rejected at admission"),
        }
    }
}

/// Revision-aware, provider-neutral update reducer. Redis-LRU adapters feed
/// snapshots and events here after validating their transport envelope. The
/// controller independently enforces the configured keyspace, ordered revisions,
/// and atomic mapping onto the sidecar's allowlisted runtime override surface.
#[derive(Clone, Debug)]
pub struct RuntimeUpdateController {
    values: RuntimeValues,
    target: Option<RuntimeUpdateTarget>,
    state: Arc<Mutex<RuntimeUpdateInner>>,
}

impl RuntimeUpdateController {
    /// Construct an update-disabled controller. Calls that try to apply backend
    /// state fail closed until a target is explicitly configured.
    pub fn new(values: RuntimeValues) -> Self {
        Self {
            values,
            target: None,
            state: Arc::new(Mutex::new(RuntimeUpdateInner::default())),
        }
    }

    pub fn for_target(
        values: RuntimeValues,
        namespace: impl Into<String>,
        cache: impl Into<String>,
    ) -> Result<Self, SidecarError> {
        Ok(Self {
            values,
            target: Some(RuntimeUpdateTarget::new(namespace, cache)?),
            state: Arc::new(Mutex::new(RuntimeUpdateInner::default())),
        })
    }

    pub fn values(&self) -> RuntimeValues {
        self.values.clone()
    }

    pub fn target(&self) -> Option<&RuntimeUpdateTarget> {
        self.target.as_ref()
    }

    pub fn state(&self) -> Result<RuntimeUpdateState, SidecarError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        Ok(RuntimeUpdateState {
            revision: state.revision,
            stale: state.stale,
            backend_key_count: state.backend_keys.len(),
        })
    }

    /// Install one authoritative backend snapshot. Older snapshots are ignored;
    /// equal revisions are allowed so reconciliation can repair local overrides.
    pub fn apply_snapshot(
        &self,
        namespace: &str,
        cache: &str,
        revision: u64,
        entries: BTreeMap<String, String>,
    ) -> Result<RuntimeUpdateOutcome, SidecarError> {
        self.validate_target(namespace, cache)?;
        validate_snapshot(revision, &entries)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;
        if revision < state.revision {
            return Ok(RuntimeUpdateOutcome::StaleSnapshotIgnored {
                current: state.revision,
                incoming: revision,
            });
        }

        self.values.replace_overrides(&entries)?;
        let next = RuntimeUpdateInner::advanced(revision, entries.keys().cloned().collect());
        *state = next;
        Ok(RuntimeUpdateOutcome::Applied { revision })
    }

    /// Reduce one ordered backend event. Revision gaps, explicit resync events,
    /// and all events received while stale never mutate values; an authoritative
    /// snapshot must repair the controller before event application resumes.
    pub fn apply_batch(
        &self,
        batch: RuntimeUpdateBatch,
    ) -> Result<RuntimeUpdateOutcome, SidecarError> {
        self.validate_target(&batch.namespace, &batch.cache)?;
        validate_batch_shape(&batch)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| SidecarError::RuntimeStateUnavailable)?;

        if let Admission::Rejected { next, outcome } = state.admit(&batch) {
            *state = next;
            return Ok(outcome);
        }

        // The value effect is the one boundary; the next controller state is a
        // value computed from the current one and installed only after the
        // effect succeeded, so a rejected patch leaves the revision untouched.
        self.apply_values(&batch)?;
        let next = RuntimeUpdateInner::advanced(batch.revision, state.backend_keys_after(&batch));
        *state = next;
        Ok(RuntimeUpdateOutcome::Applied {
            revision: batch.revision,
        })
    }

    fn apply_values(&self, batch: &RuntimeUpdateBatch) -> Result<(), SidecarError> {
        match batch.operation {
            RuntimeUpdateOperation::Upsert => {
                let patch = batch
                    .entries
                    .iter()
                    .map(|(key, value)| RuntimeValueUpdate::set(key.clone(), value.clone()))
                    .collect::<Vec<_>>();
                self.values.apply_patch(&patch)
            }
            RuntimeUpdateOperation::Delete => {
                let patch = batch
                    .keys
                    .iter()
                    .cloned()
                    .map(RuntimeValueUpdate::remove)
                    .collect::<Vec<_>>();
                self.values.apply_patch(&patch)
            }
            RuntimeUpdateOperation::Replace => self.values.replace_overrides(&batch.entries),
            RuntimeUpdateOperation::Invalidate => self.values.replace_overrides(&BTreeMap::new()),
            RuntimeUpdateOperation::Resync => unreachable!("resync is rejected at admission"),
        }
    }

    fn validate_target(&self, namespace: &str, cache: &str) -> Result<(), SidecarError> {
        let Some(target) = &self.target else {
            return Err(SidecarError::InvalidConfig {
                reason: "runtime update target is not configured",
            });
        };
        if target.namespace != namespace || target.cache != cache {
            return Err(SidecarError::InvalidConfig {
                reason: "runtime update target does not match configured keyspace",
            });
        }
        Ok(())
    }
}

pub(crate) fn valid_keyspace_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RUNTIME_KEYSPACE_SEGMENT_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_snapshot(
    revision: u64,
    entries: &BTreeMap<String, String>,
) -> Result<(), SidecarError> {
    if revision > MAX_SAFE_RUNTIME_REVISION || (revision == 0 && !entries.is_empty()) {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime snapshot revision is invalid",
        });
    }
    if entries.len() > MAX_RUNTIME_KEYS {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime snapshot exceeds the bounded key count",
        });
    }
    Ok(())
}

fn validate_batch_shape(batch: &RuntimeUpdateBatch) -> Result<(), SidecarError> {
    if batch.revision == 0 || batch.revision > MAX_SAFE_RUNTIME_REVISION {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime event revision is invalid",
        });
    }
    if batch.entries.len() > MAX_RUNTIME_KEYS || batch.keys.len() > MAX_RUNTIME_KEYS {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime event exceeds the bounded key count",
        });
    }
    let valid_shape = match batch.operation {
        RuntimeUpdateOperation::Upsert => !batch.entries.is_empty() && batch.keys.is_empty(),
        RuntimeUpdateOperation::Delete => batch.entries.is_empty() && !batch.keys.is_empty(),
        RuntimeUpdateOperation::Replace => batch.keys.is_empty(),
        RuntimeUpdateOperation::Invalidate | RuntimeUpdateOperation::Resync => {
            batch.entries.is_empty() && batch.keys.is_empty()
        }
    };
    if !valid_shape {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime event operation payload is invalid",
        });
    }
    let unique = batch.keys.iter().collect::<BTreeSet<_>>();
    if unique.len() != batch.keys.len() {
        return Err(SidecarError::InvalidConfig {
            reason: "runtime delete event contains duplicate keys",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "ores-otel-sidecar";
    const CACHE: &str = "runtime-env";

    fn controller() -> RuntimeUpdateController {
        let values = RuntimeValues::new(
            ["LOG_FILTER".to_owned(), "REQUEST_TIMEOUT_MS".to_owned()],
            [("LOG_FILTER".to_owned(), "info".to_owned())],
        )
        .unwrap();
        RuntimeUpdateController::for_target(values, NS, CACHE).unwrap()
    }

    fn batch(revision: u64, operation: RuntimeUpdateOperation) -> RuntimeUpdateBatch {
        RuntimeUpdateBatch::new(NS, CACHE, revision, operation)
    }

    #[test]
    fn snapshot_override_and_delete_restore_baseline() {
        let controller = controller();
        assert_eq!(
            controller
                .apply_snapshot(
                    NS,
                    CACHE,
                    5,
                    BTreeMap::from([("LOG_FILTER".to_owned(), "debug".to_owned())])
                )
                .unwrap(),
            RuntimeUpdateOutcome::Applied { revision: 5 }
        );
        assert_eq!(
            controller.values().get("LOG_FILTER").unwrap().as_deref(),
            Some("debug")
        );
        controller
            .apply_snapshot(NS, CACHE, 6, BTreeMap::new())
            .unwrap();
        assert_eq!(
            controller.values().get("LOG_FILTER").unwrap().as_deref(),
            Some("info")
        );
    }

    #[test]
    fn target_mismatch_is_atomic() {
        let controller = controller();
        assert!(controller
            .apply_snapshot("another-service", CACHE, 1, BTreeMap::new())
            .is_err());
        let wrong = RuntimeUpdateBatch::new("another-service", CACHE, 1, RuntimeUpdateOperation::Invalidate);
        assert!(controller.apply_batch(wrong).is_err());
        assert_eq!(controller.state().unwrap(), RuntimeUpdateState::default());
    }

    #[test]
    fn unconfigured_controller_rejects_backend_updates() {
        let controller = RuntimeUpdateController::new(RuntimeValues::default());
        assert!(controller
            .apply_snapshot(NS, CACHE, 0, BTreeMap::new())
            .is_err());
    }

    #[test]
    fn stale_snapshot_cannot_roll_back_newer_state() {
        let controller = controller();
        controller
            .apply_snapshot(
                NS,
                CACHE,
                7,
                BTreeMap::from([("LOG_FILTER".to_owned(), "trace".to_owned())]),
            )
            .unwrap();
        assert_eq!(
            controller
                .apply_snapshot(
                    NS,
                    CACHE,
                    6,
                    BTreeMap::from([("LOG_FILTER".to_owned(), "stale".to_owned())])
                )
                .unwrap(),
            RuntimeUpdateOutcome::StaleSnapshotIgnored {
                current: 7,
                incoming: 6
            }
        );
        assert_eq!(
            controller.values().get("LOG_FILTER").unwrap().as_deref(),
            Some("trace")
        );
    }

    #[test]
    fn ordered_events_apply_and_revision_gap_requires_snapshot_repair() {
        let controller = controller();
        controller
            .apply_snapshot(NS, CACHE, 1, BTreeMap::new())
            .unwrap();
        let mut second = batch(2, RuntimeUpdateOperation::Upsert);
        second
            .entries
            .insert("REQUEST_TIMEOUT_MS".to_owned(), "5000".to_owned());
        assert_eq!(
            controller.apply_batch(second).unwrap(),
            RuntimeUpdateOutcome::Applied { revision: 2 }
        );
        let mut gap = batch(4, RuntimeUpdateOperation::Upsert);
        gap.entries
            .insert("REQUEST_TIMEOUT_MS".to_owned(), "9000".to_owned());
        assert_eq!(
            controller.apply_batch(gap).unwrap(),
            RuntimeUpdateOutcome::ReconcileRequired {
                current: 2,
                incoming: 4
            }
        );

        let mut would_be_next = batch(3, RuntimeUpdateOperation::Upsert);
        would_be_next
            .entries
            .insert("REQUEST_TIMEOUT_MS".to_owned(), "7000".to_owned());
        assert_eq!(
            controller.apply_batch(would_be_next).unwrap(),
            RuntimeUpdateOutcome::ReconcileRequired {
                current: 2,
                incoming: 3
            }
        );
        assert_eq!(
            controller
                .values()
                .get("REQUEST_TIMEOUT_MS")
                .unwrap()
                .as_deref(),
            Some("5000")
        );
        assert!(controller.state().unwrap().stale);

        controller
            .apply_snapshot(
                NS,
                CACHE,
                4,
                BTreeMap::from([("REQUEST_TIMEOUT_MS".to_owned(), "9000".to_owned())]),
            )
            .unwrap();
        assert!(!controller.state().unwrap().stale);
        assert_eq!(controller.state().unwrap().revision, 4);
    }

    #[test]
    fn forbidden_update_is_atomic_and_does_not_advance_revision() {
        let controller = controller();
        controller
            .apply_snapshot(NS, CACHE, 1, BTreeMap::new())
            .unwrap();
        let mut event = batch(2, RuntimeUpdateOperation::Upsert);
        event.entries.insert("LOG_FILTER".to_owned(), "debug".to_owned());
        event
            .entries
            .insert("DATABASE_URL".to_owned(), "synthetic".to_owned());
        assert!(controller.apply_batch(event).is_err());
        assert_eq!(controller.state().unwrap().revision, 1);
        assert_eq!(
            controller.values().get("LOG_FILTER").unwrap().as_deref(),
            Some("info")
        );
    }

    #[test]
    fn replace_invalidate_and_resync_follow_backend_semantics() {
        let controller = controller();
        controller
            .apply_snapshot(
                NS,
                CACHE,
                1,
                BTreeMap::from([("LOG_FILTER".to_owned(), "debug".to_owned())]),
            )
            .unwrap();
        let mut replace = batch(2, RuntimeUpdateOperation::Replace);
        replace.entries.insert(
            "REQUEST_TIMEOUT_MS".to_owned(),
            "6000".to_owned(),
        );
        controller.apply_batch(replace).unwrap();
        assert_eq!(
            controller.values().get("LOG_FILTER").unwrap().as_deref(),
            Some("info")
        );
        assert_eq!(
            controller
                .values()
                .get("REQUEST_TIMEOUT_MS")
                .unwrap()
                .as_deref(),
            Some("6000")
        );

        controller
            .apply_batch(batch(3, RuntimeUpdateOperation::Invalidate))
            .unwrap();
        assert_eq!(controller.values().get("REQUEST_TIMEOUT_MS").unwrap(), None);

        assert_eq!(
            controller
                .apply_batch(batch(4, RuntimeUpdateOperation::Resync))
                .unwrap(),
            RuntimeUpdateOutcome::ReconcileRequired {
                current: 3,
                incoming: 4
            }
        );
        assert!(controller.state().unwrap().stale);
    }

    #[test]
    fn duplicate_event_is_idempotent() {
        let controller = controller();
        controller
            .apply_snapshot(NS, CACHE, 2, BTreeMap::new())
            .unwrap();
        assert_eq!(
            controller
                .apply_batch(batch(2, RuntimeUpdateOperation::Invalidate))
                .unwrap(),
            RuntimeUpdateOutcome::Duplicate { revision: 2 }
        );
    }

    #[test]
    fn invalid_snapshot_is_atomic() {
        let controller = controller();
        controller
            .apply_snapshot(NS, CACHE, 1, BTreeMap::new())
            .unwrap();
        let result = controller.apply_snapshot(
            NS,
            CACHE,
            2,
            BTreeMap::from([
                ("LOG_FILTER".to_owned(), "debug".to_owned()),
                ("GITHUB_TOKEN".to_owned(), "synthetic".to_owned()),
            ]),
        );
        assert!(result.is_err());
        assert_eq!(controller.state().unwrap().revision, 1);
        assert_eq!(
            controller.values().get("LOG_FILTER").unwrap().as_deref(),
            Some("info")
        );
    }
}
