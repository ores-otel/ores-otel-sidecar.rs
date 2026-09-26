//! Pure process lifecycle policy for host agents that suspend idle workloads.
//!
//! Effects stay outside this module. Queue observation, cgroup freeze/thaw,
//! checkpoint/restore, and fenced lease acquisition are adapters owned by the
//! host agent. This module only decides the next explicit state and requested
//! effect.

#![forbid(unsafe_code)]

/// Resource-reclamation strategy selected for one managed workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendStrategy {
    /// Stop CPU scheduling while keeping the process and its memory image alive.
    Freeze,
    /// Persist restorable state and terminate the process so its RSS is released.
    Hibernate,
}

/// Stable lifecycle state persisted by the control plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    Running,
    Quiescing(SuspendStrategy),
    Freezing,
    Frozen,
    Checkpointing,
    Hibernated,
    Resuming(SuspendStrategy),
}

/// Queue/work observation supplied by the product adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivitySnapshot {
    pub queue_depth: u64,
    pub in_flight: u64,
    /// Duration for which both queue depth and in-flight work have remained zero.
    pub idle_for_ms: u64,
}

impl ActivitySnapshot {
    #[must_use]
    pub const fn has_demand(self) -> bool {
        return self.queue_depth > 0 || self.in_flight > 0;
    }

    #[must_use]
    pub const fn is_idle_for(self, minimum_idle_ms: u64) -> bool {
        return !self.has_demand() && self.idle_for_ms >= minimum_idle_ms;
    }
}

/// Policy is intentionally small enough to be owned by product configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecyclePolicy {
    pub strategy: SuspendStrategy,
    pub minimum_idle_ms: u64,
}

impl LifecyclePolicy {
    pub const MINIMUM_IDLE_MS: u64 = 1_000;

    pub fn new(strategy: SuspendStrategy, minimum_idle_ms: u64) -> Result<Self, PolicyError> {
        if minimum_idle_ms < Self::MINIMUM_IDLE_MS {
            return Err(PolicyError::IdleGraceTooShort {
                minimum_idle_ms,
                required_ms: Self::MINIMUM_IDLE_MS,
            });
        }

        return Ok(Self {
            strategy,
            minimum_idle_ms,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    IdleGraceTooShort {
        minimum_idle_ms: u64,
        required_ms: u64,
    },
}

/// Events are either observations or acknowledgements from an effect adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEvent {
    Observe(ActivitySnapshot),
    QuiesceSucceeded,
    FreezeSucceeded,
    CheckpointSucceeded,
    ResumeSucceeded,
}

/// Effect requested from the host-agent adapter after persisting `next_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleAction {
    None,
    BeginQuiesce,
    CancelQuiesce,
    Freeze,
    CheckpointAndTerminate,
    Thaw,
    Restore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleDecision {
    pub next_state: LifecycleState,
    pub action: LifecycleAction,
}

impl LifecycleDecision {
    const fn stay(state: LifecycleState) -> Self {
        return Self {
            next_state: state,
            action: LifecycleAction::None,
        };
    }

    const fn transition(next_state: LifecycleState, action: LifecycleAction) -> Self {
        return Self { next_state, action };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTransition {
    pub state: LifecycleState,
    pub event: LifecycleEvent,
}

/// Decide one lifecycle transition with no I/O, clock, or locking effects.
pub fn decide(
    state: LifecycleState,
    policy: LifecyclePolicy,
    event: LifecycleEvent,
) -> Result<LifecycleDecision, InvalidTransition> {
    match state {
        LifecycleState::Running => {
            return decide_running(policy, event);
        }
        LifecycleState::Quiescing(strategy) => {
            return decide_quiescing(strategy, event);
        }
        LifecycleState::Freezing => {
            return decide_freezing(event);
        }
        LifecycleState::Frozen => {
            return decide_frozen(event);
        }
        LifecycleState::Checkpointing => {
            return decide_checkpointing(event);
        }
        LifecycleState::Hibernated => {
            return decide_hibernated(event);
        }
        LifecycleState::Resuming(strategy) => {
            return decide_resuming(strategy, event);
        }
    }
}

fn decide_running(
    policy: LifecyclePolicy,
    event: LifecycleEvent,
) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::Observe(activity) => {
            if activity.is_idle_for(policy.minimum_idle_ms) {
                return Ok(LifecycleDecision::transition(
                    LifecycleState::Quiescing(policy.strategy),
                    LifecycleAction::BeginQuiesce,
                ));
            }

            return Ok(LifecycleDecision::stay(LifecycleState::Running));
        }
        LifecycleEvent::QuiesceSucceeded
        | LifecycleEvent::FreezeSucceeded
        | LifecycleEvent::CheckpointSucceeded
        | LifecycleEvent::ResumeSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Running,
                event,
            });
        }
    }
}

fn decide_quiescing(
    strategy: SuspendStrategy,
    event: LifecycleEvent,
) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::Observe(activity) => {
            if activity.has_demand() {
                return Ok(LifecycleDecision::transition(
                    LifecycleState::Running,
                    LifecycleAction::CancelQuiesce,
                ));
            }

            return Ok(LifecycleDecision::stay(LifecycleState::Quiescing(strategy)));
        }
        LifecycleEvent::QuiesceSucceeded => {
            match strategy {
                SuspendStrategy::Freeze => {
                    return Ok(LifecycleDecision::transition(
                        LifecycleState::Freezing,
                        LifecycleAction::Freeze,
                    ));
                }
                SuspendStrategy::Hibernate => {
                    return Ok(LifecycleDecision::transition(
                        LifecycleState::Checkpointing,
                        LifecycleAction::CheckpointAndTerminate,
                    ));
                }
            }
        }
        LifecycleEvent::FreezeSucceeded
        | LifecycleEvent::CheckpointSucceeded
        | LifecycleEvent::ResumeSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Quiescing(strategy),
                event,
            });
        }
    }
}

fn decide_freezing(event: LifecycleEvent) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::FreezeSucceeded => {
            return Ok(LifecycleDecision::stay(LifecycleState::Frozen));
        }
        LifecycleEvent::Observe(_)
        | LifecycleEvent::QuiesceSucceeded
        | LifecycleEvent::CheckpointSucceeded
        | LifecycleEvent::ResumeSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Freezing,
                event,
            });
        }
    }
}

fn decide_frozen(event: LifecycleEvent) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::Observe(activity) => {
            if activity.has_demand() {
                return Ok(LifecycleDecision::transition(
                    LifecycleState::Resuming(SuspendStrategy::Freeze),
                    LifecycleAction::Thaw,
                ));
            }

            return Ok(LifecycleDecision::stay(LifecycleState::Frozen));
        }
        LifecycleEvent::QuiesceSucceeded
        | LifecycleEvent::FreezeSucceeded
        | LifecycleEvent::CheckpointSucceeded
        | LifecycleEvent::ResumeSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Frozen,
                event,
            });
        }
    }
}

fn decide_checkpointing(event: LifecycleEvent) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::CheckpointSucceeded => {
            return Ok(LifecycleDecision::stay(LifecycleState::Hibernated));
        }
        LifecycleEvent::Observe(_)
        | LifecycleEvent::QuiesceSucceeded
        | LifecycleEvent::FreezeSucceeded
        | LifecycleEvent::ResumeSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Checkpointing,
                event,
            });
        }
    }
}

fn decide_hibernated(event: LifecycleEvent) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::Observe(activity) => {
            if activity.has_demand() {
                return Ok(LifecycleDecision::transition(
                    LifecycleState::Resuming(SuspendStrategy::Hibernate),
                    LifecycleAction::Restore,
                ));
            }

            return Ok(LifecycleDecision::stay(LifecycleState::Hibernated));
        }
        LifecycleEvent::QuiesceSucceeded
        | LifecycleEvent::FreezeSucceeded
        | LifecycleEvent::CheckpointSucceeded
        | LifecycleEvent::ResumeSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Hibernated,
                event,
            });
        }
    }
}

fn decide_resuming(
    strategy: SuspendStrategy,
    event: LifecycleEvent,
) -> Result<LifecycleDecision, InvalidTransition> {
    match event {
        LifecycleEvent::ResumeSucceeded => {
            return Ok(LifecycleDecision::stay(LifecycleState::Running));
        }
        LifecycleEvent::Observe(_)
        | LifecycleEvent::QuiesceSucceeded
        | LifecycleEvent::FreezeSucceeded
        | LifecycleEvent::CheckpointSucceeded => {
            return Err(InvalidTransition {
                state: LifecycleState::Resuming(strategy),
                event,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREEZE_POLICY: LifecyclePolicy = LifecyclePolicy {
        strategy: SuspendStrategy::Freeze,
        minimum_idle_ms: 30_000,
    };

    const HIBERNATE_POLICY: LifecyclePolicy = LifecyclePolicy {
        strategy: SuspendStrategy::Hibernate,
        minimum_idle_ms: 120_000,
    };

    const fn idle(ms: u64) -> ActivitySnapshot {
        return ActivitySnapshot {
            queue_depth: 0,
            in_flight: 0,
            idle_for_ms: ms,
        };
    }

    const fn busy() -> ActivitySnapshot {
        return ActivitySnapshot {
            queue_depth: 1,
            in_flight: 0,
            idle_for_ms: 0,
        };
    }

    #[test]
    fn idle_grace_prevents_suspend_flapping() {
        let decision = decide(
            LifecycleState::Running,
            FREEZE_POLICY,
            LifecycleEvent::Observe(idle(29_999)),
        );

        assert_eq!(
            decision,
            Ok(LifecycleDecision::stay(LifecycleState::Running))
        );
    }

    #[test]
    fn freeze_strategy_quiesces_then_freezes() {
        let begin = decide(
            LifecycleState::Running,
            FREEZE_POLICY,
            LifecycleEvent::Observe(idle(30_000)),
        );
        assert_eq!(
            begin,
            Ok(LifecycleDecision::transition(
                LifecycleState::Quiescing(SuspendStrategy::Freeze),
                LifecycleAction::BeginQuiesce,
            ))
        );

        let freeze = decide(
            LifecycleState::Quiescing(SuspendStrategy::Freeze),
            FREEZE_POLICY,
            LifecycleEvent::QuiesceSucceeded,
        );
        assert_eq!(
            freeze,
            Ok(LifecycleDecision::transition(
                LifecycleState::Freezing,
                LifecycleAction::Freeze,
            ))
        );

        let frozen = decide(
            LifecycleState::Freezing,
            FREEZE_POLICY,
            LifecycleEvent::FreezeSucceeded,
        );
        assert_eq!(
            frozen,
            Ok(LifecycleDecision::stay(LifecycleState::Frozen))
        );
    }

    #[test]
    fn hibernate_strategy_checkpoints_and_terminates() {
        let begin = decide(
            LifecycleState::Running,
            HIBERNATE_POLICY,
            LifecycleEvent::Observe(idle(120_000)),
        );
        assert_eq!(
            begin,
            Ok(LifecycleDecision::transition(
                LifecycleState::Quiescing(SuspendStrategy::Hibernate),
                LifecycleAction::BeginQuiesce,
            ))
        );

        let checkpoint = decide(
            LifecycleState::Quiescing(SuspendStrategy::Hibernate),
            HIBERNATE_POLICY,
            LifecycleEvent::QuiesceSucceeded,
        );
        assert_eq!(
            checkpoint,
            Ok(LifecycleDecision::transition(
                LifecycleState::Checkpointing,
                LifecycleAction::CheckpointAndTerminate,
            ))
        );
    }

    #[test]
    fn demand_during_quiesce_cancels_suspend() {
        let decision = decide(
            LifecycleState::Quiescing(SuspendStrategy::Freeze),
            FREEZE_POLICY,
            LifecycleEvent::Observe(busy()),
        );

        assert_eq!(
            decision,
            Ok(LifecycleDecision::transition(
                LifecycleState::Running,
                LifecycleAction::CancelQuiesce,
            ))
        );
    }

    #[test]
    fn demand_thaws_a_frozen_workload() {
        let decision = decide(
            LifecycleState::Frozen,
            FREEZE_POLICY,
            LifecycleEvent::Observe(busy()),
        );

        assert_eq!(
            decision,
            Ok(LifecycleDecision::transition(
                LifecycleState::Resuming(SuspendStrategy::Freeze),
                LifecycleAction::Thaw,
            ))
        );
    }

    #[test]
    fn demand_restores_a_hibernated_workload() {
        let decision = decide(
            LifecycleState::Hibernated,
            HIBERNATE_POLICY,
            LifecycleEvent::Observe(busy()),
        );

        assert_eq!(
            decision,
            Ok(LifecycleDecision::transition(
                LifecycleState::Resuming(SuspendStrategy::Hibernate),
                LifecycleAction::Restore,
            ))
        );
    }

    #[test]
    fn too_short_idle_grace_is_rejected() {
        let policy = LifecyclePolicy::new(SuspendStrategy::Freeze, 999);
        assert_eq!(
            policy,
            Err(PolicyError::IdleGraceTooShort {
                minimum_idle_ms: 999,
                required_ms: 1_000,
            })
        );
    }
}
