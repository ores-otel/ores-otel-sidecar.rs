#![forbid(unsafe_code)]

#[allow(clippy::match_like_matches_macro)]
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

use ores_otel_sidecar::{
    cli, runtime, SidecarConfig, SidecarHooks, SidecarIdentity, DEFAULT_SIDECAR_CONFIG_PATH,
};

fn main() {
    let invocation = match cli::resolve_process(runtime::DEFAULT_CLI_CONFIG_PATH) {
        Ok(invocation) => invocation,
        Err(_error) => runtime::exit_invalid_cli(SidecarIdentity::ORES_OTEL),
    };
    let command = invocation.command;
    let values = env_runtime::load_from(|key| invocation.value(key));
    let cfg = SidecarConfig::from_env_with(
        SidecarIdentity::ORES_OTEL,
        SidecarHooks::new()
            .bind_raw(move |_| values.bind.clone())
            .allow_non_loopback(move |_| values.allow_non_loopback),
    );
    let cfg = match cfg.with_optional_sidecar_file(DEFAULT_SIDECAR_CONFIG_PATH) {
        Ok(config) => config,
        Err(_error) => runtime::exit_invalid_cli(SidecarIdentity::ORES_OTEL),
    };
    runtime::run_command(&cfg, command);
}
