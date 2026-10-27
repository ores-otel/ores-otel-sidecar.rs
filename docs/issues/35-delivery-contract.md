# Delivery contract for #35: Ship the ores-process-lifecycle-agent executable consumed by BeamScale and Scintilla

Tracks https://github.com/ores-otel/ores-otel-sidecar.rs/issues/35

## Required boundaries

- Preserve the shared sidecar/runtime ownership model; product repositories consume this crate rather than copying lifecycle/config logic.
- Keep authentication, synchronization, telemetry, flags, and package concerns behind their canonical shared dependencies.
- Resolve state transitions explicitly and fail closed on stale or ambiguous authority.
- Do not raise the FP conformance budget to hide new violations.
- Process or host effects must remain bounded, auditable, and separated from cooperative application hints.
- Treat zero-step CI as non-evidence.

## Implementation slices

1. Add the smallest typed core/config/state change required by the issue.
2. Add adversarial tests for stale state, retry/replay, partial failure, and restart/reconciliation where applicable.
3. Add runtime adapter/effect code only after the pure decision boundary is covered.
4. Run the repository's conformance ratchet and normal Rust gates on the exact head.
5. Bind external consumer certification to immutable source revisions.

## Completion

This PR records the reviewable engineering contract. Issue #35 remains open until executable behavior and exact-head tests satisfy its acceptance criteria.
