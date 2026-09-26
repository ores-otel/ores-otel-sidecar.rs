//! Host-agent orchestration between pure lifecycle policy and outward effects.
//!
//! Distributed lease acquisition intentionally stays outside this module. A
//! caller acquires one fenced lifecycle grant through `ores-locks-and-leases`,
//! then calls [`reconcile_once`] with that fencing token. This keeps Cloudflare
//! Durable Objects, Fiducia, and future BeamScale-native lease authorities behind
//! one interchangeable seam while making effect ordering independently testable.

#![forbid(unsafe_code)]

use crate::process_lifecycle::{
    decide, ActivitySnapshot, LifecycleAction, LifecycleEvent, LifecyclePolicy, LifecycleState,
    SuspendStrategy,
};
use crate::process_lifecycle_record::{
    validate_record_update, LifecycleCheckpoint, LifecycleRecord, PersistedLifecycleState,
    PersistedSuspendStrategy,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductQuiesceOutcome {
    Drained,
    DemandReturned,
}

/// Product-specific runtime boundary. Implementations own their local opaque
/// quiesce handle (Erlang refs/PIDs must never cross the process boundary).
pub trait ProductLifecycleControl {
    fn observe(&self) -> Result<ActivitySnapshot, String>;
    fn quiesce(&self) -> Result<ProductQuiesceOutcome, String>;
    fn cancel_quiesce(&self) -> Result<(), String>;
    fn ensure_ready(&self) -> Result<(), String>;
}

/// Durable compare-and-set record boundary. Implementations must make replace
/// atomic and reject a changed expected revision/fence rather than overwriting it.
pub trait LifecycleRecordStore {
    fn load(&self, workload_id: &str) -> Result<LifecycleRecord, String>;
    fn replace(
        &self,
        expected: &LifecycleRecord,
        next: &LifecycleRecord,
    ) -> Result<(), String>;
}

/// Linux/process effects. The shared Linux adapter provides cgroup and CRIU
/// primitives; product/infra wiring decides how checkpoint artifacts become
/// durable and content-addressed.
pub trait LifecycleEffects {
    fn freeze(&self) -> Result<(), String>;
    fn thaw(&self) -> Result<(), String>;
    fn checkpoint_and_terminate(&self) -> Result<LifecycleCheckpoint, String>;
    fn restore(&self, checkpoint: &LifecycleCheckpoint) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerScope {
    pub workload_id: String,
    pub node: String,
    pub placement_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    NoChange,
    Suspended,
    Resumed,
    DemandCancelledSuspend,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileError {
    Record(String),
    Product(String),
    Effect(String),
    InvalidRecord,
    StalePlacement,
    StaleFence,
    InvalidTransition,
    RecoveryRequired(PersistedLifecycleState),
}

/// Reconcile one stable lifecycle state while holding the workload's fenced
/// distributed lease. Transitional process-effect states are deliberately not
/// replayed here: `freezing`, `checkpointing`, `thawing`, and `restoring` require
/// crash recovery against observed cgroup/process/checkpoint reality.
pub fn reconcile_once<S, P, E>(
    scope: &ControllerScope,
    fencing_token: u64,
    policy: LifecyclePolicy,
    store: &S,
    product: &P,
    effects: &E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    P: ProductLifecycleControl,
    E: LifecycleEffects,
{
    let current = store
        .load(&scope.workload_id)
        .map_err(ReconcileError::Record)?;
    current.validate().map_err(|_| ReconcileError::InvalidRecord)?;
    validate_authority(scope, fencing_token, &current)?;

    match current.state {
        PersistedLifecycleState::Running
        | PersistedLifecycleState::Quiescing
        | PersistedLifecycleState::Frozen
        | PersistedLifecycleState::Hibernated => {}
        PersistedLifecycleState::Freezing
        | PersistedLifecycleState::Checkpointing
        | PersistedLifecycleState::Thawing
        | PersistedLifecycleState::Restoring => {
            return Err(ReconcileError::RecoveryRequired(current.state));
        }
    }

    let activity = product.observe().map_err(ReconcileError::Product)?;
    let state = to_policy_state(&current);
    let decision = decide(state, policy, LifecycleEvent::Observe(activity))
        .map_err(|_| ReconcileError::InvalidTransition)?;

    match decision.action {
        LifecycleAction::None => {
            return Ok(ReconcileOutcome::NoChange);
        }
        LifecycleAction::BeginQuiesce => {
            return suspend_from_running(
                scope,
                fencing_token,
                policy,
                current,
                store,
                product,
                effects,
            );
        }
        LifecycleAction::CancelQuiesce => {
            product.cancel_quiesce().map_err(ReconcileError::Product)?;
            let running = next_record(
                &current,
                fencing_token,
                PersistedLifecycleState::Running,
                None,
            );
            persist(store, &current, &running)?;
            return Ok(ReconcileOutcome::DemandCancelledSuspend);
        }
        LifecycleAction::Thaw => {
            return resume_frozen(scope, fencing_token, current, store, product, effects);
        }
        LifecycleAction::Restore => {
            return restore_hibernated(scope, fencing_token, current, store, product, effects);
        }
        LifecycleAction::Freeze | LifecycleAction::CheckpointAndTerminate => {
            return Err(ReconcileError::InvalidTransition);
        }
    }
}

fn suspend_from_running<S, P, E>(
    scope: &ControllerScope,
    fencing_token: u64,
    policy: LifecyclePolicy,
    current: LifecycleRecord,
    store: &S,
    product: &P,
    effects: &E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    P: ProductLifecycleControl,
    E: LifecycleEffects,
{
    let quiescing = next_record(
        &current,
        fencing_token,
        PersistedLifecycleState::Quiescing,
        None,
    );
    persist(store, &current, &quiescing)?;

    match product.quiesce().map_err(ReconcileError::Product)? {
        ProductQuiesceOutcome::DemandReturned => {
            product.cancel_quiesce().map_err(ReconcileError::Product)?;
            let running = next_record(
                &quiescing,
                fencing_token,
                PersistedLifecycleState::Running,
                None,
            );
            persist(store, &quiescing, &running)?;
            return Ok(ReconcileOutcome::DemandCancelledSuspend);
        }
        ProductQuiesceOutcome::Drained => {}
    }

    let after_quiesce = decide(
        LifecycleState::Quiescing(policy.strategy),
        policy,
        LifecycleEvent::QuiesceSucceeded,
    )
    .map_err(|_| ReconcileError::InvalidTransition)?;

    match after_quiesce.action {
        LifecycleAction::Freeze => {
            let intent = next_record(
                &quiescing,
                fencing_token,
                PersistedLifecycleState::Freezing,
                None,
            );
            persist(store, &quiescing, &intent)?;
            effects.freeze().map_err(ReconcileError::Effect)?;
            let frozen = next_record(
                &intent,
                fencing_token,
                PersistedLifecycleState::Frozen,
                None,
            );
            persist(store, &intent, &frozen)?;
            return Ok(ReconcileOutcome::Suspended);
        }
        LifecycleAction::CheckpointAndTerminate => {
            let intent = next_record(
                &quiescing,
                fencing_token,
                PersistedLifecycleState::Checkpointing,
                None,
            );
            persist(store, &quiescing, &intent)?;
            let checkpoint = effects
                .checkpoint_and_terminate()
                .map_err(ReconcileError::Effect)?;
            let hibernated = next_record(
                &intent,
                fencing_token,
                PersistedLifecycleState::Hibernated,
                Some(checkpoint),
            );
            persist(store, &intent, &hibernated)?;
            return Ok(ReconcileOutcome::Suspended);
        }
        _ => {
            let _ = scope;
            return Err(ReconcileError::InvalidTransition);
        }
    }
}

fn resume_frozen<S, P, E>(
    _scope: &ControllerScope,
    fencing_token: u64,
    current: LifecycleRecord,
    store: &S,
    product: &P,
    effects: &E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    P: ProductLifecycleControl,
    E: LifecycleEffects,
{
    let thawing = next_record(
        &current,
        fencing_token,
        PersistedLifecycleState::Thawing,
        None,
    );
    persist(store, &current, &thawing)?;
    effects.thaw().map_err(ReconcileError::Effect)?;
    product.ensure_ready().map_err(ReconcileError::Product)?;
    let running = next_record(
        &thawing,
        fencing_token,
        PersistedLifecycleState::Running,
        None,
    );
    persist(store, &thawing, &running)?;
    return Ok(ReconcileOutcome::Resumed);
}

fn restore_hibernated<S, P, E>(
    _scope: &ControllerScope,
    fencing_token: u64,
    current: LifecycleRecord,
    store: &S,
    product: &P,
    effects: &E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    P: ProductLifecycleControl,
    E: LifecycleEffects,
{
    let checkpoint = current
        .checkpoint
        .clone()
        .ok_or(ReconcileError::InvalidRecord)?;
    let restoring = next_record(
        &current,
        fencing_token,
        PersistedLifecycleState::Restoring,
        Some(checkpoint.clone()),
    );
    persist(store, &current, &restoring)?;
    effects
        .restore(&checkpoint)
        .map_err(ReconcileError::Effect)?;
    product.ensure_ready().map_err(ReconcileError::Product)?;
    let running = next_record(
        &restoring,
        fencing_token,
        PersistedLifecycleState::Running,
        None,
    );
    persist(store, &restoring, &running)?;
    return Ok(ReconcileOutcome::Resumed);
}

fn persist<S: LifecycleRecordStore>(
    store: &S,
    current: &LifecycleRecord,
    next: &LifecycleRecord,
) -> Result<(), ReconcileError> {
    validate_record_update(current, next).map_err(|_| ReconcileError::InvalidRecord)?;
    return store
        .replace(current, next)
        .map_err(ReconcileError::Record);
}

fn validate_authority(
    scope: &ControllerScope,
    fencing_token: u64,
    record: &LifecycleRecord,
) -> Result<(), ReconcileError> {
    if record.assigned_node != scope.node || record.placement_epoch != scope.placement_epoch {
        return Err(ReconcileError::StalePlacement);
    }
    if fencing_token < record.fencing_token {
        return Err(ReconcileError::StaleFence);
    }
    return Ok(());
}

fn next_record(
    current: &LifecycleRecord,
    fencing_token: u64,
    state: PersistedLifecycleState,
    checkpoint: Option<LifecycleCheckpoint>,
) -> LifecycleRecord {
    return LifecycleRecord {
        workload_id: current.workload_id.clone(),
        assigned_node: current.assigned_node.clone(),
        placement_epoch: current.placement_epoch,
        fencing_token,
        revision: current.revision.saturating_add(1),
        state,
        strategy: current.strategy,
        checkpoint,
    };
}

fn to_policy_state(record: &LifecycleRecord) -> LifecycleState {
    match record.state {
        PersistedLifecycleState::Running => {
            return LifecycleState::Running;
        }
        PersistedLifecycleState::Quiescing => {
            return LifecycleState::Quiescing(to_strategy(record.strategy));
        }
        PersistedLifecycleState::Freezing => {
            return LifecycleState::Freezing;
        }
        PersistedLifecycleState::Frozen => {
            return LifecycleState::Frozen;
        }
        PersistedLifecycleState::Checkpointing => {
            return LifecycleState::Checkpointing;
        }
        PersistedLifecycleState::Hibernated => {
            return LifecycleState::Hibernated;
        }
        PersistedLifecycleState::Thawing => {
            return LifecycleState::Resuming(SuspendStrategy::Freeze);
        }
        PersistedLifecycleState::Restoring => {
            return LifecycleState::Resuming(SuspendStrategy::Hibernate);
        }
    }
}

const fn to_strategy(strategy: PersistedSuspendStrategy) -> SuspendStrategy {
    match strategy {
        PersistedSuspendStrategy::Freeze => {
            return SuspendStrategy::Freeze;
        }
        PersistedSuspendStrategy::Hibernate => {
            return SuspendStrategy::Hibernate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct MemoryStore {
        record: RefCell<LifecycleRecord>,
    }

    impl LifecycleRecordStore for MemoryStore {
        fn load(&self, _workload_id: &str) -> Result<LifecycleRecord, String> {
            return Ok(self.record.borrow().clone());
        }

        fn replace(
            &self,
            expected: &LifecycleRecord,
            next: &LifecycleRecord,
        ) -> Result<(), String> {
            if *self.record.borrow() != *expected {
                return Err("compare-and-set conflict".to_owned());
            }
            *self.record.borrow_mut() = next.clone();
            return Ok(());
        }
    }

    struct FakeProduct {
        activity: ActivitySnapshot,
        quiesce: ProductQuiesceOutcome,
        ready: bool,
    }

    impl ProductLifecycleControl for FakeProduct {
        fn observe(&self) -> Result<ActivitySnapshot, String> {
            return Ok(self.activity);
        }

        fn quiesce(&self) -> Result<ProductQuiesceOutcome, String> {
            return Ok(self.quiesce);
        }

        fn cancel_quiesce(&self) -> Result<(), String> {
            return Ok(());
        }

        fn ensure_ready(&self) -> Result<(), String> {
            if self.ready {
                return Ok(());
            }
            return Err("not ready".to_owned());
        }
    }

    #[derive(Default)]
    struct FakeEffects {
        frozen: RefCell<bool>,
    }

    impl LifecycleEffects for FakeEffects {
        fn freeze(&self) -> Result<(), String> {
            *self.frozen.borrow_mut() = true;
            return Ok(());
        }

        fn thaw(&self) -> Result<(), String> {
            *self.frozen.borrow_mut() = false;
            return Ok(());
        }

        fn checkpoint_and_terminate(&self) -> Result<LifecycleCheckpoint, String> {
            return Ok(LifecycleCheckpoint {
                artifact_ref: "checkpoint://workload/sha256:abc".to_owned(),
                digest: "sha256:abc".to_owned(),
                format: "criu-v1".to_owned(),
            });
        }

        fn restore(&self, _checkpoint: &LifecycleCheckpoint) -> Result<(), String> {
            return Ok(());
        }
    }

    fn running_record(strategy: PersistedSuspendStrategy) -> LifecycleRecord {
        return LifecycleRecord {
            workload_id: "workload-7".to_owned(),
            assigned_node: "node-a".to_owned(),
            placement_epoch: 4,
            fencing_token: 9,
            revision: 12,
            state: PersistedLifecycleState::Running,
            strategy,
            checkpoint: None,
        };
    }

    fn scope() -> ControllerScope {
        return ControllerScope {
            workload_id: "workload-7".to_owned(),
            node: "node-a".to_owned(),
            placement_epoch: 4,
        };
    }

    #[test]
    fn idle_freeze_persists_intents_before_effect_completion() {
        let store = MemoryStore {
            record: RefCell::new(running_record(PersistedSuspendStrategy::Freeze)),
        };
        let product = FakeProduct {
            activity: ActivitySnapshot {
                queue_depth: 0,
                in_flight: 0,
                idle_for_ms: 60_000,
            },
            quiesce: ProductQuiesceOutcome::Drained,
            ready: true,
        };
        let effects = FakeEffects::default();
        let policy = LifecyclePolicy::new(SuspendStrategy::Freeze, 30_000).unwrap();

        let outcome = reconcile_once(&scope(), 10, policy, &store, &product, &effects).unwrap();

        assert_eq!(outcome, ReconcileOutcome::Suspended);
        assert_eq!(store.record.borrow().state, PersistedLifecycleState::Frozen);
        assert_eq!(store.record.borrow().fencing_token, 10);
        assert!(*effects.frozen.borrow());
    }

    #[test]
    fn demand_returned_during_quiesce_leaves_workload_running() {
        let store = MemoryStore {
            record: RefCell::new(running_record(PersistedSuspendStrategy::Freeze)),
        };
        let product = FakeProduct {
            activity: ActivitySnapshot {
                queue_depth: 0,
                in_flight: 0,
                idle_for_ms: 60_000,
            },
            quiesce: ProductQuiesceOutcome::DemandReturned,
            ready: true,
        };
        let effects = FakeEffects::default();
        let policy = LifecyclePolicy::new(SuspendStrategy::Freeze, 30_000).unwrap();

        let outcome = reconcile_once(&scope(), 10, policy, &store, &product, &effects).unwrap();

        assert_eq!(outcome, ReconcileOutcome::DemandCancelledSuspend);
        assert_eq!(store.record.borrow().state, PersistedLifecycleState::Running);
        assert!(!*effects.frozen.borrow());
    }

    #[test]
    fn frozen_demand_thaws_then_requires_product_readiness() {
        let mut record = running_record(PersistedSuspendStrategy::Freeze);
        record.state = PersistedLifecycleState::Frozen;
        let store = MemoryStore {
            record: RefCell::new(record),
        };
        let product = FakeProduct {
            activity: ActivitySnapshot {
                queue_depth: 1,
                in_flight: 0,
                idle_for_ms: 0,
            },
            quiesce: ProductQuiesceOutcome::Drained,
            ready: true,
        };
        let effects = FakeEffects {
            frozen: RefCell::new(true),
        };
        let policy = LifecyclePolicy::new(SuspendStrategy::Freeze, 30_000).unwrap();

        let outcome = reconcile_once(&scope(), 10, policy, &store, &product, &effects).unwrap();

        assert_eq!(outcome, ReconcileOutcome::Resumed);
        assert_eq!(store.record.borrow().state, PersistedLifecycleState::Running);
        assert!(!*effects.frozen.borrow());
    }

    #[test]
    fn transitional_state_requires_crash_recovery_path() {
        let mut record = running_record(PersistedSuspendStrategy::Freeze);
        record.state = PersistedLifecycleState::Freezing;
        let store = MemoryStore {
            record: RefCell::new(record),
        };
        let product = FakeProduct {
            activity: ActivitySnapshot {
                queue_depth: 0,
                in_flight: 0,
                idle_for_ms: 60_000,
            },
            quiesce: ProductQuiesceOutcome::Drained,
            ready: true,
        };
        let effects = FakeEffects::default();
        let policy = LifecyclePolicy::new(SuspendStrategy::Freeze, 30_000).unwrap();

        assert_eq!(
            reconcile_once(&scope(), 10, policy, &store, &product, &effects),
            Err(ReconcileError::RecoveryRequired(
                PersistedLifecycleState::Freezing
            ))
        );
    }
}
