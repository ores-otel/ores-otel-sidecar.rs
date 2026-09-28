use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{LifecycleAgentConfig, LifecycleRuntimeError};

const MAX_HOST_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_WORKLOADS: usize = 1024;
const HOST_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IDENTITY_BYTES: usize = 96;
const POLICY_DIGEST_BYTES: usize = 71;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TrustedWorkloadSnapshot {
    pub workload_id: String,
    pub runtime_epoch: u64,
    pub placement_epoch: u64,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub managed_cgroup: PathBuf,
    pub isolation_policy_digest: String,
    pub queue_depth: u64,
    pub in_flight: u64,
    pub idle_for_ms: u64,
    pub routable: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TrustedHostSnapshot {
    pub version: u32,
    pub workloads: Vec<TrustedWorkloadSnapshot>,
}

#[derive(Clone, Debug)]
pub struct TrustedHostControlClient {
    socket_path: PathBuf,
    cgroup_root: PathBuf,
}

impl TrustedHostControlClient {
    #[must_use]
    pub fn new(config: &LifecycleAgentConfig) -> Self {
        return Self {
            socket_path: config.host_control_socket.clone(),
            cgroup_root: config.cgroup_root.clone(),
        };
    }

    pub fn snapshot(&self) -> Result<TrustedHostSnapshot, LifecycleRuntimeError> {
        let response = self.request(b"v1 snapshot\n")?;
        let snapshot = serde_json::from_str::<TrustedHostSnapshot>(&response)
            .map_err(|_error| LifecycleRuntimeError::InvalidHostControlResponse)?;
        validate_snapshot(&snapshot, &self.cgroup_root)?;
        return Ok(snapshot);
    }

    pub fn admit_after_wake(
        &self,
        workload: &TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<(), LifecycleRuntimeError> {
        validate_workload(workload, &self.cgroup_root)?;
        if fencing_token == 0 {
            return Err(LifecycleRuntimeError::InvalidHostControlRequest);
        }
        let request = format!(
            "v1 admit {} {} {} {}\n",
            workload.workload_id,
            workload.runtime_epoch,
            workload.placement_epoch,
            fencing_token
        );
        let response = self.request(request.as_bytes())?;
        if response == "ok routable" {
            return Ok(());
        }
        if response == "error stale_identity" {
            return Err(LifecycleRuntimeError::HostIdentityMismatch);
        }
        if response == "error demand_changed" {
            return Err(LifecycleRuntimeError::HostDemandChanged);
        }
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }

    fn request(&self, request: &[u8]) -> Result<String, LifecycleRuntimeError> {
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|_error| LifecycleRuntimeError::HostControlUnavailable)?;
        configure_timeouts(&stream)?;
        return write_request(stream, request);
    }
}

fn configure_timeouts(stream: &UnixStream) -> Result<(), LifecycleRuntimeError> {
    stream
        .set_read_timeout(Some(HOST_CONTROL_TIMEOUT))
        .map_err(|_error| LifecycleRuntimeError::HostControlUnavailable)?;
    stream
        .set_write_timeout(Some(HOST_CONTROL_TIMEOUT))
        .map_err(|_error| LifecycleRuntimeError::HostControlUnavailable)?;
    return Ok(());
}

fn write_request(
    mut stream: UnixStream,
    request: &[u8],
) -> Result<String, LifecycleRuntimeError> {
    stream
        .write_all(request)
        .map_err(|_error| LifecycleRuntimeError::HostControlUnavailable)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|_error| LifecycleRuntimeError::HostControlUnavailable)?;
    return read_response(stream, [0_u8; 4096], Vec::new());
}

fn read_response(
    mut stream: UnixStream,
    mut chunk: [u8; 4096],
    mut bytes: Vec<u8>,
) -> Result<String, LifecycleRuntimeError> {
    let count = stream
        .read(&mut chunk)
        .map_err(|_error| LifecycleRuntimeError::HostControlUnavailable)?;
    if count == 0 {
        return finish_response(bytes);
    }
    bytes.extend_from_slice(&chunk[..count]);
    if bytes.len() > MAX_HOST_RESPONSE_BYTES {
        return Err(LifecycleRuntimeError::HostControlResponseTooLarge);
    }
    if bytes.contains(&b'\n') {
        return finish_response(bytes);
    }
    return read_response(stream, [0_u8; 4096], bytes);
}

fn finish_response(bytes: Vec<u8>) -> Result<String, LifecycleRuntimeError> {
    let newline = match bytes.iter().position(|byte| *byte == b'\n') {
        Some(index) => index,
        None => bytes.len(),
    };
    let line = std::str::from_utf8(&bytes[..newline])
        .map_err(|_error| LifecycleRuntimeError::InvalidHostControlResponse)?;
    let trimmed = line.trim_matches(['\r', ' ', '\t']);
    if trimmed.is_empty() {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    return Ok(trimmed.to_owned());
}

fn validate_snapshot(
    snapshot: &TrustedHostSnapshot,
    cgroup_root: &Path,
) -> Result<(), LifecycleRuntimeError> {
    if snapshot.version != 1 || snapshot.workloads.len() > MAX_WORKLOADS {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    for workload in &snapshot.workloads {
        validate_workload(workload, cgroup_root)?;
    }
    return Ok(());
}

fn validate_workload(
    workload: &TrustedWorkloadSnapshot,
    cgroup_root: &Path,
) -> Result<(), LifecycleRuntimeError> {
    validate_identity(&workload.workload_id)?;
    if workload.runtime_epoch == 0
        || workload.placement_epoch == 0
        || workload.pid == 0
        || workload.process_start_ticks == 0
    {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    if workload.queue_depth > i64::MAX as u64 || workload.in_flight > i64::MAX as u64 {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    validate_policy_digest(&workload.isolation_policy_digest)?;
    validate_managed_cgroup(&workload.managed_cgroup, cgroup_root)?;
    return Ok(());
}

fn validate_identity(value: &str) -> Result<(), LifecycleRuntimeError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTITY_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.' || byte == b':'
        });
    if !valid {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    return Ok(());
}

fn validate_policy_digest(value: &str) -> Result<(), LifecycleRuntimeError> {
    if value.len() != POLICY_DIGEST_BYTES || !value.starts_with("sha256:") {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    let digest = &value[7..];
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    return Ok(());
}

fn validate_managed_cgroup(
    path: &Path,
    cgroup_root: &Path,
) -> Result<(), LifecycleRuntimeError> {
    if !path.is_absolute() || path == cgroup_root || !path.starts_with(cgroup_root) {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir || component == Component::CurDir)
    {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload() -> TrustedWorkloadSnapshot {
        return TrustedWorkloadSnapshot {
            workload_id: "tenant-42-shard-3".to_owned(),
            runtime_epoch: 9,
            placement_epoch: 7,
            pid: 4242,
            process_start_ticks: 987_654,
            managed_cgroup: PathBuf::from(
                "/sys/fs/cgroup/beamscale-workloads.slice/tenant-42-shard-3.scope",
            ),
            isolation_policy_digest: format!("sha256:{}", "a".repeat(64)),
            queue_depth: 0,
            in_flight: 0,
            idle_for_ms: 30_000,
            routable: true,
        };
    }

    #[test]
    fn validates_trusted_snapshot_identity_tuple() {
        let snapshot = TrustedHostSnapshot {
            version: 1,
            workloads: vec![workload()],
        };
        let root = Path::new("/sys/fs/cgroup/beamscale-workloads.slice");
        assert_eq!(validate_snapshot(&snapshot, root), Ok(()));
    }

    #[test]
    fn rejects_root_or_foreign_cgroup_targets() {
        let root = Path::new("/sys/fs/cgroup/beamscale-workloads.slice");
        let root_target = TrustedWorkloadSnapshot {
            managed_cgroup: root.to_path_buf(),
            ..workload()
        };
        let foreign_target = TrustedWorkloadSnapshot {
            managed_cgroup: PathBuf::from("/sys/fs/cgroup/system.slice/ssh.service"),
            ..workload()
        };
        assert_eq!(
            validate_workload(&root_target, root),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
        assert_eq!(
            validate_workload(&foreign_target, root),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn rejects_zero_epochs_or_process_identity() {
        let root = Path::new("/sys/fs/cgroup/beamscale-workloads.slice");
        let invalid = TrustedWorkloadSnapshot {
            runtime_epoch: 0,
            ..workload()
        };
        assert_eq!(
            validate_workload(&invalid, root),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn rejects_non_sha256_policy_digest() {
        let root = Path::new("/sys/fs/cgroup/beamscale-workloads.slice");
        let invalid = TrustedWorkloadSnapshot {
            isolation_policy_digest: "sha256:not-a-real-digest".to_owned(),
            ..workload()
        };
        assert_eq!(
            validate_workload(&invalid, root),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn parses_one_line_snapshot_json() {
        let encoded = serde_json::to_string(&TrustedHostSnapshot {
            version: 1,
            workloads: vec![workload()],
        });
        assert!(encoded.is_ok());
    }
}
