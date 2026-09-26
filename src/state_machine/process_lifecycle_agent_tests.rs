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

struct FakeDemand {
    observations: Vec<ActivitySnapshot>,
    cursor: usize,
}

impl HostDemandObservation for FakeDemand {
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
}

struct FakeProduct {
    quiesce: ProductQuiesceOutcome,
    cancelled: bool,
}

impl ProductLifecycleControl for FakeProduct {
    fn quiesce(&mut self) -> Result<ProductQuiesceOutcome, String> {
        return Ok(self.quiesce);
    }

    fn cancel_quiesce(&mut self) -> Result<(), String> {
        self.cancelled = true;
        return Ok(());
    }
}

struct FakeAdmission {
    ready: bool,
    verified: bool,
}

impl RuntimeAdmissionVerifier for FakeAdmission {
    fn verify_ready(&mut self) -> Result<(), String> {
        self.verified = true;
        if self.ready {
            return Ok(());
        }

        return Err("host admission verification failed".to_owned());
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

fn demand_with(observations: Vec<ActivitySnapshot>) -> FakeDemand {
    return FakeDemand {
        observations,
        cursor: 0,
    };
}

fn idle_demand() -> FakeDemand {
    return demand_with(vec![idle_activity(), idle_activity()]);
}

fn product(quiesce: ProductQuiesceOutcome) -> FakeProduct {
    return FakeProduct {
        quiesce,
        cancelled: false,
    };
}

fn ready_admission() -> FakeAdmission {
    return FakeAdmission {
        ready: true,
        verified: false,
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
    let mut demand = idle_demand();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = ready_admission();
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut demand,
        &mut product,
        &mut admission,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::Suspended));
    assert_eq!(store.record.state, PersistedLifecycleState::Frozen);
    assert_eq!(store.record.fencing_token, 10);
    assert!(effects.frozen);
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
    let mut demand = demand_with(vec![idle_activity(), demand_activity()]);
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = ready_admission();
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut demand,
        &mut product,
        &mut admission,
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
    let mut demand = idle_demand();
    let mut product = product(ProductQuiesceOutcome::DemandReturned);
    let mut admission = ready_admission();
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut demand,
        &mut product,
        &mut admission,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::DemandCancelledSuspend));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(product.cancelled);
    assert!(!effects.frozen);
}

#[test]
fn frozen_trusted_demand_thaws_then_requires_host_admission() {
    let mut store = MemoryStore {
        record: record(
            PersistedSuspendStrategy::Freeze,
            PersistedLifecycleState::Frozen,
            None,
        ),
    };
    let mut demand = demand_with(vec![demand_activity()]);
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = ready_admission();
    let mut effects = FakeEffects { frozen: true };

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut demand,
        &mut product,
        &mut admission,
        &mut effects,
    );

    assert_eq!(outcome, Ok(ReconcileOutcome::Resumed));
    assert_eq!(store.record.state, PersistedLifecycleState::Running);
    assert!(!effects.frozen);
    assert!(admission.verified);
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
    let mut demand = demand_with(vec![demand_activity()]);
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = FakeAdmission {
        ready: false,
        verified: false,
    };
    let mut effects = FakeEffects { frozen: true };

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut demand,
        &mut product,
        &mut admission,
        &mut effects,
    );

    assert_eq!(
        outcome,
        Err(ReconcileError::Admission(
            "host admission verification failed".to_owned()
        ))
    );
    assert_eq!(store.record.state, PersistedLifecycleState::Thawing);
    assert!(!effects.frozen);
    assert!(admission.verified);
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
    let mut demand = idle_demand();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = ready_admission();
    let mut effects = unfrozen_effects();

    let outcome = reconcile_once(
        &scope(),
        10,
        freeze_policy(),
        &mut store,
        &mut demand,
        &mut product,
        &mut admission,
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
    let mut demand = idle_demand();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = ready_admission();
    let mut effects = unfrozen_effects();

    assert_eq!(
        reconcile_once(
            &scope(),
            10,
            freeze_policy(),
            &mut store,
            &mut demand,
            &mut product,
            &mut admission,
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
    let mut demand = idle_demand();
    let mut product = product(ProductQuiesceOutcome::Drained);
    let mut admission = ready_admission();
    let mut effects = unfrozen_effects();

    assert_eq!(
        reconcile_once(
            &scope(),
            10,
            freeze_policy(),
            &mut store,
            &mut demand,
            &mut product,
            &mut admission,
            &mut effects,
        ),
        Err(ReconcileError::RecoveryRequired(
            PersistedLifecycleState::Freezing
        ))
    );
}
