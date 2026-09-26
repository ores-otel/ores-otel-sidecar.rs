use super::*;

struct MemoryStore {
    record: LifecycleRecord,
}

impl LifecycleRecordStore for MemoryStore {
    fn load(&mut self, _workload_id: &str) -> Result<LifecycleRecord, String> {
        return Ok(self.record.clone());
    }

    fn replace(
        &mut self,
        expected: &LifecycleRecord,
        next: &LifecycleRecord,
    ) -> Result<(), String> {
        if self.record != *expected {
            return Err("compare-and-set conflict".to_owned());
        }

        self.record = next.clone();
        return Ok(());
    }
}

struct FakeProduct {
    activity: ActivitySnapshot,
    quiesce: ProductQuiesceOutcome,
    ready: bool,
    cancelled: bool,
}

impl ProductLifecycleControl for FakeProduct {
    fn observe(&mut self) -> Result<ActivitySnapshot, String> {
        return Ok(self.activity);
    }

    fn quiesce(&mut self) -> Result<ProductQuiesceOutcome, String> {
        return Ok(self.quiesce);
    }

    fn cancel_quiesce(&mut self) -> Result<(), String> {
        self.cancelled = true;
        return Ok(());
    }

    fn ensure_ready(&mut self) -> Result<(), String> {
        if self.ready {
            return Ok(());
        }

        return Err("not ready".to_owned());
    }
}

struct FakeEffects {
    frozen: bool,
}

impl LifecycleEffects for FakeEffects {
    fn freeze(&mut self) -> Result<(), String> {
        self.frozen = true;
        return Ok(());
    }

    fn thaw(&mut self) -> Result<(), String> {
        self.frozen = false;
        return Ok(());
    }

    fn checkpoint_and_terminate(&mut self) -> Result<LifecycleCheckpoint, String> {
        return Ok(LifecycleCheckpoint {
            artifact_ref: "checkpoint://workload/sha256:abc".to_owned(),
            digest: "sha256:abc".to_owned(),
            format: "criu-v1".to_owned(),
        });
    }

    fn restore(&mut self, _checkpoint: &LifecycleCheckpoint) -> Result<(), String> {
        return Ok(());
    }
}

fn record(
    strategy: PersistedSuspendStrategy,
    state: PersistedLifecycleState,
    checkpoint: Option<LifecycleCheckpoint>,
) -> LifecycleRecord {
    return LifecycleRecord {
        workload_id: "workload-7".to_owned(),
        assigned_node: "node-a".to_owned(),
        placement_epoch: 4,
        fencing_token: 9,
        revision: 12,
        state,
        strategy,
        checkpoint,
    };
}

fn scope() -> ControllerScope {
    return ControllerScope {
        workload_id: "workload-7".to_owned(),
        node: "node-a".to_owned(),
        placement_epoch: 4,
    };
}

const fn freeze_policy() -> LifecyclePolicy {
    return LifecyclePolicy {
        strategy: SuspendStrategy::Freeze,
        minimum_idle_ms: 30_000,
    };
}

fn idle_product(quiesce: ProductQuiesceOutcome) -> FakeProduct {
    return FakeProduct {
        activity: ActivitySnapshot {
            queue_depth: 0,
            in_flight: 0,
            idle_for_ms: 60_000,
        },
        quiesce,
        ready: true,
        cancelled: false,
    };
}

fn demand_product() -> FakeProduct {
    return FakeProduct {
        activity: ActivitySnapshot {
            queue_depth: 1,
            in_flight: 0,
            idle_for_ms: 0,
        },
        quiesce: ProductQuiesceOutcome::Drained,
        ready: true,
        cancelled: false,
    };
}

fn unfrozen_effects() -> FakeEffects {
    return FakeEffects { frozen: false };
}

#[test]
fn idle_freeze_persists_intents_before_effect_completion() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Running,
            None,
        ),
    };
    let mut product = idle_product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::Suspended));
    assert_eq!(store.record.state, PersistedLifecycleState::Frozen);
    assert_eq!(store.record.fencing_token, 10);
    assert!(effects.frozen);
}

#[test]
fn demand_returned_during_quiesce_leaves_workload_running() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Running,
            None,
        ),
    };
    let mut product = idle_product(ProductQuiesceOutcome::DemandReturned);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::DemandCancelledSuspend));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(product.cancelled);
    assert!(!effects.frozen);
}

#[test]
fn frozen_demand_thaws_then_requires_product_readiness() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Frozen,
            None,
        ),
    };
    let mut product = demand_product();
    let mut effects = FakeEffects { frozen: true };

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::Resumed));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(!effects.frozen);
}

#[test]
fn persisted_quiesce_is_cancelled_under_the_current_fence() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Quiescing,
            None,
        ),
    };
    let mut product = idle_product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::RecoveredQuiesce));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert_eq!(store.record.fencing_token, 10);
    assert!(product.cancelled);
}

#[test]
fn policy_strategy_must_match_the_durable_record() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Hibernate,
            PersistedLifecycleState::Running,
            None,
        ),
    };
    let mut product = idle_product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    assert_eq!(
        reconcile_once(
            &scope(),
            10,
            freeze_policy(),
            &mut store,
            &mut product,
            &mut effects,
        ),
        Err(ReconcileError::InvalidRecord)
    );
}

#[test]
fn transitional_state_requires_crash_recovery_path() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Freezing,
            None,
        ),
    };
    let mut product = idle_product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    assert_eq!(
        reconcile_once(
            &scope(),
            10,
            freeze_policy(),
            &mut store,
            &mut product,
            &mut effects,
        ),
        Err(ReconcileError::RecoveryRequired(
            PersistedLifecycleState::Freezing
        ))
    );
}
