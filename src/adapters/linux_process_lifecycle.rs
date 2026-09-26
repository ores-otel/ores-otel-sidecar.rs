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
use std::process::{Command, Stdio};
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
        return self.wait_for_frozen_state(true, timeout, poll_interval);
    }

    pub fn wait_until_thawed(
        &self,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), LinuxLifecycleError> {
        return self.wait_for_frozen_state(false, timeout, poll_interval);
    }

    fn wait_for_frozen_state(
        &self,
        expected_frozen: bool,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), LinuxLifecycleError> {
        if poll_interval.is_zero() {
            return Err(LinuxLifecycleError::InvalidPollInterval);
        }

        let deadline = Instant::now() + timeout;
        loop {
            if self.status()?.frozen == expected_frozen {
                return Ok(());
            }

            if Instant::now() >= deadline {
                return Err(if expected_frozen {
                    LinuxLifecycleError::FreezeTimeout
                } else {
                    LinuxLifecycleError::ThawTimeout
                });
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
    ThawTimeout,
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
            Self::ThawTimeout => {
                return formatter.write_str("cgroup did not reach thawed state before timeout");
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
        self.validate_binary()?;
        if root_pid == 0 {
            return Err(CriuLifecycleError::InvalidRootPid);
        }
        prepare_private_checkpoint_directory(images_dir, true)?;
        let status = Command::new(&self.binary)
            .arg("dump")
            .arg("--tree")
            .arg(root_pid.to_string())
            .arg("--images-dir")
            .arg(images_dir)
            .arg("--log-file")
            .arg("dump.log")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
        self.validate_binary()?;
        prepare_private_checkpoint_directory(images_dir, false)?;
        match fs::remove_file(pid_file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CriuLifecycleError::Io(error)),
        }

        let status = Command::new(&self.binary)
            .arg("restore")
            .arg("--images-dir")
            .arg(images_dir)
            .arg("--restore-detached")
            .arg("--pidfile")
            .arg(pid_file)
            .arg("--log-file")
            .arg("restore.log")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
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
        if pid == 0 {
            return Err(CriuLifecycleError::InvalidPidFile);
        }

        return Ok(pid);
    }

    fn validate_binary(&self) -> Result<(), CriuLifecycleError> {
        if !self.binary.is_absolute() {
            return Err(CriuLifecycleError::NonAbsoluteBinary);
        }

        let metadata = fs::symlink_metadata(&self.binary).map_err(CriuLifecycleError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(CriuLifecycleError::InvalidBinary);
        }

        return Ok(());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CriuOperation {
    Dump,
    Restore,
}

fn prepare_private_checkpoint_directory(
    path: &Path,
    create_if_missing: bool,
) -> Result<(), CriuLifecycleError> {
    use std::os::unix::fs::PermissionsExt;

    if create_if_missing {
        fs::create_dir_all(path).map_err(CriuLifecycleError::Io)?;
    }

    let metadata = fs::symlink_metadata(path).map_err(CriuLifecycleError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CriuLifecycleError::InvalidCheckpointDirectory);
    }

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(CriuLifecycleError::Io)?;
    return Ok(());
}

#[derive(Debug)]
pub enum CriuLifecycleError {
    Io(io::Error),
    CommandFailed {
        operation: CriuOperation,
        exit_code: Option<i32>,
    },
    InvalidPidFile,
    InvalidRootPid,
    NonAbsoluteBinary,
    InvalidBinary,
    InvalidCheckpointDirectory,
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
            Self::InvalidRootPid => {
                return formatter.write_str("CRIU root pid must be positive");
            }
            Self::NonAbsoluteBinary => {
                return formatter.write_str("CRIU binary path must be absolute");
            }
            Self::InvalidBinary => {
                return formatter.write_str("CRIU binary must be a regular non-symlink file");
            }
            Self::InvalidCheckpointDirectory => {
                return formatter.write_str("CRIU checkpoint directory must be a real directory");
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
        let populated = parse_event_flag(input, "populated");
        let frozen = parse_event_flag(input, "frozen");

        assert!(matches!(populated, Ok(true)));
        assert!(matches!(frozen, Ok(false)));
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

    #[test]
    fn criu_requires_absolute_binary_path() {
        let controller = CriuController::new("criu");
        let result = controller.validate_binary();

        assert!(matches!(result, Err(CriuLifecycleError::NonAbsoluteBinary)));
    }

    #[test]
    fn private_checkpoint_directory_is_mode_0700() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "ores-lifecycle-checkpoint-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        prepare_private_checkpoint_directory(&root, true).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        fs::remove_dir_all(&root).unwrap();
    }
}
