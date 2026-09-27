//! Crash-safe local durable lifecycle record storage.
//!
//! The distributed lifecycle lease remains the cross-host writer serializer.
//! This adapter adds the host-local durability boundary required before any
//! process effect: records are validated, compared against the caller's expected
//! revision/fence, written to a synced temporary file, and atomically renamed.
//! An interrupted write therefore leaves either the previous canonical record or
//! the complete replacement. Orphaned adapter-owned temporary files are safe to
//! discard on restart because they were never published as the canonical record.

#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::process_lifecycle_agent::LifecycleRecordStore;
use crate::process_lifecycle_record::{
    validate_record_update, LifecycleRecord, LifecycleRecordError,
};

const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_WORKLOAD_ID_BYTES: usize = 96;
const RECORDS_DIRECTORY: &str = "records";
const TEMP_PREFIX: &str = ".ores-lifecycle-";
const TEMP_SUFFIX: &str = ".tmp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileLifecycleRecordStoreError {
    InvalidRoot,
    InvalidRecordsDirectory,
    InvalidWorkloadId,
    InvalidRecord,
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
        &mut self,
        record: &LifecycleRecord,
    ) -> Result<(), FileLifecycleRecordStoreError> {
        validate_record(record)?;
        let canonical = self.record_path(&record.workload_id)?;
        if path_exists(&canonical)? {
            return Err(FileLifecycleRecordStoreError::AlreadyInitialized);
        }

        let bytes = serialize_record(record)?;
        let temporary = self.write_temporary(record, &bytes)?;
        let publish = fs::hard_link(&temporary, &canonical);
        if let Err(error) = publish {
            let _cleanup = fs::remove_file(&temporary);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(FileLifecycleRecordStoreError::AlreadyInitialized);
            }
            return Err(io_error(error));
        }
        fs::remove_file(&temporary).map_err(io_error)?;
        sync_directory(&self.records_root)?;
        let published = self.read_record(&record.workload_id)?;
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
            Err(error) => return Err(io_error(error)),
        };
        if !metadata.file_type().is_file() {
            return Err(FileLifecycleRecordStoreError::InvalidRecord);
        }
        if metadata.len() > MAX_RECORD_BYTES {
            return Err(FileLifecycleRecordStoreError::RecordTooLarge);
        }

        let file = File::open(&path).map_err(io_error)?;
        let mut limited = file.take(MAX_RECORD_BYTES + 1);
        let mut bytes = Vec::new();
        limited.read_to_end(&mut bytes).map_err(io_error)?;
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
        &mut self,
        expected: &LifecycleRecord,
        next: &LifecycleRecord,
    ) -> Result<(), FileLifecycleRecordStoreError> {
        validate_record(expected)?;
        validate_record(next)?;
        validate_record_update(expected, next)
            .map_err(|_error| FileLifecycleRecordStoreError::InvalidRecord)?;

        let current = self.read_record(&expected.workload_id)?;
        if current != *expected {
            return Err(FileLifecycleRecordStoreError::UnexpectedCurrent);
        }

        let bytes = serialize_record(next)?;
        let temporary = self.write_temporary(next, &bytes)?;
        let canonical = self.record_path(&next.workload_id)?;
        if let Err(error) = fs::rename(&temporary, &canonical) {
            let _cleanup = fs::remove_file(&temporary);
            return Err(io_error(error));
        }
        sync_directory(&self.records_root)?;

        let published = self.read_record(&next.workload_id)?;
        if published != *next {
            return Err(FileLifecycleRecordStoreError::InvalidRecord);
        }
        return Ok(());
    }

    fn record_path(&self, workload_id: &str) -> Result<PathBuf, FileLifecycleRecordStoreError> {
        let key = workload_file_key(workload_id)?;
        return Ok(self.records_root.join(format!("{key}.json")));
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
        let mut file = create_private_new_file(&path).map_err(io_error)?;
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _cleanup = fs::remove_file(&path);
            return Err(io_error(error));
        }
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
    if bytes.is_empty() || bytes.len() > MAX_WORKLOAD_ID_BYTES || workload_id.chars().any(char::is_control)
    {
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
        Err(error) => return Err(io_error(error)),
    }

    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(error)),
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

fn sync_directory(path: &Path) -> Result<(), FileLifecycleRecordStoreError> {
    return File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error);
}

fn io_error(error: std::io::Error) -> FileLifecycleRecordStoreError {
    return FileLifecycleRecordStoreError::Io(error.kind());
}

#[cfg(unix)]
fn create_private_new_file(path: &Path) -> Result<File, std::io::Error> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    return options.open(path);
}

#[cfg(not(unix))]
fn create_private_new_file(path: &Path) -> Result<File, std::io::Error> {
    return OpenOptions::new().write(true).create_new(true).open(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_lifecycle_record::{
        PersistedLifecycleState, PersistedSuspendStrategy,
    };

    fn record(revision: u64, fence: u64) -> LifecycleRecord {
        return LifecycleRecord {
            workload_id: "tenant-42-shard-3".to_owned(),
            assigned_node: "node-a".to_owned(),
            placement_epoch: 7,
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
        let mut store = FileLifecycleRecordStore::open(&root)?;
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
        let mut store = FileLifecycleRecordStore::open(&root)?;
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
    fn restart_discards_unpublished_adapter_temporary_file(
    ) -> Result<(), FileLifecycleRecordStoreError> {
        let root = temporary_root("recovery")?;
        let mut store = FileLifecycleRecordStore::open(&root)?;
        let initial = record(1, 5);
        store.initialize(&initial)?;
        let orphan = store.records_root.join(format!("{TEMP_PREFIX}orphan{TEMP_SUFFIX}"));
        let mut file = create_private_new_file(&orphan).map_err(io_error)?;
        file.write_all(b"partial").map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        drop(file);
        drop(store);

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
        let mut store = FileLifecycleRecordStore::open(&root)?;
        let mut invalid = record(1, 5);
        invalid.workload_id = "../escape".to_owned();
        store.initialize(&invalid)?;

        let records_root = root.join(RECORDS_DIRECTORY);
        let escaped = root.join("escape.json");
        assert!(!escaped.exists());
        assert_eq!(store.load_record("../escape")?, invalid);
        assert!(records_root.is_dir());
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
