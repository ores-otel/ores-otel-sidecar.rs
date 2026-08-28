#![forbid(unsafe_code)]

#[allow(clippy::match_like_matches_macro)]
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

use ores_otel_sidecar::{runtime, SidecarConfig, SidecarHooks, SidecarIdentity};

fn main() {
    let values = env_runtime::load_from_os();
    let cfg = SidecarConfig::from_env_with(
        SidecarIdentity::ORES_OTEL,
        SidecarHooks::new()
            .bind_raw(move |_| values.bind.clone())
            .allow_non_loopback(move |_| values.allow_non_loopback),
    );
    runtime::run(&cfg);
}
