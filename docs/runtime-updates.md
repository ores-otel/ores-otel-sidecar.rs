# Runtime update reconciliation

`.ores-sidecar.toml` keeps startup configuration separate from runtime-mutable values. `flags-2-env` and `.cli-flags.toml` remain the argv/environment authority; runtime updates never call `std::env::set_var` and cannot rebind the listener.

## Redis-LRU compatibility

A receive-only policy names the exact two-part `ores-lru-redis` keyspace:

```toml
[sidecars.runtime_updates]
provider = "ores-redis-lru-cache"
mode = "receive-only"
poll_seconds = 180
namespace = "ores-otel-sidecar"
cache = "runtime-env"
```

`namespace` and `cache` use the same bounded segment grammar as `ores-redis-lru-cache/ores-lru-redis.rs`: 1–64 ASCII alphanumeric, `.`, `_`, or `-` characters. The reconciliation interval is bounded to 180 seconds, matching the Redis-LRU runtime maximum.

The shared sidecar does not own Redis credentials or transport connections. A product/runtime adapter validates the Redis-LRU transport envelope, then passes the event or authoritative snapshot to `RuntimeUpdateController`. This keeps the generic sidecar library backend-neutral while preserving Redis-LRU's revision and keyspace semantics.

## State rules

`RuntimeValues` has two layers: immutable baseline values from checked-in configuration and backend overrides. An override wins while present; deleting it restores the baseline rather than erasing the configured default.

`RuntimeUpdateController` enforces the configured namespace/cache again at the consumer boundary. Snapshot reconciliation may advance directly to a newer revision and may repeat the current revision to repair local state. Older snapshots are ignored. Events must advance exactly one revision. Duplicate/older events are idempotent while healthy. A revision gap or explicit `resync` marks the controller stale and blocks all further events until an authoritative snapshot repairs it.

`upsert`, `delete`, `replace`, and `invalidate` map to the corresponding Redis-LRU semantics. Whole snapshots and `replace` atomically swap the backend override set, so a 64-key old state can be replaced by a different 64-key state without being rejected as a synthetic 128-operation patch.

Every candidate key/value is validated before state mutation. Secret-like keys, keys outside `runtime_mutable`, oversized values, duplicate delete keys, malformed operation payloads, unsafe revisions, target mismatches, and oversized state are rejected without advancing the local revision.

## Adapter sketch

A Redis-LRU adapter should map its validated envelope without weakening it:

- `CacheSnapshot { revision, entries }` -> `apply_snapshot(namespace, cache, revision, entries)`
- `CacheOperation::Upsert` -> `RuntimeUpdateOperation::Upsert`
- `CacheOperation::Delete` -> `RuntimeUpdateOperation::Delete`
- `CacheOperation::Replace` -> `RuntimeUpdateOperation::Replace`
- `CacheOperation::Invalidate` -> `RuntimeUpdateOperation::Invalidate`
- `CacheOperation::Resync` -> `RuntimeUpdateOperation::Resync`

When the controller returns `ReconcileRequired`, the adapter must obtain a fresh authoritative snapshot before applying additional events. Runtime-update values and diagnostics must never include credentials or secret payloads.
