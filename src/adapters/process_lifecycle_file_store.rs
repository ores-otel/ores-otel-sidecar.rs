//! Crash-safe local durable lifecycle record storage.
//!
//! The distributed lifecycle lease remains the cross-host writer serializer.
//! This adapter adds the host-local durability boundary required before any
//! process effect: records are validated, compared against the caller's expected
//! revision/fence, written to a synced temporary file, and atomically renamed.
//! A stable per-workload lock file serializes the compare+replace critical
//! section across local controller processes; the lock inode is never replaced
//! when the canonical JSON record is renamed.

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::stream::{read_bounded, write_synced_new};
use crate::process_lifecycle_agent::LifecycleRecordStore;
use crate::process_lifecycle_record::{
    LifecycleRecord, LifecycleRecordError, validate_record_update,
};

const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_WORKLOAD_ID_BYTES: usize = 96;
const RECORDS_DIRECTORY: &str = "records";
const TEMP_PREFIX: &str = ".ores-lifecycle-";
const TEMP_SUFFIX: &str = ".tmp";
const LOCK_SUFFIX: &str = ".lock";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileLifecycleRecordStoreError {
    InvalidRoot,
    InvalidRecordsDirectory,
    InvalidWorkloadId,
    InvalidRecord,
    InvalidLockFile,
    RecordTooLarge,
    MissingRecord,
    AlreadyInitialized,
    UnexpectedCurrent,
    InvalidTemporaryEntry,
    ClockUnavailable,
    Json,
    Io(std::io::ErrorKind),
}

impl std::fmt::Display for FileLifecycleRecordStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        return formatter.write_str(match self {
            Self::InvalidRoot => "lifecycle record root must be an existing real directory",
            Self::InvalidRecordsDirectory => {
                "lifecycle records directory must be a real directory"
            }
            Self::InvalidWorkloadId => "lifecycle workload id is not file-store safe",
            Self::InvalidRecord => "lifecycle record violates durable invariants",
            Self::InvalidLockFile => "lifecycle CAS lock path must be a real regular file",
            Self::RecordTooLarge => "lifecycle record exceeds durable byte limit",
            Self::MissingRecord => "lifecycle record does not exist",
            Self::AlreadyInitialized => "lifecycle record is already initialized",
            Self::UnexpectedCurrent => "lifecycle compare-and-set expected record is stale",
            Self::InvalidTemporaryEntry => "lifecycle temporary record entry is invalid",
            Self::ClockUnavailable => "system clock unavailable for temporary record identity",
            Self::Json => "lifecycle record JSON is invalid",
            Self::Io(_kind) => "lifecycle record filesystem operation failed",
        });
    }
}

impl std::error::Error for FileLifecycleRecordStoreError {}

#[derive(Clone, Debug)]
pub struct FileLifecycleRecordStore {
    records_root: PathBuf,
}

impl FileLifecycleRecordStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, FileLifecycleRecordStoreError> {
        let root = root.as_ref();
        validate_real_directory(root).map_err(|_error| FileLifecycleRecordStoreError::InvalidRoot)?;
        let records_root = root.join(RECORDS_DIRECTORY);
        ensure_records_directory(&records_root)?;
        recover_temporary_files(&records_root)?;
        return Ok(Self { records_root });
    }

    pub fn initialize(
        &self,
        record: &LifecycleRecord,
    ) -> Result<(), FileLifecycleRecordStoreError> {
        validate_record(record)?;
        let _lock = self.acquire_workload_lock(&record.workload_id)?;
        let canonical = self.record_path(&record.workload_id)?;
        if path_exists(&canonical)? {
            return Err(FileLifecycleRecordStoreError::AlreadyInitialized);
        }

        let bytes = serialize_record(record)?;
        let temporary = self.write_temporary(record, &bytes)?;
        if let Err(error) = fs::hard_link(&temporary, &canonical) {
            cleanup_temporary(&temporary)?;
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(FileLifecycleRecordStoreError::AlreadyInitialized);
            }
            return Err(io_error(error));
        }
        cleanup_temporary(&temporary)?;
        sync_directory(&self.records_root)?;
        let published = self.load_record(&record.workload_id)?;
        if published != *record {
            return Err(FileLifecycleRecordStoreError::InvalidRecord);
        }
        return Ok(());
    }

    pub fn load_record(
        &self,
        workload_id: &str,
    ) -> Result<LifecycleRecord, FileLifecycleRecordStoreError> {
        let path = self.record_path(workload_id)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FileLifecycleRecordStoreError::MissingRecord);
            }
            Err(error) => {
                return Err(io_error(error));
            }
        };
        if !metadata.file_type().is_file() {
            return Err(FileLifecycleRecordStoreError::InvalidRecord);
        }
        if metadata.len() > MAX_RECORD_BYTES {
            return Err(FileLifecycleRecordStoreError::RecordTooLarge);
        }

        let bytes = read_bounded(&path, MAX_RECORD_BYTES).map_err(io_error)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(FileLifecycleRecordStoreError::RecordTooLarge);
        }
        let record = serde_json::from_slice::<LifecycleRecord>(&bytes)
            .map_err(|_error| FileLifecycleRecordStoreError::Json)?;
        validate_record(&record)?;
        if record.workload_id != workload_id {
            return Err(FileLifecycleRecordStoreError::InvalidRecord);
        }
        return Ok(record);
    }

    pub fn replace_record(
        &self,
        expected: &LifecycleRecord,
        next: &LifecycleRecord,
    ) -> Result<(), FileLifecycleRecordStoreError> {
        validate_record(expected)?;
        validate_record(next)?;
        validate_record_update(expected, next)
            .map_err(|_error| FileLifecycleRecordStoreError::InvalidRecord)?;

        // The stable lock file, not the replaceable JSON inode, serializes this
        // entire local CAS. Re-read current only after the lock is held.
        let _lock = self.acquire_workload_lock(&expected.workload_id)?;
        let current = self.load_record(&expected.workload_id)?;
        if current != *expected {
            return Err(FileLifecycleRecordStoreError::UnexpectedCurrent);
        }

        let bytes = serialize_record(next)?;
        let temporary = self.write_temporary(next, &bytes)?;
        let canonical = self.record_path(&next.workload_id)?;
        if let Err(error) = fs::rename(&temporary, &canonical) {
            cleanup_temporary(&temporary)?;
            return Err(io_error(error));
        }
        sync_directory(&self.records_root)?;

        let published = self.load_record(&next.workload_id)?;
        if published != *next {
            return Err(FileLifecycleRecordStoreError::InvalidRecord);
        }
        return Ok(());
    }

    fn record_path(&self, workload_id: &str) -> Result<PathBuf, FileLifecycleRecordStoreError> {
        let key = workload_file_key(workload_id)?;
        return Ok(self.records_root.join(format!("{key}.json")));
    }

    fn lock_path(&self, workload_id: &str) -> Result<PathBuf, FileLifecycleRecordStoreError> {
        let key = workload_file_key(workload_id)?;
        return Ok(self.records_root.join(format!("{key}{LOCK_SUFFIX}")));
    }

    fn acquire_workload_lock(
        &self,
        workload_id: &str,
    ) -> Result<File, FileLifecycleRecordStoreError> {
        let path = self.lock_path(workload_id)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                    return Err(FileLifecycleRecordStoreError::InvalidLockFile);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io_error(error));
            }
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_error)?;
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(FileLifecycleRecordStoreError::InvalidLockFile);
        }
        file.lock().map_err(io_error)?;
        return Ok(file);
    }

    fn write_temporary(
        &self,
        record: &LifecycleRecord,
        bytes: &[u8],
    ) -> Result<PathBuf, FileLifecycleRecordStoreError> {
        let key = workload_file_key(&record.workload_id)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_error| FileLifecycleRecordStoreError::ClockUnavailable)?
            .as_nanos();
        let path = self.records_root.join(format!(
            "{TEMP_PREFIX}{key}-{}-{}-{nonce}{TEMP_SUFFIX}",
            record.revision,
            std::process::id()
        ));
        write_synced_new(&path, bytes).map_err(io_error)?;
        return Ok(path);
    }
}

impl LifecycleRecordStore for FileLifecycleRecordStore {
    fn load(&mut self, workload_id: &str) -> Result<LifecycleRecord, String> {
        return self
            .load_record(workload_id)
            .map_err(|error| error.to_string());
    }

    fn replace(
        &mut self,
        expected: &LifecycleRecord,
        next: &LifecycleRecord,
    ) -> Result<(), String> {
        return self
            .replace_record(expected, next)
            .map_err(|error| error.to_string());
    }
}

fn validate_record(record: &LifecycleRecord) -> Result<(), FileLifecycleRecordStoreError> {
    workload_file_key(&record.workload_id)?;
    return record
        .validate()
        .map_err(|_error: LifecycleRecordError| FileLifecycleRecordStoreError::InvalidRecord);
}

fn workload_file_key(workload_id: &str) -> Result<String, FileLifecycleRecordStoreError> {
    let bytes = workload_id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_WORKLOAD_ID_BYTES
        && bytes.iter().copied().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
        });
    if !valid {
        return Err(FileLifecycleRecordStoreError::InvalidWorkloadId);
    }
    return Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect());
}

fn serialize_record(record: &LifecycleRecord) -> Result<Vec<u8>, FileLifecycleRecordStoreError> {
    let bytes = serde_json::to_vec(record).map_err(|_error| FileLifecycleRecordStoreError::Json)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(FileLifecycleRecordStoreError::RecordTooLarge);
    }
    return Ok(bytes);
}

fn ensure_records_directory(path: &Path) -> Result<(), FileLifecycleRecordStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_metadata) => {
            return validate_real_directory(path)
                .map_err(|_error| FileLifecycleRecordStoreError::InvalidRecordsDirectory);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(io_error(error));
        }
    }

    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(io_error(error));
        }
    }
    return validate_real_directory(path)
        .map_err(|_error| FileLifecycleRecordStoreError::InvalidRecordsDirectory);
}

fn recover_temporary_files(path: &Path) -> Result<(), FileLifecycleRecordStoreError> {
    for entry in fs::read_dir(path).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !name.starts_with(TEMP_PREFIX) || !name.ends_with(TEMP_SUFFIX) {
            continue;
        }
        let file_type = entry.file_type().map_err(io_error)?;
        if !file_type.is_file() {
            return Err(FileLifecycleRecordStoreError::InvalidTemporaryEntry);
        }
        fs::remove_file(entry.path()).map_err(io_error)?;
    }
    return Ok(());
}

fn validate_real_directory(path: &Path) -> Result<(), std::io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected real directory",
        ));
    }
    return Ok(());
}

fn path_exists(path: &Path) -> Result<bool, FileLifecycleRecordStoreError> {
    return match fs::symlink_metadata(path) {
        Ok(_metadata) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    };
}

fn cleanup_temporary(path: &Path) -> Result<(), FileLifecycleRecordStoreError> {
    return fs::remove_file(path).map_err(io_error);
}

fn sync_directory(path: &Path) -> Result<(), FileLifecycleRecordStoreError> {
    return File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error);
}

fn io_error(error: std::io::Error) -> FileLifecycleRecordStoreError {
    return FileLifecycleRecordStoreError::Io(error.kind());
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::process_lifecycle_record::{
        PersistedLifecycleState, PersistedSuspendStrategy,
    };

    fn record(revision: u64, fence: u64) -> LifecycleRecord {
        return LifecycleRecord {
            workload_id: "tenant-42-shard-3".to_owned(),
            assigned_node: "node-a".to_owned(),
            placement_epoch: 7,
            runtime_epoch: 13,
            fencing_token: fence,
            revision,
            state: PersistedLifecycleState::Running,
            strategy: PersistedSuspendStrategy::Freeze,
            checkpoint: None,
        };
    }

    fn temporary_root(name: &str) -> Result<PathBuf, FileLifecycleRecordStoreError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_error| FileLifecycleRecordStoreError::ClockUnavailable)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ores-lifecycle-store-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(io_error)?;
        return Ok(path);
    }

    #[test]
    fn initialize_load_and_replace_round_trip() -> Result<(), FileLifecycleRecordStoreError> {
        let root = temporary_root("round-trip")?;
        let store = FileLifecycleRecordStore::open(&root)?;
        let initial = record(1, 5);
        store.initialize(&initial)?;
        assert_eq!(store.load_record(&initial.workload_id)?, initial);

        let next = record(2, 6);
        store.replace_record(&initial, &next)?;
        assert_eq!(store.load_record(&next.workload_id)?, next);
        fs::remove_dir_all(root).map_err(io_error)?;
        return Ok(());
    }

    #[test]
    fn stale_compare_and_set_is_rejected() -> Result<(), FileLifecycleRecordStoreError> {
        let root = temporary_root("stale-cas")?;
        let store = FileLifecycleRecordStore::open(&root)?;
        let initial = record(1, 5);
        let current = record(2, 6);
        let stale_next = record(2, 7);
        store.initialize(&initial)?;
        store.replace_record(&initial, &current)?;

        assert_eq!(
            store.replace_record(&initial, &stale_next),
            Err(FileLifecycleRecordStoreError::UnexpectedCurrent)
        );
        assert_eq!(store.load_record(&current.workload_id)?, current);
        fs::remove_dir_all(root).map_err(io_error)?;
        return Ok(());
    }

    #[test]
    fn stable_workload_lock_serializes_local_cas_writers(
    ) -> Result<(), FileLifecycleRecordStoreError> {
        let root = temporary_root("local-lock")?;
        let store = FileLifecycleRecordStore::open(&root)?;
        let initial = record(1, 5);
        store.initialize(&initial)?;

        let held = store.acquire_workload_lock(&initial.workload_id)?;
        let writer_store = store.clone();
        let expected = initial.clone();
        let next = record(2, 6);
        let (started_tx, started_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let _send_result = started_tx.send(());
            return writer_store.replace_record(&expected, &next);
        });

        assert!(started_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        thread::sleep(Duration::from_millis(25));
        assert!(!writer.is_finished());
        drop(held);

        let writer_result = match writer.join() {
            Ok(result) => result,
            Err(_panic_payload) => {
                return Err(FileLifecycleRecordStoreError::Io(
                    std::io::ErrorKind::Other,
                ));
            }
        };
        assert_eq!(writer_result, Ok(()));
        assert_eq!(store.load_record(&initial.workload_id)?, record(2, 6));
        fs::remove_dir_all(root).map_err(io_error)?;
        return Ok(());
    }

    #[test]
    fn restart_discards_unpublished_adapter_temporary_file(
    ) -> Result<(), FileLifecycleRecordStoreError> {
        let root = temporary_root("recovery")?;
        let store = FileLifecycleRecordStore::open(&root)?;
        let initial = record(1, 5);
        store.initialize(&initial)?;
        let orphan = store.records_root.join(format!("{TEMP_PREFIX}orphan{TEMP_SUFFIX}"));
        write_synced_new(&orphan, b"partial").map_err(io_error)?;

        let recovered = FileLifecycleRecordStore::open(&root)?;
        assert!(!orphan.exists());
        assert_eq!(recovered.load_record(&initial.workload_id)?, initial);
        fs::remove_dir_all(root).map_err(io_error)?;
        return Ok(());
    }

    #[test]
    fn workload_identity_cannot_escape_record_root(
    ) -> Result<(), FileLifecycleRecordStoreError> {
        let root = temporary_root("identity")?;
        let store = FileLifecycleRecordStore::open(&root)?;
        let invalid = LifecycleRecord {
            workload_id: "../escape".to_owned(),
            ..record(1, 5)
        };

        assert_eq!(
            store.initialize(&invalid),
            Err(FileLifecycleRecordStoreError::InvalidWorkloadId)
        );
        assert!(!root.join("escape.json").exists());
        fs::remove_dir_all(root).map_err(io_error)?;
        return Ok(());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_lock_file_fails_closed() -> Result<(), FileLifecycleRecordStoreError> {
        use std::os::unix::fs::symlink;

        let root = temporary_root("lock-symlink")?;
        let outside = root.join("outside");
        fs::write(&outside, b"outside").map_err(io_error)?;
        let store = FileLifecycleRecordStore::open(&root)?;
        let lock_path = store.lock_path("tenant-42-shard-3")?;
        symlink(&outside, &lock_path).map_err(io_error)?;

        assert_eq!(
            store.initialize(&record(1, 5)),
            Err(FileLifecycleRecordStoreError::InvalidLockFile)
        );
        fs::remove_file(lock_path).map_err(io_error)?;
        fs::remove_dir_all(root).map_err(io_error)?;
        return Ok(());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_records_directory_fails_closed() -> Result<(), FileLifecycleRecordStoreError> {
        use std::os::unix::fs::symlink;

        let root = temporary_root("symlink")?;
        let outside = temporary_root("outside")?;
        symlink(&outside, root.join(RECORDS_DIRECTORY)).map_err(io_error)?;
        assert!(FileLifecycleRecordStore::open(&root).is_err());
        fs::remove_file(root.join(RECORDS_DIRECTORY)).map_err(io_error)?;
        fs::remove_dir_all(root).map_err(io_error)?;
        fs::remove_dir_all(outside).map_err(io_error)?;
        return Ok(());
    }
}
