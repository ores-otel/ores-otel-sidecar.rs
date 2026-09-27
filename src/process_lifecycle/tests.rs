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
