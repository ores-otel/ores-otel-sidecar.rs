//! Concrete runtime boundary for `ores-process-lifecycle-agent`.
//!
//! The executable remains mutation-disabled. It validates both independent
//! control seams required before effects can be composed: the cooperative
//! product runtime and the trusted host/control-plane authority. Durable CAS,
//! distributed lease transport, process attestation, and effect reconciliation
//! are still mandatory before mutation effects may be honored.

#![forbid(unsafe_code)]

mod config;
mod host_control;
mod product_control;

use std::collections::BTreeMap;
use std::time::Duration;

use flags2env::BundledFlags2Env;

pub use config::{
    LifecycleAgentConfig, LifecycleConfigVersion, LifecycleEffectsMode, LifecycleLeaseProvider,
    LifecycleProduct,
};
pub use host_control::{
    TrustedHostAdmission, TrustedHostControlClient, TrustedHostSnapshot, TrustedWorkloadSnapshot,
};
pub use product_control::{
    CooperativeAdmission, CooperativeProductStatus, ProductControlClient,
};

pub const DEFAULT_LIFECYCLE_CLI_CONFIG_PATH: &str = ".process-lifecycle-cli-flags.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleAgentCommand {
    Serve,
    Preflight,
    ProbeProduct,
    ProbeHost,
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
    InvalidHostControlSocket,
    InvalidReconcileSeconds,
    InvalidBoolean,
    InvalidConfigVersion,
    UnsupportedLeaseBackend,
    UnsupportedLeaseProvider,
    InvalidLeaseEndpoint,
    InvalidCredentialName,
    InvalidEffectsMode,
    MissingLeaseAuthority,
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
    HostControlUnavailable,
    HostControlResponseTooLarge,
    InvalidHostControlResponse,
    InvalidHostControlRequest,
    HostIdentityMismatch,
    HostDemandChanged,
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
            Self::InvalidIdentity => "invalid lifecycle deployment identity",
            Self::InvalidPath => "invalid lifecycle authority path",
            Self::InvalidProductSocket => "product socket does not match the product contract",
            Self::InvalidHostControlSocket => {
                "host-control socket does not match the product contract"
            }
            Self::InvalidReconcileSeconds => "invalid lifecycle reconciliation cadence",
            Self::InvalidBoolean => "invalid lifecycle boolean value",
            Self::InvalidConfigVersion => "unsupported lifecycle configuration contract version",
            Self::UnsupportedLeaseBackend => "unsupported v1 lifecycle lease backend",
            Self::UnsupportedLeaseProvider => "unsupported lifecycle lease provider",
            Self::InvalidLeaseEndpoint => "invalid lifecycle lease authority endpoint",
            Self::InvalidCredentialName => "invalid lifecycle systemd credential name",
            Self::InvalidEffectsMode => "unsupported lifecycle effects mode",
            Self::MissingLeaseAuthority => {
                "active lifecycle effects require a lease endpoint and credential name"
            }
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
            Self::HostControlUnavailable => "trusted host lifecycle control is unavailable",
            Self::HostControlResponseTooLarge => {
                "trusted host lifecycle response exceeded its bound"
            }
            Self::InvalidHostControlResponse => "trusted host lifecycle response is invalid",
            Self::InvalidHostControlRequest => "trusted host lifecycle request is invalid",
            Self::HostIdentityMismatch => "trusted host lifecycle identity is stale or mismatched",
            Self::HostDemandChanged => "trusted host lifecycle demand changed during transition",
            Self::MutationRuntimeNotEnabled => {
                "lifecycle effects require durable store, lease transport, attestation, and recovery wiring"
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
    if value == "probe-host" {
        return Ok(LifecycleAgentCommand::ProbeHost);
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
    if invocation.command == LifecycleAgentCommand::ProbeHost {
        invocation.config.preflight_host()?;
        let _snapshot = TrustedHostControlClient::new(&invocation.config).snapshot()?;
        return Ok(());
    }

    invocation.config.preflight_host()?;
    if invocation.config.effects_enabled {
        return Err(LifecycleRuntimeError::MutationRuntimeNotEnabled);
    }
    return serve_observe_only(invocation.config);
}

fn serve_observe_only(config: LifecycleAgentConfig) -> Result<(), LifecycleRuntimeError> {
    let product = ProductControlClient::new(&config);
    let host = TrustedHostControlClient::new(&config);
    let cadence = Duration::from_secs(config.reconcile_seconds);
    loop {
        // These observations remain deliberately non-mutating. The product seam
        // proves cooperative reachability while the host seam proves that a
        // distinct trusted authority can enumerate validated workload identity,
        // placement, runtime incarnation, demand, and isolation-policy evidence.
        let _product_observation = product.status();
        let _host_observation = host.snapshot();
        std::thread::sleep(cadence);
    }
}
