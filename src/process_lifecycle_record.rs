//! Durable lifecycle record invariants shared by host controllers.
//!
//! The distributed lease serializes controllers. The persisted record makes the
//! lease's fencing token durable so a controller that wakes up after lease loss
//! cannot publish stale lifecycle or placement state.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Persisted lifecycle states use explicit stable wire names rather than Rust
/// enum layout or debug formatting.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedLifecycleState {
    Running,
    Quiescing,
    Freezing,
    Frozen,
    Checkpointing,
    Hibernated,
    Restoring,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedSuspendStrategy {
    Freeze,
    Hibernate,
}

/// Checkpoint identity must be immutable and content-addressable by the product
/// store. The local path is deliberately not part of the durable record because
/// a workload may be restored by a different host.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct LifecycleCheckpoint {
    pub artifact_ref: String,
    pub digest: String,
    pub format: String,
}

/// Durable control-plane record for one logical workload/shard.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct LifecycleRecord {
    pub workload_id: String,
    pub assigned_node: String,
    /// Monotonic scheduler placement generation. Increment on reassignment.
    pub placement_epoch: u64,
    /// Monotonic token minted by the distributed lifecycle lease authority.
    pub fencing_token: u64,
    /// Monotonic record revision inside one logical workload history.
    pub revision: u64,
    pub state: PersistedLifecycleState,
    pub strategy: PersistedSuspendStrategy,
    pub checkpoint: Option<LifecycleCheckpoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleRecordError {
    EmptyWorkloadId,
    EmptyAssignedNode,
    InvalidCheckpoint,
    ZeroPlacementEpoch,
    ZeroFencingToken,
    ZeroRevision,
    WorkloadChanged,
    StaleFencingToken,
    RevisionDidNotAdvance,
    PlacementEpochRegressed,
    PlacementChangedWithoutEpochAdvance,
    CheckpointRequired,
    CheckpointNotAllowed,
}

impl LifecycleRecord {
    pub fn validate(&self) -> Result<(), LifecycleRecordError> {
        if self.workload_id.is_empty() {
            return Err(LifecycleRecordError::EmptyWorkloadId);
        }

        if self.assigned_node.is_empty() {
            return Err(LifecycleRecordError::EmptyAssignedNode);
        }

        if self.placement_epoch == 0 {
            return Err(LifecycleRecordError::ZeroPlacementEpoch);
        }

        if self.fencing_token == 0 {
            return Err(LifecycleRecordError::ZeroFencingToken);
        }

        if self.revision == 0 {
            return Err(LifecycleRecordError::ZeroRevision);
        }

        if let Some(checkpoint) = &self.checkpoint {
            if checkpoint.artifact_ref.is_empty()
                || checkpoint.digest.is_empty()
                || checkpoint.format.is_empty()
            {
                return Err(LifecycleRecordError::InvalidCheckpoint);
            }
        }

        match self.state {
            PersistedLifecycleState::Hibernated | PersistedLifecycleState::Restoring => {
                if self.checkpoint.is_none() {
                    return Err(LifecycleRecordError::CheckpointRequired);
                }
            }
            PersistedLifecycleState::Running
            | PersistedLifecycleState::Quiescing
            | PersistedLifecycleState::Freezing
            | PersistedLifecycleState::Frozen => {
                if self.checkpoint.is_some() {
                    return Err(LifecycleRecordError::CheckpointNotAllowed);
                }
            }
            PersistedLifecycleState::Checkpointing => {}
        }

        return Ok(());
    }

    /// A host may perform a local process effect only while it still owns the
    /// record's placement and the record was written under its current fence.
    #[must_use]
    pub fn authorizes_controller(
        &self,
        node: &str,
        placement_epoch: u64,
        fencing_token: u64,
    ) -> bool {
        return self.assigned_node == node
            && self.placement_epoch == placement_epoch
            && self.fencing_token == fencing_token;
    }
}

/// Validate an atomic compare-and-set replacement of one lifecycle record.
///
/// A strictly newer fence may continue from any prior revision. Reusing the same
/// fence is allowed for a multi-step transition, but the durable revision must
/// advance. Placement can only move forward, and moving nodes requires an epoch
/// increment so the prior host is unambiguously stale.
pub fn validate_record_update(
    current: &LifecycleRecord,
    next: &LifecycleRecord,
) -> Result<(), LifecycleRecordError> {
    current.validate()?;
    next.validate()?;

    if next.workload_id != current.workload_id {
        return Err(LifecycleRecordError::WorkloadChanged);
    }

    if next.fencing_token < current.fencing_token {
        return Err(LifecycleRecordError::StaleFencingToken);
    }

    if next.revision <= current.revision {
        return Err(LifecycleRecordError::RevisionDidNotAdvance);
    }

    if next.placement_epoch < current.placement_epoch {
        return Err(LifecycleRecordError::PlacementEpochRegressed);
    }

    if next.assigned_node != current.assigned_node
        && next.placement_epoch <= current.placement_epoch
    {
        return Err(LifecycleRecordError::PlacementChangedWithoutEpochAdvance);
    }

    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> LifecycleRecord {
        return LifecycleRecord {
            workload_id: "tenant-42-shard-3".to_owned(),
            assigned_node: "node-a".to_owned(),
            placement_epoch: 7,
            fencing_token: 19,
            revision: 11,
            state: PersistedLifecycleState::Frozen,
            strategy: PersistedSuspendStrategy::Freeze,
            checkpoint: None,
        };
    }

    #[test]
    fn stale_fence_cannot_publish_state() {
        let current = record();
        let mut next = current.clone();
        next.fencing_token = 18;
        next.revision = 12;

        assert_eq!(
            validate_record_update(&current, &next),
            Err(LifecycleRecordError::StaleFencingToken)
        );
    }

    #[test]
    fn workload_identity_cannot_change_in_place() {
        let current = record();
        let mut next = current.clone();
        next.workload_id = "tenant-99-shard-1".to_owned();
        next.fencing_token = 20;
        next.revision = 12;

        assert_eq!(
            validate_record_update(&current, &next),
            Err(LifecycleRecordError::WorkloadChanged)
        );
    }

    #[test]
    fn reassignment_requires_new_placement_epoch() {
        let current = record();
        let mut next = current.clone();
        next.assigned_node = "node-b".to_owned();
        next.fencing_token = 20;
        next.revision = 12;

        assert_eq!(
            validate_record_update(&current, &next),
            Err(LifecycleRecordError::PlacementChangedWithoutEpochAdvance)
        );
    }

    #[test]
    fn reassignment_invalidates_old_host_authority() {
        let current = record();
        let mut next = current.clone();
        next.assigned_node = "node-b".to_owned();
        next.placement_epoch = 8;
        next.fencing_token = 20;
        next.revision = 12;

        assert_eq!(validate_record_update(&current, &next), Ok(()));
        assert!(!next.authorizes_controller("node-a", 7, 19));
        assert!(next.authorizes_controller("node-b", 8, 20));
    }

    #[test]
    fn hibernated_record_requires_checkpoint() {
        let mut value = record();
        value.state = PersistedLifecycleState::Hibernated;
        value.strategy = PersistedSuspendStrategy::Hibernate;

        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::CheckpointRequired)
        );
    }

    #[test]
    fn same_fence_can_advance_multi_step_transition() {
        let current = record();
        let mut next = current.clone();
        next.revision = 12;
        next.state = PersistedLifecycleState::Running;

        assert_eq!(validate_record_update(&current, &next), Ok(()));
    }
}
