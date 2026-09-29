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
    // Compatibility mirrors retained while BeamScale/Scintilla move from the v1
    // environment contract to v2. v2 hard-codes hibernation off and derives the
    // effect boolean from the typed mode.
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
            effects: if effects_enabled {
                LifecycleEffectsMode::FreezeThaw
            } else {
                LifecycleEffectsMode::Observe
            },
            hibernate_enabled,
            effects_enabled,
        });
    }

    fn from_v2(values: &BTreeMap<String, String>) -> Result<Self, LifecycleRuntimeError> {
        let common = CommonConfig::from_values(values)?;
        let environment = required_value(values, "ORES_PROCESS_LIFECYCLE_ENVIRONMENT")?;
        let region = required_value(values, "ORES_PROCESS_LIFECYCLE_REGION")?;
        validate_identity_segment(&environment)?;
        validate_identity_segment(&region)?;

        // v2 removes duplicated socket authority. The product adapter is the one
        // source of truth for both cooperative and trusted Unix socket paths.
        reject_nonempty_legacy_value(values, "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET")?;
        reject_nonempty_legacy_value(values, "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET")?;
        let product_socket = PathBuf::from(common.product.required_socket());
        let host_control_socket = PathBuf::from(common.product.required_host_control_socket());
        validate_socket_trust_split(&product_socket, &host_control_socket)?;

        let lease_provider = parse_lease_provider(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_LEASE_PROVIDER",
        )?)?;
        let effects = parse_effects_mode(required_value(
            values,
            "ORES_PROCESS_LIFECYCLE_EFFECTS",
        )?)?;

        // The old mutation booleans are not alternate v2 authority switches.
        if optional_bool(values, "ORES_PROCESS_LIFECYCLE_HIBERNATE_ENABLED")?.unwrap_or(false)
            || optional_bool(values, "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED")?.unwrap_or(false)
        {
            return Err(LifecycleRuntimeError::InvalidArguments);
        }

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

        // Freeze/thaw v2 has no checkpoint authority. Keep a private sentinel
        // path only for temporary source compatibility with v1 consumers; it is
        // never preflighted or used while hibernation is false.
        let checkpoint_root = common.state_root.join("checkpoints-disabled");

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
            effects_enabled: matches!(effects, LifecycleEffectsMode::FreezeThaw),
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
mod tests {
    use super::*;

    fn values(product: &str) -> BTreeMap<String, String> {
        let (product_socket, host_control_socket) = if product == "beamscale" {
            (
                "/run/beamscale-lifecycle/product/control.sock",
                "/run/beamscale-lifecycle/host/control.sock",
            )
        } else {
            (
                "/run/scintilla-lifecycle/product/control.sock",
                "/run/scintilla-lifecycle/host/control.sock",
            )
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
            (
                "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
                product_socket.to_owned(),
            ),
            (
                "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET".to_owned(),
                host_control_socket.to_owned(),
            ),
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

    fn v2_values(product: &str) -> BTreeMap<String, String> {
        let mut values = values(product);
        values.insert(
            "ORES_PROCESS_LIFECYCLE_CONFIG_VERSION".to_owned(),
            "v2".to_owned(),
        );
        values.insert(
            "ORES_PROCESS_LIFECYCLE_ENVIRONMENT".to_owned(),
            "prod".to_owned(),
        );
        values.insert(
            "ORES_PROCESS_LIFECYCLE_REGION".to_owned(),
            "us-east".to_owned(),
        );
        values.insert(
            "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
            String::new(),
        );
        values.insert(
            "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET".to_owned(),
            String::new(),
        );
        values.insert(
            "ORES_PROCESS_LIFECYCLE_LEASE_PROVIDER".to_owned(),
            "cloudflare-do".to_owned(),
        );
        values.insert(
            "ORES_PROCESS_LIFECYCLE_EFFECTS".to_owned(),
            "observe".to_owned(),
        );
        return values;
    }

    #[test]
    fn v1_remains_the_default_contract() {
        let config = LifecycleAgentConfig::from_values(&values("beamscale"));
        assert!(matches!(
            config,
            Ok(LifecycleAgentConfig {
                config_version: LifecycleConfigVersion::V1,
                effects: LifecycleEffectsMode::Observe,
                ..
            })
        ));
    }

    #[test]
    fn product_and_host_sockets_are_exact_product_specific_and_separate() {
        assert!(LifecycleAgentConfig::from_values(&values("beamscale")).is_ok());
        let invalid_product = values("beamscale")
            .into_iter()
            .map(|(key, value)| {
                if key == "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET" {
                    return (
                        key,
                        "/run/scintilla-lifecycle/product/control.sock".to_owned(),
                    );
                }
                return (key, value);
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid_product),
            Err(LifecycleRuntimeError::InvalidProductSocket)
        );

        let invalid_host = values("beamscale")
            .into_iter()
            .map(|(key, value)| {
                if key == "ORES_PROCESS_LIFECYCLE_HOST_CONTROL_SOCKET" {
                    return (
                        key,
                        "/run/scintilla-lifecycle/host/control.sock".to_owned(),
                    );
                }
                return (key, value);
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid_host),
            Err(LifecycleRuntimeError::InvalidHostControlSocket)
        );
    }

    #[test]
    fn v2_derives_sockets_and_canonical_product_prefix() {
        let config = LifecycleAgentConfig::from_values(&v2_values("scintilla-run"));
        assert!(matches!(
            config,
            Ok(LifecycleAgentConfig {
                config_version: LifecycleConfigVersion::V2,
                product_socket,
                host_control_socket,
                environment: Some(ref environment),
                region: Some(ref region),
                ..
            }) if product_socket == PathBuf::from("/run/scintilla-lifecycle/product/control.sock")
                && host_control_socket == PathBuf::from("/run/scintilla-lifecycle/host/control.sock")
                && environment == "prod"
                && region == "us-east"
        ));
        assert_eq!(
            LifecycleAgentConfig::from_values(&v2_values("beamscale"))
                .map(|config| config.lease_key_prefix()),
            Ok("beamscale/runtime-lifecycle")
        );
    }

    #[test]
    fn v2_observe_does_not_require_lease_secret_authority() {
        let config = LifecycleAgentConfig::from_values(&v2_values("beamscale"));
        assert!(matches!(
            config,
            Ok(LifecycleAgentConfig {
                effects: LifecycleEffectsMode::Observe,
                effects_enabled: false,
                lease_endpoint: None,
                credential_name: None,
                ..
            })
        ));
    }

    #[test]
    fn v2_freeze_thaw_requires_valid_endpoint_and_credential_name() {
        let mut active = v2_values("beamscale");
        active.insert(
            "ORES_PROCESS_LIFECYCLE_EFFECTS".to_owned(),
            "freeze_thaw".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&active),
            Err(LifecycleRuntimeError::MissingLeaseAuthority)
        );

        active.insert(
            "ORES_PROCESS_LIFECYCLE_LEASE_ENDPOINT".to_owned(),
            "https://locks.example.test/".to_owned(),
        );
        active.insert(
            "ORES_PROCESS_LIFECYCLE_CREDENTIAL_NAME".to_owned(),
            "ores-locks-api-token".to_owned(),
        );
        assert!(matches!(
            LifecycleAgentConfig::from_values(&active),
            Ok(LifecycleAgentConfig {
                effects: LifecycleEffectsMode::FreezeThaw,
                effects_enabled: true,
                hibernate_enabled: false,
                lease_endpoint: Some(ref endpoint),
                credential_name: Some(ref credential),
                ..
            }) if endpoint == "https://locks.example.test"
                && credential == "ores-locks-api-token"
        ));

        active.insert(
            "ORES_PROCESS_LIFECYCLE_LEASE_ENDPOINT".to_owned(),
            "http://locks.example.test".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&active),
            Err(LifecycleRuntimeError::InvalidLeaseEndpoint)
        );
    }

    #[test]
    fn v2_rejects_legacy_mutation_switches_and_socket_authority() {
        let mut invalid = v2_values("beamscale");
        invalid.insert(
            "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED".to_owned(),
            "true".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidArguments)
        );

        let mut invalid_socket = v2_values("beamscale");
        invalid_socket.insert(
            "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET".to_owned(),
            "/run/beamscale-lifecycle/product/control.sock".to_owned(),
        );
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid_socket),
            Err(LifecycleRuntimeError::InvalidArguments)
        );
    }

    #[test]
    fn shared_or_nested_socket_parent_is_rejected() {
        assert_eq!(
            validate_socket_trust_split(
                Path::new("/run/example/control.sock"),
                Path::new("/run/example/host-control.sock")
            ),
            Err(LifecycleRuntimeError::InvalidPath)
        );
        assert_eq!(
            validate_socket_trust_split(
                Path::new("/run/example/product/control.sock"),
                Path::new("/run/example/product/host/control.sock")
            ),
            Err(LifecycleRuntimeError::InvalidPath)
        );
    }

    #[test]
    fn lifecycle_identity_rejects_path_separators() {
        let invalid = values("scintilla-run")
            .into_iter()
            .map(|(key, value)| {
                if key == "ORES_PROCESS_LIFECYCLE_CLUSTER" {
                    return (key, "prod/escape".to_owned());
                }
                return (key, value);
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidIdentity)
        );
    }

    #[test]
    fn cgroup_root_cannot_expand_to_host_root() {
        let invalid = values("beamscale")
            .into_iter()
            .map(|(key, value)| {
                if key == "ORES_PROCESS_LIFECYCLE_CGROUP_ROOT" {
                    return (key, "/sys/fs/cgroup".to_owned());
                }
                return (key, value);
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidPath)
        );
    }

    #[test]
    fn effects_flag_is_explicitly_preserved_for_v1() {
        let enabled = values("beamscale")
            .into_iter()
            .map(|(key, value)| {
                if key == "ORES_PROCESS_LIFECYCLE_EFFECTS_ENABLED" {
                    return (key, "true".to_owned());
                }
                return (key, value);
            })
            .collect::<BTreeMap<_, _>>();
        assert!(matches!(
            LifecycleAgentConfig::from_values(&enabled),
            Ok(LifecycleAgentConfig {
                config_version: LifecycleConfigVersion::V1,
                effects_enabled: true,
                ..
            })
        ));
    }
}
