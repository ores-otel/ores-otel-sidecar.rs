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

struct FakeHost {
    observations: Vec<ActivitySnapshot>,
    cursor: usize,
    ready: bool,
    verified: bool,
}

impl HostLifecycleControl for FakeHost {
    fn observe(&mut self) -> Result<ActivitySnapshot, String> {
        let observation = self
            .observations
            .get(self.cursor)
            .copied()
            .or_else(|| self.observations.last().copied())
            .ok_or_else(|| "missing trusted demand observation".to_owned())?;
        self.cursor = self.cursor.saturating_add(1);
        return Ok(observation);
    }

    fn verify_ready(&mut self) -> Result<(), String> {
        self.verified = true;
        if self.ready {
            return Ok(());
        }

        return Err("host admission verification failed".to_owned());
    }
}

struct FakeProduct {
    quiesce: ProductQuiesceOutcome,
    cancelled: bool,
    cancel_error: Option<String>,
}

impl ProductLifecycleControl for FakeProduct {
    fn quiesce(&mut self) -> Result<ProductQuiesceOutcome, String> {
        return Ok(self.quiesce);
    }

    fn cancel_quiesce(&mut self) -> Result<(), String> {
        self.cancelled = true;
        match self.cancel_error.clone() {
            Some(error) => {
                return Err(error);
            }
            None => {
                return Ok(());
            }
        }
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
            artifact_ref: "checkpoint://workload/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
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
        runtime_epoch: 21,
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
        runtime_epoch: 21,
    };
}

const fn freeze_policy() -> LifecyclePolicy {
    return LifecyclePolicy {
        strategy: SuspendStrategy::Freeze,
        minimum_idle_ms: 30_000,
    };
}

const fn idle_activity() -> ActivitySnapshot {
    return ActivitySnapshot {
        queue_depth: 0,
        in_flight: 0,
        idle_for_ms: 60_000,
    };
}

const fn demand_activity() -> ActivitySnapshot {
    return ActivitySnapshot {
        queue_depth: 1,
        in_flight: 0,
        idle_for_ms: 0,
    };
}

fn host_with(observations: Vec<ActivitySnapshot>) -> FakeHost {
    return FakeHost {
        observations,
        cursor: 0,
        ready: true,
        verified: false,
    };
}

fn idle_host() -> FakeHost {
    return host_with(vec![idle_activity(), idle_activity()]);
}

fn product(quiesce: ProductQuiesceOutcome) -> FakeProduct {
    return FakeProduct {
        quiesce,
        cancelled: false,
        cancel_error: None,
    };
}

fn product_with_cancel_error(error: &str) -> FakeProduct {
    return FakeProduct {
        quiesce: ProductQuiesceOutcome::Drained,
        cancelled: false,
        cancel_error: Some(error.to_owned()),
    };
}

fn unfrozen_effects() -> FakeEffects {
    return FakeEffects { frozen: false };
}

fn reconcile_product_resume_failure(
    mut store: MemoryStore,
    mut host: FakeHost,
    mut product: FakeProduct,
    mut effects: FakeEffects,
) -> (
    Result<ReconcileOutcome, ReconcileError>,
    MemoryStore,
    FakeHost,
    FakeProduct,
    FakeEffects,
) {
    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );
    return (outcome, store, host, product, effects);
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
    let mut host = idle_host();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::Suspended));
    assert_eq!(store.record.state, PersistedLifecycleState::Frozen);
    assert_eq!(store.record.fencing_token, 10);
    assert_eq!(store.record.runtime_epoch, 21);
    assert!(effects.frozen);
}

#[test]
fn stale_runtime_incarnation_cannot_reconcile_replacement_process() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Running,
            None,
        ),
    };
    let mut stale_scope = scope();
    stale_scope.runtime_epoch = 20;
    let mut host = idle_host();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &stale_scope,
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Err(ReconcileError::StaleRuntime));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(!effects.frozen);
}

#[test]
fn tenant_drained_claim_cannot_override_trusted_returned_demand() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Running,
            None,
        ),
    };
    let mut host = host_with(vec![idle_activity(), demand_activity()]);
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::DemandCancelledSuspend));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(product.cancelled);
    assert!(!effects.frozen);
}

#[test]
fn cooperative_demand_return_during_quiesce_leaves_workload_running() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Running,
            None,
        ),
    };
    let mut host = idle_host();
    let mut product = product(ProductQuiesceOutcome::DemandReturned);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::DemandCancelledSuspend));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(product.cancelled);
    assert!(!effects.frozen);
}

#[test]
fn frozen_trusted_demand_thaws_reopens_product_then_requires_host_admission() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Frozen,
            None,
        ),
    };
    let mut host = host_with(vec![demand_activity()]);
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = FakeEffects { frozen: true };

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::Resumed));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(!effects.frozen);
    assert!(product.cancelled);
    assert!(host.verified);
}

#[test]
fn failed_product_resume_never_publishes_running_after_thaw() {
    let (outcome, store, host, product, effects) = reconcile_product_resume_failure(
        MemoryStore {
            record: record(
                PersistedSuspendStrategy::Freeze,
                PersistedLifecycleState::Frozen,
                None,
            ),
        },
        host_with(vec![demand_activity()]),
        product_with_cancel_error("product resume failed"),
        FakeEffects { frozen: true },
    );

    assert_eq!(
        outcome,
        Err(ReconcileError::Product("product resume failed".to_owned()))
    );
    assert_eq!(store.record.state, PersistedLifecycleState::Thawing);
    assert!(!effects.frozen);
    assert!(product.cancelled);
    assert!(!host.verified);
}

#[test]
fn failed_host_admission_never_publishes_running_after_thaw() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Frozen,
            None,
        ),
    };
    let mut host = host_with(vec![demand_activity()]);
    host.ready = false;
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = FakeEffects { frozen: true };

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
        &mut product,
        &mut effects,
    );

    assert_eq!(
        outcome,
        Err(ReconcileError::Host(
            "host admission verification failed".to_owned()
        ))
    );
    assert_eq!(store.record.state, PersistedLifecycleState::Thawing);
    assert!(!effects.frozen);
    assert!(product.cancelled);
    assert!(host.verified);
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
    let mut host = idle_host();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut host,
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
    let mut host = idle_host();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    assert_eq!(
        reconcile_once(
            &scope(),
            10,
            freeze_policy(),
            &mut store,
            &mut host,
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
    let mut host = idle_host();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut effects = unfrozen_effects();

    assert_eq!(
        reconcile_once(
            &scope(),
            10,
            freeze_policy(),
            &mut store,
            &mut host,
            &mut product,
            &mut effects,
        ),
        Err(ReconcileError::RecoveryRequired(
            PersistedLifecycleState::Freezing
        ))
    );
}
