//! Concrete runtime boundary for `ores-process-lifecycle-agent`.
//!
//! This module intentionally starts with a read-only control-plane slice. It
//! validates the host authority configuration and the cooperative product socket
//! protocol without performing lifecycle mutations. Enabling process effects is
//! fail-closed until the durable CAS store and concrete managed-lease transport
//! are wired in the next increment; this prevents an infra package from silently
//! claiming fenced suspend/resume semantics that it cannot yet provide.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flags2env::BundledFlags2Env;

pub const DEFAULT_LIFECYCLE_CLI_CONFIG_PATH: &str = ".process-lifecycle-cli-flags.toml";
const MAX_CONTROL_RESPONSE_BYTES: usize = 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MIN_RECONCILE_SECONDS: u64 = 1;
const MAX_RECONCILE_SECONDS: u64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleAgentCommand {
    Serve,
    Preflight,
    ProbeProduct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleProduct {
    BeamScale,
    ScintillaRun,
}

impl LifecycleProduct {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeamScale => {
                return "beamscale";
            }
            Self::ScintillaRun => {
                return "scintilla-run";
            }
        }
    }

    #[must_use]
    pub const fn required_socket(self) -> &'static str {
        match self {
            Self::BeamScale => {
                return "/run/beamscale-lifecycle/control.sock";
            }
            Self::ScintillaRun => {
                return "/run/scintilla-lifecycle/control.sock";
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleAgentConfig {
    pub product: LifecycleProduct,
    pub cluster: String,
    pub node: String,
    pub state_root: PathBuf,
    pub checkpoint_root: PathBuf,
    pub cgroup_root: PathBuf,
    pub product_socket: PathBuf,
    pub reconcile_seconds: u64,
    pub hibernate_enabled: bool,
    pub effects_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleAgentInvocation {
    pub command: LifecycleAgentCommand,
    pub config: LifecycleAgentConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CooperativeAdmission {
    Accepting,
    Quiescing,
    Sealed,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CooperativeProductStatus {
    pub admission: CooperativeAdmission,
    pub in_flight: u64,
    pub queue_depth: u64,
    pub idle_for_ms: u64,
}

/// The product status is cooperative evidence only. A hostile tenant runtime may
/// lie about these values, so callers MUST NOT use this type as trusted lifecycle
/// demand/admission authority.
#[derive(Clone, Debug)]
pub struct ProductControlClient {
    socket_path: PathBuf,
}

impl ProductControlClient {
    #[must_use]
    pub fn new(config: &LifecycleAgentConfig) -> Self {
        return Self {
            socket_path: config.product_socket.clone(),
        };
    }

    pub fn status(&self) -> Result<CooperativeProductStatus, LifecycleRuntimeError> {
        let response = self.request(b"v1 status\n")?;
        return parse_status_response(&response);
    }

    pub fn quiesce(&self, timeout_ms: u64) -> Result<(), LifecycleRuntimeError> {
        if timeout_ms > 300_000 {
            return Err(LifecycleRuntimeError::InvalidControlRequest);
        }
        let request = format!("v1 quiesce {timeout_ms}\n");
        let response = self.request(request.as_bytes())?;
        match response.as_str() {
            "ok sealed" => {
                return Ok(());
            }
            "error drain_timeout" => {
                return Err(LifecycleRuntimeError::ProductDrainTimeout);
            }
            "error demand_returned" => {
                return Err(LifecycleRuntimeError::ProductDemandReturned);
            }
            "error lifecycle_busy" => {
                return Err(LifecycleRuntimeError::ProductLifecycleBusy);
            }
            _ => {
                return Err(LifecycleRuntimeError::InvalidControlResponse);
            }
        }
    }

    pub fn resume(&self) -> Result<(), LifecycleRuntimeError> {
        let response = self.request(b"v1 resume\n")?;
        if response == "ok running" {
            return Ok(());
        }
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }

    fn request(&self, request: &[u8]) -> Result<String, LifecycleRuntimeError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
        stream
            .set_read_timeout(Some(CONTROL_TIMEOUT))
            .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
        stream
            .set_write_timeout(Some(CONTROL_TIMEOUT))
            .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
        stream
            .write_all(request)
            .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;

        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 256];
        loop {
            let count = stream
                .read(&mut chunk)
                .map_err(|_error| LifecycleRuntimeError::ProductControlUnavailable)?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
                return Err(LifecycleRuntimeError::ControlResponseTooLarge);
            }
            if bytes.contains(&b'\n') {
                break;
            }
        }

        let newline = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(bytes.len());
        let line = std::str::from_utf8(&bytes[..newline])
            .map_err(|_error| LifecycleRuntimeError::InvalidControlResponse)?;
        let trimmed = line.trim_matches(['\r', ' ', '\t']);
        if trimmed.is_empty() {
            return Err(LifecycleRuntimeError::InvalidControlResponse);
        }
        return Ok(trimmed.to_owned());
    }
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

impl Display for LifecycleRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
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
                "lifecycle effects are disabled until durable store and lease transport wiring is present"
            }
        };
        return formatter.write_str(message);
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

    let command = match parsed.command.as_str() {
        "" => LifecycleAgentCommand::Serve,
        "preflight" => LifecycleAgentCommand::Preflight,
        "probe-product" => LifecycleAgentCommand::ProbeProduct,
        _ => return Err(LifecycleRuntimeError::InvalidArguments),
    };

    let mut values = BTreeMap::new();
    values.extend(parsed.flags);
    values.extend(parsed.dotenv);
    values.extend(process_env);
    values.extend(parsed.dotenv_overrides);
    values.extend(parsed.provided_flags);

    let config = LifecycleAgentConfig::from_values(&values)?;
    return Ok(LifecycleAgentInvocation { command, config });
}

impl LifecycleAgentConfig {
    pub fn from_values(
        values: &BTreeMap<String, String>,
    ) -> Result<Self, LifecycleRuntimeError> {
        let product = match required_value(values, "ORES_PROCESS_LIFECYCLE_PRODUCT")?.as_str() {
            "beamscale" => LifecycleProduct::BeamScale,
            "scintilla-run" => LifecycleProduct::ScintillaRun,
            _ => return Err(LifecycleRuntimeError::InvalidProduct),
        };
        let cluster = required_value(values, "ORES_PROCESS_LIFECYCLE_CLUSTER")?;
        let node = required_value(values, "ORES_PROCESS_LIFECYCLE_NODE")?;
        validate_identity_segment(&cluster)?;
        validate_identity_segment(&node)?;

        let state_root = parse_authority_path(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_STATE_ROOT",
        )?)?;
        let checkpoint_root = parse_authority_path(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_CHECKPOINT_ROOT",
        )?)?;
        let cgroup_root = parse_cgroup_root(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT",
        )?)?;
        let product_socket = parse_product_socket(
            product,
            required_value(values, "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET")?,
        )?;
        if state_root == checkpoint_root {
            return Err(LifecycleRuntimeError::InvalidPath);
        }

        let reconcile_seconds = required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_RECONCILE_SECONDS",
        )?
        .parse::<u64>()
        .map_err(|_error| LifecycleRuntimeError::InvalidReconcileSeconds)?;
        if !(MIN_RECONCILE_SECONDS..=MAX_RECONCILE_SECONDS).contains(&reconcile_seconds) {
            return Err(LifecycleRuntimeError::InvalidReconcileSeconds);
        }

        let backend = required_value(values, "ORES_PROCESS_LIFECYCLE_LEASE_BACKEND")?;
        if backend != "cloudflare-do" {
            return Err(LifecycleRuntimeError::UnsupportedLeaseBackend);
        }

        let hibernate_enabled = parse_bool(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED",
        )?)?;
        let effects_enabled = parse_bool(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED",
        )?)?;

        return Ok(Self {
            product,
            cluster,
            node,
            state_root,
            checkpoint_root,
            cgroup_root,
            product_socket,
            reconcile_seconds,
            hibernate_enabled,
            effects_enabled,
        });
    }

    /// Validate host prerequisites without contacting the product runtime or
    /// performing any process mutation.
    pub fn preflight_host(&self) -> Result<(), LifecycleRuntimeError> {
        if !cfg!(target_os = "linux") {
            return Err(LifecycleRuntimeError::UnsupportedHost);
        }
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
            return Err(LifecycleRuntimeError::MissingCgroupV2);
        }
        for path in [&self.state_root, &self.checkpoint_root, &self.cgroup_root] {
            validate_existing_directory(path)?;
        }
        let parent = self
            .product_socket
            .parent()
            .ok_or(LifecycleRuntimeError::InvalidProductSocket)?;
        validate_existing_directory(parent)?;
        return Ok(());
    }
}

pub fn run(invocation: LifecycleAgentInvocation) -> Result<(), LifecycleRuntimeError> {
    match invocation.command {
        LifecycleAgentCommand::Preflight => {
            return invocation.config.preflight_host();
        }
        LifecycleAgentCommand::ProbeProduct => {
            invocation.config.preflight_host()?;
            let client = ProductControlClient::new(&invocation.config);
            let _status = client.status()?;
            return Ok(());
        }
        LifecycleAgentCommand::Serve => {
            invocation.config.preflight_host()?;
            if invocation.config.effects_enabled {
                return Err(LifecycleRuntimeError::MutationRuntimeNotEnabled);
            }
            return serve_observe_only(invocation.config);
        }
    }
}

fn serve_observe_only(config: LifecycleAgentConfig) -> Result<(), LifecycleRuntimeError> {
    let client = ProductControlClient::new(&config);
    let cadence = Duration::from_secs(config.reconcile_seconds);
    loop {
        // The product status is intentionally not fed into lifecycle policy: it
        // is cooperative/untrusted for the hostile runtime class. Until trusted
        // demand + durable store + fenced transport are wired, this daemon only
        // proves/monitors the local product-control protocol and performs no
        // freeze, checkpoint, restore, lease, or durable state mutation.
        match client.status() {
            Ok(_status) => {
                eprintln!("{\"event\":\"process_lifecycle_product_control_ok\"}");
            }
            Err(_error) => {
                eprintln!("{\"event\":\"process_lifecycle_product_control_unavailable\"}");
            }
        }
        std::thread::sleep(cadence);
    }
}

fn required_value(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<String, LifecycleRuntimeError> {
    let value = values
        .get(key)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or(LifecycleRuntimeError::InvalidArguments)?;
    return Ok(value.to_owned());
}

fn validate_identity_segment(value: &str) -> Result<(), LifecycleRuntimeError> {
    let valid = !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    if !valid {
        return Err(LifecycleRuntimeError::InvalidIdentity);
    }
    return Ok(());
}

fn parse_authority_path(value: String) -> Result<PathBuf, LifecycleRuntimeError> {
    let path = PathBuf::from(value);
    validate_absolute_no_traversal(&path)?;
    if path == Path::new("/") || path == Path::new("/var") || path == Path::new("/var/lib") {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    return Ok(path);
}

fn parse_cgroup_root(value: String) -> Result<PathBuf, LifecycleRuntimeError> {
    let path = PathBuf::from(value);
    validate_absolute_no_traversal(&path)?;
    let cgroup_root = Path::new("/sys/fs/cgroup");
    if path == cgroup_root || !path.starts_with(cgroup_root) {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    return Ok(path);
}

fn parse_product_socket(
    product: LifecycleProduct,
    value: String,
) -> Result<PathBuf, LifecycleRuntimeError> {
    if value != product.required_socket() {
        return Err(LifecycleRuntimeError::InvalidProductSocket);
    }
    let path = PathBuf::from(value);
    validate_absolute_no_traversal(&path)?;
    return Ok(path);
}

fn validate_absolute_no_traversal(path: &Path) -> Result<(), LifecycleRuntimeError> {
    if !path.is_absolute() {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    if path.components().any(|component| {
        matches!(component, Component::ParentDir | Component::CurDir)
    }) {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    return Ok(());
}

fn validate_existing_directory(path: &Path) -> Result<(), LifecycleRuntimeError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_error| LifecycleRuntimeError::MissingHostDirectory)?;
    if metadata.file_type().is_symlink() {
        return Err(LifecycleRuntimeError::SymlinkHostDirectory);
    }
    if !metadata.is_dir() {
        return Err(LifecycleRuntimeError::MissingHostDirectory);
    }
    return Ok(());
}

fn parse_bool(value: String) -> Result<bool, LifecycleRuntimeError> {
    match value.as_str() {
        "true" => {
            return Ok(true);
        }
        "false" => {
            return Ok(false);
        }
        _ => {
            return Err(LifecycleRuntimeError::InvalidBoolean);
        }
    }
}

fn parse_status_response(response: &str) -> Result<CooperativeProductStatus, LifecycleRuntimeError> {
    let parts = response.split_ascii_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "ok" || parts[1] != "status" {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    let admission = match parts[2] {
        "accepting" => CooperativeAdmission::Accepting,
        "quiescing" => CooperativeAdmission::Quiescing,
        "sealed" => CooperativeAdmission::Sealed,
        "unavailable" => CooperativeAdmission::Unavailable,
        _ => return Err(LifecycleRuntimeError::InvalidControlResponse),
    };
    let in_flight = parse_bounded_counter(parts[3])?;
    let queue_depth = parse_bounded_counter(parts[4])?;
    let idle_for_ms = parse_bounded_counter(parts[5])?;
    return Ok(CooperativeProductStatus {
        admission,
        in_flight,
        queue_depth,
        idle_for_ms,
    });
}

fn parse_bounded_counter(value: &str) -> Result<u64, LifecycleRuntimeError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_error| LifecycleRuntimeError::InvalidControlResponse)?;
    if parsed > i64::MAX as u64 {
        return Err(LifecycleRuntimeError::InvalidControlResponse);
    }
    return Ok(parsed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(product: &str) -> BTreeMap<String, String> {
        let socket = if product == "beamscale" {
            "/run/beamscale-lifecycle/control.sock"
        } else {
            "/run/scintilla-lifecycle/control.sock"
        };
        return BTreeMap::from([
            ("ORES_PROCESS_LIFECYCLE_PRODUCT".to_owned(), product.to_owned()),
            ("ORES_PROCESS_LIFECYCLE_CLUSTER".to_owned(), "prod-us-east".to_owned()),
            ("ORES_PROCESS_LIFECYCLE_NODE".to_owned(), "host-7".to_owned()),
            (
                "ORES_PROCESS_LIFECYCLE_STATE_ROOT".to_owned(),
                "/var/lib/ores-lifecycle/state".to_owned(),
            ),
            (
                "ORES_PROCESS_LIFECYCLE_CHECKPOINT_ROOT".to_owned(),
                "/var/lib/ores-lifecycle/checkpoints".to_owned(),
            ),
            (
                "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT".to_owned(),
                "/sys/fs/cgroup/ores-workloads.slice".to_owned(),
            ),
            ("ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(), socket.to_owned()),
            (
                "ORES_PROCESS_LIFECYCLE_RECONCILE_SECONDS".to_owned(),
                "15".to_owned(),
            ),
            (
                "ORES_PROCESS_LIFECYCLE_LEASE_BACKEND".to_owned(),
                "cloudflare-do".to_owned(),
            ),
            (
                "ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED".to_owned(),
                "false".to_owned(),
            ),
            (
                "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED".to_owned(),
                "false".to_owned(),
            ),
        ]);
    }

    #[test]
    fn product_socket_is_exact_and_product_specific() {
        let config = LifecycleAgentConfig::from_values(&values("beamscale"));
        assert!(config.is_ok());

        let mut invalid = values("beamscale");
        invalid.insert(
            "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
            "/run/scintilla-lifecycle/control.sock".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidProductSocket)
        );
    }

    #[test]
    fn lifecycle_identity_rejects_path_separators() {
        let mut invalid = values("scintilla-run");
        invalid.insert(
            "ORES_PROCESS_LIFECYCLE_CLUSTER".to_owned(),
            "prod/escape".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidIdentity)
        );
    }

    #[test]
    fn cgroup_root_cannot_expand_to_host_root() {
        let mut invalid = values("beamscale");
        invalid.insert(
            "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT".to_owned(),
            "/sys/fs/cgroup".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidPath)
        );
    }

    #[test]
    fn effects_flag_does_not_silently_enable_mutation_runtime() {
        let mut enabled = values("beamscale");
        enabled.insert(
            "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED".to_owned(),
            "true".to_owned(),
        );
        let config = LifecycleAgentConfig::from_values(&enabled).unwrap();
        assert!(config.effects_enabled);
    }

    #[test]
    fn strict_status_parser_accepts_shared_v1_shape() {
        assert_eq!(
            parse_status_response("ok status sealed 0 0 125"),
            Ok(CooperativeProductStatus {
                admission: CooperativeAdmission::Sealed,
                in_flight: 0,
                queue_depth: 0,
                idle_for_ms: 125,
            })
        );
        assert_eq!(
            parse_status_response("ok status accepting -1 0 0"),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
        assert_eq!(
            parse_status_response("ok status accepting 0 0 0 extra"),
            Err(LifecycleRuntimeError::InvalidControlResponse)
        );
    }

    #[test]
    fn bool_values_are_canonical_only_after_resolution() {
        let mut invalid = values("beamscale");
        invalid.insert(
            "ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED".to_owned(),
            "yes".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidBoolean)
        );
    }
}
