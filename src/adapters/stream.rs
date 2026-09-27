//! Small stateful file-stream helpers for outward adapters.

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub(super) fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, std::io::Error> {
    let file = File::open(path)?;
    let mut limited = file.take(max_bytes + 1);
    let mut bytes = Vec::new();
    limited.read_to_end(&mut bytes)?;
    return Ok(bytes);
}

pub(super) fn write_synced_new(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut file = create_private_new_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    return Ok(());
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
