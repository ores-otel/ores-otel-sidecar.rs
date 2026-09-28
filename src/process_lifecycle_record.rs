//! Durable lifecycle record invariants shared by host controllers.
//!
//! The distributed lease serializes controllers. The persisted record makes the
//! lease's fencing token durable so a controller that wakes up after lease loss
//! cannot publish stale lifecycle, placement, or runtime-incarnation state.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

const MAX_CHECKPOINT_ARTIFACT_REF_BYTES: usize = 2_048;
const MAX_CHECKPOINT_FORMAT_BYTES: usize = 128;
const MAX_IDENTITY_BYTES: usize = 96;

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
    Thawing,
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
    /// Monotonic runtime incarnation. Increment whenever the managed process is
    /// replaced even when it remains on the same node and placement.
    #[serde(default)]
    pub runtime_epoch: u64,
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
    InvalidWorkloadId,
    InvalidAssignedNode,
    InvalidCheckpoint,
    InvalidCheckpointDigest,
    StateStrategyMismatch,
    ZeroPlacementEpoch,
    ZeroRuntimeEpoch,
    ZeroFencingToken,
    ZeroRevision,
    WorkloadChanged,
    StaleFencingToken,
    RevisionDidNotAdvance,
    PlacementEpochRegressed,
    PlacementChangedWithoutEpochAdvance,
    RuntimeEpochRegressed,
    RuntimeChangedWithoutFenceAdvance,
    RuntimeChangedOutsideRunning,
    CheckpointRequired,
    CheckpointNotAllowed,
}

impl LifecycleRecord {
    pub fn validate(&self) -> Result<(), LifecycleRecordError> {
        if self.workload_id.is_empty() {
            return Err(LifecycleRecordError::EmptyWorkloadId);
        }
        if !valid_identity_segment(&self.workload_id) {
            return Err(LifecycleRecordError::InvalidWorkloadId);
        }

        if self.assigned_node.is_empty() {
            return Err(LifecycleRecordError::EmptyAssignedNode);
        }
        if !valid_identity_segment(&self.assigned_node) {
            return Err(LifecycleRecordError::InvalidAssignedNode);
        }

        if self.placement_epoch == 0 {
            return Err(LifecycleRecordError::ZeroPlacementEpoch);
        }

        if self.runtime_epoch == 0 {
            return Err(LifecycleRecordError::ZeroRuntimeEpoch);
        }

        if self.fencing_token == 0 {
            return Err(LifecycleRecordError::ZeroFencingToken);
        }

        if self.revision == 0 {
            return Err(LifecycleRecordError::ZeroRevision);
        }

        if let Some(checkpoint) = &self.checkpoint {
            if checkpoint.artifact_ref.is_empty()
                || checkpoint.artifact_ref.len() > MAX_CHECKPOINT_ARTIFACT_REF_BYTES
                || checkpoint.artifact_ref.chars().any(char::is_control)
                || checkpoint.format.is_empty()
                || checkpoint.format.len() > MAX_CHECKPOINT_FORMAT_BYTES
                || checkpoint.format.chars().any(char::is_control)
            {
                return Err(LifecycleRecordError::InvalidCheckpoint);
            }

            if !valid_checkpoint_digest(&checkpoint.digest) {
                return Err(LifecycleRecordError::InvalidCheckpointDigest);
            }
        }

        match self.state {
            PersistedLifecycleState::Hibernated | PersistedLifecycleState::Restoring => {
                if self.strategy != PersistedSuspendStrategy::Hibernate {
                    return Err(LifecycleRecordError::StateStrategyMismatch);
                }

                if self.checkpoint.is_none() {
                    return Err(LifecycleRecordError::CheckpointRequired);
                }
            }
            PersistedLifecycleState::Checkpointing => {
                if self.strategy != PersistedSuspendStrategy::Hibernate {
                    return Err(LifecycleRecordError::StateStrategyMismatch);
                }

                if self.checkpoint.is_some() {
                    return Err(LifecycleRecordError::CheckpointNotAllowed);
                }
            }
            PersistedLifecycleState::Freezing
            | PersistedLifecycleState::Frozen
            | PersistedLifecycleState::Thawing => {
                if self.strategy != PersistedSuspendStrategy::Freeze {
                    return Err(LifecycleRecordError::StateStrategyMismatch);
                }

                if self.checkpoint.is_some() {
                    return Err(LifecycleRecordError::CheckpointNotAllowed);
                }
            }
            PersistedLifecycleState::Running | PersistedLifecycleState::Quiescing => {
                if self.checkpoint.is_some() {
                    return Err(LifecycleRecordError::CheckpointNotAllowed);
                }
            }
        }

        return Ok(());
    }

    /// A host may perform a local process effect only while it still owns the
    /// record's placement/runtime incarnation and the record was written under
    /// its current fence.
    #[must_use]
    pub fn authorizes_controller(
        &self,
        node: &str,
        placement_epoch: u64,
        runtime_epoch: u64,
        fencing_token: u64,
    ) -> bool {
        return self.assigned_node == node
            && self.placement_epoch == placement_epoch
            && self.runtime_epoch == runtime_epoch
            && self.fencing_token == fencing_token;
    }
}

fn valid_identity_segment(value: &str) -> bool {
    return !value.is_empty()
        && value.len() <= MAX_IDENTITY_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
        });
}

fn valid_checkpoint_digest(value: &str) -> bool {
    let Some(raw) = value.strip_prefix("sha256:") else {
        return false;
    };

    return raw.len() == 64
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
}

/// Validate an atomic compare-and-set replacement of one lifecycle record.
///
/// A strictly newer fence may continue from any prior revision. Reusing the same
/// fence is allowed for a multi-step transition, but the durable revision must
/// advance. Placement and runtime epochs can only move forward. Moving nodes
/// requires a placement epoch increment. Replacing a runtime incarnation,
/// including on the same node, requires a strictly newer distributed fence and
/// resets the durable lifecycle to `running`; a stale frozen/checkpointed record
/// must never be silently inherited by a new process.
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

    if next.runtime_epoch < current.runtime_epoch {
        return Err(LifecycleRecordError::RuntimeEpochRegressed);
    }

    if next.runtime_epoch > current.runtime_epoch {
        if next.fencing_token <= current.fencing_token {
            return Err(LifecycleRecordError::RuntimeChangedWithoutFenceAdvance);
        }
        if next.state != PersistedLifecycleState::Running || next.checkpoint.is_some() {
            return Err(LifecycleRecordError::RuntimeChangedOutsideRunning);
        }
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
            runtime_epoch: 13,
            fencing_token: 19,
            revision: 11,
            state: PersistedLifecycleState::Frozen,
            strategy: PersistedSuspendStrategy::Freeze,
            checkpoint: None,
        };
    }

    fn checkpoint() -> LifecycleCheckpoint {
        return LifecycleCheckpoint {
            artifact_ref: "checkpoint://tenant-42-shard-3/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            format: "criu-v1".to_owned(),
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
    fn identity_segments_are_bounded_and_path_safe() {
        let mut value = record();
        value.workload_id = "../tenant".to_owned();
        assert_eq!(value.validate(), Err(LifecycleRecordError::InvalidWorkloadId));

        let mut value = record();
        value.assigned_node = "node/escape".to_owned();
        assert_eq!(value.validate(), Err(LifecycleRecordError::InvalidAssignedNode));
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
        assert!(!next.authorizes_controller("node-a", 7, 13, 19));
        assert!(next.authorizes_controller("node-b", 8, 13, 20));
    }

    #[test]
    fn replacement_runtime_requires_new_fence_and_running_reset() {
        let current = record();
        let mut same_fence = current.clone();
        same_fence.runtime_epoch = 14;
        same_fence.revision = 12;
        same_fence.state = PersistedLifecycleState::Running;
        assert_eq!(
            validate_record_update(&current, &same_fence),
            Err(LifecycleRecordError::RuntimeChangedWithoutFenceAdvance)
        );

        let mut still_frozen = same_fence.clone();
        still_frozen.fencing_token = 20;
        still_frozen.state = PersistedLifecycleState::Frozen;
        assert_eq!(
            validate_record_update(&current, &still_frozen),
            Err(LifecycleRecordError::RuntimeChangedOutsideRunning)
        );

        let mut replacement = same_fence;
        replacement.fencing_token = 20;
        assert_eq!(validate_record_update(&current, &replacement), Ok(()));
        assert!(!replacement.authorizes_controller("node-a", 7, 13, 20));
        assert!(replacement.authorizes_controller("node-a", 7, 14, 20));
    }

    #[test]
    fn runtime_epoch_cannot_regress() {
        let current = record();
        let mut next = current.clone();
        next.runtime_epoch = 12;
        next.fencing_token = 20;
        next.revision = 12;

        assert_eq!(
            validate_record_update(&current, &next),
            Err(LifecycleRecordError::RuntimeEpochRegressed)
        );
    }

    #[test]
    fn legacy_record_without_runtime_epoch_fails_closed() {
        let encoded = serde_json::json!({
            "workload_id": "tenant-42-shard-3",
            "assigned_node": "node-a",
            "placement_epoch": 7,
            "fencing_token": 19,
            "revision": 11,
            "state": "frozen",
            "strategy": "freeze",
            "checkpoint": null
        });
        let value = serde_json::from_value::<LifecycleRecord>(encoded)
            .expect("legacy record should deserialize to zero runtime epoch");
        assert_eq!(value.runtime_epoch, 0);
        assert_eq!(value.validate(), Err(LifecycleRecordError::ZeroRuntimeEpoch));
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
    fn restoring_record_requires_checkpoint() {
        let mut value = record();
        value.state = PersistedLifecycleState::Restoring;
        value.strategy = PersistedSuspendStrategy::Hibernate;

        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::CheckpointRequired)
        );

        value.checkpoint = Some(checkpoint());
        assert_eq!(value.validate(), Ok(()));
    }

    #[test]
    fn thawing_record_must_not_have_checkpoint() {
        let mut value = record();
        value.state = PersistedLifecycleState::Thawing;
        assert_eq!(value.validate(), Ok(()));

        value.checkpoint = Some(checkpoint());
        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::CheckpointNotAllowed)
        );
    }

    #[test]
    fn state_and_strategy_must_match() {
        let mut value = record();
        value.state = PersistedLifecycleState::Hibernated;
        value.checkpoint = Some(checkpoint());

        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::StateStrategyMismatch)
        );

        value.strategy = PersistedSuspendStrategy::Hibernate;
        assert_eq!(value.validate(), Ok(()));

        value.state = PersistedLifecycleState::Frozen;
        value.checkpoint = None;
        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::StateStrategyMismatch)
        );
    }

    #[test]
    fn checkpoint_digest_must_be_canonical_sha256() {
        let mut value = record();
        value.state = PersistedLifecycleState::Hibernated;
        value.strategy = PersistedSuspendStrategy::Hibernate;
        value.checkpoint = Some(LifecycleCheckpoint {
            artifact_ref: "checkpoint://tenant-42-shard-3/object".to_owned(),
            digest: "sha256:abc".to_owned(),
            format: "criu-v1".to_owned(),
        });

        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::InvalidCheckpointDigest)
        );
    }

    #[test]
    fn checkpointing_record_cannot_publish_checkpoint_metadata_early() {
        let mut value = record();
        value.state = PersistedLifecycleState::Checkpointing;
        value.strategy = PersistedSuspendStrategy::Hibernate;
        value.checkpoint = Some(checkpoint());

        assert_eq!(
            value.validate(),
            Err(LifecycleRecordError::CheckpointNotAllowed)
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
