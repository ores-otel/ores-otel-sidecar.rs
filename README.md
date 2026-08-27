# ores-otel-sidecar.rs

Shared sidecar runtime for Logging, telemetry, and observability for ORESoftware runtimes.

Product `*-sidecar.rs` crates inherit this crate instead of copying health/bind
logic. Declare both:

1. **zed-pkg** in `.zpkg.toml`:
   ```toml
   [dependencies]
   "ores-otel/ores-otel-sidecar" = "^0.1.0"
   ```
2. **Cargo** (zed can invoke cargo for the Rust adapter):
   ```toml
   ores-otel-sidecar = { git = "https://github.com/ores-otel/ores-otel-sidecar.rs", rev = "<pinned-commit>" }
   ```

Then run:

```rust
use ores_otel_sidecar::{runtime, SidecarConfig, SidecarIdentity};

fn main() {
    let cfg = SidecarConfig::from_env(SidecarIdentity::new(
        "pmap-sidecar",
        "PMAP_SIDECAR_BIND",
    ));
    runtime::run(&cfg);
}
```

k8s-deployable product sidecars also belong in that org's `*-monorepo` as a git
submodule under `apps/` (or `apps/deployments/` when that layout is already in use).
