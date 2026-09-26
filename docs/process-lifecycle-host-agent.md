# Fenced process lifecycle host agent

This document defines the ordering contract for host-level process suspension used by BeamScale and Scintilla Run. Product runtimes provide authoritative activity and quiesce/drain signals. The shared host agent owns fenced lifecycle persistence and Linux process effects.

## Trust boundary

The lifecycle agent is a trusted per-host service outside every managed workload cgroup/process tree. It must remain schedulable while the target is frozen or checkpointed/terminated.

The agent does not infer that a workload is idle from CPU usage alone. Product adapters provide authoritative queue depth, in-flight work, and quiesce/drain acknowledgement. The pure shared lifecycle policy applies the configured idle grace/hysteresis to those observations.

All lifecycle mutations are serialized by the generic `ores-locks-and-leases::Lease` seam. The initial production backend is Cloudflare Durable Objects through `ManagedLease::cloudflare`. Fiducia or a future BeamScale-native durable lock may replace that authority without changing lifecycle policy.

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
observe product queue_depth == 0 and in_flight == 0 for idle grace
        |
        v
persist Quiescing with new fence + revision
        |
        v
product adapter closes admissions and drains in-flight work
        |
        +---- demand returns before commit ----> cancel quiesce
        |                                      persist Running
        |                                      reopen admissions
        |                                      release lease
        v
persist Freezing with same fence + next revision
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

The host must never write `cgroup.freeze=1` before the `Freezing` intent is durably fenced. A crash after the process effect but before the final `Frozen` write is recovered from the intent record plus observed cgroup state; a replacement controller must acquire a newer fence before repairing the record.

Freeze/thaw releases CPU scheduling capacity. It does not claim that the workload's resident memory has been released.

## Hibernate transaction

The RAM-reclamation path is:

```text
acquire fenced lease for logical workload
        |
        v
re-read record and verify placement/fence authority
        |
        v
observe idle grace, persist Quiescing, drain product runtime
        |
        v
persist Checkpointing with fence + revision
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

CRIU is opt-in per workload class. A class with unproven file-descriptor, socket, namespace, runtime, or external-resource semantics remains freeze-only.

## Wake from Frozen

Dispatch must not race thaw. The product scheduler/dispatcher first observes the durable state and blocks delivery while wake is incomplete.

```text
new demand observes Frozen
        |
        v
acquire fenced lifecycle lease
        |
        v
re-read record; verify assignment and cgroup state
        |
        v
persist Thawing with fence + revision
        |
        v
write cgroup.freeze=0 and wait for cgroup.events frozen=0
        |
        v
product runtime readiness validation
        |
        v
persist Running; reopen admissions
        |
        v
release lease
        |
        v
dispatch queued work
```

## Wake from Hibernated

```text
new demand observes Hibernated
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
product runtime trust/readiness validation
        |
        v
persist Running with checkpoint cleared; reopen admissions
        |
        v
release lease
        |
        v
dispatch queued work
```

The workload is not dispatchable merely because CRIU returned success. Product readiness must re-establish its runtime invariants first.

## Demand during quiesce

If queue demand appears after `Quiescing` is persisted but before `Freezing` or `Checkpointing` commits:

1. cancel the product quiesce operation;
2. persist `Running` under the currently held fence and a newer revision;
3. reopen admissions;
4. release the lease;
5. dispatch normally.

Once `Freezing` or `Checkpointing` has committed, demand follows the normal wake path instead of trying to cancel the process effect.

## Placement changes

Placement is a separate monotonic generation from lease fencing. Reassignment must increment `placement_epoch`. A controller on the old node is stale even if its local process still exists.

The scheduler must not assign a new node and then permit both nodes to execute lifecycle effects under the same placement epoch. The common lifecycle lock plus persisted placement epoch/fence provides the handoff boundary.

## Failure policy

- Lock-authority outage: do not start a new suspend/resume mutation. Healthy running workloads remain running.
- Lease renewal loss: stop authoritative lifecycle writes; do not claim completion.
- Product drain timeout: cancel quiesce and leave the workload running.
- Freeze timeout: retain fenced transitional state for recovery; do not claim `Frozen`.
- Checkpoint failure before target termination: cancel/repair under the current fence and reopen according to product policy.
- Checkpoint ambiguity after termination: recover from `Checkpointing`; never fabricate a successful checkpoint.
- Restore failure: keep `Restoring` plus checkpoint metadata and remain non-ready/non-dispatchable until repaired or explicitly rolled back under a newer fence.
- Controller crash: a replacement controller must acquire a new lease/fence and reconcile durable state against local cgroup/process/checkpoint reality.

## Product adapter responsibilities

BeamScale and Scintilla integrations remain intentionally thin. They provide:

- queue depth and in-flight count;
- idle duration or enough observations for the host agent to derive it;
- close-admission/quiesce operation;
- drain acknowledgement;
- quiesce cancellation;
- readiness after thaw/restore;
- stable logical workload and placement identity.

They do not copy the shared lifecycle state machine, Linux cgroup/CRIU implementation, or distributed lease implementation.
