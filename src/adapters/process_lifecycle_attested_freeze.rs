//! Identity-attested freeze/thaw effects for hostile multi-tenant runtimes.
//!
//! The expected PID, process start time, and managed cgroup are one authority
//! tuple. The cgroup controller is constructed from that same tuple, then the
//! tuple is re-attested immediately before and after every freeze/thaw/status
//! operation. Callers cannot attest one cgroup and mutate another through this
//! adapter.

#![forbid(unsafe_code)]

use std::time::Duration;

use crate::process_lifecycle_agent::LifecycleEffects;
use crate::process_lifecycle_record::LifecycleCheckpoint;

use super::linux_process_lifecycle::CgroupV2Controller;
use super::process_lifecycle_freeze::{
    FreezeOnlyCgroupEffects, FreezeTransitionEffects, FreezeTransitionStatus,
};
use super::process_lifecycle_identity::{
    ExpectedLinuxProcessIdentity, LinuxProcessIdentityAttestor,
};

const IDENTITY_PREFIX: &str = "lifecycle process identity attestation failed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestedFreezeOnlyCgroupEffects {
    expected: ExpectedLinuxProcessIdentity,
    attestor: LinuxProcessIdentityAttestor,
    effects: FreezeOnlyCgroupEffects,
}

impl AttestedFreezeOnlyCgroupEffects {
    #[must_use]
    pub fn new(expected: ExpectedLinuxProcessIdentity) -> Self {
        let controller = CgroupV2Controller::new(expected.managed_cgroup.clone());
        return Self {
            expected,
            attestor: LinuxProcessIdentityAttestor::default(),
            effects: FreezeOnlyCgroupEffects::new(controller),
        };
    }

    #[must_use]
    pub fn with_attestor_and_timing(
        expected: ExpectedLinuxProcessIdentity,
        attestor: LinuxProcessIdentityAttestor,
        effect_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        let controller = CgroupV2Controller::new(expected.managed_cgroup.clone());
        return Self {
            expected,
            attestor,
            effects: FreezeOnlyCgroupEffects::with_timing(
                controller,
                effect_timeout,
                poll_interval,
            ),
        };
    }

    fn attest(&self) -> Result<(), String> {
        self.attestor
            .attest(&self.expected)
            .map(|_identity| ())
            .map_err(|error| format!("{IDENTITY_PREFIX}: {error}"))
    }

    fn guarded_effect(
        &mut self,
        effect: impl FnOnce(&mut FreezeOnlyCgroupEffects) -> Result<(), String>,
    ) -> Result<(), String> {
        self.attest()?;
        effect(&mut self.effects)?;
        return self.attest();
    }
}

impl LifecycleEffects for AttestedFreezeOnlyCgroupEffects {
    fn freeze(&mut self) -> Result<(), String> {
        return self.guarded_effect(LifecycleEffects::freeze);
    }

    fn thaw(&mut self) -> Result<(), String> {
        return self.guarded_effect(LifecycleEffects::thaw);
    }

    fn checkpoint_and_terminate(&mut self) -> Result<LifecycleCheckpoint, String> {
        return LifecycleEffects::checkpoint_and_terminate(&mut self.effects);
    }

    fn restore(&mut self, checkpoint: &LifecycleCheckpoint) -> Result<(), String> {
        return LifecycleEffects::restore(&mut self.effects, checkpoint);
    }
}

impl FreezeTransitionEffects for AttestedFreezeOnlyCgroupEffects {
    fn freeze_status(&mut self) -> Result<FreezeTransitionStatus, String> {
        self.attest()?;
        let status = FreezeTransitionEffects::freeze_status(&mut self.effects)?;
        self.attest()?;
        return Ok(status);
    }
}
