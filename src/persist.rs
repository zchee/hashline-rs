// Copyright 2026 The hashline-rs Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//! Atomic same-directory persistence for versioned edits.
//!
//! Writes go to a unique temporary file beside the destination, then
//! `rename` replaces the target. Immediately before rename the destination
//! identity/stamp is re-checked against the pre-edit stamp when one is known.
//! Durability (fsync of the temp file and parent directory) is optional and
//! measured separately from the CPU apply path.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crate::snapshot::FileStamp;

/// Serialize same-path writes inside one process.
fn path_lock(path: &Path) -> std::sync::MutexGuard<'static, ()> {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<PathBuf, &'static Mutex<()>>>> =
        OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = path.to_path_buf();
    let mut map = locks.lock().expect("path lock map poisoned");
    let entry = map
        .entry(key)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))));
    // Re-lock the per-path mutex without holding the map lock across I/O.
    // Drop map guard by cloning the static reference first.
    let mutex: &'static Mutex<()> = entry;
    drop(map);
    mutex.lock().expect("path mutex poisoned")
}

/// Errors from atomic persistence.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// Underlying I/O failure.
    #[error("failed to {operation} {path}: {source}")]
    Io {
        /// Operation name.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// OS error.
        #[source]
        source: io::Error,
    },
    /// Destination changed after the pre-edit snapshot was taken.
    #[error("destination changed before atomic rename: {path}")]
    DestinationChanged {
        /// Destination path.
        path: PathBuf,
    },
    /// Destination already exists for an exclusive create.
    #[error("destination already exists: {path}")]
    DestinationExists {
        /// Destination path.
        path: PathBuf,
    },
}

fn io_err(operation: &'static str, path: &Path, source: io::Error) -> PersistError {
    PersistError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// Write `bytes` to `path` via temp file + atomic rename.
///
/// When `expected` is `Some`, the destination's current [`FileStamp`] must
/// still match before rename; a mismatch leaves the destination untouched.
///
/// # Errors
///
/// Returns I/O errors or [`PersistError::DestinationChanged`].
pub fn atomic_write(
    path: &Path,
    bytes: &[u8],
    expected: Option<FileStamp>,
) -> Result<(), PersistError> {
    let _guard = path_lock(path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_err("create parent of", path, source))?;
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = write_unique_temp(parent, path, bytes)?;

    // Re-stat destination immediately before rename.
    if let Some(expected) = expected {
        match fs::metadata(path) {
            Ok(metadata) => {
                let current = FileStamp::from_metadata_public(&metadata)
                    .map_err(|source| io_err("stat destination", path, source))?;
                if current != expected {
                    let _ = fs::remove_file(&temp_path);
                    return Err(PersistError::DestinationChanged {
                        path: path.to_path_buf(),
                    });
                }
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                // Creating a new file: only valid when expected was for a missing path,
                // which we model as no stamp on detached/new-file writes.
                let _ = fs::remove_file(&temp_path);
                return Err(PersistError::DestinationChanged {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                let _ = fs::remove_file(&temp_path);
                return Err(io_err("stat destination", path, source));
            }
        }
    }

    fs::rename(&temp_path, path).map_err(|source| {
        let _ = fs::remove_file(&temp_path);
        io_err("rename into", path, source)
    })?;

    fsync_parent(parent);
    Ok(())
}

/// Monotone per-process discriminator for temp-file names.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Build a temp-file name no concurrent writer can collide with.
///
/// The process id separates processes, the counter separates threads inside
/// one process, and the timestamp separates process-id reuse across reboots —
/// so a same-nanosecond race can no longer surface as a spurious
/// `create_new` failure.
fn unique_temp_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let discriminator = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        ".hashline-{}-{discriminator}-{nanos}.tmp",
        std::process::id()
    )
}

/// Write `bytes` to a unique temporary file beside the destination.
///
/// Returns the temp path on success; the temp file is removed on failure.
fn write_unique_temp(parent: &Path, path: &Path, bytes: &[u8]) -> Result<PathBuf, PersistError> {
    let temp_path = parent.join(unique_temp_name());

    let write_temp = || -> Result<(), PersistError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| io_err("create temp for", path, source))?;
        file.write_all(bytes)
            .map_err(|source| io_err("write temp for", path, source))?;
        file.sync_all()
            .map_err(|source| io_err("fsync temp for", path, source))?;
        Ok(())
    };

    if let Err(error) = write_temp() {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(temp_path)
}

/// Best-effort parent-directory fsync for durability of the entry itself.
#[cfg(unix)]
fn fsync_parent(parent: &Path) {
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

/// Parent fsync is a Unix-only durability refinement.
#[cfg(not(unix))]
fn fsync_parent(_parent: &Path) {}

/// Write `bytes` to a brand-new `path` via temp file + atomic hard link.
///
/// The link into place fails when the destination already exists, so two
/// concurrent creates of one path have exactly one winner even across
/// processes. Missing parent directories are created first.
///
/// # Errors
///
/// Returns DestinationExists when `path` already exists, or an I/O error.
pub fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    let _guard = path_lock(path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_err("create parent of", path, source))?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = write_unique_temp(parent, path, bytes)?;

    let linked = fs::hard_link(&temp_path, path);
    let _ = fs::remove_file(&temp_path);
    match linked {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PersistError::DestinationExists {
                path: path.to_path_buf(),
            });
        }
        Err(source) => return Err(io_err("link into", path, source)),
    }

    fsync_parent(parent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;

    #[test]
    fn atomic_write_creates_and_replaces() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("f.txt");
        atomic_write(&path, b"hello\n", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello\n");
        let snap = Snapshot::load(&path).unwrap();
        atomic_write(&path, b"world\n", snap.stamp()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"world\n");
    }

    #[test]
    fn unique_temp_names_never_collide_in_process() {
        let prefix = format!(".hashline-{}-", std::process::id());
        let names: Vec<String> = (0..64).map(|_| unique_temp_name()).collect();
        let mut deduplicated = names.clone();
        deduplicated.sort();
        deduplicated.dedup();
        assert_eq!(deduplicated.len(), names.len(), "{names:?}");
        for name in &names {
            assert!(name.starts_with(&prefix), "{name}");
            assert!(name.ends_with(".tmp"), "{name}");
        }
    }

    #[test]
    fn atomic_create_is_exclusive_and_creates_parents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("fresh.txt");
        atomic_create(&path, b"first\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first\n");

        let err = atomic_create(&path, b"second\n").unwrap_err();
        assert!(matches!(err, PersistError::DestinationExists { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"first\n");

        // The losing temp file must not linger beside the destination.
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".hashline-"))
        );
    }

    #[test]
    fn atomic_write_detects_external_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"a\n").unwrap();
        let snap = Snapshot::load(&path).unwrap();
        // External mutation.
        fs::write(&path, b"b\n").unwrap();
        let err = atomic_write(&path, b"c\n", snap.stamp()).unwrap_err();
        assert!(matches!(err, PersistError::DestinationChanged { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"b\n");
    }
}
