//! Concrete runtime boundary for `ores-process-lifecycle-agent`.
//!
//! The first executable increment is intentionally observe-only. It validates
//! host authority configuration and the cooperative product protocol, but it
//! cannot execute lifecycle mutations until durable CAS storage, trusted demand
//! observation, managed-lease transport, and post-wake admission are composed.

#![forbid(unsafe_code)]

mod config;
mod product_control;

use std::collections::BTreeMap;
use std::time::Duration;

use flags2env::BundledFlags2Env;

pub use config::{LifecycleAgentConfig, LifecycleProduct};
pub use product_control::{
    CooperativeAdmission, CooperativeProductStatus, ProductControlClient,
};

pub const DEFAULT_LIFECYCLE_CLI_CONFIG_PATH: &str = ".process-lifecycle-cli-flags.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleAgentCommand {
    Serve,
    Preflight,
    ProbeProduct,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleAgentInvocation {
    pub command: LifecycleAgentCommand,
    pub config: LifecycleAgentConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleRuntimeError {
    InvalidCliConfiguration,
    InvalidArguments,
    InvalidProduct,
    InvalidIdentity,
    InvalidPath,
    InvalidProductSocket,
    InvalidReconcileSeconds,
    InvalidBoolean,
    UnsupportedLeaseBackend,
    UnsupportedHost,
    MissingCgroupV2,
    MissingHostDirectory,
    SymlinkHostDirectory,
    ProductControlUnavailable,
    ControlResponseTooLarge,
    InvalidControlResponse,
    InvalidControlRequest,
    ProductDrainTimeout,
    ProductDemandReturned,
    ProductLifecycleBusy,
    MutationRuntimeNotEnabled,
}

impl std::fmt::Display for LifecycleRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return formatter.write_str(self.message());
    }
}

impl LifecycleRuntimeError {
    #[must_use]
    pub const fn message(self) -> &'static str {
        return match self {
            Self::InvalidCliConfiguration => "lifecycle CLI contract is invalid",
            Self::InvalidArguments => "lifecycle CLI arguments are invalid",
            Self::InvalidProduct => "unsupported lifecycle product adapter",
            Self::InvalidIdentity => "invalid lifecycle cluster/node identity",
            Self::InvalidPath => "invalid lifecycle authority path",
            Self::InvalidProductSocket => "product socket does not match the product contract",
            Self::InvalidReconcileSeconds => "invalid lifecycle reconciliation cadence",
            Self::InvalidBoolean => "invalid lifecycle boolean value",
            Self::UnsupportedLeaseBackend => "unsupported lifecycle lease backend",
            Self::UnsupportedHost => "process lifecycle agent requires Linux",
            Self::MissingCgroupV2 => "cgroup v2 is unavailable",
            Self::MissingHostDirectory => "required lifecycle host directory is missing",
            Self::SymlinkHostDirectory => "lifecycle host directory must not be a symlink",
            Self::ProductControlUnavailable => "product lifecycle control is unavailable",
            Self::ControlResponseTooLarge => "product lifecycle response exceeded its bound",
            Self::InvalidControlResponse => "product lifecycle response is invalid",
            Self::InvalidControlRequest => "product lifecycle request is invalid",
            Self::ProductDrainTimeout => "product runtime drain timed out",
            Self::ProductDemandReturned => "product runtime observed returned demand",
            Self::ProductLifecycleBusy => "product lifecycle control is busy",
            Self::MutationRuntimeNotEnabled => {
                "lifecycle effects require durable store and lease transport wiring"
            }
        };
    }
}

impl std::error::Error for LifecycleRuntimeError {}

pub fn resolve_process(
    config_path: &str,
) -> Result<LifecycleAgentInvocation, LifecycleRuntimeError> {
    return resolve(
        &std::env::args().collect::<Vec<_>>(),
        std::env::vars(),
        config_path,
    );
}

pub fn resolve<I>(
    argv: &[String],
    process_env: I,
    config_path: &str,
) -> Result<LifecycleAgentInvocation, LifecycleRuntimeError>
where
    I: IntoIterator<Item = (String, String)>,
{
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(config_path))
        .map_err(|_error| LifecycleRuntimeError::InvalidCliConfiguration)?;
    let parsed = parser
        .parse_structured(argv, Some(config_path))
        .map_err(|_error| LifecycleRuntimeError::InvalidArguments)?;
    if !parsed.errors.is_empty() || !parsed.unknown_options.is_empty() || !parsed.extras.is_empty()
    {
        return Err(LifecycleRuntimeError::InvalidArguments);
    }

    let command = parse_command(&parsed.command)?;
    let values = parsed
        .flags
        .into_iter()
        .chain(parsed.dotenv)
        .chain(process_env)
        .chain(parsed.dotenv_overrides)
        .chain(parsed.provided_flags)
        .collect::<BTreeMap<_, _>>();
    let config = LifecycleAgentConfig::from_values(&values)?;
    return Ok(LifecycleAgentInvocation { command, config });
}

fn parse_command(value: &str) -> Result<LifecycleAgentCommand, LifecycleRuntimeError> {
    if value.is_empty() {
        return Ok(LifecycleAgentCommand::Serve);
    }
    if value == "preflight" {
        return Ok(LifecycleAgentCommand::Preflight);
    }
    if value == "probe-product" {
        return Ok(LifecycleAgentCommand::ProbeProduct);
    }
    return Err(LifecycleRuntimeError::InvalidArguments);
}

pub fn run(invocation: LifecycleAgentInvocation) -> Result<(), LifecycleRuntimeError> {
    if invocation.command == LifecycleAgentCommand::Preflight {
        return invocation.config.preflight_host();
    }
    if invocation.command == LifecycleAgentCommand::ProbeProduct {
        invocation.config.preflight_host()?;
        let _status = ProductControlClient::new(&invocation.config).status()?;
        return Ok(());
    }

    invocation.config.preflight_host()?;
    if invocation.config.effects_enabled {
        return Err(LifecycleRuntimeError::MutationRuntimeNotEnabled);
    }
    return serve_observe_only(invocation.config);
}

fn serve_observe_only(config: LifecycleAgentConfig) -> Result<(), LifecycleRuntimeError> {
    let client = ProductControlClient::new(&config);
    let cadence = Duration::from_secs(config.reconcile_seconds);
    loop {
        // Cooperative status is intentionally discarded. Until trusted demand,
        // durable state and fenced authority are wired, this loop proves only
        // that the product-control seam remains reachable and bounded.
        let _observation = client.status();
        std::thread::sleep(cadence);
    }
}
