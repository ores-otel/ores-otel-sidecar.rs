use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::LifecycleRuntimeError;

const MIN_RECONCILE_SECONDS: u64 = 1;
const MAX_RECONCILE_SECONDS: u64 = 3600;
const DEFAULT_CONFIG_VERSION: &str = "v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleConfigVersion {
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEffectsMode {
    Observe,
    FreezeThaw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleLeaseProvider {
    CloudflareDurableObject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleProduct {
    BeamScale,
    ScintillaRun,
}

impl LifecycleProduct {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        if matches!(self, Self::BeamScale) {
            return "beamscale";
        }
        return "scintilla-run";
    }

    #[must_use]
    pub const fn required_socket(self) -> &'static str {
        if matches!(self, Self::BeamScale) {
            return "/run/beamscale-lifecycle/product/control.sock";
        }
        return "/run/scintilla-lifecycle/product/control.sock";
    }

    #[must_use]
    pub const fn required_host_control_socket(self) -> &'static str {
        if matches!(self, Self::BeamScale) {
            return "/run/beamscale-lifecycle/host/control.sock";
        }
        return "/run/scintilla-lifecycle/host/control.sock";
    }

    #[must_use]
    pub const fn lifecycle_lease_key_prefix(self) -> &'static str {
        if matches!(self, Self::BeamScale) {
            return "beamscale/runtime-lifecycle";
        }
        return "scintilla-run/runtime-lifecycle";
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleAgentConfig {
    pub config_version: LifecycleConfigVersion,
    pub product: LifecycleProduct,
    pub cluster: String,
    pub environment: Option<String>,
    pub region: Option<String>,
    pub node: String,
    pub state_root: PathBuf,
    pub checkpoint_root: PathBuf,
    pub cgroup_root: PathBuf,
    pub product_socket: PathBuf,
    pub host_control_socket: PathBuf,
    pub reconcile_seconds: u64,
    pub lease_provider: LifecycleLeaseProvider,
    pub lease_endpoint: Option<String>,
    pub credential_name: Option<String>,
    pub effects: LifecycleEffectsMode,
    pub hibernate_enabled: bool,
    pub effects_enabled: bool,
}

impl LifecycleAgentConfig {
    pub fn from_values(
        values: &BTreeMap<String, String>,
    ) -> Result<Self, LifecycleRuntimeError> {
        let config_version = parse_config_version(
            optional_value(values, "ORES_PROCESS_LIFECYCLE_CONFIG_VERSION")
                .as_deref()
                .unwrap_or(DEFAULT_CONFIG_VERSION),
        )?;
        return match config_version {
            LifecycleConfigVersion::V1 => Self::from_v1(values),
            LifecycleConfigVersion::V2 => Self::from_v2(values),
        };
    }

    fn from_v1(values: &BTreeMap<String, String>) -> Result<Self, LifecycleRuntimeError> {
        let common = CommonConfig::from_values(values)?;
        let checkpoint_root = parse_authority_path(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_CHECKPOINT_ROOT",
        )?)?;
        let product_socket = parse_product_socket(
            common.product,
            required_value(values, "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET")?,
        )?;
        let host_control_socket = parse_host_control_socket(
            common.product,
            required_value(values, "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET")?,
        )?;
        validate_socket_trust_split(&product_socket, &host_control_socket)?;
        if common.state_root == checkpoint_root {
            return Err(LifecycleRuntimeError::InvalidPath);
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
            config_version: LifecycleConfigVersion::V1,
            product: common.product,
            cluster: common.cluster,
            environment: None,
            region: None,
            node: common.node,
            state_root: common.state_root,
            checkpoint_root,
            cgroup_root: common.cgroup_root,
            product_socket,
            host_control_socket,
            reconcile_seconds: common.reconcile_seconds,
            lease_provider: LifecycleLeaseProvider::CloudflareDurableObject,
            lease_endpoint: None,
            credential_name: None,
            effects: effects_mode_from_bool(effects_enabled),
            hibernate_enabled,
            effects_enabled,
        });
    }

    fn from_v2(values: &BTreeMap<String, String>) -> Result<Self, LifecycleRuntimeError> {
        let common = CommonConfig::from_values(values)?;
        let (environment, region) = parse_v2_deployment_identity(values)?;
        reject_nonempty_legacy_value(values, "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET")?;
        reject_nonempty_legacy_value(values, "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET")?;
        reject_v2_legacy_mutation_switches(values)?;
        let (lease_provider, effects, lease_endpoint, credential_name) =
            parse_v2_lease_authority(values)?;
        let product_socket = PathBuf::from(common.product.required_socket());
        let host_control_socket = PathBuf::from(common.product.required_host_control_socket());
        validate_socket_trust_split(&product_socket, &host_control_socket)?;
        let checkpoint_root = common.state_root.join("checkpoints-disabled");
        let effects_enabled = matches!(effects, LifecycleEffectsMode::FreezeThaw);
        return Ok(Self {
            config_version: LifecycleConfigVersion::V2,
            product: common.product,
            cluster: common.cluster,
            environment: Some(environment),
            region: Some(region),
            node: common.node,
            state_root: common.state_root,
            checkpoint_root,
            cgroup_root: common.cgroup_root,
            product_socket,
            host_control_socket,
            reconcile_seconds: common.reconcile_seconds,
            lease_provider,
            lease_endpoint,
            credential_name,
            effects,
            hibernate_enabled: false,
            effects_enabled,
        });
    }

    #[must_use]
    pub const fn lease_key_prefix(&self) -> &'static str {
        return self.product.lifecycle_lease_key_prefix();
    }

    pub fn preflight_host(&self) -> Result<(), LifecycleRuntimeError> {
        if !cfg!(target_os = "linux") {
            return Err(LifecycleRuntimeError::UnsupportedHost);
        }
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
            return Err(LifecycleRuntimeError::MissingCgroupV2);
        }
        for path in [&self.state_root, &self.cgroup_root] {
            validate_existing_directory(path)?;
        }
        if matches!(self.config_version, LifecycleConfigVersion::V1) || self.hibernate_enabled {
            validate_existing_directory(&self.checkpoint_root)?;
        }
        validate_socket_trust_split(&self.product_socket, &self.host_control_socket)?;
        for socket in [&self.product_socket, &self.host_control_socket] {
            let parent = socket
                .parent()
                .ok_or(LifecycleRuntimeError::InvalidPath)?;
            validate_existing_directory(parent)?;
        }
        return Ok(());
    }
}

struct CommonConfig {
    product: LifecycleProduct,
    cluster: String,
    node: String,
    state_root: PathBuf,
    cgroup_root: PathBuf,
    reconcile_seconds: u64,
}

impl CommonConfig {
    fn from_values(values: &BTreeMap<String, String>) -> Result<Self, LifecycleRuntimeError> {
        let product = parse_product(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_PRODUCT",
        )?)?;
        let cluster = required_value(values, "ORES_PROCESS_LIFECYCLE_CLUSTER")?;
        let node = required_value(values, "ORES_PROCESS_LIFECYCLE_NODE")?;
        validate_identity_segment(&cluster)?;
        validate_identity_segment(&node)?;
        let state_root = parse_authority_path(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_STATE_ROOT",
        )?)?;
        let cgroup_root = parse_cgroup_root(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT",
        )?)?;
        let reconcile_seconds = required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_RECONCILE_SECONDS",
        )?
        .parse::<u64>()
        .map_err(|_error| LifecycleRuntimeError::InvalidReconcileSeconds)?;
        if !(MIN_RECONCILE_SECONDS..=MAX_RECONCILE_SECONDS).contains(&reconcile_seconds) {
            return Err(LifecycleRuntimeError::InvalidReconcileSeconds);
        }
        return Ok(Self {
            product,
            cluster,
            node,
            state_root,
            cgroup_root,
            reconcile_seconds,
        });
    }
}

fn parse_v2_deployment_identity(
    values: &BTreeMap<String, String>,
) -> Result<(String, String), LifecycleRuntimeError> {
    let environment = required_value(values, "ORES_PROCESS_LIFECYCLE_ENVIRONMENT")?;
    let region = required_value(values, "ORES_PROCESS_LIFECYCLE_REGION")?;
    validate_identity_segment(&environment)?;
    validate_identity_segment(&region)?;
    return Ok((environment, region));
}

fn parse_v2_lease_authority(
    values: &BTreeMap<String, String>,
) -> Result<
    (
        LifecycleLeaseProvider,
        LifecycleEffectsMode,
        Option<String>,
        Option<String>,
    ),
    LifecycleRuntimeError,
> {
    let lease_provider = parse_lease_provider(required_value(
        values,
        "ORES_PROCESS_LIFECYCLE_LEASE_PROVIDER",
    )?)?;
    let effects = parse_effects_mode(required_value(
        values,
        "ORES_PROCESS_LIFECYCLE_EFFECTS",
    )?)?;
    let lease_endpoint = optional_value(values, "ORES_PROCESS_LIFECYCLE_LEASE_ENDPOINT")
        .map(|value| validate_lease_endpoint(value).map(str::to_owned))
        .transpose()?;
    let credential_name = optional_value(values, "ORES_PROCESS_LIFECYCLE_CREDENTIAL_NAME")
        .map(|value| validate_credential_name(value).map(str::to_owned))
        .transpose()?;
    if matches!(effects, LifecycleEffectsMode::FreezeThaw)
        && (lease_endpoint.is_none() || credential_name.is_none())
    {
        return Err(LifecycleRuntimeError::MissingLeaseAuthority);
    }
    return Ok((lease_provider, effects, lease_endpoint, credential_name));
}

fn reject_v2_legacy_mutation_switches(
    values: &BTreeMap<String, String>,
) -> Result<(), LifecycleRuntimeError> {
    if optional_bool(values, "ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED")?.unwrap_or(false)
        || optional_bool(values, "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED")?.unwrap_or(false)
    {
        return Err(LifecycleRuntimeError::InvalidArguments);
    }
    return Ok(());
}

const fn effects_mode_from_bool(enabled: bool) -> LifecycleEffectsMode {
    if enabled {
        return LifecycleEffectsMode::FreezeThaw;
    }
    return LifecycleEffectsMode::Observe;
}

fn parse_config_version(value: &str) -> Result<LifecycleConfigVersion, LifecycleRuntimeError> {
    if value == "v1" {
        return Ok(LifecycleConfigVersion::V1);
    }
    if value == "v2" {
        return Ok(LifecycleConfigVersion::V2);
    }
    return Err(LifecycleRuntimeError::InvalidConfigVersion);
}

fn parse_product(value: String) -> Result<LifecycleProduct, LifecycleRuntimeError> {
    if value == "beamscale" {
        return Ok(LifecycleProduct::BeamScale);
    }
    if value == "scintilla-run" {
        return Ok(LifecycleProduct::ScintillaRun);
    }
    return Err(LifecycleRuntimeError::InvalidProduct);
}

fn parse_lease_provider(value: String) -> Result<LifecycleLeaseProvider, LifecycleRuntimeError> {
    if value == "cloudflare-do" {
        return Ok(LifecycleLeaseProvider::CloudflareDurableObject);
    }
    return Err(LifecycleRuntimeError::UnsupportedLeaseProvider);
}

fn parse_effects_mode(value: String) -> Result<LifecycleEffectsMode, LifecycleRuntimeError> {
    if value == "observe" {
        return Ok(LifecycleEffectsMode::Observe);
    }
    if value == "freeze_thaw" {
        return Ok(LifecycleEffectsMode::FreezeThaw);
    }
    return Err(LifecycleRuntimeError::InvalidEffectsMode);
}

fn required_value(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<String, LifecycleRuntimeError> {
    return optional_value(values, key)
        .map(str::to_owned)
        .ok_or(LifecycleRuntimeError::InvalidArguments);
}

fn optional_value<'a>(values: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    return values
        .get(key)
        .map(|candidate| candidate.trim())
        .filter(|candidate| !candidate.is_empty());
}

fn reject_nonempty_legacy_value(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<(), LifecycleRuntimeError> {
    if optional_value(values, key).is_some() {
        return Err(LifecycleRuntimeError::InvalidArguments);
    }
    return Ok(());
}

fn optional_bool(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<bool>, LifecycleRuntimeError> {
    return optional_value(values, key)
        .map(|value| parse_bool(value.to_owned()))
        .transpose();
}

fn validate_identity_segment(value: &str) -> Result<(), LifecycleRuntimeError> {
    let valid = !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
        });
    if !valid {
        return Err(LifecycleRuntimeError::InvalidIdentity);
    }
    return Ok(());
}

fn validate_credential_name(value: &str) -> Result<&str, LifecycleRuntimeError> {
    let valid = value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid || value == "." || value == ".." {
        return Err(LifecycleRuntimeError::InvalidCredentialName);
    }
    return Ok(value);
}

fn validate_lease_endpoint(value: &str) -> Result<&str, LifecycleRuntimeError> {
    let remainder = value
        .strip_prefix("https://")
        .ok_or(LifecycleRuntimeError::InvalidLeaseEndpoint)?;
    let valid = !remainder.is_empty()
        && !remainder.starts_with('/')
        && !value.bytes().any(|byte| byte.is_ascii_whitespace())
        && !remainder.contains('@')
        && !remainder.contains('?')
        && !remainder.contains('#');
    if !valid {
        return Err(LifecycleRuntimeError::InvalidLeaseEndpoint);
    }
    return Ok(value.trim_end_matches('/'));
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

fn parse_host_control_socket(
    product: LifecycleProduct,
    value: String,
) -> Result<PathBuf, LifecycleRuntimeError> {
    if value != product.required_host_control_socket() {
        return Err(LifecycleRuntimeError::InvalidHostControlSocket);
    }
    let path = PathBuf::from(value);
    validate_absolute_no_traversal(&path)?;
    return Ok(path);
}

fn validate_socket_trust_split(
    product_socket: &Path,
    host_control_socket: &Path,
) -> Result<(), LifecycleRuntimeError> {
    if product_socket == host_control_socket {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    let product_parent = product_socket
        .parent()
        .ok_or(LifecycleRuntimeError::InvalidPath)?;
    let host_parent = host_control_socket
        .parent()
        .ok_or(LifecycleRuntimeError::InvalidPath)?;
    if product_parent == host_parent
        || product_parent.starts_with(host_parent)
        || host_parent.starts_with(product_parent)
    {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    return Ok(());
}

fn validate_absolute_no_traversal(path: &Path) -> Result<(), LifecycleRuntimeError> {
    if !path.is_absolute() {
        return Err(LifecycleRuntimeError::InvalidPath);
    }
    let has_traversal = path
        .components()
        .any(|component| component == Component::ParentDir || component == Component::CurDir);
    if has_traversal {
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
    if value == "true" {
        return Ok(true);
    }
    if value == "false" {
        return Ok(false);
    }
    return Err(LifecycleRuntimeError::InvalidBoolean);
}

#[cfg(test)]
#[path = "config/tests/mod.rs"]
mod tests;
