# ores-otel-sidecar.rs

Shared sidecar runtime for Logging, telemetry, and observability for ORESoftware runtimes.

## I/O

The process **does not read stdin** and **does not use stdout as a protocol**.

| Surface | Role |
|---|---|
| HTTP on the bind address (loopback by default) | `/healthz`, `/readyz`, `/metrics` |
| stderr JSON | listen/fatal diagnostics |
| `ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK=1` | required to bind a non-loopback unicast address; `0.0.0.0`/`::` stay rejected |

Product binaries inherit this crate:

```toml
[dependencies]
"ores-otel/ores-otel-sidecar" = "^0.1.0"
```

```toml
ores-otel-sidecar = { git = "https://github.com/ores-otel/ores-otel-sidecar.rs", rev = "<pinned-commit>" }
```
