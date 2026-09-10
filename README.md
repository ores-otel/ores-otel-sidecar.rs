# ores-otel-sidecar.rs

Shared sidecar runtime for logging, telemetry, and observability across ORESoftware runtimes.

## I/O

The process **does not read stdin** and **does not use stdout as a protocol**.

| Surface | Role |
|---|---|
| HTTP on the bind address (loopback by default) | `/healthz`, `/readyz`, `/metrics` |
| stderr JSON | closed, payload-free failure diagnostics for an independent platform collector |
| `ORES_OTEL_SIDECAR_ALLOW_NON_LOOPBACK=true` | required to bind a non-loopback unicast address; `0.0.0.0`/`::` stay rejected |

Product binaries inherit this crate:

```toml
[dependencies]
"ores-otel/ores-otel-sidecar" = "^0.1.0"
```

```toml
ores-otel-sidecar = { git = "https://github.com/ores-otel/ores-otel-sidecar.rs", rev = "<pinned-commit>" }
```

## Deterministic startup preflight

The executable does not enter application runtime code until its final startup
environment has passed one deterministic typed gate. `flags-2-env` first
materializes declared defaults and resolves source precedence. The canonical
preflight then validates the resulting immutable map using shared lexical rules
rather than Rust/Go/Dart/JavaScript parser conveniences.

For environment values, booleans are exactly `true` or `false`; integers are
canonical signed base-10 i64 values; doubles use finite JSON-number syntax; JSON
must parse strictly; array and map contracts require the corresponding JSON root
type. Empty strings are accepted only when the declaring domain explicitly
allows them. Diagnostics use stable `ENV_MISSING`, `ENV_PARSE`, or
`ENV_CONTRACT` codes and never contain the rejected runtime value.

CLI aliases remain ergonomic because flags-2-env canonicalizes argv first. For
example a schema may accept `--flag=yes` and materialize `true`; a raw process
environment value `FLAG=yes` is not canonical and fails preflight.

`ores-otel-sidecar preflight` executes the same startup validation and
`.ores-sidecar.toml` admission as normal boot, but exits successfully without
opening a listener. Normal server boot runs that gate unconditionally before
constructing `SidecarConfig`.

## CLI authority and probe modes

`.cli-flags.toml` is the sole command and flag authority. The executable uses
the pinned bundled Rust `flags2env` client; there is no secondary argv parser.
The schema disables implicit `.env` loading and is copied read-only into the
final distroless image.

Supported process modes are:

| Invocation | Result |
|---|---|
| no command | validate startup config, then serve loopback HTTP |
| `preflight` | validate startup config and exit without listening |
| `probe` or `probe-healthz` | validate startup config, make one bounded `/healthz` request, then exit |
| `probe-readyz` | validate startup config, make one bounded `/readyz` request, then exit |
| unknown option, command, or operand | payload-free diagnostic and exit 2 |

Global flags continue to work after a command. For example,
`probe --bind=127.0.0.1:19090` probes that exact loopback listener. Structured
precedence is flags-2-env's canonical order: schema defaults, declared dotenv
values, process environment, dotenv overrides, then argv-provided flags. This
repository turns dotenv loading off, so normal deployments reduce that to
schema defaults, process environment, then argv.

## `.ores-sidecar.toml` and runtime values

`.ores-sidecar.toml` is the sidecar composition/runtime-policy contract; it does
not replace `.cli-flags.toml` or duplicate its startup argv/env authority. The
same file shape supports one sidecar in a product repository or many sidecars
in a central fleet file. Every `[[sidecars]]` entry has an explicit identity,
optional relative path, startup-config path, runtime-update policy, and its own
runtime-mutable allowlist.

```toml
schema = "ores.sidecar/config/v1"

[[sidecars]]
id = "ores-otel-sidecar"
path = "."
startup_config = ".cli-flags.toml"
runtime_mutable = ["LOG_FILTER", "REQUEST_TIMEOUT_MS"]

[sidecars.runtime_updates]
provider = "ores-redis-lru-cache"
mode = "receive-only"
poll_seconds = 180
namespace = "ores-otel-sidecar/runtime"
```

`SidecarConfig::with_sidecar_file` resolves the exact entry matching
`SidecarIdentity.service`. `with_optional_sidecar_file` treats only a missing
file as absent; malformed TOML, an unsupported schema, duplicate identities,
an absent matching identity, incompatible update policy, duplicate runtime
keys, or secret-like runtime keys fail closed.

The returned `RuntimeValues` handle is cloneable and shared. A future/current
`ores-redis-lru-cache` adapter can keep that handle and call `apply_patch` when
Redis Pub/Sub or the bounded reconciliation poll produces updates. Patches are
validated completely before taking the write lock, are restricted to the
entry's explicit allowlist, reject secret-like keys, and never call
`std::env::set_var`. Startup-only settings such as the listener bind therefore
cannot be accidentally "hot reconfigured" without rebuilding the listener.
The v1 Redis integration is receive-only so sidecar-local changes cannot create
a Redis feedback loop.

The externally serialized file shape has two independent human-authored
contract authorities: `contracts/sidecar/main.tsp` and
`contracts/sidecar/authored.schema.json`. CI runs the pinned
`ORESoftware/typespec-json-schema-validator` action over both authorities and a
recorded valid/invalid instance corpus. The generated TypeSpec schema is only
comparison evidence, never a third authority.

## Overrides

`from_env` remains available for compatibility, but new product binaries should
resolve `.cli-flags.toml`, call `preflight_startup_with_keys`, and construct
`SidecarConfig` from the resulting typed immutable snapshot. This prevents a
second ad-hoc environment parser from appearing after preflight.

```rust
#[path = "../generated/rust/env.rs"]
mod env;

use ores_otel_sidecar::{
    cli, preflight_startup_with_keys, runtime, SidecarConfig, SidecarHooks,
    SidecarIdentity, DEFAULT_SIDECAR_CONFIG_PATH,
};

fn main() {
    let identity = SidecarIdentity::new(env::SERVICE, env::BIND);
    let invocation = match cli::resolve_process(cli::DEFAULT_CONFIG_PATH) {
        Ok(invocation) => invocation,
        Err(_) => runtime::exit_invalid_cli(identity),
    };
    let command = invocation.command;
    let startup = match preflight_startup_with_keys(
        invocation.values(),
        env::BIND,
        env::ALLOW_NON_LOOPBACK,
    ) {
        Ok(startup) => startup,
        Err(_) => runtime::exit_invalid_config(identity),
    };
    let cfg = match SidecarConfig::from_bind_with(
        identity,
        &startup.bind,
        startup.allow_non_loopback,
        SidecarHooks::new().ready(|| true),
    ) {
        Ok(cfg) => cfg,
        Err(_) => runtime::exit_invalid_config(identity),
    };
    let cfg = match cfg.with_optional_sidecar_file(DEFAULT_SIDECAR_CONFIG_PATH) {
        Ok(cfg) => cfg,
        Err(_) => runtime::exit_invalid_config(identity),
    };
    runtime::run_command(&cfg, command);
}
```

Named policy types work the same way: implement `SidecarOverrides` (and
`ProductProbe`) and pass that value to `from_bind_with`. `run_command` uses those
overrides as the probe; tests can still call `run_with_probe` with a different
one. `runtime::run` remains a compatibility dispatcher for binaries that have
already constructed configuration, but it cannot retroactively apply argv
values to that configuration.

## Kubernetes ([oresoftware/k8s-cluster](https://github.com/ORESoftware/k8s-cluster))

The sidecar binds **loopback**. kubelet `httpGet` probes the **pod IP**, so those
probes would fail and must not be used. Copy [`k8s/container.yaml`](k8s/container.yaml)
next to the app container:

- `exec` liveness: same binary `probe` (no curl, distroless-safe)
- **no** `readinessProbe` (a down sidecar must not remove the app from a Service)
- **no** `containerPort` / Service port 9090
- **no** `hostPort`, **no** `0.0.0.0`
- stdout unused; the platform collector routes stderr JSON to CloudWatch,
  Google Cloud Logging, Azure Monitor, or Loki without invoking ORES/OTLP

The stderr record conforms to `ores.otel.log/internal-diagnostic/v1`. It uses
only fixed sidecar operation/outcome enums and bounded counters; argv, bind
values, OS errors, URLs, headers, and credentials are never serialized. Keep
the sidecar cloud-SDK-free and use workload-identity-backed platform agents for
cloud delivery.

The CI matrix builds the actual read-only distroless image, starts it without a
shell, runs `preflight`, and runs `/ores-otel-sidecar probe`, `probe-healthz`,
and `probe-readyz` through `docker exec`, matching kubelet's executable
contract.

Browser automation contracts (Playwright, Puppeteer, Selenium) live in
[`ores-otel-test/ores-otel-sidecar-contract-tests`](https://github.com/ores-otel-test/ores-otel-sidecar-contract-tests).

Env keys remain centralized in `.cli-flags.toml`; generated language artifacts
are evidence/SDK conveniences, while executable startup uses the canonical
flags-2-env immutable-map preflight rather than reparsing generated defaults.
