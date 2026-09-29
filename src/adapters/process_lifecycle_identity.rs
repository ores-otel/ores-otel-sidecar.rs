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
