//! Linux host-agent effects for process suspension and hibernation.
//!
//! cgroup v2 freeze/thaw is the lightweight CPU-reclamation path. CRIU is an
//! optional hibernation backend that checkpoints a process tree and terminates
//! it so the process RSS is actually released. Product code decides when an
//! idle workload is eligible; this adapter only performs explicit effects.

#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const CGROUP_FREEZE_FILE: &str = "cgroup.freeze";
const CGROUP_EVENTS_FILE: &str = "cgroup.events";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CgroupV2Status {
    pub populated: bool,
    pub frozen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CgroupV2Controller {
    cgroup_dir: PathBuf,
}

impl CgroupV2Controller {
    #[must_use]
    pub fn new(cgroup_dir: impl Into<PathBuf>) -> Self {
        return Self {
            cgroup_dir: cgroup_dir.into(),
        };
    }

    /// Request a hierarchical freeze. Completion is asynchronous in the kernel;
    /// call `wait_until_frozen` or inspect `status()` before persisting Frozen.
    pub fn request_freeze(&self) -> Result<(), LinuxLifecycleError> {
        return fs::write(self.cgroup_dir.join(CGROUP_FREEZE_FILE), b"1\n")
            .map_err(LinuxLifecycleError::Io);
    }

    /// Request thaw of the managed cgroup hierarchy.
    pub fn request_thaw(&self) -> Result<(), LinuxLifecycleError> {
        return fs::write(self.cgroup_dir.join(CGROUP_FREEZE_FILE), b"0\n")
            .map_err(LinuxLifecycleError::Io);
    }

    pub fn status(&self) -> Result<CgroupV2Status, LinuxLifecycleError> {
        let input = fs::read_to_string(self.cgroup_dir.join(CGROUP_EVENTS_FILE))
            .map_err(LinuxLifecycleError::Io)?;
        let populated = parse_event_flag(&input, "populated")?;
        let frozen = parse_event_flag(&input, "frozen")?;

        return Ok(CgroupV2Status { populated, frozen });
    }

    pub fn wait_until_frozen(
        &self,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), LinuxLifecycleError> {
        if poll_interval.is_zero() {
            return Err(LinuxLifecycleError::InvalidPollInterval);
        }

        let deadline = Instant::now() + timeout;
        loop {
            if self.status()?.frozen {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(LinuxLifecycleError::FreezeTimeout);
            }

            thread::sleep(poll_interval);
        }
    }
}

#[derive(Debug)]
pub enum LinuxLifecycleError {
    Io(io::Error),
    MissingCgroupEvent { key: &'static str },
    InvalidCgroupEvent { key: &'static str },
    InvalidPollInterval,
    FreezeTimeout,
}

impl Display for LinuxLifecycleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => {
                return write!(formatter, "linux lifecycle I/O failed: {error}");
            }
            Self::MissingCgroupEvent { key } => {
                return write!(formatter, "cgroup.events is missing {key}");
            }
            Self::InvalidCgroupEvent { key } => {
                return write!(formatter, "cgroup.events has invalid {key}");
            }
            Self::InvalidPollInterval => {
                return formatter.write_str("cgroup freeze poll interval must be positive");
            }
            Self::FreezeTimeout => {
                return formatter.write_str("cgroup did not reach frozen state before timeout");
            }
        }
    }
}

impl std::error::Error for LinuxLifecycleError {}

fn parse_event_flag(input: &str, key: &'static str) -> Result<bool, LinuxLifecycleError> {
    let value = input.lines().find_map(|line| {
        let (candidate, value) = line.split_once(' ')?;
        if candidate == key {
            return Some(value);
        }

        return None;
    });

    match value {
        Some("0") => {
            return Ok(false);
        }
        Some("1") => {
            return Ok(true);
        }
        Some(_value) => {
            return Err(LinuxLifecycleError::InvalidCgroupEvent { key });
        }
        None => {
            return Err(LinuxLifecycleError::MissingCgroupEvent { key });
        }
    }
}

/// Controlled CRIU CLI adapter. No shell is involved; every value is passed as
/// a distinct argv entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CriuController {
    binary: PathBuf,
}

impl Default for CriuController {
    fn default() -> Self {
        return Self::new("criu");
    }
}

impl CriuController {
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        return Self {
            binary: binary.into(),
        };
    }

    /// Checkpoint a process tree. CRIU's normal dump mode terminates the dumped
    /// tasks after a successful checkpoint because `--leave-running` is not used.
    pub fn checkpoint_and_terminate(
        &self,
        root_pid: u32,
        images_dir: &Path,
    ) -> Result<(), CriuLifecycleError> {
        fs::create_dir_all(images_dir).map_err(CriuLifecycleError::Io)?;
        let status = Command::new(&self.binary)
            .arg("dump")
            .arg("--tree")
            .arg(root_pid.to_string())
            .arg("--images-dir")
            .arg(images_dir)
            .arg("--log-file")
            .arg("dump.log")
            .status()
            .map_err(CriuLifecycleError::Io)?;

        if !status.success() {
            return Err(CriuLifecycleError::CommandFailed {
                operation: CriuOperation::Dump,
                exit_code: status.code(),
            });
        }

        return Ok(());
    }

    /// Restore the checkpoint detached from the CRIU process and return the
    /// restored root PID written by CRIU.
    pub fn restore(
        &self,
        images_dir: &Path,
        pid_file: &Path,
    ) -> Result<u32, CriuLifecycleError> {
        let status = Command::new(&self.binary)
            .arg("restore")
            .arg("--images-dir")
            .arg(images_dir)
            .arg("--restore-detached")
            .arg("--pidfile")
            .arg(pid_file)
            .arg("--log-file")
            .arg("restore.log")
            .status()
            .map_err(CriuLifecycleError::Io)?;

        if !status.success() {
            return Err(CriuLifecycleError::CommandFailed {
                operation: CriuOperation::Restore,
                exit_code: status.code(),
            });
        }

        let pid_text = fs::read_to_string(pid_file).map_err(CriuLifecycleError::Io)?;
        let pid = pid_text
            .trim()
            .parse::<u32>()
            .map_err(|_error| CriuLifecycleError::InvalidPidFile)?;

        return Ok(pid);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CriuOperation {
    Dump,
    Restore,
}

#[derive(Debug)]
pub enum CriuLifecycleError {
    Io(io::Error),
    CommandFailed {
        operation: CriuOperation,
        exit_code: Option<i32>,
    },
    InvalidPidFile,
}

impl Display for CriuLifecycleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => {
                return write!(formatter, "CRIU I/O failed: {error}");
            }
            Self::CommandFailed {
                operation,
                exit_code,
            } => {
                return write!(
                    formatter,
                    "CRIU {operation:?} failed with exit code {exit_code:?}"
                );
            }
            Self::InvalidPidFile => {
                return formatter.write_str("CRIU restore pidfile is invalid");
            }
        }
    }
}

impl std::error::Error for CriuLifecycleError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cgroup_v2_status() {
        let input = "populated 1\nfrozen 0\n";

        assert_eq!(parse_event_flag(input, "populated"), Ok(true));
        assert_eq!(parse_event_flag(input, "frozen"), Ok(false));
    }

    #[test]
    fn rejects_missing_cgroup_event() {
        let input = "populated 1\n";
        let result = parse_event_flag(input, "frozen");

        assert!(matches!(
            result,
            Err(LinuxLifecycleError::MissingCgroupEvent { key: "frozen" })
        ));
    }
}
