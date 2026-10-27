use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use super::*;

#[derive(Debug)]
struct FakeState {
    snapshots: VecDeque<TrustedHostSnapshot>,
    fallback: TrustedHostSnapshot,
    admissions: Vec<(TrustedWorkloadSnapshot, u64)>,
}

#[derive(Clone, Debug)]
struct FakeAuthority {
    state: Rc<RefCell<FakeState>>,
    admission_result: Result<(), LifecycleRuntimeError>,
}

impl FakeAuthority {
    fn new(snapshots: Vec<TrustedHostSnapshot>) -> Self {
        let fallback = snapshots
            .last()
            .cloned()
            .expect("test authority needs at least one snapshot");
        return Self {
            state: Rc::new(RefCell::new(FakeState {
                snapshots: snapshots.into(),
                fallback,
                admissions: Vec::new(),
            })),
            admission_result: Ok(()),
        };
    }

    fn admissions(&self) -> Vec<(TrustedWorkloadSnapshot, u64)> {
        return self.state.borrow().admissions.clone();
    }
}

impl TrustedHostAuthority for FakeAuthority {
    fn snapshot(&self) -> Result<TrustedHostSnapshot, LifecycleRuntimeError> {
        let state = &mut *self.state.borrow_mut();
        return Ok(state
            .snapshots
            .pop_front()
            .unwrap_or_else(|| state.fallback.clone()));
    }

    fn admit_after_wake(
        &self,
        workload: &TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<(), LifecycleRuntimeError> {
        self.state
            .borrow_mut()
            .admissions
            .push((workload.clone(), fencing_token));
        return self.admission_result;
    }
}

fn workload() -> TrustedWorkloadSnapshot {
    return TrustedWorkloadSnapshot {
        workload_id: "tenant-42-shard-3".to_owned(),
        assigned_node: "node-a".to_owned(),
        runtime_epoch: 9,
        placement_epoch: 7,
        pid: 4242,
        process_start_ticks: 987_654,
        managed_cgroup: PathBuf::from(
            "/sys/fs/cgroup/beamscale-workloads.slice/tenant-42-shard-3.scope",
        ),
        isolation_policy_digest: format!("sha256:{}", "a".repeat(64)),
        queue_depth: 0,
        in_flight: 0,
        idle_for_ms: 60_000,
        routable: true,
    };
}

fn snapshot(workloads: Vec<TrustedWorkloadSnapshot>) -> TrustedHostSnapshot {
    return TrustedHostSnapshot {
        version: 2,
        workloads,
    };
}

#[test]
fn trusted_demand_refreshes_without_changing_pinned_identity() {
    let expected = workload();
    let changed_demand = TrustedWorkloadSnapshot {
        queue_depth: 3,
        in_flight: 2,
        idle_for_ms: 0,
        routable: false,
        ..expected.clone()
    };
    let authority = FakeAuthority::new(vec![
        snapshot(vec![expected.clone()]),
        snapshot(vec![changed_demand]),
    ]);
    let mut control = TrustedHostWorkloadControl::with_authority(authority, expected, 41)
        .expect("trusted exact identity should construct");

    assert_eq!(
        control.observe(),
        Ok(ActivitySnapshot {
            queue_depth: 3,
            in_flight: 2,
            idle_for_ms: 0,
        })
    );
}

#[test]
fn pid_reuse_is_rejected_before_demand_becomes_authoritative() {
    let expected = workload();
    let reused_pid = TrustedWorkloadSnapshot {
        process_start_ticks: expected.process_start_ticks + 1,
        ..expected.clone()
    };
    let authority = FakeAuthority::new(vec![
        snapshot(vec![expected.clone()]),
        snapshot(vec![reused_pid]),
    ]);
    let mut control = TrustedHostWorkloadControl::with_authority(authority, expected, 41)
        .expect("initial trusted exact identity should construct");

    assert_eq!(
        control.observe(),
        Err(LifecycleRuntimeError::HostIdentityMismatch.to_string())
    );
}

#[test]
fn placement_reassignment_is_rejected_before_effect_policy_can_continue() {
    let expected = workload();
    let replacement = TrustedWorkloadSnapshot {
        assigned_node: "node-b".to_owned(),
        placement_epoch: expected.placement_epoch + 1,
        ..expected.clone()
    };
    let authority = FakeAuthority::new(vec![
        snapshot(vec![expected.clone()]),
        snapshot(vec![replacement]),
    ]);
    let mut control = TrustedHostWorkloadControl::with_authority(authority, expected, 41)
        .expect("initial trusted exact identity should construct");

    assert_eq!(
        control.observe(),
        Err(LifecycleRuntimeError::HostIdentityMismatch.to_string())
    );
}

#[test]
fn construction_rejects_missing_or_duplicate_logical_workload() {
    let expected = workload();
    let missing = FakeAuthority::new(vec![snapshot(Vec::new())]);
    assert!(matches!(
        TrustedHostWorkloadControl::with_authority(missing, expected.clone(), 41),
        Err(LifecycleRuntimeError::HostIdentityMismatch)
    ));

    let duplicate = FakeAuthority::new(vec![snapshot(vec![expected.clone(), expected.clone()])]);
    assert!(matches!(
        TrustedHostWorkloadControl::with_authority(duplicate, expected, 41),
        Err(LifecycleRuntimeError::HostIdentityMismatch)
    ));
}

#[test]
fn zero_fence_is_never_accepted_as_post_wake_authority() {
    let expected = workload();
    let authority = FakeAuthority::new(vec![snapshot(vec![expected.clone()])]);
    assert!(matches!(
        TrustedHostWorkloadControl::with_authority(authority, expected, 0),
        Err(LifecycleRuntimeError::InvalidHostControlRequest)
    ));
}

#[test]
fn post_wake_admission_uses_exact_identity_and_current_fence() {
    let expected = workload();
    let authority = FakeAuthority::new(vec![
        snapshot(vec![expected.clone()]),
        snapshot(vec![expected.clone()]),
        snapshot(vec![expected.clone()]),
    ]);
    let witness = authority.clone();
    let mut control = TrustedHostWorkloadControl::with_authority(authority, expected.clone(), 77)
        .expect("trusted exact identity should construct");

    assert_eq!(control.verify_ready(), Ok(()));
    assert_eq!(witness.admissions(), vec![(expected, 77)]);
}

#[test]
fn identity_change_after_host_admission_fails_closed() {
    let expected = workload();
    let changed_after_admission = TrustedWorkloadSnapshot {
        managed_cgroup: PathBuf::from(
            "/sys/fs/cgroup/beamscale-workloads.slice/reassigned.scope",
        ),
        ..expected.clone()
    };
    let authority = FakeAuthority::new(vec![
        snapshot(vec![expected.clone()]),
        snapshot(vec![expected.clone()]),
        snapshot(vec![changed_after_admission]),
    ]);
    let witness = authority.clone();
    let mut control = TrustedHostWorkloadControl::with_authority(authority, expected.clone(), 88)
        .expect("trusted exact identity should construct");

    assert_eq!(
        control.verify_ready(),
        Err(LifecycleRuntimeError::HostIdentityMismatch.to_string())
    );
    assert_eq!(witness.admissions(), vec![(expected, 88)]);
}
