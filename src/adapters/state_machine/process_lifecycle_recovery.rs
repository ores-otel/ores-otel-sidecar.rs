//! Crash recovery for the freeze-only lifecycle tranche.
//!
//! Stable reconciliation deliberately refuses to replay transitional effects.
//! This adapter handles only persisted `freezing` / `thawing` records while a
//! caller holds a fresh fenced lifecycle grant, and decides recovery from the
//! kernel-observed managed-cgroup state. Checkpoint/restore transitions remain
//! unsupported by construction.

#![forbid(unsafe_code)]

use crate::process_lifecycle::{LifecyclePolicy, SuspendStrategy};
use crate::process_lifecycle_agent::{
    ControllerScope, HostLifecycleControl, LifecycleRecordStore, ProductLifecycleControl,
    ReconcileError,
};
use crate::process_lifecycle_record::{
    LifecycleRecord, PersistedLifecycleState, PersistedSuspendStrategy, validate_record_update,
};

use super::process_lifecycle_freeze::{FreezeTransitionEffects, FreezeTransitionStatus};
use super::process_lifecycle_lease::FencedLifecycleGrant;

const UNPOPULATED_TARGET: &str = "managed cgroup is not populated during freeze transition recovery";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezeRecoveryOutcome {
    Frozen,
    Running,
}

pub fn recover_freeze_transition_with_fenced_grant<S, H, P, E>(
    grant: &FencedLifecycleGrant,
    scope: &ControllerScope,
    policy: LifecyclePolicy,
    store: &mut S,
    host: &mut H,
    product: &mut P,
    effects: &mut E,
) -> Result<FreezeRecoveryOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    H: HostLifecycleControl,
    P: ProductLifecycleControl,
    E: FreezeTransitionEffects,
{
    return recover_freeze_transition(
        grant.fencing_token(),
        scope,
        policy,
        store,
        host,
        product,
        effects,
    );
}

fn recover_freeze_transition<S, H, P, E>(
    fencing_token: u64,
    scope: &ControllerScope,
    policy: LifecyclePolicy,
    store: &mut S,
    host: &mut H,
    product: &mut P,
    effects: &mut E,
) -> Result<FreezeRecoveryOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    H: HostLifecycleControl,
    P: ProductLifecycleControl,
    E: FreezeTransitionEffects,
{
    let current = store
        .load(&scope.workload_id)
        .map_err(ReconcileError::Record)?;
    current
        .validate()
        .map_err(|_error| ReconcileError::InvalidRecord)?;
    validate_recovery_authority(scope, fencing_token, policy, &current)?;

    let status = effects.freeze_status().map_err(ReconcileError::Effect)?;
    if !status.populated {
        return Err(ReconcileError::Effect(UNPOPULATED_TARGET.to_owned()));
    }

    match current.state {
        PersistedLifecycleState::Freezing => {
            return recover_freezing(
                fencing_token,
                current,
                status,
                store,
                host,
                product,
                effects,
            );
        }
        PersistedLifecycleState::Thawing => {
            return recover_thawing(
                fencing_token,
                current,
                status,
                store,
                host,
                product,
                effects,
            );
        }
        state => {
            return Err(ReconcileError::RecoveryRequired(state));
        }
    }
}

fn recover_freezing<S, H, P, E>(
    fencing_token: u64,
    current: LifecycleRecord,
    status: FreezeTransitionStatus,
    store: &mut S,
    host: &mut H,
    product: &mut P,
    effects: &mut E,
) -> Result<FreezeRecoveryOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    H: HostLifecycleControl,
    P: ProductLifecycleControl,
    E: FreezeTransitionEffects,
{
    if status.frozen {
        let frozen = next_record(
            &current,
            fencing_token,
            PersistedLifecycleState::Frozen,
        );
        persist(store, &current, &frozen)?;
        return Ok(FreezeRecoveryOutcome::Frozen);
    }

    let thawing = next_record(
        &current,
        fencing_token,
        PersistedLifecycleState::Thawing,
    );
    persist(store, &current, &thawing)?;

    // Explicitly write thaw even though the sampled state was thawed. A freeze
    // request may have been accepted but not yet reached the kernel-observed
    // terminal state when the prior agent died. This cancels that pending intent.
    effects.thaw().map_err(ReconcileError::Effect)?;
    return finish_running(thawing, fencing_token, store, host, product);
}

fn recover_thawing<S, H, P, E>(
    fencing_token: u64,
    current: LifecycleRecord,
    status: FreezeTransitionStatus,
    store: &mut S,
    host: &mut H,
    product: &mut P,
    effects: &mut E,
) -> Result<FreezeRecoveryOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    H: HostLifecycleControl,
    P: ProductLifecycleControl,
    E: FreezeTransitionEffects,
{
    if status.frozen {
        effects.thaw().map_err(ReconcileError::Effect)?;
    }

    return finish_running(current, fencing_token, store, host, product);
}

fn finish_running<S, H, P>(
    current: LifecycleRecord,
    fencing_token: u64,
    store: &mut S,
    host: &mut H,
    product: &mut P,
) -> Result<FreezeRecoveryOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    H: HostLifecycleControl,
    P: ProductLifecycleControl,
{
    // Both product bridges define `v1 resume` as idempotent: an already-open
    // product returns `ok running`. This makes retry after a crash between product
    // reopen and durable Running publication safe.
    product
        .cancel_quiesce()
        .map_err(ReconcileError::Product)?;
    host.verify_ready().map_err(ReconcileError::Host)?;

    let running = next_record(
        &current,
        fencing_token,
        PersistedLifecycleState::Running,
    );
    persist(store, &current, &running)?;
    return Ok(FreezeRecoveryOutcome::Running);
}

fn validate_recovery_authority(
    scope: &ControllerScope,
    fencing_token: u64,
    policy: LifecyclePolicy,
    record: &LifecycleRecord,
) -> Result<(), ReconcileError> {
    if record.workload_id != scope.workload_id
        || record.assigned_node != scope.node
        || record.placement_epoch != scope.placement_epoch
        || record.runtime_epoch != scope.runtime_epoch
    {
        return Err(ReconcileError::StalePlacement);
    }
    if fencing_token < record.fencing_token {
        return Err(ReconcileError::StaleFence);
    }
    if policy.strategy != SuspendStrategy::Freeze
        || record.strategy != PersistedSuspendStrategy::Freeze
    {
        return Err(ReconcileError::InvalidRecord);
    }

    return Ok(());
}

fn persist<S: LifecycleRecordStore>(
    store: &mut S,
    current: &LifecycleRecord,
    next: &LifecycleRecord,
) -> Result<(), ReconcileError> {
    validate_record_update(current, next).map_err(|_error| ReconcileError::InvalidRecord)?;
    return store.replace(current, next).map_err(ReconcileError::Record);
}

fn next_record(
    current: &LifecycleRecord,
    fencing_token: u64,
    state: PersistedLifecycleState,
) -> LifecycleRecord {
    return LifecycleRecord {
        workload_id: current.workload_id.clone(),
        assigned_node: current.assigned_node.clone(),
        placement_epoch: current.placement_epoch,
        runtime_epoch: current.runtime_epoch,
        fencing_token,
        revision: current.revision.saturating_add(1),
        state,
        strategy: current.strategy,
        checkpoint: None,
    };
}

#[cfg(test)]
mod tests {
    use crate::process_lifecycle::ActivitySnapshot;
    use crate::process_lifecycle_agent::{LifecycleEffects, ProductQuiesceOutcome};
    use crate::process_lifecycle_record::LifecycleCheckpoint;

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
        verified: bool,
    }

    impl HostLifecycleControl for FakeHost {
        fn observe(&mut self) -> Result<ActivitySnapshot, String> {
            return Err("recovery must not consult ordinary demand observation".to_owned());
        }

        fn verify_ready(&mut self) -> Result<(), String> {
            self.verified = true;
            return Ok(());
        }
    }

    struct FakeProduct {
        reopened: bool,
    }

    impl ProductLifecycleControl for FakeProduct {
        fn quiesce(&mut self) -> Result<ProductQuiesceOutcome, String> {
            return Err("recovery must not quiesce again".to_owned());
        }

        fn cancel_quiesce(&mut self) -> Result<(), String> {
            self.reopened = true;
            return Ok(());
        }
    }

    struct FakeEffects {
        status: FreezeTransitionStatus,
        thaw_calls: u64,
    }

    impl LifecycleEffects for FakeEffects {
        fn freeze(&mut self) -> Result<(), String> {
            return Err("recovery must not replay freeze".to_owned());
        }

        fn thaw(&mut self) -> Result<(), String> {
            self.thaw_calls = self.thaw_calls.saturating_add(1);
            return Ok(());
        }

        fn checkpoint_and_terminate(&mut self) -> Result<LifecycleCheckpoint, String> {
            return Err("hibernate unavailable".to_owned());
        }

        fn restore(&mut self, _checkpoint: &LifecycleCheckpoint) -> Result<(), String> {
            return Err("hibernate unavailable".to_owned());
        }
    }

    impl FreezeTransitionEffects for FakeEffects {
        fn freeze_status(&mut self) -> Result<FreezeTransitionStatus, String> {
            return Ok(self.status);
        }
    }

    fn scope() -> ControllerScope {
        return ControllerScope {
            workload_id: "workload-7".to_owned(),
            node: "node-a".to_owned(),
            placement_epoch: 4,
            runtime_epoch: 8,
        };
    }

    const fn policy() -> LifecyclePolicy {
        return LifecyclePolicy {
            strategy: SuspendStrategy::Freeze,
            minimum_idle_ms: 30_000,
        };
    }

    fn record(state: PersistedLifecycleState) -> LifecycleRecord {
        return LifecycleRecord {
            workload_id: "workload-7".to_owned(),
            assigned_node: "node-a".to_owned(),
            placement_epoch: 4,
            runtime_epoch: 8,
            fencing_token: 9,
            revision: 12,
            state,
            strategy: PersistedSuspendStrategy::Freeze,
            checkpoint: None,
        };
    }

    fn run_recovery(
        state: PersistedLifecycleState,
        status: FreezeTransitionStatus,
    ) -> Result<(FreezeRecoveryOutcome, LifecycleRecord, bool, bool, u64), String> {
        let mut store = MemoryStore {
            record: record(state),
        };
        let mut host = FakeHost { verified: false };
        let mut product = FakeProduct { reopened: false };
        let mut effects = FakeEffects {
            status,
            thaw_calls: 0,
        };

        let outcome = recover_freeze_transition(
            10,
            &scope(),
            policy(),
            &mut store,
            &mut host,
            &mut product,
            &mut effects,
        )
        .map_err(|error| format!("recovery failed: {error:?}"))?;

        return Ok((
            outcome,
            store.record,
            host.verified,
            product.reopened,
            effects.thaw_calls,
        ));
    }

    fn require(condition: bool, message: &str) -> Result<(), String> {
        if !condition {
            return Err(message.to_owned());
        }
        return Ok(());
    }

    #[test]
    fn freezing_with_kernel_frozen_finishes_frozen_without_replaying_effect() -> Result<(), String> {
        let (outcome, record, verified, reopened, thaw_calls) = run_recovery(
            PersistedLifecycleState::Freezing,
            FreezeTransitionStatus {
                populated: true,
                frozen: true,
            },
        )?;

        require(outcome == FreezeRecoveryOutcome::Frozen, "expected Frozen outcome")?;
        require(record.state == PersistedLifecycleState::Frozen, "expected durable Frozen")?;
        require(record.runtime_epoch == 8, "expected runtime epoch to be preserved")?;
        require(record.fencing_token == 10, "expected fresh fence")?;
        require(!verified, "Frozen recovery must not admit host routing")?;
        require(!reopened, "Frozen recovery must keep product sealed")?;
        return require(thaw_calls == 0, "Frozen recovery must not thaw");
    }

    #[test]
    fn freezing_but_kernel_thawed_cancels_pending_freeze_and_returns_running() -> Result<(), String> {
        let (outcome, record, verified, reopened, thaw_calls) = run_recovery(
            PersistedLifecycleState::Freezing,
            FreezeTransitionStatus {
                populated: true,
                frozen: false,
            },
        )?;

        require(outcome == FreezeRecoveryOutcome::Running, "expected Running outcome")?;
        require(record.state == PersistedLifecycleState::Running, "expected durable Running")?;
        require(verified, "host admission must be verified")?;
        require(reopened, "product must be reopened")?;
        return require(thaw_calls == 1, "pending freeze must be cancelled by explicit thaw");
    }

    #[test]
    fn thawing_while_still_frozen_finishes_thaw_and_returns_running() -> Result<(), String> {
        let (outcome, record, verified, reopened, thaw_calls) = run_recovery(
            PersistedLifecycleState::Thawing,
            FreezeTransitionStatus {
                populated: true,
                frozen: true,
            },
        )?;

        require(outcome == FreezeRecoveryOutcome::Running, "expected Running outcome")?;
        require(record.state == PersistedLifecycleState::Running, "expected durable Running")?;
        require(verified, "host admission must be verified")?;
        require(reopened, "product must be reopened")?;
        return require(thaw_calls == 1, "frozen thawing recovery must thaw once");
    }

    #[test]
    fn thawing_already_thawed_does_not_replay_effect() -> Result<(), String> {
        let (outcome, record, verified, reopened, thaw_calls) = run_recovery(
            PersistedLifecycleState::Thawing,
            FreezeTransitionStatus {
                populated: true,
                frozen: false,
            },
        )?;

        require(outcome == FreezeRecoveryOutcome::Running, "expected Running outcome")?;
        require(record.state == PersistedLifecycleState::Running, "expected durable Running")?;
        require(verified, "host admission must be verified")?;
        require(reopened, "product must be reopened")?;
        return require(thaw_calls == 0, "already-thawed recovery must not replay thaw");
    }

    #[test]
    fn stale_runtime_epoch_fails_closed_before_effects() -> Result<(), String> {
        let mut store = MemoryStore {
            record: record(PersistedLifecycleState::Freezing),
        };
        let mut host = FakeHost { verified: false };
        let mut product = FakeProduct { reopened: false };
        let mut effects = FakeEffects {
            status: FreezeTransitionStatus {
                populated: true,
                frozen: true,
            },
            thaw_calls: 0,
        };
        let mut stale_scope = scope();
        stale_scope.runtime_epoch = 9;

        let result = recover_freeze_transition(
            10,
            &stale_scope,
            policy(),
            &mut store,
            &mut host,
            &mut product,
            &mut effects,
        );

        require(
            result == Err(ReconcileError::StalePlacement),
            "runtime epoch mismatch must fail closed",
        )?;
        require(store.record.state == PersistedLifecycleState::Freezing, "record must stay Freezing")?;
        require(!host.verified, "host must not be admitted")?;
        return require(!product.reopened, "product must stay sealed");
    }

    #[test]
    fn unpopulated_target_fails_closed() -> Result<(), String> {
        let mut store = MemoryStore {
            record: record(PersistedLifecycleState::Freezing),
        };
        let mut host = FakeHost { verified: false };
        let mut product = FakeProduct { reopened: false };
        let mut effects = FakeEffects {
            status: FreezeTransitionStatus {
                populated: false,
                frozen: false,
            },
            thaw_calls: 0,
        };

        let result = recover_freeze_transition(
            10,
            &scope(),
            policy(),
            &mut store,
            &mut host,
            &mut product,
            &mut effects,
        );

        require(
            result == Err(ReconcileError::Effect(UNPOPULATED_TARGET.to_owned())),
            "unpopulated target must fail closed",
        )?;
        require(store.record.state == PersistedLifecycleState::Freezing, "record must stay Freezing")?;
        require(!host.verified, "host must not be admitted")?;
        return require(!product.reopened, "product must stay sealed");
    }
}
