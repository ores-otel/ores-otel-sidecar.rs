# ores-otel — sidecar.rs

Canonical shared `sidecar.rs` library for [`ores-otel`](https://github.com/ores-otel).

Product org sidecars (`pmap-sidecar.rs`, `hhm-sidecar.rs`, …) import this crate
via zed-pkg (`ores-otel/ores-otel-sidecar`) and Cargo git (`rev` pin). They do
not copy `config`/`health`/`runtime`.

- Internal runtimes: Rust, TypeScript, Dart.
- Contracts: JSON Schema in `ores-otel-interfaces`.
- Auth: github.com/shared-auth.
- Sync: github.com/opto-sync.
- Telemetry: github.com/ores-otel.
- Flags: github.com/flags-2-env.
- Packages: github.com/zed-pkg.
- Never use React/JSX or webviews.
- Resolve git conflicts semantically; never rebase, stash, or reset.
- Build values, don't mutate them: functions return new values instead of filling `&mut`/pointer parameters or caller-owned collections; parsers and the runtime-update reducer are folds over immutable state. Deliberate exceptions on hot paths (the stdio frame receiver) carry a `HOT-PATH (imperative by design)` comment with the reason. See [`docs/FUNCTIONAL-STYLE.md`](./docs/FUNCTIONAL-STYLE.md).
