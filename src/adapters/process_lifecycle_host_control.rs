//! Trusted per-workload host authority for the lifecycle reconciler.
//!
//! The generic reconciler consumes `HostLifecycleControl`, but the concrete host
//! protocol returns a fleet snapshot containing both demand and exact Linux
//! process identity. This adapter pins one validated workload incarnation and
//! refuses to turn later observations into lifecycle authority if placement,
//! runtime, PID/start-time, managed cgroup, or isolation policy identity moves.
//!
//! Queue depth, in-flight work, and idle time are intentionally refreshed on
//! every observation. They are trusted host/control-plane facts, not part of the
//! pinned identity tuple. Product-socket status remains cooperative evidence and
//! never enters this adapter.

#![forbid(unsafe_code)]

use crate::process_lifecycle::ActivitySnapshot;
use crate::process_lifecycle_agent::HostLifecycleControl;
use crate::process_lifecycle_runtime::{
    LifecycleAgentConfig, LifecycleRuntimeError, TrustedHostControlClient, TrustedHostSnapshot,
    TrustedWorkloadSnapshot,
};

/// Trusted host/control-plane source used by the per-workload adapter.
///
/// Implementations must run outside the tenant-controlled workload boundary and
/// must validate their wire/storage input before returning a snapshot. The
/// production implementation is `TrustedHostControlClient`.
pub trait TrustedHostAuthority {
    fn snapshot(&self) -> Result<TrustedHostSnapshot, LifecycleRuntimeError>;

    fn admit_after_wake(
        &self,
        workload: &TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<(), LifecycleRuntimeError>;
}

impl TrustedHostAuthority for TrustedHostControlClient {
    fn snapshot(&self) -> Result<TrustedHostSnapshot, LifecycleRuntimeError> {
        return TrustedHostControlClient::snapshot(self);
    }

    fn admit_after_wake(
        &self,
        workload: &TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<(), LifecycleRuntimeError> {
        return TrustedHostControlClient::admit_after_wake(self, workload, fencing_token);
    }
}

#[derive(Clone, Debug)]
pub struct TrustedHostWorkloadControl<A = TrustedHostControlClient> {
    authority: A,
    expected: TrustedWorkloadSnapshot,
    fencing_token: u64,
}

impl TrustedHostWorkloadControl<TrustedHostControlClient> {
    pub fn new(
        config: &LifecycleAgentConfig,
        expected: TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<Self, LifecycleRuntimeError> {
        return Self::with_authority(
            TrustedHostControlClient::new(config),
            expected,
            fencing_token,
        );
    }
}

impl<A> TrustedHostWorkloadControl<A>
where
    A: TrustedHostAuthority,
{
    pub fn with_authority(
        authority: A,
        expected: TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<Self, LifecycleRuntimeError> {
        if fencing_token == 0 {
            return Err(LifecycleRuntimeError::InvalidHostControlRequest);
        }
        let control = Self {
            authority,
            expected,
            fencing_token,
        };
        control.revalidate_identity()?;
        return Ok(control);
    }

    #[must_use]
    pub fn expected(&self) -> &TrustedWorkloadSnapshot {
        return &self.expected;
    }

    #[must_use]
    pub const fn fencing_token(&self) -> u64 {
        return self.fencing_token;
    }

    /// Re-check only the pinned workload authority tuple.
    ///
    /// Crash recovery uses this immediately before an effect without consulting
    /// queue/in-flight demand. Recovery semantics therefore remain based on
    /// durable intent + kernel state rather than ordinary demand observation.
    pub fn revalidate_identity(&self) -> Result<(), LifecycleRuntimeError> {
        let _workload = self.refresh_identity()?;
        return Ok(());
    }

    fn refresh_identity(&self) -> Result<TrustedWorkloadSnapshot, LifecycleRuntimeError> {
        let snapshot = self.authority.snapshot()?;
        return select_exact_workload(&snapshot, &self.expected).cloned();
    }
}

impl<A> HostLifecycleControl for TrustedHostWorkloadControl<A>
where
    A: TrustedHostAuthority,
{
    fn observe(&mut self) -> Result<ActivitySnapshot, String> {
        let workload = self.refresh_identity().map_err(|error| error.to_string())?;
        return Ok(ActivitySnapshot {
            queue_depth: workload.queue_depth,
            in_flight: workload.in_flight,
            idle_for_ms: workload.idle_for_ms,
        });
    }

    fn verify_ready(&mut self) -> Result<(), String> {
        let workload = self.refresh_identity().map_err(|error| error.to_string())?;
        self.authority
            .admit_after_wake(&workload, self.fencing_token)
            .map_err(|error| error.to_string())?;
        self.revalidate_identity().map_err(|error| error.to_string())?;
        return Ok(());
    }
}

fn select_exact_workload<'a>(
    snapshot: &'a TrustedHostSnapshot,
    expected: &TrustedWorkloadSnapshot,
) -> Result<&'a TrustedWorkloadSnapshot, LifecycleRuntimeError> {
    let matching = snapshot
        .workloads
        .iter()
        .filter(|workload| workload.workload_id == expected.workload_id)
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(LifecycleRuntimeError::HostIdentityMismatch);
    }
    let observed = matching[0];
    if !same_authority_identity(expected, observed) {
        return Err(LifecycleRuntimeError::HostIdentityMismatch);
    }
    return Ok(observed);
}

fn same_authority_identity(
    expected: &TrustedWorkloadSnapshot,
    observed: &TrustedWorkloadSnapshot,
) -> bool {
    return observed.workload_id == expected.workload_id
        && observed.assigned_node == expected.assigned_node
        && observed.runtime_epoch == expected.runtime_epoch
        && observed.placement_epoch == expected.placement_epoch
        && observed.pid == expected.pid
        && observed.process_start_ticks == expected.process_start_ticks
        && observed.managed_cgroup == expected.managed_cgroup
        && observed.isolation_policy_digest == expected.isolation_policy_digest;
}

#[cfg(test)]
#[path = "tests/process_lifecycle_host_control.rs"]
mod tests;
