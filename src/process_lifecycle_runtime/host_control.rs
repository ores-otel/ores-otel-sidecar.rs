use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{LifecycleAgentConfig, LifecycleRuntimeError};

const HOST_PROTOCOL_VERSION: u32 = 2;
const MAX_HOST_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_HOST_REQUEST_BYTES: usize = 16 * 1024;
const MAX_WORKLOADS: usize = 1024;
const HOST_CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IDENTITY_BYTES: usize = 96;
const POLICY_DIGEST_BYTES: usize = 71;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedWorkloadSnapshot {
    pub workload_id: String,
    pub assigned_node: String,
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
#[serde(deny_unknown_fields)]
pub struct TrustedHostSnapshot {
    pub version: u32,
    pub workloads: Vec<TrustedWorkloadSnapshot>,
}

/// Exact host-owned identity that must be re-attested before routing resumes.
/// The lifecycle client sends back the identity it previously observed rather
/// than asking the host to admit a logical workload name alone.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedHostAdmission {
    pub workload_id: String,
    pub assigned_node: String,
    pub runtime_epoch: u64,
    pub placement_epoch: u64,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub managed_cgroup: PathBuf,
    pub isolation_policy_digest: String,
    pub fencing_token: u64,
}

#[derive(Clone, Debug)]
pub struct TrustedHostControlClient {
    socket_path: PathBuf,
    cgroup_root: PathBuf,
    expected_node: String,
}

impl TrustedHostControlClient {
    #[must_use]
    pub fn new(config: &LifecycleAgentConfig) -> Self {
        return Self {
            socket_path: config.host_control_socket.clone(),
            cgroup_root: config.cgroup_root.clone(),
            expected_node: config.node.clone(),
        };
    }

    pub fn snapshot(&self) -> Result<TrustedHostSnapshot, LifecycleRuntimeError> {
        let response = self.request(b"v2 snapshot\n")?;
        let snapshot = serde_json::from_str::<TrustedHostSnapshot>(&response)
            .map_err(|_error| LifecycleRuntimeError::InvalidHostControlResponse)?;
        validate_snapshot(&snapshot, &self.cgroup_root, &self.expected_node)?;
        return Ok(snapshot);
    }

    pub fn admit_after_wake(
        &self,
        workload: &TrustedWorkloadSnapshot,
        fencing_token: u64,
    ) -> Result<(), LifecycleRuntimeError> {
        validate_workload(workload, &self.cgroup_root, &self.expected_node)?;
        if fencing_token == 0 {
            return Err(LifecycleRuntimeError::InvalidHostControlRequest);
        }
        let admission = TrustedHostAdmission {
            workload_id: workload.workload_id.clone(),
            assigned_node: workload.assigned_node.clone(),
            runtime_epoch: workload.runtime_epoch,
            placement_epoch: workload.placement_epoch,
            pid: workload.pid,
            process_start_ticks: workload.process_start_ticks,
            managed_cgroup: workload.managed_cgroup.clone(),
            isolation_policy_digest: workload.isolation_policy_digest.clone(),
            fencing_token,
        };
        let payload = serde_json::to_string(&admission)
            .map_err(|_error| LifecycleRuntimeError::InvalidHostControlRequest)?;
        let request = format!("v2 admit {payload}\n");
        if request.len() > MAX_HOST_REQUEST_BYTES {
            return Err(LifecycleRuntimeError::InvalidHostControlRequest);
        }
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
        if request.is_empty() || request.len() > MAX_HOST_REQUEST_BYTES || !request.ends_with(b"\n") {
            return Err(LifecycleRuntimeError::InvalidHostControlRequest);
        }
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
    return read_response(stream, [0_u8; 4096], bytes);
}

fn finish_response(bytes: Vec<u8>) -> Result<String, LifecycleRuntimeError> {
    if bytes.is_empty() || bytes.last() != Some(&b'\n') {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    let line = &bytes[..bytes.len() - 1];
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    let text = std::str::from_utf8(line)
        .map_err(|_error| LifecycleRuntimeError::InvalidHostControlResponse)?;
    if text.trim() != text {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    return Ok(text.to_owned());
}

fn validate_snapshot(
    snapshot: &TrustedHostSnapshot,
    cgroup_root: &Path,
    expected_node: &str,
) -> Result<(), LifecycleRuntimeError> {
    if snapshot.version != HOST_PROTOCOL_VERSION || snapshot.workloads.len() > MAX_WORKLOADS {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }

    let mut workload_ids = BTreeSet::new();
    let mut process_identities = BTreeSet::new();
    let mut managed_cgroups = BTreeSet::new();
    for workload in &snapshot.workloads {
        validate_workload(workload, cgroup_root, expected_node)?;
        if !workload_ids.insert(workload.workload_id.clone())
            || !process_identities.insert((workload.pid, workload.process_start_ticks))
            || !managed_cgroups.insert(workload.managed_cgroup.clone())
        {
            return Err(LifecycleRuntimeError::InvalidHostControlResponse);
        }
    }
    return Ok(());
}

fn validate_workload(
    workload: &TrustedWorkloadSnapshot,
    cgroup_root: &Path,
    expected_node: &str,
) -> Result<(), LifecycleRuntimeError> {
    validate_identity(&workload.workload_id)?;
    validate_identity(&workload.assigned_node)?;
    if workload.assigned_node != expected_node {
        return Err(LifecycleRuntimeError::HostIdentityMismatch);
    }
    if workload.runtime_epoch == 0
        || workload.placement_epoch == 0
        || workload.pid == 0
        || workload.process_start_ticks == 0
    {
        return Err(LifecycleRuntimeError::InvalidHostControlResponse);
    }
    if workload.queue_depth > i64::MAX as u64
        || workload.in_flight > i64::MAX as u64
        || workload.idle_for_ms > i64::MAX as u64
    {
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
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
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
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
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

    const NODE: &str = "node-a";

    fn workload() -> TrustedWorkloadSnapshot {
        return TrustedWorkloadSnapshot {
            workload_id: "tenant-42-shard-3".to_owned(),
            assigned_node: NODE.to_owned(),
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

    fn root() -> &'static Path {
        return Path::new("/sys/fs/cgroup/beamscale-workloads.slice");
    }

    #[test]
    fn validates_trusted_snapshot_identity_tuple() {
        let snapshot = TrustedHostSnapshot {
            version: HOST_PROTOCOL_VERSION,
            workloads: vec![workload()],
        };
        assert_eq!(validate_snapshot(&snapshot, root(), NODE), Ok(()));
    }

    #[test]
    fn rejects_foreign_node_identity() {
        let snapshot = TrustedHostSnapshot {
            version: HOST_PROTOCOL_VERSION,
            workloads: vec![TrustedWorkloadSnapshot {
                assigned_node: "node-b".to_owned(),
                ..workload()
            }],
        };
        assert_eq!(
            validate_snapshot(&snapshot, root(), NODE),
            Err(LifecycleRuntimeError::HostIdentityMismatch)
        );
    }

    #[test]
    fn rejects_duplicate_logical_process_or_cgroup_identity() {
        let first = workload();
        let mut duplicate_workload = workload();
        duplicate_workload.pid += 1;
        duplicate_workload.process_start_ticks += 1;
        duplicate_workload.managed_cgroup = PathBuf::from(
            "/sys/fs/cgroup/beamscale-workloads.slice/other.scope",
        );
        let duplicate_id = TrustedHostSnapshot {
            version: HOST_PROTOCOL_VERSION,
            workloads: vec![first.clone(), duplicate_workload],
        };
        assert_eq!(
            validate_snapshot(&duplicate_id, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );

        let mut duplicate_process = first.clone();
        duplicate_process.workload_id = "tenant-43-shard-1".to_owned();
        duplicate_process.managed_cgroup = PathBuf::from(
            "/sys/fs/cgroup/beamscale-workloads.slice/tenant-43-shard-1.scope",
        );
        let duplicate_process_snapshot = TrustedHostSnapshot {
            version: HOST_PROTOCOL_VERSION,
            workloads: vec![first.clone(), duplicate_process],
        };
        assert_eq!(
            validate_snapshot(&duplicate_process_snapshot, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );

        let mut duplicate_cgroup = first.clone();
        duplicate_cgroup.workload_id = "tenant-44-shard-1".to_owned();
        duplicate_cgroup.pid += 2;
        duplicate_cgroup.process_start_ticks += 2;
        let duplicate_cgroup_snapshot = TrustedHostSnapshot {
            version: HOST_PROTOCOL_VERSION,
            workloads: vec![first, duplicate_cgroup],
        };
        assert_eq!(
            validate_snapshot(&duplicate_cgroup_snapshot, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn rejects_root_or_foreign_cgroup_targets() {
        let root_target = TrustedWorkloadSnapshot {
            managed_cgroup: root().to_path_buf(),
            ..workload()
        };
        let foreign_target = TrustedWorkloadSnapshot {
            managed_cgroup: PathBuf::from("/sys/fs/cgroup/system.slice/ssh.service"),
            ..workload()
        };
        assert_eq!(
            validate_workload(&root_target, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
        assert_eq!(
            validate_workload(&foreign_target, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn rejects_zero_epochs_or_process_identity() {
        let invalid = TrustedWorkloadSnapshot {
            runtime_epoch: 0,
            ..workload()
        };
        assert_eq!(
            validate_workload(&invalid, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn rejects_noncanonical_policy_digest() {
        let invalid = TrustedWorkloadSnapshot {
            isolation_policy_digest: format!("sha256:{}", "A".repeat(64)),
            ..workload()
        };
        assert_eq!(
            validate_workload(&invalid, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn rejects_unbounded_idle_counter() {
        let invalid = TrustedWorkloadSnapshot {
            idle_for_ms: i64::MAX as u64 + 1,
            ..workload()
        };
        assert_eq!(
            validate_workload(&invalid, root(), NODE),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn response_requires_exactly_one_newline_terminated_frame() {
        assert_eq!(finish_response(b"ok routable\n".to_vec()), Ok("ok routable".to_owned()));
        assert_eq!(
            finish_response(b"ok routable".to_vec()),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
        assert_eq!(
            finish_response(b"ok routable\nignored\n".to_vec()),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
        assert_eq!(
            finish_response(b" ok routable\n".to_vec()),
            Err(LifecycleRuntimeError::InvalidHostControlResponse)
        );
    }

    #[test]
    fn parses_one_line_snapshot_json() {
        let encoded = serde_json::to_string(&TrustedHostSnapshot {
            version: HOST_PROTOCOL_VERSION,
            workloads: vec![workload()],
        })
        .expect("serialize snapshot");
        assert_eq!(
            finish_response(format!("{encoded}\n").into_bytes()),
            Ok(encoded)
        );
    }
}
