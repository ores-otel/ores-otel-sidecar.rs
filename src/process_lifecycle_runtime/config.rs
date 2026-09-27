use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::LifecycleRuntimeError;

const MIN_RECONCILE_SECONDS: u64 = 1;
const MAX_RECONCILE_SECONDS: u64 = 3600;

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
            return "/run/beamscale-lifecycle/control.sock";
        }
        return "/run/scintilla-lifecycle/control.sock";
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

impl LifecycleAgentConfig {
    pub fn from_values(
        values: &BTreeMap<String, String>,
    ) -> Result<Self, LifecycleRuntimeError> {
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

fn parse_product(value: String) -> Result<LifecycleProduct, LifecycleRuntimeError> {
    if value == "beamscale" {
        return Ok(LifecycleProduct::BeamScale);
    }
    if value == "scintilla-run" {
        return Ok(LifecycleProduct::ScintillaRun);
    }
    return Err(LifecycleRuntimeError::InvalidProduct);
}

fn required_value(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<String, LifecycleRuntimeError> {
    let value = values
        .get(key)
        .map(|candidate| candidate.trim())
        .filter(|candidate| !candidate.is_empty())
        .ok_or(LifecycleRuntimeError::InvalidArguments)?;
    return Ok(value.to_owned());
}

fn validate_identity_segment(value: &str) -> Result<(), LifecycleRuntimeError> {
    let valid = !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.' || byte == b':'
        });
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
        assert!(LifecycleAgentConfig::from_values(&values("beamscale")).is_ok());
        let invalid = values("beamscale")
            .into_iter()
            .map(|(key, value)| {
                if key == "ORES_PROCESS_LIFECYCLE_PRODUCT_SOCKET" {
                    return (key, "/run/scintilla-lifecycle/control.sock".to_owned());
                }
                return (key, value);
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            LifecycleAgentConfig::from_values(&invalid),
            Err(LifecycleRuntimeError::InvalidProductSocket)
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
    fn effects_flag_is_explicitly_preserved() {
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
                effects_enabled: true,
                ..
            })
        ));
    }
}
