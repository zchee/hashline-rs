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
//! Immutable, strictly validated v2 file snapshots.
//!
//! Snapshot construction owns exact UTF-8 bytes, derives one process-scoped
//! 128-bit identity, and counts logical lines without building a line index.
//! Position metadata is materialized once on first random-access use.

use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use memchr::{memchr, memchr_iter};
use thiserror::Error;
use xxhash_rust::xxh3::xxh3_128_with_seed;

use crate::protocol::{ContractError, Position, SnapshotId, validate_file_size};
use crate::util::process_random_seed;

const READ_BUFFER_BYTES: usize = 64 * 1024;
const MAX_READ_ATTEMPTS: u8 = 2;

/// Owned text that has passed the complete v2 file policy.
///
/// Construction rejects oversized input, NUL bytes, and invalid UTF-8 before
/// the text can enter a [`Snapshot`]. The inner `String` is private so cached
/// text cannot bypass these checks.
#[derive(Debug, PartialEq, Eq)]
pub struct ValidatedText(String);

impl ValidatedText {
    /// Validate and take ownership of complete file bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::FileTooLarge`], [`ContractError::NulFile`], or
    /// [`ContractError::InvalidUtf8`] when the bytes violate the v2 text
    /// contract.
    pub fn try_from_bytes(bytes: Vec<u8>) -> Result<Self, ContractError> {
        let byte_len = u64::try_from(bytes.len())
            .map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?;
        validate_file_size(byte_len)?;

        if let Some(offset) = memchr(0, &bytes) {
            let byte = u64::try_from(offset)
                .map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?;
            return Err(ContractError::NulFile { byte });
        }

        if let Err(error) = simdutf8::compat::from_utf8(&bytes) {
            let valid_up_to = u64::try_from(error.valid_up_to())
                .map_err(|_| ContractError::FileTooLarge { bytes: u64::MAX })?;
            return Err(ContractError::InvalidUtf8 { valid_up_to });
        }

        // SAFETY: `simdutf8::compat::from_utf8` validated this exact owned
        // buffer above. The bytes are not mutated between that validation and
        // the ownership transfer. `miri_validated_text_round_trip` exercises
        // this constructor and its borrowed slices under Miri.
        let text = unsafe { String::from_utf8_unchecked(bytes) };
        Ok(Self(text))
    }

    /// Borrow the validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrow the exact validated bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Return the exact byte length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return whether the text has zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<u8>> for ValidatedText {
    type Error = ContractError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::try_from_bytes(bytes)
    }
}

impl AsRef<str> for ValidatedText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    index: u64,
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity;

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileTime {
    seconds: i64,
    nanoseconds: i64,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileTime(u64);

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileTime(std::time::SystemTime);

/// Metadata captured from the same open file descriptor as snapshot bytes.
///
/// Identity, size, modification time, and (where exposed by the platform)
/// change time are compared before and after each read attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    identity: FileIdentity,
    len: u64,
    modified: FileTime,
    changed: Option<FileTime>,
}

impl FileStamp {
    /// Return the descriptor-reported byte length.
    #[must_use]
    pub const fn len(self) -> u64 {
        self.len
    }

    /// Return whether the descriptor reported an empty file.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Capture a stamp from filesystem metadata (used by atomic persist).
    pub(crate) fn from_metadata_public(metadata: &Metadata) -> io::Result<Self> {
        Self::from_metadata(metadata)
    }

    #[cfg(unix)]
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        Ok(Self {
            identity: FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            len: metadata.len(),
            modified: FileTime {
                seconds: metadata.mtime(),
                nanoseconds: metadata.mtime_nsec(),
            },
            changed: Some(FileTime {
                seconds: metadata.ctime(),
                nanoseconds: metadata.ctime_nsec(),
            }),
        })
    }

    #[cfg(windows)]
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        use std::os::windows::fs::MetadataExt as _;

        Ok(Self {
            identity: FileIdentity {
                volume: u64::from(metadata.volume_serial_number().unwrap_or_default()),
                index: metadata.file_index().unwrap_or_default(),
            },
            len: metadata.len(),
            modified: FileTime(metadata.last_write_time()),
            changed: None,
        })
    }

    #[cfg(not(any(unix, windows)))]
    fn from_metadata(metadata: &Metadata) -> io::Result<Self> {
        Ok(Self {
            identity: FileIdentity,
            len: metadata.len(),
            modified: FileTime(metadata.modified()?),
            changed: None,
        })
    }
}

/// Storage width used by materialized logical-line offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetKind {
    /// Four bytes per logical line, used through `u32::MAX` byte offsets.
    U32,
    /// Eight bytes per logical line for larger addressable files.
    U64,
}

impl OffsetKind {
    fn for_byte_len(byte_len: u64) -> Self {
        if byte_len <= u64::from(u32::MAX) {
            Self::U32
        } else {
            Self::U64
        }
    }

    const fn bytes(self) -> usize {
        match self {
            Self::U32 => mem::size_of::<u32>(),
            Self::U64 => mem::size_of::<u64>(),
        }
    }
}

/// Failure while materializing or addressing line offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OffsetError {
    /// The logical line count cannot fit the current address space.
    #[error("snapshot line count {lines} does not fit the current address space")]
    LineCountAddressSpace {
        /// Rejected logical line count.
        lines: u64,
    },
    /// Reserving the complete offset vector failed.
    #[error("could not reserve offsets for {lines} logical lines")]
    Capacity {
        /// Requested logical line count.
        lines: u64,
    },
    /// A newline-derived byte offset did not fit the selected width.
    #[error("byte offset {offset} does not fit {kind:?} line metadata")]
    OffsetWidth {
        /// Rejected address-space offset.
        offset: usize,
        /// Selected representation.
        kind: OffsetKind,
    },
    /// Counted lines and materialized offsets disagreed.
    #[error("expected {expected} line offsets but materialized {actual}")]
    LineCountMismatch {
        /// Snapshot line count.
        expected: usize,
        /// Materialized count.
        actual: usize,
    },
    /// Resident metadata byte arithmetic overflowed.
    #[error("resident line-metadata size overflowed")]
    MetadataSizeOverflow,
    /// A validated line offset could not be mapped back into the text.
    #[error("validated line offset is outside the addressable text")]
    AddressInvariant,
}

#[derive(Debug)]
enum LineOffsets {
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl LineOffsets {
    fn build(text: &str, line_count: u64, byte_len: u64) -> Result<Self, OffsetError> {
        Self::build_with_kind(text, line_count, OffsetKind::for_byte_len(byte_len))
    }

    fn build_with_kind(text: &str, line_count: u64, kind: OffsetKind) -> Result<Self, OffsetError> {
        let expected = usize::try_from(line_count)
            .map_err(|_| OffsetError::LineCountAddressSpace { lines: line_count })?;
        match kind {
            OffsetKind::U32 => {
                let mut offsets = Vec::new();
                offsets
                    .try_reserve_exact(expected)
                    .map_err(|_| OffsetError::Capacity { lines: line_count })?;
                offsets.push(0);
                for newline in memchr_iter(b'\n', text.as_bytes()) {
                    let start = newline.checked_add(1).ok_or(OffsetError::OffsetWidth {
                        offset: newline,
                        kind,
                    })?;
                    offsets.push(u32::try_from(start).map_err(|_| OffsetError::OffsetWidth {
                        offset: start,
                        kind,
                    })?);
                }
                if offsets.len() != expected {
                    return Err(OffsetError::LineCountMismatch {
                        expected,
                        actual: offsets.len(),
                    });
                }
                Ok(Self::U32(offsets))
            }
            OffsetKind::U64 => {
                let mut offsets = Vec::new();
                offsets
                    .try_reserve_exact(expected)
                    .map_err(|_| OffsetError::Capacity { lines: line_count })?;
                offsets.push(0);
                for newline in memchr_iter(b'\n', text.as_bytes()) {
                    let start = newline.checked_add(1).ok_or(OffsetError::OffsetWidth {
                        offset: newline,
                        kind,
                    })?;
                    offsets.push(u64::try_from(start).map_err(|_| OffsetError::OffsetWidth {
                        offset: start,
                        kind,
                    })?);
                }
                if offsets.len() != expected {
                    return Err(OffsetError::LineCountMismatch {
                        expected,
                        actual: offsets.len(),
                    });
                }
                Ok(Self::U64(offsets))
            }
        }
    }

    fn get(&self, index: usize) -> Option<u64> {
        match self {
            Self::U32(offsets) => offsets.get(index).copied().map(u64::from),
            Self::U64(offsets) => offsets.get(index).copied(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::U32(offsets) => offsets.len(),
            Self::U64(offsets) => offsets.len(),
        }
    }

    const fn kind(&self) -> OffsetKind {
        match self {
            Self::U32(_) => OffsetKind::U32,
            Self::U64(_) => OffsetKind::U64,
        }
    }

    fn resident_bytes(&self) -> Result<usize, OffsetError> {
        Self::metadata_bytes_for_lines(self.len(), self.kind())
    }

    const fn metadata_bytes_for_lines(
        lines: usize,
        kind: OffsetKind,
    ) -> Result<usize, OffsetError> {
        match lines.checked_mul(kind.bytes()) {
            Some(bytes) => Ok(bytes),
            None => Err(OffsetError::MetadataSizeOverflow),
        }
    }
}

/// Errors raised while constructing or addressing a snapshot.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// A file operation failed.
    #[error("failed to {operation} {path}: {source}")]
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Path associated with the operation.
        path: PathBuf,
        /// Operating-system error.
        #[source]
        source: io::Error,
    },
    /// Complete bytes violated the frozen v2 text policy.
    #[error(transparent)]
    Contract(#[from] ContractError),
    /// A file changed during both permitted read attempts.
    #[error("file changed during both snapshot read attempts: {path}")]
    ConcurrentModification {
        /// Path that could not be read consistently.
        path: PathBuf,
    },
    /// A descriptor length cannot fit the current address space.
    #[error("file length {bytes} cannot fit the current address space")]
    AddressSpace {
        /// Descriptor-reported length.
        bytes: u64,
    },
    /// Reading or owning the complete file allocation failed.
    #[error("could not reserve {bytes} bytes for snapshot text")]
    Allocation {
        /// Requested capacity.
        bytes: u64,
    },
    /// The logical line count overflowed.
    #[error("snapshot logical line count overflowed")]
    LineCountOverflow,
    /// Lazy line-offset construction failed.
    #[error(transparent)]
    Offsets(#[from] OffsetError),
}

/// Immutable validated bytes, process-scoped identity, and lazy line metadata.
#[derive(Debug)]
pub struct Snapshot {
    text: ValidatedText,
    id: SnapshotId,
    stamp: Option<FileStamp>,
    byte_len: u64,
    line_count: u64,
    offsets: OnceLock<Result<LineOffsets, OffsetError>>,
}

impl Snapshot {
    /// Construct a detached snapshot from owned complete file bytes.
    ///
    /// Detached snapshots have no filesystem stamp; [`Self::load`] attaches one
    /// after a stable descriptor read.
    ///
    /// # Errors
    ///
    /// Returns a v2 text-policy, line-count, or address-size error.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, SnapshotError> {
        let text = ValidatedText::try_from_bytes(bytes)?;
        Self::from_validated(text)
    }

    /// Construct a detached snapshot from already validated owned text.
    ///
    /// This is the only constructor accepted by the cached-text core after raw
    /// byte validation.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::LineCountOverflow`] if logical-line arithmetic
    /// cannot be represented.
    pub fn from_validated(text: ValidatedText) -> Result<Self, SnapshotError> {
        Self::from_validated_with_stamp(text, None)
    }

    /// Load one stable snapshot through one descriptor per attempt.
    ///
    /// Metadata from the open descriptor is captured before and after the
    /// complete read, then compared with the path's post-read identity. A
    /// mismatch discards every byte and retries once.
    ///
    /// # Errors
    ///
    /// Returns an I/O or v2 text-policy error, or
    /// [`SnapshotError::ConcurrentModification`] when both attempts observe a
    /// metadata change.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SnapshotError> {
        Self::load_with_observer(path.as_ref(), |_, _| {})
    }

    fn from_validated_with_stamp(
        text: ValidatedText,
        stamp: Option<FileStamp>,
    ) -> Result<Self, SnapshotError> {
        let byte_len = u64::try_from(text.len())
            .map_err(|_| SnapshotError::AddressSpace { bytes: u64::MAX })?;
        let newline_count = memchr_iter(b'\n', text.as_bytes()).count();
        let line_count_usize = newline_count
            .checked_add(1)
            .ok_or(SnapshotError::LineCountOverflow)?;
        let line_count =
            u64::try_from(line_count_usize).map_err(|_| SnapshotError::LineCountOverflow)?;
        let id = SnapshotId::from_u128(xxh3_128_with_seed(text.as_bytes(), process_random_seed()));
        Ok(Self {
            text,
            id,
            stamp,
            byte_len,
            line_count,
            offsets: OnceLock::new(),
        })
    }

    fn load_with_observer(
        path: &Path,
        mut observer: impl FnMut(u8, usize),
    ) -> Result<Self, SnapshotError> {
        for attempt in 1..=MAX_READ_ATTEMPTS {
            if let Some((text, stamp)) = Self::read_candidate(path, attempt, &mut observer)? {
                return Self::from_validated_with_stamp(text, Some(stamp));
            }
        }
        Err(SnapshotError::ConcurrentModification {
            path: path.to_path_buf(),
        })
    }

    fn read_candidate(
        path: &Path,
        attempt: u8,
        observer: &mut impl FnMut(u8, usize),
    ) -> Result<Option<(ValidatedText, FileStamp)>, SnapshotError> {
        let mut file = File::open(path).map_err(|source| Self::io_error(path, "open", source))?;
        let before_metadata = file
            .metadata()
            .map_err(|source| Self::io_error(path, "read initial metadata for", source))?;
        let before = FileStamp::from_metadata(&before_metadata)
            .map_err(|source| Self::io_error(path, "decode initial metadata for", source))?;
        validate_file_size(before.len())?;

        let expected = usize::try_from(before.len()).map_err(|_| SnapshotError::AddressSpace {
            bytes: before.len(),
        })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(expected)
            .map_err(|_| SnapshotError::Allocation {
                bytes: before.len(),
            })?;

        let mut buffer = [0_u8; READ_BUFFER_BYTES];
        loop {
            let read = match file.read(&mut buffer) {
                Ok(read) => read,
                Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => return Err(Self::io_error(path, "read", source)),
            };
            if read == 0 {
                break;
            }

            let new_len = bytes
                .len()
                .checked_add(read)
                .ok_or(SnapshotError::AddressSpace { bytes: u64::MAX })?;
            let new_len_u64 = u64::try_from(new_len)
                .map_err(|_| SnapshotError::AddressSpace { bytes: u64::MAX })?;
            validate_file_size(new_len_u64)?;
            if new_len > bytes.capacity() {
                bytes
                    .try_reserve_exact(new_len - bytes.len())
                    .map_err(|_| SnapshotError::Allocation { bytes: new_len_u64 })?;
            }
            bytes.extend_from_slice(&buffer[..read]);
            observer(attempt, bytes.len());
        }

        let after_metadata = file
            .metadata()
            .map_err(|source| Self::io_error(path, "read final metadata for", source))?;
        let after = FileStamp::from_metadata(&after_metadata)
            .map_err(|source| Self::io_error(path, "decode final metadata for", source))?;
        let path_stamp = match std::fs::metadata(path) {
            Ok(metadata) => Some(
                FileStamp::from_metadata(&metadata)
                    .map_err(|source| Self::io_error(path, "decode path metadata for", source))?,
            ),
            Err(source) if source.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(Self::io_error(path, "read path metadata for", source)),
        };
        let actual_len = u64::try_from(bytes.len())
            .map_err(|_| SnapshotError::AddressSpace { bytes: u64::MAX })?;
        if before != after || path_stamp != Some(after) || actual_len != after.len() {
            return Ok(None);
        }

        let text = ValidatedText::try_from_bytes(bytes)?;
        Ok(Some((text, after)))
    }

    fn io_error(path: &Path, operation: &'static str, source: io::Error) -> SnapshotError {
        SnapshotError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    fn offsets(&self) -> Result<&LineOffsets, SnapshotError> {
        match self
            .offsets
            .get_or_init(|| LineOffsets::build(self.text.as_str(), self.line_count, self.byte_len))
        {
            Ok(offsets) => Ok(offsets),
            Err(error) => Err(SnapshotError::Offsets(*error)),
        }
    }

    fn boundary_ordinal(&self, position: Position) -> Result<u64, SnapshotError> {
        let offsets = self.offsets()?;
        if position.line() <= self.line_count {
            let zero_based = position
                .line()
                .checked_sub(1)
                .ok_or(OffsetError::AddressInvariant)?;
            let index = usize::try_from(zero_based).map_err(|_| OffsetError::AddressInvariant)?;
            if offsets.get(index) == Some(position.byte()) {
                return Ok(zero_based);
            }
        } else if self
            .line_count
            .checked_add(1)
            .is_some_and(|terminal_line| position.line() == terminal_line)
            && position.byte() == self.byte_len
        {
            return Ok(self.line_count);
        }
        Err(ContractError::InvalidPosition { position }.into())
    }

    /// Borrow the exact validated text.
    #[must_use]
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    /// Borrow the exact validated bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    /// Return the process-scoped identity of the exact bytes.
    #[must_use]
    pub const fn id(&self) -> SnapshotId {
        self.id
    }

    /// Return the stable filesystem stamp, if loaded from a path.
    #[must_use]
    pub const fn stamp(&self) -> Option<FileStamp> {
        self.stamp
    }

    /// Return the exact byte length.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// Return the logical line count, which is always at least one.
    #[must_use]
    pub const fn line_count(&self) -> u64 {
        self.line_count
    }

    /// Return whether the line-offset table has been requested.
    #[must_use]
    pub fn offsets_materialized(&self) -> bool {
        self.offsets.get().is_some()
    }

    /// Materialize line offsets once and return their selected width.
    ///
    /// Files no larger than `u32::MAX` bytes use four bytes per logical line;
    /// larger addressable files use the checked U64 representation.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Offsets`] if allocation or offset conversion
    /// fails.
    pub fn materialize_offsets(&self) -> Result<OffsetKind, SnapshotError> {
        self.offsets().map(LineOffsets::kind)
    }

    /// Return resident per-line metadata bytes.
    ///
    /// The result is zero before lazy materialization.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Offsets`] if materialization previously failed
    /// or resident-size arithmetic overflows.
    pub fn resident_line_metadata_bytes(&self) -> Result<usize, SnapshotError> {
        match self.offsets.get() {
            None => Ok(0),
            Some(Ok(offsets)) => offsets.resident_bytes().map_err(SnapshotError::from),
            Some(Err(error)) => Err(SnapshotError::Offsets(*error)),
        }
    }

    /// Return the byte start of a one-based logical line.
    ///
    /// The first call materializes the compact offset table. Later calls are
    /// O(1).
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Offsets`] if lazy materialization fails.
    pub fn line_start(&self, line: u64) -> Result<Option<u64>, SnapshotError> {
        let Some(zero_based) = line.checked_sub(1) else {
            return Ok(None);
        };
        let index = match usize::try_from(zero_based) {
            Ok(index) => index,
            Err(_) => return Ok(None),
        };
        Ok(self.offsets()?.get(index))
    }

    /// Validate an exact logical-line or terminal boundary.
    ///
    /// The first call materializes offsets. Every validation after that is
    /// O(1).
    ///
    /// # Errors
    ///
    /// Returns [`ContractError::InvalidPosition`] for a mismatched line/byte
    /// pair, or [`SnapshotError::Offsets`] if lazy materialization fails.
    pub fn validate_boundary(&self, position: Position) -> Result<(), SnapshotError> {
        self.boundary_ordinal(position).map(|_| ())
    }

    /// Borrow the exact byte range between two validated boundaries.
    ///
    /// The first call materializes offsets. Boundary checks, capacity
    /// calculation, and slicing are O(1) afterwards.
    ///
    /// # Errors
    ///
    /// Returns the frozen invalid-position or reversed-range contract error,
    /// or a lazy offset error.
    pub fn slice(&self, start: Position, end: Position) -> Result<&str, SnapshotError> {
        let start_ordinal = self.boundary_ordinal(start)?;
        let end_ordinal = self.boundary_ordinal(end)?;
        if start_ordinal > end_ordinal {
            return Err(ContractError::ReversedRange { start, end }.into());
        }

        let start_byte =
            usize::try_from(start.byte()).map_err(|_| OffsetError::AddressInvariant)?;
        let end_byte = usize::try_from(end.byte()).map_err(|_| OffsetError::AddressInvariant)?;
        self.text
            .as_str()
            .get(start_byte..end_byte)
            .ok_or(SnapshotError::Offsets(OffsetError::AddressInvariant))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::protocol::reference_line_starts;

    fn position(line: u64, byte: u64) -> Position {
        Position::new(line, byte).expect("test position has a nonzero line")
    }

    fn repeated(byte: u8, len: usize) -> Vec<u8> {
        vec![byte; len]
    }

    #[test]
    fn phase2_validated_text_rejects_invalid_utf8_and_nul() {
        let valid = ValidatedText::try_from_bytes("alpha\nβeta\n".as_bytes().to_vec())
            .expect("valid UTF-8 without NUL");
        assert_eq!(valid.as_str(), "alpha\nβeta\n");

        assert!(matches!(
            ValidatedText::try_from_bytes(b"left\0right".to_vec()),
            Err(ContractError::NulFile { byte: 4 })
        ));
        assert!(matches!(
            ValidatedText::try_from_bytes(vec![b'a', 0xff, b'b']),
            Err(ContractError::InvalidUtf8 { valid_up_to: 1 })
        ));
    }

    #[test]
    fn phase2_snapshot_id_is_process_scoped_and_content_stable() {
        let bytes = b"same exact bytes\n";
        let first = Snapshot::from_bytes(bytes.to_vec()).expect("first snapshot");
        let second = Snapshot::from_bytes(bytes.to_vec()).expect("second snapshot");
        let changed =
            Snapshot::from_bytes(b"same exact byteS\n".to_vec()).expect("changed snapshot");

        assert_eq!(first.id(), second.id());
        assert_ne!(first.id(), changed.id());
        assert_eq!(
            first.id().as_u128(),
            xxh3_128_with_seed(bytes, process_random_seed())
        );
    }

    #[test]
    fn phase2_stable_read_retries_once_without_returning_mixed_bytes() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("retry.txt");
        let initial = repeated(b'a', READ_BUFFER_BYTES * 4);
        let replacement = repeated(b'b', initial.len());
        fs::write(&path, &initial).expect("write initial file");

        let mut mutated = false;
        let snapshot = Snapshot::load_with_observer(&path, |attempt, bytes_read| {
            if attempt == 1 && bytes_read >= READ_BUFFER_BYTES && !mutated {
                thread::sleep(Duration::from_millis(2));
                fs::write(&path, &replacement).expect("replace during first read");
                mutated = true;
            }
        })
        .expect("second attempt is stable");

        assert!(mutated);
        assert_eq!(snapshot.bytes(), replacement);
        assert_ne!(snapshot.bytes(), initial);
        assert!(snapshot.stamp().is_some());
    }

    #[test]
    fn phase2_stable_read_rejects_two_mutated_attempts() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("always-changing.txt");
        let first = repeated(b'a', READ_BUFFER_BYTES * 4);
        let second = repeated(b'b', first.len());
        fs::write(&path, &first).expect("write initial file");

        let mut changed = [false; 2];
        let error = Snapshot::load_with_observer(&path, |attempt, bytes_read| {
            let index = usize::from(attempt - 1);
            if bytes_read >= READ_BUFFER_BYTES && !changed[index] {
                thread::sleep(Duration::from_millis(2));
                let replacement = if attempt == 1 { &second } else { &first };
                fs::write(&path, replacement).expect("mutate current attempt");
                changed[index] = true;
            }
        })
        .expect_err("both inconsistent attempts must fail");

        assert_eq!(changed, [true, true]);
        assert!(matches!(
            error,
            SnapshotError::ConcurrentModification { .. }
        ));
    }

    #[test]
    fn phase2_offsets_are_lazy_u32_and_u64_checked() {
        let snapshot = Snapshot::from_bytes(b"zero\none\n".to_vec()).expect("snapshot");
        assert_eq!(snapshot.line_count(), 3);
        assert!(!snapshot.offsets_materialized());
        assert_eq!(snapshot.resident_line_metadata_bytes().expect("bytes"), 0);

        assert_eq!(snapshot.line_start(2).expect("line start"), Some(5));
        assert!(snapshot.offsets_materialized());
        assert_eq!(
            snapshot.materialize_offsets().expect("materialized width"),
            OffsetKind::U32
        );
        assert_eq!(
            snapshot
                .resident_line_metadata_bytes()
                .expect("resident bytes"),
            3 * mem::size_of::<u32>()
        );

        let u64_offsets = LineOffsets::build_with_kind("zero\none\n", 3, OffsetKind::U64)
            .expect("forced U64 test representation");
        assert_eq!(u64_offsets.kind(), OffsetKind::U64);
        assert_eq!(u64_offsets.get(2), Some(9));
        assert_eq!(u64_offsets.resident_bytes().expect("resident bytes"), 24);
        assert_eq!(
            OffsetKind::for_byte_len(u64::from(u32::MAX) + 1),
            OffsetKind::U64
        );
    }

    #[test]
    fn phase2_boundaries_and_ranges_match_reference() {
        for text in ["", "alpha\n", "αβ\r\nsecond\n終", "\n\nthree"] {
            let snapshot = Snapshot::from_bytes(text.as_bytes().to_vec())
                .expect("reference-compatible snapshot");
            let starts = reference_line_starts(text);
            assert_eq!(
                snapshot.line_count(),
                u64::try_from(starts.len()).expect("test line count")
            );

            for (index, &start) in starts.iter().enumerate() {
                let line = u64::try_from(index + 1).expect("test line number");
                let current = position(line, start);
                snapshot
                    .validate_boundary(current)
                    .expect("reference line start is valid");
                let next_byte = starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(u64::try_from(text.len()).expect("test byte length"));
                let next = position(line + 1, next_byte);
                assert_eq!(
                    snapshot.slice(current, next).expect("valid line slice"),
                    &text[usize::try_from(start).expect("start")
                        ..usize::try_from(next_byte).expect("end")]
                );
            }

            let terminal = position(
                snapshot.line_count() + 1,
                u64::try_from(text.len()).expect("test byte length"),
            );
            snapshot
                .validate_boundary(terminal)
                .expect("terminal boundary is valid");
            assert!(matches!(
                snapshot.validate_boundary(position(1, 1)),
                Err(SnapshotError::Contract(
                    ContractError::InvalidPosition { .. }
                ))
            ));
        }
    }

    #[test]
    fn phase2_integer_overflow_paths_fail_closed() {
        assert_eq!(
            LineOffsets::metadata_bytes_for_lines(usize::MAX, OffsetKind::U64),
            Err(OffsetError::MetadataSizeOverflow)
        );
        assert!(matches!(
            LineOffsets::build_with_kind("", u64::MAX, OffsetKind::U32),
            Err(OffsetError::LineCountAddressSpace { .. })
                | Err(OffsetError::Capacity { .. })
                | Err(OffsetError::LineCountMismatch { .. })
        ));

        let snapshot = Snapshot::from_bytes(b"a\nb\n".to_vec()).expect("snapshot");
        assert_eq!(snapshot.line_start(0).expect("zero line"), None);
        assert_eq!(snapshot.line_start(u64::MAX).expect("huge line"), None);
        assert!(matches!(
            snapshot.slice(position(3, 4), position(2, 2)),
            Err(SnapshotError::Contract(ContractError::ReversedRange { .. }))
        ));
    }

    #[test]
    fn phase2_concurrent_read_mutation_never_returns_mixed_snapshot() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("concurrent.txt");
        let initial = repeated(b'a', READ_BUFFER_BYTES * 8);
        let replacement = repeated(b'b', initial.len());
        fs::write(&path, &initial).expect("write initial file");

        let (mutate_tx, mutate_rx) = mpsc::sync_channel::<()>(0);
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(0);
        let writer_path = path.clone();
        let writer_replacement = replacement.clone();
        let writer = thread::spawn(move || {
            mutate_rx.recv().expect("receive mutation request");
            thread::sleep(Duration::from_millis(2));
            fs::write(writer_path, writer_replacement).expect("concurrent replacement");
            done_tx.send(()).expect("signal replacement completion");
        });

        let mut requested = false;
        let snapshot = Snapshot::load_with_observer(&path, |attempt, bytes_read| {
            if attempt == 1 && bytes_read >= READ_BUFFER_BYTES && !requested {
                mutate_tx.send(()).expect("request concurrent mutation");
                done_rx.recv().expect("wait for concurrent mutation");
                requested = true;
            }
        })
        .expect("retry returns one complete version");
        writer.join().expect("writer thread");

        assert!(requested);
        assert_eq!(snapshot.bytes(), replacement);
        assert!(!snapshot.bytes().contains(&b'a'));
    }

    #[test]
    fn miri_validated_text_round_trip() {
        let snapshot =
            Snapshot::from_bytes("zero\n一\n".as_bytes().to_vec()).expect("valid snapshot");
        let start = position(2, 5);
        let end = position(3, 9);

        assert_eq!(snapshot.slice(start, end).expect("valid range"), "一\n");
        assert_eq!(
            snapshot.materialize_offsets().expect("offsets"),
            OffsetKind::U32
        );
    }
}
