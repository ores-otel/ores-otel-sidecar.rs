# Fenced process lifecycle host agent

This document defines the ordering contract for host-level process suspension used by BeamScale and Scintilla Run. The shared host agent owns trusted demand observation, fenced lifecycle persistence, post-wake admission verification, and Linux process effects. Product runtimes provide only cooperative quiesce/drain behavior.

## Trust boundary

The lifecycle agent is a trusted per-host service outside every managed workload cgroup/process tree. It must remain schedulable while the target is frozen or checkpointed/terminated.

For the hostile-process execution class, assume a tenant may fully compromise its own BEAM/process tree. A tenant runtime is therefore **not** authoritative for queue depth, assigned/in-flight work, process identity, cgroup/scope identity, isolation state, or post-wake readiness. Those facts must come from a trusted scheduler, durable queue/dispatch-lease authority, and host-side process/isolation evidence.

The cooperative product adapter may close admissions, drain accepted work, and report that its local drain completed. That report improves consistency but is not a security proof. After cooperative quiesce succeeds, the host must re-observe trusted demand immediately before committing `Freezing` or `Checkpointing`. A compromised tenant cannot suspend itself by falsely claiming `queue_depth = 0` or `drained`.

After thaw or restore, the host must independently verify the runtime before durable state returns to `Running`. For the hostile-process class this verification binds the expected logical workload and placement/runtime epoch to the managed systemd scope/cgroup and process-start identity, and verifies required isolation evidence such as namespace separation, `NoNewPrivs`, empty capability sets, and the selected isolation-policy identity. Product readiness may be an additional cooperative check, but it never replaces host admission.

All lifecycle mutations are serialized by the generic `ores-locks-and-leases::Lease` seam. The initial production backend is Cloudflare Durable Objects through `ManagedLease::cloudflare`. Fiducia or a future BeamScale-native durable lock may replace that authority without changing lifecycle policy. BeamScale must not require its own future lock service to boot, recover, or wake BeamScale itself.

The lease key identifies the logical workload and is intentionally independent of the assigned node. A workload reassigned from node A to node B must still contend on the same lifecycle lock.

## Durable record invariant

Before performing a process effect, persist an intent record containing:

- logical `workload_id`;
- `assigned_node`;
- monotonically increasing `placement_epoch`;
- lease `fencing_token`;
- monotonically increasing record `revision`;
- lifecycle state and strategy;
- checkpoint metadata when required.

A process effect is authorized only when the host, placement epoch, and fencing token still match the durable record. Record replacement must be atomic compare-and-set and reject stale fencing tokens, stale revisions, and placement regression.

The process target must be resolved from trusted host state. A tenant-provided PID, cgroup path, namespace handle, socket path, or process identifier is never sufficient authority for a lifecycle mutation. The host binds its target to a managed scope/cgroup plus process-start identity to defeat PID reuse.

The durable wake states are deliberately distinct:

- `thawing` is the freeze/thaw path and must not contain checkpoint metadata;
- `restoring` is the hibernate/CRIU path and must contain immutable checkpoint metadata.

## Freeze transaction

The CPU-reclamation path is:

```text
acquire fenced lease for logical workload
        |
        v
re-read durable lifecycle + placement record
        |
        v
verify current node / placement epoch / fence authority
        |
        v
trusted host/control plane observes queue_depth == 0,
assigned/in_flight == 0, and idle grace elapsed
        |
        v
persist Quiescing with new fence + revision
        |
        v
cooperative product adapter closes admissions and drains accepted work
        |
        +---- product reports demand returned ----> cancel quiesce
        |                                          persist Running
        |                                          release lease
        v
trusted host/control plane RE-OBSERVES demand
        |
        +---- trusted demand returned ------------> cancel quiesce
        |                                          persist Running
        |                                          release lease
        v
persist Freezing with same fence + next revision
        |
        v
re-verify managed scope/cgroup + process-start identity
        |
        v
write cgroup.freeze=1 and wait for cgroup.events frozen=1
        |
        v
persist Frozen with same fence + next revision
        |
        v
release lease
```

The post-quiesce trusted recheck is mandatory. A stale pre-lock or pre-quiesce empty observation is not sufficient authority to suspend a runtime.

The host must never write `cgroup.freeze=1` before the `Freezing` intent is durably fenced. A crash after the process effect but before the final `Frozen` write is recovered from the intent record plus observed cgroup state; a replacement controller must acquire a newer fence before repairing the record.

Freeze/thaw releases CPU scheduling capacity. It does not claim that the workload's resident memory has been released.

## Reclaim after freeze

Memory reclaim is a separate pressure-management effect from freezing. A frozen process still owns its address space and may retain resident memory. When policy permits, the host may apply cgroup-v2 memory reclaim/swap pressure to the same trusted runtime cgroup after the runtime is proven frozen.

Reclaim is best-effort memory-pressure relief, not a durability primitive. Durable state must never depend on anonymous process pages remaining resident or swappable. If real RAM release is required, use hibernation/checkpoint+terminate or deterministic reconstruction from immutable artifacts plus external durable state.

## Hibernate transaction

The RAM-reclamation path is:

```text
acquire fenced lease for logical workload
        |
        v
re-read record and verify placement/fence authority
        |
        v
trusted host observes idle grace and no assigned/in-flight work
        |
        v
persist Quiescing, cooperatively drain runtime
        |
        v
trusted host RE-OBSERVES demand
        |
        +---- demand returned --------------------> cancel quiesce / Running
        v
persist Checkpointing with fence + revision
        |
        v
re-verify managed process identity and checkpoint eligibility
        |
        v
CRIU dump to a private temporary checkpoint directory
(checkpoint+terminate; do not use --leave-running)
        |
        v
hash/validate checkpoint artifact
        |
        v
publish checkpoint to the configured durable checkpoint store
        |
        v
persist Hibernated with immutable checkpoint ref/digest/format
        |
        v
release lease
```

The lease must be renewed while checkpointing/uploading. Losing the lease is fail-closed for further authoritative writes.

A crash after CRIU terminates the target but before `Hibernated` is committed leaves a fenced `Checkpointing` record. Recovery must inspect the private local checkpoint staging area and durable checkpoint store under a newly acquired fence. It must never silently mark the workload `Running` merely because the process is absent.

CRIU is opt-in per workload class. A class with unproven file-descriptor, socket, namespace, runtime, or external-resource semantics remains freeze/reclaim-only. Firecracker snapshotting is the preferred stronger isolation/snapshot class where a workload cannot accept a shared host kernel.

## Wake from Frozen

Dispatch must not race thaw. The trusted scheduler/dispatcher first observes the durable state and blocks delivery while wake is incomplete.

```text
trusted demand observes Frozen
        |
        v
acquire fenced lifecycle lease
        |
        v
re-read record; verify assignment, process-start identity and cgroup state
        |
        v
persist Thawing with fence + revision
        |
        v
write cgroup.freeze=0 and wait for cgroup.events frozen=0
        |
        v
host independently re-verifies process identity + isolation + routing admission
        |
        v
optional cooperative product readiness check
        |
        v
persist Running; reopen trusted dispatch admission
        |
        v
release lease
        |
        v
dispatch queued work
```

If host admission verification fails after the process has thawed, durable state remains transitional/non-dispatchable. A thawed process is not automatically trusted or routable.

## Wake from Hibernated

```text
trusted demand observes Hibernated
        |
        v
acquire fenced lifecycle lease
        |
        v
re-read record and verify checkpoint digest/reference
        |
        v
persist Restoring with fence + revision and checkpoint metadata
        |
        v
materialize verified checkpoint into private local restore directory
        |
        v
CRIU restore
        |
        v
host independently verifies restored process identity + scope/cgroup + isolation
        |
        v
optional cooperative product readiness check
        |
        v
persist Running with checkpoint cleared; reopen trusted dispatch admission
        |
        v
release lease
        |
        v
dispatch queued work
```

The workload is not dispatchable merely because CRIU returned success or the product runtime says it is ready. Trusted host admission must re-establish the runtime's security and placement invariants first.

## Demand during quiesce

If trusted queue/dispatch demand appears after `Quiescing` is persisted but before `Freezing` or `Checkpointing` commits:

1. cancel the cooperative product quiesce operation;
2. persist `Running` under the currently held fence and a newer revision;
3. reopen trusted dispatch admission;
4. release the lease;
5. dispatch normally.

Once `Freezing` or `Checkpointing` has committed, demand follows the normal wake path instead of trying to cancel the process effect.

## Placement changes

Placement is a separate monotonic generation from lease fencing. Reassignment must increment `placement_epoch`. A controller on the old node is stale even if its local process still exists.

The scheduler must not assign a new node and then permit both nodes to execute lifecycle effects under the same placement epoch. The common lifecycle lock plus persisted placement epoch/fence provides the handoff boundary.

A replacement controller must never infer authority from a still-running local PID. It reacquires the logical workload lease, validates the newer placement/runtime epoch, resolves the managed scope/cgroup from trusted state, and verifies process-start identity before acting.

## Failure policy

- Lock-authority outage: do not start a new suspend/resume mutation. Healthy running workloads remain running.
- Lease renewal loss: stop authoritative lifecycle writes; do not claim completion.
- Trusted observation unavailable or ambiguous: do not newly suspend/hibernate; a healthy running runtime remains running.
- Cooperative product drain timeout: cancel quiesce and leave the workload running.
- Product claims drained but trusted demand exists: ignore the claim, cancel quiesce, leave the workload running.
- Freeze timeout: retain fenced transitional state for recovery; do not claim `Frozen`.
- Process-start/cgroup identity mismatch: fail closed; never redirect the mutation to the observed PID.
- Checkpoint failure before target termination: cancel/repair under the current fence and reopen according to policy.
- Checkpoint ambiguity after termination: recover from `Checkpointing`; never fabricate a successful checkpoint.
- Restore failure: keep `Restoring` plus checkpoint metadata and remain non-ready/non-dispatchable until repaired or explicitly rolled back under a newer fence.
- Post-wake host admission failure: remain non-dispatchable in the transitional wake state; do not publish `Running`.
- Controller crash: a replacement controller must acquire a new lease/fence and reconcile durable state against trusted cgroup/process/checkpoint reality.

## Integration responsibilities

BeamScale and Scintilla integrations remain intentionally thin, but the trusted and untrusted roles are separate.

Trusted host/control-plane integration provides:

- durable queue depth and assigned/dispatch-lease state;
- authoritative in-flight count or equivalent dispatch ownership;
- idle duration or enough trusted observations to derive it;
- stable logical workload, assigned node, and placement/runtime epoch;
- expected managed systemd scope/cgroup and process-start identity;
- post-thaw/restore host admission verification, including required isolation evidence;
- scheduler/routing admission gating while lifecycle state is not `Running`.

The cooperative product runtime provides only:

- close-admission/quiesce operation for locally accepted work;
- local drain acknowledgement;
- quiesce cancellation/resume;
- optional application-level readiness information after wake.

The product runtime does not authorize its own suspension, choose the target PID/cgroup, certify its own isolation, or publish itself `Running`.

Neither integration copies the shared lifecycle state machine, Linux cgroup/CRIU implementation, or distributed lease implementation.
