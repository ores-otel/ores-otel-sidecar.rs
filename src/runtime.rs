#![forbid(unsafe_code)]

use crate::cli::{self, SidecarCommand};
use crate::config::SidecarConfig;
use crate::error::SidecarError;
use crate::http::{bind, probe_get, serve_listener};
use crate::identity::SidecarIdentity;
use crate::log;
use crate::log::{Operation, Outcome, Severity};
use crate::probe::ProductProbe;

pub const DEFAULT_CLI_CONFIG_PATH: &str = cli::DEFAULT_CONFIG_PATH;

/// Compatibility entrypoint for product sidecars.
///
/// New binaries should resolve flags-2-env before constructing `SidecarConfig`
/// and call [`run_command`] so argv-derived configuration participates in the
/// same typed snapshot as the process environment.
pub fn run(config: &SidecarConfig) {
    let invocation = match cli::resolve_process(DEFAULT_CLI_CONFIG_PATH) {
        Ok(invocation) => invocation,
        Err(_error) => exit_invalid_cli(config.identity),
    };
    run_command(config, invocation.command);
}

/// Execute an already validated flags-2-env command.
pub fn run_command(config: &SidecarConfig, command: SidecarCommand) {
    match command {
        SidecarCommand::ProbeHealthz => exit_probe(config, "/healthz"),
        SidecarCommand::ProbeReadyz => exit_probe(config, "/readyz"),
        SidecarCommand::Serve => {
            if let Err(_error) = run_with_probe(config, config.overrides()) {
                log::write_stderr(
                    config.identity.service,
                    Severity::Fatal,
                    Operation::SidecarListen,
                    Outcome::Failed,
                    false,
                );
                std::process::exit(1);
            }
        }
    }
}

/// Emit the stable, payload-free diagnostic used for invalid CLI input.
pub fn exit_invalid_cli(identity: SidecarIdentity) -> ! {
    log::write_stderr(
        identity.service,
        Severity::Fatal,
        Operation::SidecarConfigure,
        Outcome::Rejected,
        false,
    );
    std::process::exit(2);
}

fn exit_probe(config: &SidecarConfig, path: &str) -> ! {
    let code = probe_exit(config.listen, path);
    if code != 0 {
        log::write_stderr(
            config.identity.service,
            Severity::Error,
            Operation::SidecarProbe,
            Outcome::Failed,
            true,
        );
    }
    std::process::exit(code);
}

pub fn probe_exit(addr: std::net::SocketAddr, path: &str) -> i32 {
    match probe_get(addr, path) {
        Ok(200) => 0,
        _ => 1,
    }
}

pub fn run_with_probe(
    config: &SidecarConfig,
    probe: &(impl ProductProbe + ?Sized),
) -> Result<(), SidecarError> {
    let listener = bind(config.listen)?;
    serve_listener(listener, config, probe)?;
    Ok(())
}
