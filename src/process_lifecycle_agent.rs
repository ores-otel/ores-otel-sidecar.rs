//! Host-agent orchestration between pure lifecycle policy and outward effects.
//!
//! Distributed lease acquisition intentionally stays outside this module. A
//! caller acquires one fenced lifecycle grant through `ores-locks-and-leases`,
//! then calls [`reconcile_once`] with that fencing token. This keeps Cloudflare
//! Durable Objects, Fiducia, and future BeamScale-native lease authorities behind
//! one interchangeable seam while making effect ordering independently testable.
//!
//! A hostile tenant may control its entire runtime process. Therefore tenant
//! runtime messages are never authoritative for queue demand, in-flight work,
//! process identity, isolation, or post-wake admission. Those facts come from
//! trusted host/control-plane adapters outside the managed workload boundary.

#![forbid(unsafe_code)]

use crate::process_lifecycle::{
    ActivitySnapshot, LifecycleAction, LifecycleEvent, LifecyclePolicy, LifecycleState,
    SuspendStrategy, decide,
};
use crate::process_lifecycle_record::{
    LifecycleCheckpoint, LifecycleRecord, PersistedLifecycleState, PersistedSuspendStrategy,
    validate_record_update,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProductQuiesceOutcome {
    /// The cooperative product runtime reports that its local drain completed.
    /// The host must still re-observe trusted demand before a process effect.
    Drained,
    /// The cooperative product runtime observed new local demand while draining.
    DemandReturned,
}

/// Trusted host/control-plane demand observation.
///
/// Implementations must live outside the tenant-controlled runtime and derive
/// queue depth, assigned/in-flight work, and idle duration from trusted scheduler,
/// queue, dispatch-lease, or host-agent state. A tenant BEAM/process must never
/// implement this authority for its own lifecycle decision.
pub trait HostDemandObservation {
    fn observe(&mut self) -> Result<ActivitySnapshot, String>;
}

/// Product-specific cooperative runtime boundary.
///
/// A hostile tenant may compromise this implementation because it can run inside
/// the tenant BEAM/process. Quiesce therefore improves consistency and wake
/// latency but is not itself a security or lifecycle-authority proof. Erlang
/// references/PIDs remain process-local and must never cross this boundary.
pub trait ProductLifecycleControl {
    fn quiesce(&mut self) -> Result<ProductQuiesceOutcome, String>;
    fn cancel_quiesce(&mut self) -> Result<(), String>;
}

/// Trusted post-wake admission verification.
///
/// Implementations run outside the tenant runtime and must bind the observed
/// process identity to the expected managed scope/cgroup/runtime epoch before
/// returning success. For the hostile-process class this is where namespace,
/// no-new-privileges, zero-capability, process-start, cgroup, and product routing
/// invariants are re-established before durable state becomes `running`.
pub trait RuntimeAdmissionVerifier {
    fn verify_ready(&mut self) -> Result<(), String>;
}

/// Durable compare-and-set record boundary. Implementations must make replace
/// atomic and reject a changed expected revision/fence rather than overwriting it.
pub trait LifecycleRecordStore {
    fn load(&mut self, workload_id: &str) -> Result<LifecycleRecord, String>;
    fn replace(
        &mut self,
        expected: &LifecycleRecord,
        next: &LifecycleRecord,
    ) -> Result<(), String>;
}

/// Linux/process effects. The shared Linux adapter provides cgroup and CRIU
/// primitives; product/infra wiring decides how checkpoint artifacts become
/// durable and content-addressed.
pub trait LifecycleEffects {
    fn freeze(&mut self) -> Result<(), String>;
    fn thaw(&mut self) -> Result<(), String>;
    fn checkpoint_and_terminate(&mut self) -> Result<LifecycleCheckpoint, String>;
    fn restore(&mut self, checkpoint: &LifecycleCheckpoint) -> Result<(), String>;
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
    RecoveredQuiesce,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileError {
    Record(String),
    Observation(String),
    Product(String),
    Admission(String),
    Effect(String),
    InvalidRecord,
    StalePlacement,
    StaleFence,
    InvalidTransition,
    RecoveryRequired(PersistedLifecycleState),
}

/// Reconcile one stable lifecycle state while holding the workload's fenced
/// distributed lease.
///
/// A persisted `quiescing` record is recoverable without replaying a process
/// effect: cancel the product-local quiesce session, publish `running` under the
/// new/current fence, and let a later reconciliation attempt suspension again.
/// Transitional process-effect states (`freezing`, `checkpointing`, `thawing`,
/// `restoring`) are deliberately not replayed here because crash recovery must
/// compare the durable intent with observed cgroup/process/checkpoint reality.
pub fn reconcile_once<S, D, P, A, E>(
    scope: &ControllerScope,
    fencing_token: u64,
    policy: LifecyclePolicy,
    store: &mut S,
    demand: &mut D,
    product: &mut P,
    admission: &mut A,
    effects: &mut E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    D: HostDemandObservation,
    P: ProductLifecycleControl,
    A: RuntimeAdmissionVerifier,
    E: LifecycleEffects,
{
    let current = store
        .load(&scope.workload_id)
        .map_err(ReconcileError::Record)?;
    current
        .validate()
        .map_err(|_error| ReconcileError::InvalidRecord)?;
    validate_authority(scope, fencing_token, &current)?;
    validate_strategy(policy, &current)?;

    match current.state {
        PersistedLifecycleState::Quiescing => {
            return recover_quiesce(fencing_token, current, store, product);
        }
        PersistedLifecycleState::Running
        | PersistedLifecycleState::Frozen
        | PersistedLifecycleState::Hibernated => {}
        PersistedLifecycleState::Freezing
        | PersistedLifecycleState::Checkpointing
        | PersistedLifecycleState::Thawing
        | PersistedLifecycleState::Restoring => {
            return Err(ReconcileError::RecoveryRequired(current.state));
        }
    }

    let activity = demand.observe().map_err(ReconcileError::Observation)?;
    let state = to_policy_state(&current);
    let decision = decide(state, policy, LifecycleEvent::Observe(activity))
        .map_err(|_error| ReconcileError::InvalidTransition)?;

    match decision.action {
        LifecycleAction::None => {
            return Ok(ReconcileOutcome::NoChange);
        }
        LifecycleAction::BeginQuiesce => {
            return suspend_from_running(
                fencing_token,
                policy,
                current,
                store,
                demand,
                product,
                effects,
            );
        }
        LifecycleAction::Thaw => {
            return resume_frozen(fencing_token, current, store, admission, effects);
        }
        LifecycleAction::Restore => {
            return restore_hibernated(fencing_token, current, store, admission, effects);
        }
        LifecycleAction::CancelQuiesce
        | LifecycleAction::Freeze
        | LifecycleAction::CheckpointAndTerminate => {
            return Err(ReconcileError::InvalidTransition);
        }
    }
}

fn recover_quiesce<S, P>(
    fencing_token: u64,
    current: LifecycleRecord,
    store: &mut S,
    product: &mut P,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    P: ProductLifecycleControl,
{
    product
        .cancel_quiesce()
        .map_err(ReconcileError::Product)?;
    let running = next_record(
        &current,
        fencing_token,
        PersistedLifecycleState::Running,
        None,
    );
    persist(store, &current, &running)?;
    return Ok(ReconcileOutcome::RecoveredQuiesce);
}

fn suspend_from_running<S, D, P, E>(
    fencing_token: u64,
    policy: LifecyclePolicy,
    current: LifecycleRecord,
    store: &mut S,
    demand: &mut D,
    product: &mut P,
    effects: &mut E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    D: HostDemandObservation,
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
            return cancel_suspend_for_demand(fencing_token, quiescing, store, product);
        }
        ProductQuiesceOutcome::Drained => {}
    }

    // A tenant-controlled runtime can lie about its own drain state. Re-observe
    // demand from the trusted host/control-plane authority after the cooperative
    // quiesce and immediately before committing a process-effect intent.
    let after_quiesce_activity = demand.observe().map_err(ReconcileError::Observation)?;
    if after_quiesce_activity.has_demand() {
        return cancel_suspend_for_demand(fencing_token, quiescing, store, product);
    }

    let after_quiesce = decide(
        LifecycleState::Quiescing(policy.strategy),
        policy,
        LifecycleEvent::QuiesceSucceeded,
    )
    .map_err(|_error| ReconcileError::InvalidTransition)?;

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
        LifecycleAction::None
        | LifecycleAction::BeginQuiesce
        | LifecycleAction::CancelQuiesce
        | LifecycleAction::Thaw
        | LifecycleAction::Restore => {
            return Err(ReconcileError::InvalidTransition);
        }
    }
}

fn cancel_suspend_for_demand<S, P>(
    fencing_token: u64,
    quiescing: LifecycleRecord,
    store: &mut S,
    product: &mut P,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    P: ProductLifecycleControl,
{
    product
        .cancel_quiesce()
        .map_err(ReconcileError::Product)?;
    let running = next_record(
        &quiescing,
        fencing_token,
        PersistedLifecycleState::Running,
        None,
    );
    persist(store, &quiescing, &running)?;
    return Ok(ReconcileOutcome::DemandCancelledSuspend);
}

fn resume_frozen<S, A, E>(
    fencing_token: u64,
    current: LifecycleRecord,
    store: &mut S,
    admission: &mut A,
    effects: &mut E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    A: RuntimeAdmissionVerifier,
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
    admission
        .verify_ready()
        .map_err(ReconcileError::Admission)?;
    let running = next_record(
        &thawing,
        fencing_token,
        PersistedLifecycleState::Running,
        None,
    );
    persist(store, &thawing, &running)?;
    return Ok(ReconcileOutcome::Resumed);
}

fn restore_hibernated<S, A, E>(
    fencing_token: u64,
    current: LifecycleRecord,
    store: &mut S,
    admission: &mut A,
    effects: &mut E,
) -> Result<ReconcileOutcome, ReconcileError>
where
    S: LifecycleRecordStore,
    A: RuntimeAdmissionVerifier,
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
    admission
        .verify_ready()
        .map_err(ReconcileError::Admission)?;
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
    store: &mut S,
    current: &LifecycleRecord,
    next: &LifecycleRecord,
) -> Result<(), ReconcileError> {
    validate_record_update(current, next).map_err(|_error| ReconcileError::InvalidRecord)?;
    return store.replace(current, next).map_err(ReconcileError::Record);
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

fn validate_strategy(
    policy: LifecyclePolicy,
    record: &LifecycleRecord,
) -> Result<(), ReconcileError> {
    if policy.strategy != to_strategy(record.strategy) {
        return Err(ReconcileError::InvalidRecord);
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
#[path = "state_machine/process_lifecycle_agent_tests.rs"]
mod tests;
