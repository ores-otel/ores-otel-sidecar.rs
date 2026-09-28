# Trusted process-lifecycle host control v2

This is the trusted host/control-plane protocol consumed by `ores-process-lifecycle-agent`. It is intentionally separate from the cooperative BeamScale/Scintilla product-runtime socket.

The host-control server runs outside every tenant-managed process tree and must derive its answers from host/scheduler state, not tenant input. The fixed trusted paths are:

- BeamScale: `/run/beamscale-lifecycle/host/control.sock`
- Scintilla Run: `/run/scintilla-lifecycle/host/control.sock`

The cooperative product-runtime sockets use separate parent directories:

- BeamScale: `/run/beamscale-lifecycle/product/control.sock`
- Scintilla Run: `/run/scintilla-lifecycle/product/control.sock`

This parent-directory split is a security boundary, not cosmetic layout. The product runtime may need write permission on its own socket directory so it can create or replace the cooperative socket. It must never receive write authority on the trusted host-control socket directory, because directory write permission is sufficient to unlink or replace a Unix socket entry even when the socket inode itself is root-owned. Shared or nested product/host socket parents therefore fail configuration validation.

Each request uses one Unix-stream connection. The client sends one newline-terminated request, shuts down its write half, and requires the server to return exactly one newline-terminated response and then close. Missing terminators, multiple frames, CRLF, leading/trailing response whitespace, oversized responses, malformed JSON, unknown JSON fields, and transport ambiguity fail closed.

## Snapshot

Request:

```text
v2 snapshot\n
```

Successful response is one compact JSON object followed by exactly one newline:

```json
{
  "version": 2,
  "workloads": [
    {
      "workload_id": "tenant-42-shard-3",
      "assigned_node": "node-a",
      "runtime_epoch": 14,
      "placement_epoch": 8,
      "pid": 4242,
      "process_start_ticks": 987654,
      "managed_cgroup": "/sys/fs/cgroup/beamscale-workloads.slice/tenant-42-shard-3.scope",
      "isolation_policy_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "queue_depth": 0,
      "in_flight": 0,
      "idle_for_ms": 30000,
      "routable": true
    }
  ]
}
```

The client rejects a snapshot unless all of the following hold:

- protocol `version == 2`;
- at most 1024 workloads and at most 256 KiB response bytes;
- workload/node IDs use the same bounded canonical identity alphabet as lifecycle lease keys;
- `assigned_node` exactly matches the lifecycle agent's configured node;
- runtime epoch, placement epoch, PID, and process-start identity are nonzero;
- queue, in-flight, and idle counters fit the signed 64-bit interoperability range;
- policy identity is canonical lowercase `sha256:<64 hex>`;
- the cgroup is absolute and strictly below the configured managed cgroup root;
- workload IDs are unique inside the snapshot;
- `(pid, process_start_ticks)` identities are unique inside the snapshot;
- managed cgroup identities are unique inside the snapshot.

A duplicated PID with a different start time is not itself a duplicate process incarnation; PID reuse is distinguished by the start-time field.

## Post-wake admission

After thaw/restore, routing may not reopen merely because the process exists or the cooperative runtime says it is ready. The lifecycle agent sends the exact host-owned identity it previously observed plus the current distributed fencing token:

```text
v2 admit <compact-json>\n
```

The JSON is `TrustedHostAdmission`:

```json
{
  "workload_id": "tenant-42-shard-3",
  "assigned_node": "node-a",
  "runtime_epoch": 14,
  "placement_epoch": 8,
  "pid": 4242,
  "process_start_ticks": 987654,
  "managed_cgroup": "/sys/fs/cgroup/beamscale-workloads.slice/tenant-42-shard-3.scope",
  "isolation_policy_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "fencing_token": 47
}
```

The trusted host server must re-read current placement/runtime state and re-attest the process before returning success. In particular it must compare logical workload, node, placement epoch, runtime epoch, PID, process-start identity, exact managed cgroup, policy digest, and the currently admitted lifecycle-fence watermark. It must not trust those JSON fields merely because the caller supplied them.

Responses:

```text
ok routable\n
error stale_identity\n
error demand_changed\n
```

Any other response fails closed.

## Durable lifecycle identity

The durable lifecycle record now binds both scheduling and process incarnation:

```text
workload_id
assigned_node
placement_epoch
runtime_epoch
fencing_token
revision
state
strategy
checkpoint
```

`runtime_epoch` is independent from `placement_epoch`: replacing a process on the same host still creates a new runtime incarnation. A placement/node handoff must advance both placement and runtime epochs. Any runtime-epoch advance requires a strictly newer distributed fence and resets durable lifecycle state to `running`; a replacement process may never inherit `frozen`, `thawing`, `checkpointing`, `hibernated`, or `restoring` state from the prior process.

Pre-v2 record JSON that lacks `runtime_epoch` deserializes to epoch zero and fails validation. The controller must recover/reinitialize it from trusted host state under a newly acquired fence; it must never infer the missing runtime incarnation from a naked current PID.

## Local CAS

The file-backed durable record store serializes `initialize` and compare-and-set replacement through a stable per-workload `.lock` file. The lock file is deliberately separate from the canonical JSON record because record publication uses atomic rename and therefore replaces the JSON inode. Locking the replaceable JSON inode would permit overlapping processes to hold locks on different generations of the file.

The distributed `ores-locks-and-leases` fence remains the cross-host authority. The local file lock is defense in depth against controller restart/overlap and ensures check + write + atomic publish form one local critical section.
