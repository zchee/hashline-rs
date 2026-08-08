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
//! `edit` — versioned byte-range file editing for the hashline protocol.
//!
//! Production path: [`run`] takes an [`EditRequest`], validates the named
//! snapshot, applies half-open ranges against exact bytes, and persists
//! atomically; [`run_edit`] renders the same result for MCP transport.

use std::{path::Path, sync::Arc};

use crate::{
    cache, persist,
    protocol::{
        EditRequest, EditSuccess, ErrorCode, ProtocolError, apply_versioned_reference_edits,
        reference_context, reference_header,
    },
    snapshot::{Snapshot, SnapshotError},
    util::{
        ToolOutcome, Workspace, join_protocol_error, persist_protocol_error, protocol_outcome,
        resolve_workspace_path, snapshot_protocol_error, success_outcome,
    },
};

// Re-export for apply_versioned tests; message is crate-private in protocol.
// Use the same wording without importing the private const.
const CONFLICT_MSG: &str = "the file no longer matches the requested snapshot";

/// Execute an `edit` request, returning the typed persisted result.
///
/// This is the typed embedding surface; every failure is a stable R017
/// taxonomy error. [`run_edit`] renders the same result for MCP transport.
///
/// # Errors
///
/// Returns snapshot_conflict, position/range/batch contract errors,
/// root_escape, or a text/io taxonomy error; the success is published only
/// after persistence (R019).
pub async fn run(workspace: &Workspace, input: &EditRequest) -> Result<EditSuccess, ProtocolError> {
    input.validate().map_err(ProtocolError::from)?;
    let path = resolve_workspace_path(workspace, &input.file_path)?;

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
        Ok(Err(error)) => return Err(snapshot_protocol_error("edit", &path, error)),
        Err(join) => return Err(join_protocol_error("edit", &path, join)),
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
        Ok(result) => result?,
        Err(join) => return Err(join_protocol_error("edit", &path, join)),
    };

    let path_for_write = path.clone();
    let bytes_for_write = applied.clone();
    match tokio::task::spawn_blocking(move || {
        persist::atomic_write(&path_for_write, &bytes_for_write, stamp)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(persist_protocol_error(error)),
        Err(join) => return Err(join_protocol_error("persist", &path, join)),
    }

    let new_snapshot = Snapshot::from_bytes(applied)
        .map_err(|error| snapshot_protocol_error("edit", &path, error))?;
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
    Ok(success)
}

/// Execute an `edit` request against the local filesystem (MCP text skin).
pub async fn run_edit(workspace: &Workspace, input: &EditRequest) -> ToolOutcome {
    match run(workspace, input).await {
        Ok(success) => success_outcome(&success),
        Err(error) => protocol_outcome(error),
    }
}

async fn create_new_file(path: &Path, input: &EditRequest) -> Result<EditSuccess, ProtocolError> {
    // Empty pre-image: compute empty snapshot id and require request match.
    let empty = Snapshot::from_bytes(Vec::new())
        .map_err(|error| snapshot_protocol_error("edit", path, error))?;
    if input.snapshot != empty.id() {
        let header = reference_header(input.file_path.clone(), empty.id(), b"")
            .map_err(ProtocolError::from)?;
        let context = reference_context(b"", 1).unwrap_or_default();
        return Err(ProtocolError::snapshot_conflict(
            input.snapshot,
            header,
            context,
            CONFLICT_MSG.to_owned(),
        )
        .unwrap_or_else(ProtocolError::from));
    }

    let applied = apply_versioned_reference_edits(b"", empty.id(), input)?;

    if let Some(parent) = path.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return Err(ProtocolError::new(
            ErrorCode::Io,
            format!(
                "Failed to create parent directory for {}: {e}",
                path.display()
            ),
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
        Ok(Err(error)) => return Err(persist_protocol_error(error)),
        Err(join) => return Err(join_protocol_error("persist", path, join)),
    }

    let new_snapshot = Snapshot::from_bytes(applied)
        .map_err(|error| snapshot_protocol_error("edit", path, error))?;
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
    Ok(success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::{EditOperation, Position, SnapshotId},
        util::Workspace,
    };

    fn ws(root: &Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    fn pos(line: u64, byte: u64) -> Position {
        Position::new(line, byte).unwrap()
    }

    #[tokio::test]
    async fn edit_applies_and_persists() {
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
    async fn typed_run_returns_success_and_conflict() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("typed.txt");
        std::fs::write(&file, b"alpha\nbeta\n").unwrap();
        let snap = Snapshot::load(&file).unwrap();
        let request = EditRequest {
            file_path: "typed.txt".into(),
            snapshot: snap.id(),
            edits: vec![EditOperation::replace(
                pos(1, 0),
                pos(2, 6),
                "ALPHA\n".into(),
            )],
        };

        let success = run(&ws(tmp.path()), &request)
            .await
            .expect("typed edit succeeds");
        assert_eq!(success.applied, 1);
        assert_eq!(success.previous_snapshot, snap.id());
        assert_eq!(std::fs::read(&file).unwrap(), b"ALPHA\nbeta\n");

        let stale = run(&ws(tmp.path()), &request)
            .await
            .expect_err("replaying the consumed snapshot conflicts");
        assert_eq!(stale.code, ErrorCode::SnapshotConflict);
        assert!(stale.conflict.is_some());
    }

    #[tokio::test]
    async fn stale_snapshot_applies_zero_edits() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("t.txt");
        std::fs::write(&file, b"alpha\nbeta\n").unwrap();
        let before = std::fs::read(&file).unwrap();
        let outcome = run_edit(
            &ws(tmp.path()),
            &EditRequest {
                file_path: "t.txt".into(),
                snapshot: SnapshotId::from_u128(0xbad),
                edits: vec![EditOperation::replace(pos(1, 0), pos(2, 6), "X\n".into())],
            },
        )
        .await;
        assert!(outcome.is_error, "{}", outcome.text);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(
            outcome.text.contains("snapshot_conflict")
                || outcome.text.contains("no longer matches"),
            "{}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn create_empty_file_via_whole_replace() {
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
