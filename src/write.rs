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
//! `write` — versioned file creation and replacement for the hashline
//! protocol.
//!
//! [`run_write`] decides one [`WriteRequest`] against the freshly loaded
//! destination state with [`validate_reference_write`], then persists either
//! exclusively ([`persist::atomic_create`], `expect: "absent"`) or atomically
//! over the validated stamp ([`persist::atomic_write`], versioned overwrite).

use std::{path::Path, sync::Arc};

use crate::{
    cache,
    persist::{self, PersistError},
    protocol::{ErrorCode, ProtocolError, WriteRequest, WriteSuccess, validate_reference_write},
    snapshot::{Snapshot, SnapshotError},
    util::{
        ToolOutcome, Workspace, persist_error_outcome, protocol_outcome, snapshot_error_outcome,
    },
};

/// Execute a `write` request against the local filesystem.
pub async fn run_write(workspace: &Workspace, input: &WriteRequest) -> ToolOutcome {
    if let Err(error) = input.validate() {
        return protocol_outcome(ProtocolError::from(error));
    }

    let path = match workspace.resolve(&input.file_path) {
        Ok(path) => path,
        Err(reason) => {
            return protocol_outcome(ProtocolError::new(ErrorCode::RootEscape, reason));
        }
    };

    let load_path = path.clone();
    let current = match tokio::task::spawn_blocking(move || Snapshot::load(&load_path)).await {
        Ok(Ok(snapshot)) => Some(snapshot),
        Ok(Err(SnapshotError::Io { source, .. }))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            None
        }
        Ok(Err(error)) => return snapshot_error_outcome("write", &path, error),
        Err(join) => {
            return ToolOutcome::error(format!("Failed to write {}: {join}", path.display()));
        }
    };

    let created = match validate_reference_write(
        current
            .as_ref()
            .map(|snapshot| (snapshot.bytes(), snapshot.id())),
        input,
    ) {
        Ok(created) => created,
        Err(error) => return protocol_outcome(error),
    };

    let bytes = input.content.clone().into_bytes();
    let path_for_write = path.clone();
    let persist_result = if created {
        tokio::task::spawn_blocking(move || persist::atomic_create(&path_for_write, &bytes)).await
    } else {
        let stamp = current.as_ref().and_then(|snapshot| snapshot.stamp());
        tokio::task::spawn_blocking(move || persist::atomic_write(&path_for_write, &bytes, stamp))
            .await
    };
    match persist_result {
        Ok(Ok(())) => {}
        Ok(Err(PersistError::DestinationExists { .. })) => {
            return lose_create_race(&path, input).await;
        }
        Ok(Err(error)) => return persist_error_outcome(error),
        Err(join) => {
            return ToolOutcome::error(format!("Failed to persist {}: {join}", path.display()));
        }
    }

    let persisted = match Snapshot::from_bytes(input.content.clone().into_bytes()) {
        Ok(snapshot) => snapshot,
        Err(error) => return snapshot_error_outcome("write", &path, error),
    };
    let success = WriteSuccess::new(
        input.file_path.clone(),
        persisted.id(),
        persisted.byte_len(),
        persisted.line_count(),
        created,
    );
    // Prefer a stamped resident entry so subsequent reads hit on FileStamp.
    if let Ok(stamped) = Snapshot::load(&path) {
        cache::process_cache().insert(path.clone(), Arc::new(stamped));
    } else {
        cache::process_cache().insert(path.clone(), Arc::new(persisted));
    }
    match serde_json::to_string_pretty(&success) {
        Ok(text) => ToolOutcome::success(text),
        Err(e) => ToolOutcome::error(format!("Failed to encode write success: {e}")),
    }
}

/// Report an exclusive create that lost the filesystem-level link race.
///
/// The absent check passed but another writer created the destination before
/// our hard link landed. Decide against the fresh state so the error carries
/// a truthful current header, exactly as if the winner had been there first.
async fn lose_create_race(path: &Path, input: &WriteRequest) -> ToolOutcome {
    let load_path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || Snapshot::load(&load_path)).await {
        Ok(Ok(snapshot)) => {
            match validate_reference_write(Some((snapshot.bytes(), snapshot.id())), input) {
                Ok(_) => ToolOutcome::error(format!(
                    "Failed to write {}: lost a create race but the destination no \
                     longer conflicts; retry",
                    path.display()
                )),
                Err(error) => protocol_outcome(error),
            }
        }
        Ok(Err(SnapshotError::Io { source, .. }))
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            ToolOutcome::error(format!(
                "Failed to write {}: destination appeared and vanished during create; \
                 retry",
                path.display()
            ))
        }
        Ok(Err(error)) => snapshot_error_outcome("write", path, error),
        Err(join) => ToolOutcome::error(format!("Failed to write {}: {join}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::protocol::{SnapshotId, WriteExpect};

    fn ws(root: &Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    fn create_request(file_path: &str, content: &str) -> WriteRequest {
        WriteRequest {
            file_path: file_path.to_owned(),
            content: content.to_owned(),
            expect: WriteExpect::Absent,
        }
    }

    fn overwrite_request(file_path: &str, content: &str, snapshot: SnapshotId) -> WriteRequest {
        WriteRequest {
            file_path: file_path.to_owned(),
            content: content.to_owned(),
            expect: WriteExpect::Snapshot(snapshot),
        }
    }

    fn parse(outcome: &ToolOutcome) -> Value {
        serde_json::from_str(&outcome.text).expect("structured JSON outcome")
    }

    #[tokio::test]
    async fn create_then_versioned_overwrite_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("fresh.txt");

        let created = run_write(&ws(tmp.path()), &create_request("fresh.txt", "one\ntwo\n")).await;
        assert!(!created.is_error, "{}", created.text);
        let value = parse(&created);
        assert_eq!(value["created"], Value::Bool(true));
        assert_eq!(value["bytes"], 8);
        assert_eq!(value["lines"], 3);
        assert_eq!(std::fs::read(&file).unwrap(), b"one\ntwo\n");

        let snapshot: SnapshotId = value["snapshot"].as_str().unwrap().parse().unwrap();
        let replaced = run_write(
            &ws(tmp.path()),
            &overwrite_request("fresh.txt", "ONE\n", snapshot),
        )
        .await;
        assert!(!replaced.is_error, "{}", replaced.text);
        let value = parse(&replaced);
        assert_eq!(value["created"], Value::Bool(false));
        assert_eq!(std::fs::read(&file).unwrap(), b"ONE\n");
    }

    #[tokio::test]
    async fn create_makes_missing_parent_directories() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outcome = run_write(
            &ws(tmp.path()),
            &create_request("new/dir/file.txt", "nested\n"),
        )
        .await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            std::fs::read(tmp.path().join("new/dir/file.txt")).unwrap(),
            b"nested\n"
        );
    }

    #[tokio::test]
    async fn create_on_existing_returns_already_exists_with_header() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("taken.txt"), b"occupant\n").unwrap();

        let outcome = run_write(&ws(tmp.path()), &create_request("taken.txt", "usurper\n")).await;
        assert!(outcome.is_error, "{}", outcome.text);
        let value = parse(&outcome);
        assert_eq!(value["error"]["code"], "already_exists");
        assert_eq!(value["error"]["retryable"], Value::Bool(true));
        let header = &value["error"]["existing"]["current_header"];
        assert_eq!(header["bytes"], 9);
        assert!(value["error"].get("conflict").is_none());
        assert_eq!(
            std::fs::read(tmp.path().join("taken.txt")).unwrap(),
            b"occupant\n"
        );
    }

    #[tokio::test]
    async fn stale_overwrite_conflicts_and_missing_target_is_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("live.txt"), b"current\n").unwrap();
        let stale = Snapshot::from_bytes(b"previous\n".to_vec()).unwrap().id();

        let conflict = run_write(
            &ws(tmp.path()),
            &overwrite_request("live.txt", "next\n", stale),
        )
        .await;
        assert!(conflict.is_error, "{}", conflict.text);
        let value = parse(&conflict);
        assert_eq!(value["error"]["code"], "snapshot_conflict");
        assert!(value["error"]["conflict"]["current_header"].is_object());
        assert_eq!(
            std::fs::read(tmp.path().join("live.txt")).unwrap(),
            b"current\n"
        );

        let missing = run_write(
            &ws(tmp.path()),
            &overwrite_request("absent.txt", "next\n", stale),
        )
        .await;
        assert!(missing.is_error, "{}", missing.text);
        assert_eq!(parse(&missing)["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn concurrent_creates_have_exactly_one_winner() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = ws(tmp.path());
        let left_request = create_request("race.txt", "left\n");
        let right_request = create_request("race.txt", "right\n");
        let left = run_write(&workspace, &left_request);
        let right = run_write(&workspace, &right_request);
        let (left, right) = tokio::join!(left, right);

        let winners = [&left, &right]
            .into_iter()
            .filter(|outcome| !outcome.is_error)
            .count();
        assert_eq!(winners, 1, "left: {} right: {}", left.text, right.text);
        let loser = if left.is_error { &left } else { &right };
        assert_eq!(parse(loser)["error"]["code"], "already_exists");
        let bytes = std::fs::read(tmp.path().join("race.txt")).unwrap();
        assert!(bytes == b"left\n" || bytes == b"right\n");
    }

    #[tokio::test]
    async fn restricted_workspace_rejects_escapes_as_root_escape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let target = outside.path().join("secret.txt");
        let workspace = Workspace::new(tmp.path().canonicalize().unwrap(), true);

        let outcome = run_write(
            &workspace,
            &create_request(target.to_str().unwrap(), "leak\n"),
        )
        .await;
        assert!(outcome.is_error, "{}", outcome.text);
        let value = parse(&outcome);
        assert_eq!(value["error"]["code"], "root_escape");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn empty_content_creates_the_empty_file_model() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outcome = run_write(&ws(tmp.path()), &create_request("empty.txt", "")).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        let value = parse(&outcome);
        assert_eq!(value["bytes"], 0);
        assert_eq!(value["lines"], 1);
        assert_eq!(std::fs::read(tmp.path().join("empty.txt")).unwrap(), b"");
    }
}
