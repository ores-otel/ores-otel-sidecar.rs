#![forbid(unsafe_code)]

#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

use ores_otel_sidecar::{runtime, SidecarConfig, SidecarIdentity};

fn main() {
    let values = env_runtime::load_from_os();
    let cfg = match SidecarConfig::from_bind(
        SidecarIdentity::ORES_OTEL,
        &values.bind,
        values.allow_non_loopback,
    ) {
        Ok(cfg) => cfg,
        Err(_) => SidecarConfig::from_env(SidecarIdentity::ORES_OTEL),
    };
    runtime::run(&cfg);
}
