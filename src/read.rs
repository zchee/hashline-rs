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
//! `read` — versioned positional file reading (hashline protocol v2).
//!
//! Output begins with one [`SnapshotHeader`](crate::protocol::SnapshotHeader),
//! followed by `LINE@BYTE|CONTENT` lines. When more lines remain, a
//! [`PageCursor`](crate::protocol::PageCursor) footer continues the same snapshot.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::index::FileIndex;
use crate::protocol::{
    ContractError, ErrorCode, ErrorResponse, PageCursor, ProtocolError, ReadRequest,
    validate_reference_cursor,
};
use crate::render::{render_range, render_snapshot_page};
use crate::scheme::Scheme;
use crate::cache;
use crate::snapshot::{Snapshot, SnapshotError};
use crate::util::{ToolOutcome, Workspace};

/// Frozen incompatible-v2 request schema (production wire type).
pub type HashlineReadV2Input = ReadRequest;

/// Maximum number of lines returned by a single read page (v2 wire max).
pub const MAX_LINES_READ: usize = crate::protocol::MAX_PAGE_LINES as usize;

/// Byte size above which snapshot load + render runs on a blocking thread.
///
/// Below this threshold the work is inlined on the async reactor: Snapshot
/// construction is panic-free (unlike the old partial FileIndex), so small
/// files avoid the spawn_blocking hop.
const BLOCKING_READ_BYTES: usize = 256 * 1024;


/// Legacy v1 input retained for transitional tests and benches only.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HashlineReadInput {
    /// Path of the file to read (relative to the workspace root or absolute).
    pub path: String,
    /// 1-based line number to start reading from.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to read.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Format file content with v1 anchors (bench / transitional only).
pub fn format_hashline_content(
    file_content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    scheme: Scheme,
) -> String {
    let window = line_window(offset, limit);
    let index = windowed_index(file_content, window.clone(), scheme);
    let mut out = String::new();
    render_range(&index, scheme, window, &mut out);
    out
}

fn line_window(offset: Option<usize>, limit: Option<usize>) -> Range<usize> {
    let start = offset.unwrap_or(1).saturating_sub(1);
    let end = start.saturating_add(limit.unwrap_or(usize::MAX));
    start..end
}

fn windowed_index<'a>(content: &'a str, window: Range<usize>, scheme: Scheme) -> FileIndex<'a> {
    let span = scheme.required_hash_span(window, usize::MAX);
    FileIndex::new_partial(content, &[span])
}

fn protocol_outcome(error: ProtocolError) -> ToolOutcome {
    let envelope = ErrorResponse::new(error);
    match serde_json::to_string_pretty(&envelope) {
        Ok(text) => ToolOutcome::error(text),
        Err(_) => ToolOutcome::error(envelope.error.message),
    }
}

fn map_snapshot_error(path: &Path, error: SnapshotError) -> ToolOutcome {
    match error {
        SnapshotError::Contract(contract) => protocol_outcome(ProtocolError::from(contract)),
        SnapshotError::Io {
            operation,
            path: io_path,
            source,
        } => ToolOutcome::error(format!(
            "Failed to {operation} {}: {source}",
            io_path.display()
        )),
        SnapshotError::ConcurrentModification { path: changed } => protocol_outcome(
            ProtocolError::new(
                ErrorCode::Io,
                format!(
                    "file changed during both snapshot read attempts: {}",
                    changed.display()
                ),
            ),
        ),
        other => ToolOutcome::error(format!("Failed to read {}: {other}", path.display())),
    }
}

fn render_loaded(
    snapshot: &Snapshot,
    display_path: &str,
    start_line: u64,
    limit: u16,
) -> Result<String, SnapshotError> {
    render_snapshot_page(snapshot, display_path, start_line, limit)
}

fn validate_cursor_on_snapshot(
    snapshot: &Snapshot,
    path: &str,
    cursor: &PageCursor,
) -> Result<u64, ProtocolError> {
    // Text identity already lives on the snapshot; re-check cursor via reference
    // rules so stale cursors conflict before boundary resolution.
    validate_reference_cursor(path, snapshot.bytes(), snapshot.id(), cursor)
        .map(|position| position.line())
}

/// Execute a v2 `read` request against the local filesystem.
pub async fn run_read(workspace: &Workspace, input: &ReadRequest) -> ToolOutcome {
    if let Err(error) = input.validate() {
        return protocol_outcome(ProtocolError::from(error));
    }

    let path = match workspace.resolve(&input.path) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };

    // Snapshot load (cached by path+stamp when metadata is stable).
    let load_path = path.clone();
    let snapshot: Arc<Snapshot> = if tokio::fs::metadata(&path)
        .await
        .map(|meta| meta.len() as usize > BLOCKING_READ_BYTES)
        .unwrap_or(true)
    {
        match tokio::task::spawn_blocking(move || {
            cache::process_cache().get_or_load(&load_path, || Snapshot::load(&load_path))
        })
        .await
        {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => return map_snapshot_error(&path, error),
            Err(join) => {
                return ToolOutcome::error(format!("Failed to read {}: {join}", path.display()));
            }
        }
    } else {
        match cache::process_cache().get_or_load(&path, || Snapshot::load(&path)) {
            Ok(snapshot) => snapshot,
            Err(error) => return map_snapshot_error(&path, error),
        }
    };

    let display_path = input.path.as_str();
    let start_line = if let Some(cursor) = input.cursor.as_ref() {
        match validate_cursor_on_snapshot(snapshot.as_ref(), display_path, cursor) {
            Ok(line) => line,
            Err(error) => return protocol_outcome(error),
        }
    } else {
        1
    };

    if start_line > snapshot.line_count() {
        return protocol_outcome(ProtocolError::from(ContractError::InvalidPosition {
            position: cursor_position_or_first(input.cursor.as_ref(), start_line),
        }));
    }

    let limit = input.limit;
    let snap = Arc::clone(&snapshot);
    let path_for_err = path.clone();
    let display = display_path.to_owned();
    let rendered = if snap.byte_len() > BLOCKING_READ_BYTES as u64 {
        match tokio::task::spawn_blocking(move || {
            render_loaded(snap.as_ref(), &display, start_line, limit)
        })
        .await
        {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => return map_snapshot_error(&path_for_err, error),
            Err(join) => {
                return ToolOutcome::error(format!(
                    "Failed to read {}: {join}",
                    path_for_err.display()
                ));
            }
        }
    } else {
        match render_loaded(snap.as_ref(), &display, start_line, limit) {
            Ok(text) => text,
            Err(error) => return map_snapshot_error(&path_for_err, error),
        }
    };

    ToolOutcome::success(rendered)
}

fn cursor_position_or_first(
    cursor: Option<&PageCursor>,
    start_line: u64,
) -> crate::protocol::Position {
    if let Some(cursor) = cursor {
        return cursor.next;
    }
    crate::protocol::Position::new(start_line, 0)
        .unwrap_or_else(|_| crate::protocol::Position::new(1, 0).expect("1@0"))
}

/// Legacy v1 runner kept for transitional benches that still pass a Scheme.
pub async fn run_read_v1(
    workspace: &Workspace,
    input: &HashlineReadInput,
    scheme: Scheme,
) -> ToolOutcome {
    if input.offset == Some(0) {
        return ToolOutcome::error("offset is 1-based; use offset=1 for the first line.".to_owned());
    }
    if input.limit == Some(0) {
        return ToolOutcome::error("limit must be greater than 0.".to_owned());
    }

    let path = match workspace.resolve(&input.path) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ToolOutcome::error(format!("File not found: {}", path.display()));
        }
        Err(e) => {
            return ToolOutcome::error(format!("Failed to read {}: {e}", path.display()));
        }
    };
    if bytes.is_empty() {
        return ToolOutcome::success(format!("The file {} exists but is empty.", path.display()));
    }
    let offset = input.offset.unwrap_or(1);
    let effective_limit = input.limit.unwrap_or(usize::MAX).min(MAX_LINES_READ);
    let text = format_hashline_content(
        &String::from_utf8_lossy(&bytes),
        Some(offset),
        Some(effective_limit),
        scheme,
    );
    ToolOutcome::success(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PageCursor, Position, SnapshotId};
    use crate::util::Workspace;
    use std::io::Write;
    use tempfile::tempdir;

    fn ws(root: &std::path::Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    #[tokio::test]
    async fn v2_read_empty_file_renders_header_and_empty_line() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("e.txt"), b"").unwrap();
        let outcome = run_read(
            &ws(tmp.path()),
            &ReadRequest {
                path: "e.txt".into(),
                limit: 2000,
                cursor: None,
            },
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("lines=1 bytes=0"));
        assert!(outcome.text.contains("\n1@0|"));
    }

    #[tokio::test]
    async fn v2_read_paginates_with_cursor_footer() {
        let tmp = tempdir().unwrap();
        let mut body = String::new();
        for i in 1..=5 {
            body.push_str(&format!("line {i}\n"));
        }
        std::fs::write(tmp.path().join("p.txt"), body).unwrap();
        let first = run_read(
            &ws(tmp.path()),
            &ReadRequest {
                path: "p.txt".into(),
                limit: 2,
                cursor: None,
            },
        )
        .await;
        assert!(!first.is_error, "{}", first.text);
        assert!(first.text.contains("1@0|line 1"));
        let footer = first
            .text
            .lines()
            .find(|l| l.starts_with("[hashline-v2 next"))
            .expect("cursor footer");
        // Parse snapshot= and position= from footer.
        let snapshot_hex = footer
            .split_whitespace()
            .find_map(|part| part.strip_prefix("snapshot="))
            .expect("snapshot field");
        let position_tok = footer
            .split_whitespace()
            .find_map(|part| part.strip_prefix("position="))
            .and_then(|s| s.strip_suffix(']'))
            .expect("position field");
        let snapshot: SnapshotId = snapshot_hex.parse().expect("snapshot id");
        let next: Position = position_tok.parse().expect("position");
        let second = run_read(
            &ws(tmp.path()),
            &ReadRequest {
                path: "p.txt".into(),
                limit: 10,
                cursor: Some(PageCursor { snapshot, next }),
            },
        )
        .await;
        assert!(!second.is_error, "{}", second.text);
        assert!(second.text.contains("|line 3"));
        assert!(!second.text.contains("|line 1"));
    }

    #[tokio::test]
    async fn v2_stale_cursor_conflicts() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("s.txt"), b"alpha\nbeta\n").unwrap();
        let stale = run_read(
            &ws(tmp.path()),
            &ReadRequest {
                path: "s.txt".into(),
                limit: 10,
                cursor: Some(PageCursor {
                    snapshot: SnapshotId::from_u128(0xdead),
                    next: Position::new(1, 0).unwrap(),
                }),
            },
        )
        .await;
        assert!(stale.is_error, "expected conflict");
        assert!(
            stale.text.contains("snapshot_conflict")
                || stale.text.contains("no longer matches")
                || stale.text.contains("dead"),
            "{}",
            stale.text
        );
    }

    #[tokio::test]
    async fn v2_invalid_limit_rejected() {
        let tmp = tempdir().unwrap();
        let mut f = std::fs::File::create(tmp.path().join("x.txt")).unwrap();
        writeln!(f, "hi").unwrap();
        let outcome = run_read(
            &ws(tmp.path()),
            &ReadRequest {
                path: "x.txt".into(),
                limit: 0,
                cursor: None,
            },
        )
        .await;
        assert!(outcome.is_error);
    }

    #[tokio::test]
    async fn v2_crlf_content_omits_terminator() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("c.txt"), b"a\r\nb\r\n").unwrap();
        let outcome = run_read(
            &ws(tmp.path()),
            &ReadRequest {
                path: "c.txt".into(),
                limit: 10,
                cursor: None,
            },
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("|a\n") || outcome.text.contains("|a"));
        assert!(!outcome.text.contains("|a\r"));
    }
}
