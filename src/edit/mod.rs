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
//! `edit` — versioned byte-range file editing (hashline protocol v2).
//!
//! Production path: [`run_edit`] takes an [`EditRequest`], validates the
//! named snapshot, applies half-open ranges against exact bytes, and persists
//! atomically. The transitional v1 anchor engine remains in [`apply`] for
//! benches until Phase 8 deletes it.

pub mod apply;
pub mod range_policy;
pub mod types;

use std::path::Path;
use std::sync::Arc;

pub use types::{HashlineEditInput, HashlineEditOutput, HashlineOp};

use crate::cache;
use crate::persist::{self, PersistError};
use crate::protocol::{
    EditRequest, EditSuccess, ErrorResponse, ProtocolError, apply_versioned_reference_edits,
    reference_context, reference_header,
};
use crate::scheme::Scheme;
use crate::snapshot::{Snapshot, SnapshotError};
use crate::util::{ToolOutcome, Workspace, decode_utf8};
use types::{HashlineEditError, HashlineEditErrorKind, HashlineEditsApplied};

// Re-export for apply_versioned tests; message is crate-private in protocol.
// Use the same wording without importing the private const.
const CONFLICT_MSG: &str = "the file no longer matches the requested snapshot";


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
        other => ToolOutcome::error(format!("Failed to edit {}: {other}", path.display())),
    }
}

fn map_persist_error(error: PersistError) -> ToolOutcome {
    match error {
        PersistError::DestinationChanged { path } => protocol_outcome(ProtocolError::new(
            crate::protocol::ErrorCode::SnapshotConflict,
            format!("destination changed before atomic rename: {}", path.display()),
        )),
        PersistError::Io {
            operation,
            path,
            source,
        } => ToolOutcome::error(format!(
            "Failed to {operation} {}: {source}",
            path.display()
        )),
    }
}

/// Execute a v2 `edit` request against the local filesystem.
pub async fn run_edit(workspace: &Workspace, input: &EditRequest) -> ToolOutcome {
    if let Err(error) = input.validate() {
        return protocol_outcome(ProtocolError::from(error));
    }

    let path = match workspace.resolve(&input.file_path) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };

    let load_path = path.clone();
    let snapshot = match tokio::task::spawn_blocking(move || Snapshot::load(&load_path)).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(SnapshotError::Io {
            source,
            path: _missing,
            ..
        })) if source.kind() == std::io::ErrorKind::NotFound => {
            // New-file path: only a whole-file replace from 1@0..2@0 on empty is
            // modeled by applying against empty bytes when snapshot is the empty ID.
            // Require the request snapshot to match an empty snapshot identity.
            return create_new_file(&path, input).await;
        }
        Ok(Err(error)) => return map_snapshot_error(&path, error),
        Err(join) => {
            return ToolOutcome::error(format!("Failed to edit {}: {join}", path.display()));
        }
    };

    let previous = snapshot.id();
    let stamp = snapshot.stamp();
    let display = input.file_path.clone();
    let request = input.clone();
    let source = snapshot.bytes().to_vec();

    let applied = match tokio::task::spawn_blocking(move || {
        apply_versioned_reference_edits(&source, previous, &request)
    })
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => return protocol_outcome(error),
        Err(join) => {
            return ToolOutcome::error(format!("Failed to edit {}: {join}", path.display()));
        }
    };

    let path_for_write = path.clone();
    let bytes_for_write = applied.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        persist::atomic_write(&path_for_write, &bytes_for_write, stamp)
    })
    .await;
    match write_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return map_persist_error(error),
        Err(join) => {
            return ToolOutcome::error(format!("Failed to persist {}: {join}", path.display()));
        }
    }

    let new_snapshot = match Snapshot::from_bytes(applied) {
        Ok(snap) => snap,
        Err(error) => return map_snapshot_error(&path, error),
    };
    let success = EditSuccess::new(
        display,
        previous,
        new_snapshot.id(),
        input.edits.len(),
        new_snapshot.byte_len(),
        new_snapshot.line_count(),
    );
    // Prefer a stamped resident entry so subsequent reads hit on FileStamp.
    if let Ok(stamped) = Snapshot::load(&path) {
        cache::process_cache().insert(path.clone(), Arc::new(stamped));
    } else {
        cache::process_cache().insert(path.clone(), Arc::new(new_snapshot));
    }
    match serde_json::to_string_pretty(&success) {
        Ok(text) => ToolOutcome::success(text),
        Err(e) => ToolOutcome::error(format!("Failed to encode edit success: {e}")),
    }
}

async fn create_new_file(path: &Path, input: &EditRequest) -> ToolOutcome {
    // Empty pre-image: compute empty snapshot id and require request match.
    let empty = match Snapshot::from_bytes(Vec::new()) {
        Ok(s) => s,
        Err(error) => return map_snapshot_error(path, error),
    };
    if input.snapshot != empty.id() {
        let header = reference_header(input.file_path.clone(), empty.id(), b"")
            .map_err(|e| protocol_outcome(ProtocolError::from(e)));
        let header = match header {
            Ok(h) => h,
            Err(outcome) => return outcome,
        };
        let context = reference_context(b"", 1).unwrap_or_default();
        return protocol_outcome(
            ProtocolError::snapshot_conflict(
                input.snapshot,
                header,
                context,
                CONFLICT_MSG.to_owned(),
            )
            .unwrap_or_else(ProtocolError::from),
        );
    }

    let applied = match apply_versioned_reference_edits(b"", empty.id(), input) {
        Ok(bytes) => bytes,
        Err(error) => return protocol_outcome(error),
    };

    if let Some(parent) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return ToolOutcome::error(format!(
            "Failed to create parent directory for {}: {e}",
            path.display()
        ));
    }

    let path_for_write = path.to_path_buf();
    let bytes_for_write = applied.clone();
    match tokio::task::spawn_blocking(move || {
        persist::atomic_write(&path_for_write, &bytes_for_write, None)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return map_persist_error(error),
        Err(join) => {
            return ToolOutcome::error(format!("Failed to persist {}: {join}", path.display()));
        }
    }

    let new_snapshot = match Snapshot::from_bytes(applied) {
        Ok(snap) => snap,
        Err(error) => return map_snapshot_error(path, error),
    };
    let success = EditSuccess::new(
        input.file_path.clone(),
        empty.id(),
        new_snapshot.id(),
        input.edits.len(),
        new_snapshot.byte_len(),
        new_snapshot.line_count(),
    );
    if let Ok(stamped) = Snapshot::load(path) {
        cache::process_cache().insert(path.to_path_buf(), Arc::new(stamped));
    } else {
        cache::process_cache().insert(path.to_path_buf(), Arc::new(new_snapshot));
    }
    match serde_json::to_string_pretty(&success) {
        Ok(text) => ToolOutcome::success(text),
        Err(e) => ToolOutcome::error(format!("Failed to encode edit success: {e}")),
    }
}

/// Render a successful v1 edit application as model-facing text.
fn render_applied(applied: &HashlineEditsApplied, path: &Path) -> String {
    let mut text = format!(
        "Applied {} edit(s) to {} (scheme {}).",
        applied.applied,
        path.display(),
        applied.scheme
    );
    if !applied.warnings.is_empty() {
        text.push_str("\n\n");
        text.push_str(&applied.warnings.join("\n"));
    }
    text.push_str(&format!(
        "\n\nSnippet with fresh anchors (starting at line {}):\n{}",
        applied.snippet_start_line, applied.snippet
    ));
    text
}

/// Render a v1 edit failure as model-facing text.
fn render_error(err: &HashlineEditError) -> String {
    let mut msg = err.message.clone();
    if let Some(ref ctx) = err.context {
        let label = match err.context_start_line {
            Some(start) => {
                format!("Fresh anchors around line {start} — use these to retry your edit:")
            }
            None => "Fresh anchors — use these to retry your edit:".to_owned(),
        };
        msg.push_str("\n\n");
        msg.push_str(&label);
        msg.push('\n');
        msg.push_str(ctx);
    }
    if let Some(ref anchor) = err.shifted_anchor {
        msg.push_str(&format!("\n\nSuggested anchor: {anchor}"));
    }
    msg
}

/// Legacy v1 runner kept for transitional benches.
pub async fn run_edit_v1(
    workspace: &Workspace,
    input: &HashlineEditInput,
    scheme: Scheme,
) -> ToolOutcome {
    if input.edits.is_empty() {
        return ToolOutcome::error("No edit operations provided.".to_owned());
    }

    let path = match workspace.resolve(&input.file_path) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };

    let old_bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return ToolOutcome::error(format!("Failed to read {}: {e}", path.display()));
        }
    };

    let Some(old_bytes) = old_bytes else {
        if input.edits.len() == 1
            && let HashlineOp::Write { .. } = input.edits[0]
        {
            if let Some(parent) = path.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                return ToolOutcome::error(format!(
                    "Failed to create parent directory for {}: {e}",
                    path.display()
                ));
            }
            return write_and_render_v1(Vec::new(), input, &path, scheme).await;
        }
        return ToolOutcome::error(format!(
            "File not found: {}. Only a single \"write\" op can create a new file.",
            path.display()
        ));
    };

    write_and_render_v1(old_bytes, input, &path, scheme).await
}

async fn write_and_render_v1(
    old_bytes: Vec<u8>,
    input: &HashlineEditInput,
    path: &Path,
    scheme: Scheme,
) -> ToolOutcome {
    let edits = input.edits.clone();
    let task = tokio::task::spawn_blocking(move || {
        apply::apply_edits(&decode_utf8(&old_bytes), &edits, scheme)
    });
    let result = match task.await {
        Ok(result) => result,
        Err(e) => {
            return ToolOutcome::error(format!("Failed to edit {}: {e}", path.display()));
        }
    };

    if let Some(ref new_content) = result.new_content
        && let Err(e) = tokio::fs::write(path, new_content.as_bytes()).await
    {
        let err = HashlineEditError::new(
            HashlineEditErrorKind::IoError,
            format!("Edits validated but failed to write file: {e}."),
        );
        return ToolOutcome::error(render_error(&err));
    }

    match result.output {
        HashlineEditOutput::EditsApplied(applied) => {
            ToolOutcome::success(render_applied(&applied, path))
        }
        HashlineEditOutput::Error(err) => ToolOutcome::error(render_error(&err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EditOperation, Position, SnapshotId};
    use crate::util::Workspace;

    fn ws(root: &Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    fn pos(line: u64, byte: u64) -> Position {
        Position::new(line, byte).unwrap()
    }

    #[tokio::test]
    async fn v2_edit_applies_and_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("t.txt");
        std::fs::write(&file, b"alpha\nbeta\n").unwrap();
        let snap = Snapshot::load(&file).unwrap();
        let outcome = run_edit(
            &ws(tmp.path()),
            &EditRequest {
                file_path: "t.txt".into(),
                snapshot: snap.id(),
                edits: vec![EditOperation::replace(
                    pos(2, 6),
                    pos(3, 11),
                    "BETA\n".into(),
                )],
            },
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(std::fs::read(&file).unwrap(), b"alpha\nBETA\n");
        assert!(outcome.text.contains("\"protocol\""));
        assert!(outcome.text.contains("previous_snapshot"));
    }

    #[tokio::test]
    async fn v2_stale_snapshot_applies_zero_edits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("t.txt");
        std::fs::write(&file, b"alpha\nbeta\n").unwrap();
        let before = std::fs::read(&file).unwrap();
        let outcome = run_edit(
            &ws(tmp.path()),
            &EditRequest {
                file_path: "t.txt".into(),
                snapshot: SnapshotId::from_u128(0xbad),
                edits: vec![EditOperation::replace(
                    pos(1, 0),
                    pos(2, 6),
                    "X\n".into(),
                )],
            },
        )
        .await;
        assert!(outcome.is_error, "{}", outcome.text);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(
            outcome.text.contains("snapshot_conflict") || outcome.text.contains("no longer matches"),
            "{}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn v2_create_empty_file_via_whole_replace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = Snapshot::from_bytes(Vec::new()).unwrap();
        let outcome = run_edit(
            &ws(tmp.path()),
            &EditRequest {
                file_path: "new.txt".into(),
                snapshot: empty.id(),
                edits: vec![EditOperation::replace(
                    pos(1, 0),
                    pos(2, 0),
                    "created\n".into(),
                )],
            },
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            std::fs::read(tmp.path().join("new.txt")).unwrap(),
            b"created\n"
        );
    }
}
