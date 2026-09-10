#![forbid(unsafe_code)]

use ores_otel_sidecar::{
    cli, preflight_startup, runtime, SidecarConfig, SidecarIdentity, DEFAULT_SIDECAR_CONFIG_PATH,
};

fn main() {
    let identity = SidecarIdentity::ORES_OTEL;
    let invocation = match cli::resolve_process(runtime::DEFAULT_CLI_CONFIG_PATH) {
        Ok(invocation) => invocation,
        Err(_error) => runtime::exit_invalid_cli(identity),
    };
    let command = invocation.command;

    // This is the mandatory boot gate. Nothing in the sidecar runtime is
    // initialized until the final flags-2-env snapshot has passed canonical
    // presence/type validation.
    let startup = match preflight_startup(invocation.values()) {
        Ok(startup) => startup,
        Err(_diagnostics) => runtime::exit_invalid_config(identity),
    };

    let cfg = match SidecarConfig::from_bind(
        identity,
        &startup.bind,
        startup.allow_non_loopback,
    ) {
        Ok(config) => config,
        Err(_error) => runtime::exit_invalid_config(identity),
    };
    let cfg = match cfg.with_optional_sidecar_file(DEFAULT_SIDECAR_CONFIG_PATH) {
        Ok(config) => config,
        Err(_error) => runtime::exit_invalid_config(identity),
    };
    runtime::run_command(&cfg, command);
}
