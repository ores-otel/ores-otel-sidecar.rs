//! Linux process/cgroup identity attestation for lifecycle effects.
//!
//! A logical workload id, PID, or cgroup path by itself is not mutation
//! authority. Before a trusted lifecycle controller freezes or thaws a target,
//! it must prove that the host-observed runtime incarnation still names the
//! same kernel process and the same cgroup-v2 membership. This adapter performs
//! that proof without `unsafe` or tenant-provided path selection.
//!
//! Attestation intentionally reads process start time and cgroup membership
//! twice around the managed `cgroup.procs` membership check. This detects PID
//! reuse and cgroup migration during the observation window. Callers must still
//! place this check immediately adjacent to the guarded kernel effect and repeat
//! it after the effect before publishing a terminal lifecycle state.

#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};

const DEFAULT_PROC_ROOT: &str = "/proc";
const DEFAULT_CGROUP_MOUNT: &str = "/sys/fs/cgroup";
const PROC_STAT_STARTTIME_INDEX_AFTER_COMM: usize = 19;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedLinuxProcessIdentity {
    pub pid: u32,
    pub process_start_ticks: u64,
    pub managed_cgroup: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedLinuxIsolationEnvelope {
    pub process: ExpectedLinuxProcessIdentity,
    pub pid_namespace_inode: u64,
    pub user_namespace_inode: u64,
    pub mount_namespace_inode: u64,
    pub network_namespace_inode: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestedLinuxIsolationEnvelope {
    pub process: ExpectedLinuxProcessIdentity,
    pub pid_namespace_inode: u64,
    pub user_namespace_inode: u64,
    pub mount_namespace_inode: u64,
    pub network_namespace_inode: u64,
    pub no_new_privs: bool,
    pub capability_sets_empty: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxProcessIdentityAttestor {
    proc_root: PathBuf,
    cgroup_mount: PathBuf,
}

impl Default for LinuxProcessIdentityAttestor {
    fn default() -> Self {
        return Self::new(DEFAULT_PROC_ROOT, DEFAULT_CGROUP_MOUNT);
    }
}

impl LinuxProcessIdentityAttestor {
    #[must_use]
    pub fn new(
        proc_root: impl Into<PathBuf>,
        cgroup_mount: impl Into<PathBuf>,
    ) -> Self {
        return Self {
            proc_root: proc_root.into(),
            cgroup_mount: cgroup_mount.into(),
        };
    }

    /// Re-attest one exact Linux process incarnation and managed cgroup.
    ///
    /// The returned value is the caller-provided identity only after all kernel
    /// evidence agrees. Returning a clone makes it explicit that downstream
    /// effect construction must use the attested identity rather than a second,
    /// independently selected PID or path.
    pub fn attest(
        &self,
        expected: &ExpectedLinuxProcessIdentity,
    ) -> Result<ExpectedLinuxProcessIdentity, LinuxProcessIdentityError> {
        self.validate_expected(expected)?;
        self.validate_process_directory(expected.pid)?;
        self.validate_managed_cgroup_directory(&expected.managed_cgroup)?;

        let first_start = self.read_process_start_ticks(expected.pid)?;
        require_start_ticks(expected.process_start_ticks, first_start)?;
        let first_cgroup = self.read_process_cgroup(expected.pid)?;
        let expected_cgroup = self.expected_proc_cgroup_path(&expected.managed_cgroup)?;
        require_cgroup(&expected_cgroup, &first_cgroup)?;

        self.require_cgroup_membership(&expected.managed_cgroup, expected.pid)?;

        let second_start = self.read_process_start_ticks(expected.pid)?;
        let second_cgroup = self.read_process_cgroup(expected.pid)?;
        require_start_ticks(expected.process_start_ticks, second_start)?;
        require_cgroup(&expected_cgroup, &second_cgroup)?;
        if first_start != second_start || first_cgroup != second_cgroup {
            return Err(LinuxProcessIdentityError::IdentityChangedDuringAttestation);
        }

        return Ok(expected.clone());
    }

    /// Re-attest the exact process/cgroup identity plus the Linux isolation
    /// envelope that must remain true before hostile-process lifecycle effects
    /// or post-wake routing admission can become authoritative.
    pub fn attest_isolation(
        &self,
        expected: &ExpectedLinuxIsolationEnvelope,
    ) -> Result<AttestedLinuxIsolationEnvelope, LinuxProcessIdentityError> {
        let process = self.attest(&expected.process)?;
        let pid = expected.process.pid;

        let pid_namespace_inode = self.read_namespace_inode(pid, "pid")?;
        let user_namespace_inode = self.read_namespace_inode(pid, "user")?;
        let mount_namespace_inode = self.read_namespace_inode(pid, "mnt")?;
        let network_namespace_inode = self.read_namespace_inode(pid, "net")?;

        require_namespace("pid", expected.pid_namespace_inode, pid_namespace_inode)?;
        require_namespace("user", expected.user_namespace_inode, user_namespace_inode)?;
        require_namespace("mnt", expected.mount_namespace_inode, mount_namespace_inode)?;
        require_namespace("net", expected.network_namespace_inode, network_namespace_inode)?;

        let status = fs::read_to_string(self.proc_root.join(pid.to_string()).join("status"))
            .map_err(|_error| LinuxProcessIdentityError::ProcessUnavailable)?;
        let security = parse_proc_status_security(&status)?;

        // Close the observation window the same way process/cgroup identity is
        // closed: re-read start/cgroup and namespace identities after status.
        let final_process = self.attest(&expected.process)?;
        if process != final_process {
            return Err(LinuxProcessIdentityError::IdentityChangedDuringAttestation);
        }
        for (kind, wanted, observed) in [
            ("pid", expected.pid_namespace_inode, self.read_namespace_inode(pid, "pid")?),
            ("user", expected.user_namespace_inode, self.read_namespace_inode(pid, "user")?),
            ("mnt", expected.mount_namespace_inode, self.read_namespace_inode(pid, "mnt")?),
            ("net", expected.network_namespace_inode, self.read_namespace_inode(pid, "net")?),
        ] {
            require_namespace(kind, wanted, observed)?;
        }

        if !security.no_new_privs {
            return Err(LinuxProcessIdentityError::NoNewPrivsDisabled);
        }
        if !security.capability_sets_empty {
            return Err(LinuxProcessIdentityError::CapabilitiesNotEmpty);
        }

        return Ok(AttestedLinuxIsolationEnvelope {
            process,
            pid_namespace_inode,
            user_namespace_inode,
            mount_namespace_inode,
            network_namespace_inode,
            no_new_privs: true,
            capability_sets_empty: true,
        });
    }

    fn read_namespace_inode(
        &self,
        pid: u32,
        namespace: &'static str,
    ) -> Result<u64, LinuxProcessIdentityError> {
        let target = fs::read_link(
            self.proc_root
                .join(pid.to_string())
                .join("ns")
                .join(namespace),
        )
        .map_err(|_error| LinuxProcessIdentityError::ProcessUnavailable)?;
        return parse_namespace_inode(&target, namespace);
    }

    fn validate_expected(
        &self,
        expected: &ExpectedLinuxProcessIdentity,
    ) -> Result<(), LinuxProcessIdentityError> {
        if expected.pid == 0 || expected.process_start_ticks == 0 {
            return Err(LinuxProcessIdentityError::InvalidExpectedIdentity);
        }
        validate_absolute_root(&self.proc_root)?;
        validate_absolute_root(&self.cgroup_mount)?;
        validate_managed_cgroup_path(&expected.managed_cgroup, &self.cgroup_mount)?;
        return Ok(());
    }

    fn validate_process_directory(&self, pid: u32) -> Result<(), LinuxProcessIdentityError> {
        let path = self.proc_root.join(pid.to_string());
        let metadata = fs::symlink_metadata(path)
            .map_err(|_error| LinuxProcessIdentityError::ProcessUnavailable)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(LinuxProcessIdentityError::InvalidProcessDirectory);
        }
        return Ok(());
    }

    fn validate_managed_cgroup_directory(
        &self,
        path: &Path,
    ) -> Result<(), LinuxProcessIdentityError> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_error| LinuxProcessIdentityError::ManagedCgroupUnavailable)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(LinuxProcessIdentityError::InvalidManagedCgroupDirectory);
        }
        return Ok(());
    }

    fn read_process_start_ticks(&self, pid: u32) -> Result<u64, LinuxProcessIdentityError> {
        let input = fs::read_to_string(self.proc_root.join(pid.to_string()).join("stat"))
            .map_err(|_error| LinuxProcessIdentityError::ProcessUnavailable)?;
        return parse_proc_stat_start_ticks(&input, pid);
    }

    fn read_process_cgroup(&self, pid: u32) -> Result<PathBuf, LinuxProcessIdentityError> {
        let input = fs::read_to_string(self.proc_root.join(pid.to_string()).join("cgroup"))
            .map_err(|_error| LinuxProcessIdentityError::ProcessUnavailable)?;
        return parse_proc_cgroup_v2_path(&input);
    }

    fn expected_proc_cgroup_path(
        &self,
        managed_cgroup: &Path,
    ) -> Result<PathBuf, LinuxProcessIdentityError> {
        let relative = managed_cgroup
            .strip_prefix(&self.cgroup_mount)
            .map_err(|_error| LinuxProcessIdentityError::InvalidExpectedIdentity)?;
        return Ok(Path::new("/").join(relative));
    }

    fn require_cgroup_membership(
        &self,
        managed_cgroup: &Path,
        expected_pid: u32,
    ) -> Result<(), LinuxProcessIdentityError> {
        let input = fs::read_to_string(managed_cgroup.join("cgroup.procs"))
            .map_err(|_error| LinuxProcessIdentityError::ManagedCgroupUnavailable)?;
        let contains_expected = input.lines().try_fold(false, |found, line| {
            if line.is_empty() {
                return Ok(found);
            }
            let pid = line
                .parse::<u32>()
                .map_err(|_error| LinuxProcessIdentityError::InvalidCgroupProcs)?;
            return Ok(found || pid == expected_pid);
        })?;
        if !contains_expected {
            return Err(LinuxProcessIdentityError::ProcessNotInManagedCgroup);
        }
        return Ok(());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinuxProcessIdentityError {
    InvalidExpectedIdentity,
    ProcessUnavailable,
    InvalidProcessDirectory,
    InvalidProcStat,
    ProcessStartMismatch,
    InvalidProcCgroup,
    ProcessCgroupMismatch,
    ManagedCgroupUnavailable,
    InvalidManagedCgroupDirectory,
    InvalidCgroupProcs,
    ProcessNotInManagedCgroup,
    IdentityChangedDuringAttestation,
    InvalidNamespaceIdentity,
    NamespaceMismatch,
    InvalidProcStatus,
    NoNewPrivsDisabled,
    CapabilitiesNotEmpty,
}

impl Display for LinuxProcessIdentityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        return formatter.write_str(match self {
            Self::InvalidExpectedIdentity => "invalid expected Linux process lifecycle identity",
            Self::ProcessUnavailable => "expected Linux process identity is unavailable",
            Self::InvalidProcessDirectory => "expected Linux process directory is invalid",
            Self::InvalidProcStat => "Linux process stat identity is invalid",
            Self::ProcessStartMismatch => "Linux process start identity does not match",
            Self::InvalidProcCgroup => "Linux process cgroup identity is invalid",
            Self::ProcessCgroupMismatch => "Linux process cgroup identity does not match",
            Self::ManagedCgroupUnavailable => "managed lifecycle cgroup is unavailable",
            Self::InvalidManagedCgroupDirectory => "managed lifecycle cgroup directory is invalid",
            Self::InvalidCgroupProcs => "managed lifecycle cgroup.procs is invalid",
            Self::ProcessNotInManagedCgroup => "expected process is not in the managed lifecycle cgroup",
            Self::IdentityChangedDuringAttestation => {
                "Linux process identity changed during lifecycle attestation"
            }
            Self::InvalidNamespaceIdentity => "Linux namespace identity is invalid",
            Self::NamespaceMismatch => "Linux namespace identity does not match",
            Self::InvalidProcStatus => "Linux process security status is invalid",
            Self::NoNewPrivsDisabled => "Linux process does not have NoNewPrivs enabled",
            Self::CapabilitiesNotEmpty => "Linux process capability sets are not empty",
        });
    }
}

impl std::error::Error for LinuxProcessIdentityError {}

fn validate_absolute_root(path: &Path) -> Result<(), LinuxProcessIdentityError> {
    if !path.is_absolute() || has_traversal(path) {
        return Err(LinuxProcessIdentityError::InvalidExpectedIdentity);
    }
    return Ok(());
}

fn validate_managed_cgroup_path(
    path: &Path,
    cgroup_mount: &Path,
) -> Result<(), LinuxProcessIdentityError> {
    if !path.is_absolute()
        || path == cgroup_mount
        || !path.starts_with(cgroup_mount)
        || has_traversal(path)
    {
        return Err(LinuxProcessIdentityError::InvalidExpectedIdentity);
    }
    return Ok(());
}

fn has_traversal(path: &Path) -> bool {
    return path
        .components()
        .any(|component| component == Component::ParentDir || component == Component::CurDir);
}

fn parse_proc_stat_start_ticks(
    input: &str,
    expected_pid: u32,
) -> Result<u64, LinuxProcessIdentityError> {
    let open = input.find('(').ok_or(LinuxProcessIdentityError::InvalidProcStat)?;
    let close = input.rfind(')').ok_or(LinuxProcessIdentityError::InvalidProcStat)?;
    if close <= open {
        return Err(LinuxProcessIdentityError::InvalidProcStat);
    }
    let pid = input[..open]
        .trim()
        .parse::<u32>()
        .map_err(|_error| LinuxProcessIdentityError::InvalidProcStat)?;
    if pid != expected_pid {
        return Err(LinuxProcessIdentityError::InvalidProcStat);
    }
    let start_ticks = input[close + 1..]
        .split_whitespace()
        .nth(PROC_STAT_STARTTIME_INDEX_AFTER_COMM)
        .ok_or(LinuxProcessIdentityError::InvalidProcStat)?
        .parse::<u64>()
        .map_err(|_error| LinuxProcessIdentityError::InvalidProcStat)?;
    if start_ticks == 0 {
        return Err(LinuxProcessIdentityError::InvalidProcStat);
    }
    return Ok(start_ticks);
}

fn parse_proc_cgroup_v2_path(input: &str) -> Result<PathBuf, LinuxProcessIdentityError> {
    let (first, count) = input
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .fold((None, 0_usize), |(first, count), value| {
            return (first.or(Some(value)), count.saturating_add(1));
        });
    if count != 1 {
        return Err(LinuxProcessIdentityError::InvalidProcCgroup);
    }
    let value = first.ok_or(LinuxProcessIdentityError::InvalidProcCgroup)?;
    if value.is_empty() {
        return Err(LinuxProcessIdentityError::InvalidProcCgroup);
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() || has_traversal(&path) {
        return Err(LinuxProcessIdentityError::InvalidProcCgroup);
    }
    return Ok(path);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcStatusSecurity {
    no_new_privs: bool,
    capability_sets_empty: bool,
}

fn parse_namespace_inode(
    target: &Path,
    expected_kind: &str,
) -> Result<u64, LinuxProcessIdentityError> {
    let text = target
        .to_str()
        .ok_or(LinuxProcessIdentityError::InvalidNamespaceIdentity)?;
    let prefix = format!("{expected_kind}:[");
    let inode_text = text
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or(LinuxProcessIdentityError::InvalidNamespaceIdentity)?;
    let inode = inode_text
        .parse::<u64>()
        .map_err(|_error| LinuxProcessIdentityError::InvalidNamespaceIdentity)?;
    if inode == 0 {
        return Err(LinuxProcessIdentityError::InvalidNamespaceIdentity);
    }
    return Ok(inode);
}

fn require_namespace(
    _kind: &str,
    expected: u64,
    observed: u64,
) -> Result<(), LinuxProcessIdentityError> {
    if expected == 0 {
        return Err(LinuxProcessIdentityError::InvalidExpectedIdentity);
    }
    if expected != observed {
        return Err(LinuxProcessIdentityError::NamespaceMismatch);
    }
    return Ok(());
}

fn parse_proc_status_security(input: &str) -> Result<ProcStatusSecurity, LinuxProcessIdentityError> {
    let no_new_privs = parse_unique_status_value(input, "NoNewPrivs")?;
    let no_new_privs = match no_new_privs {
        "1" => true,
        "0" => false,
        _ => return Err(LinuxProcessIdentityError::InvalidProcStatus),
    };

    let mut capability_sets_empty = true;
    for field in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        let value = parse_unique_status_value(input, field)?;
        if value.len() != 16
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(LinuxProcessIdentityError::InvalidProcStatus);
        }
        let parsed = u64::from_str_radix(value, 16)
            .map_err(|_error| LinuxProcessIdentityError::InvalidProcStatus)?;
        capability_sets_empty &= parsed == 0;
    }

    return Ok(ProcStatusSecurity {
        no_new_privs,
        capability_sets_empty,
    });
}

fn parse_unique_status_value<'a>(
    input: &'a str,
    field: &str,
) -> Result<&'a str, LinuxProcessIdentityError> {
    let prefix = format!("{field}:");
    let mut values = input.lines().filter_map(|line| {
        let rest = line.strip_prefix(&prefix)?;
        return Some(rest.trim());
    });
    let value = values
        .next()
        .ok_or(LinuxProcessIdentityError::InvalidProcStatus)?;
    if value.is_empty() || values.next().is_some() {
        return Err(LinuxProcessIdentityError::InvalidProcStatus);
    }
    return Ok(value);
}

fn require_start_ticks(expected: u64, observed: u64) -> Result<(), LinuxProcessIdentityError> {
    if expected != observed {
        return Err(LinuxProcessIdentityError::ProcessStartMismatch);
    }
    return Ok(());
}

fn require_cgroup(expected: &Path, observed: &Path) -> Result<(), LinuxProcessIdentityError> {
    if expected != observed {
        return Err(LinuxProcessIdentityError::ProcessCgroupMismatch);
    }
    return Ok(());
}

#[cfg(test)]
#[path = "tests/process_lifecycle_identity.rs"]
mod tests;
