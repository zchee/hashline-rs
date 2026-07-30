//! `hashline_edit` — anchor-based file editing.
//!
//! Supports `replace`, `insert_after`, and `write` operations. Anchors are
//! validated against the pre-edit file snapshot; edits are applied bottom-up
//! to avoid line-shift interference. Returns fresh-anchor snippets on success
//! and structured error context on validation failures.

pub mod apply;
pub mod range_policy;
pub mod types;

use std::path::Path;

pub use types::{HashlineEditInput, HashlineEditOutput, HashlineOp};

use crate::read::SPAWN_BLOCKING_THRESHOLD_BYTES;
use crate::scheme::Scheme;
use crate::util::{ToolOutcome, Workspace, decode_utf8};
use types::{HashlineEditError, HashlineEditErrorKind, HashlineEditsApplied};

/// Render a successful edit application as model-facing text.
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

/// Render an edit failure as model-facing text.
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

/// Execute a `hashline_edit` request against the local filesystem.
pub async fn run_edit(
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
        // A sole `write` op may create a new file; anything else needs an
        // existing file to anchor against.
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
            return write_and_render(Vec::new(), input, &path, scheme).await;
        }
        return ToolOutcome::error(format!(
            "File not found: {}. Only a single \"write\" op can create a new file.",
            path.display()
        ));
    };

    write_and_render(old_bytes, input, &path, scheme).await
}

/// Apply the edits to the pre-edit file bytes, persist on success, and render
/// text.
///
/// All anchors are validated before any edit is applied, so the file is either
/// fully updated or left untouched.
async fn write_and_render(
    old_bytes: Vec<u8>,
    input: &HashlineEditInput,
    path: &Path,
    scheme: Scheme,
) -> ToolOutcome {
    // Splicing and anchoring a large file would stall the reactor, so hand the
    // CPU-bound step to a blocking thread once it is big enough to matter. The
    // task needs `'static` data, so the (request-sized) op list is cloned; the
    // file bytes move, and the decoded text borrows from them on that thread.
    let result = if old_bytes.len() > SPAWN_BLOCKING_THRESHOLD_BYTES {
        let edits = input.edits.clone();
        let task = tokio::task::spawn_blocking(move || {
            apply::apply_edits(&decode_utf8(&old_bytes), &edits, scheme)
        });
        match task.await {
            Ok(result) => result,
            Err(e) => {
                return ToolOutcome::error(format!("Failed to edit {}: {e}", path.display()));
            }
        }
    } else {
        apply::apply_edits(&decode_utf8(&old_bytes), &input.edits, scheme)
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
    use crate::config::SchemeConfig;
    use crate::index::FileIndex;

    fn scheme() -> Scheme {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn anchor_for(content: &str, line: usize, scheme: Scheme) -> String {
        let index = FileIndex::new(content);
        scheme
            .anchor_at(&index, line - 1)
            .expect("line within file")
            .render()
    }

    fn ws(root: &Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    #[tokio::test]
    async fn edit_existing_file_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("code.rs");
        let content = "fn main() {\n    let x = 1;\n}\n";
        std::fs::write(&file, content).unwrap();

        let s = scheme();
        let input = HashlineEditInput {
            file_path: "code.rs".to_owned(),
            edits: vec![HashlineOp::Replace {
                anchor: anchor_for(content, 2, s),
                end_anchor: None,
                content: "    let x = 42;".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("Applied 1 edit(s)"));
        assert!(outcome.text.contains("fresh anchors"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn main() {\n    let x = 42;\n}\n"
        );
    }

    #[tokio::test]
    async fn write_op_creates_new_file_with_parents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = scheme();
        let input = HashlineEditInput {
            file_path: "nested/dir/new.txt".to_owned(),
            edits: vec![HashlineOp::Write {
                content: "created\n".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("nested/dir/new.txt")).unwrap(),
            "created\n"
        );
    }

    #[tokio::test]
    async fn non_write_on_missing_file_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = scheme();
        let input = HashlineEditInput {
            file_path: "missing.txt".to_owned(),
            edits: vec![HashlineOp::Replace {
                anchor: "1:abc:rst".to_owned(),
                end_anchor: None,
                content: "x".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("File not found"));
    }

    #[tokio::test]
    async fn empty_edits_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = scheme();
        let input = HashlineEditInput {
            file_path: "any.txt".to_owned(),
            edits: vec![],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("No edit operations"));
    }

    #[tokio::test]
    async fn stale_anchor_leaves_file_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        let original = "alpha\nbeta\n";
        std::fs::write(&file, original).unwrap();

        let s = scheme();
        let input = HashlineEditInput {
            file_path: "f.txt".to_owned(),
            edits: vec![HashlineOp::Replace {
                anchor: "1:zzz:zzz".to_owned(),
                end_anchor: None,
                content: "nope".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("retry your edit"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[tokio::test]
    async fn large_file_edit_runs_on_a_blocking_thread() {
        // Comfortably past SPAWN_BLOCKING_THRESHOLD_BYTES, so the apply step
        // takes the spawn_blocking branch rather than running on the reactor.
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("big.rs");
        let content: String = (0..30_000)
            .map(|i| format!("let value_{i} = {i};\n"))
            .collect();
        assert!(content.len() > SPAWN_BLOCKING_THRESHOLD_BYTES);
        std::fs::write(&file, &content).unwrap();

        let s = scheme();
        let input = HashlineEditInput {
            file_path: "big.rs".to_owned(),
            edits: vec![HashlineOp::Replace {
                anchor: anchor_for(&content, 20_000, s),
                end_anchor: None,
                content: "let value_19999 = EDITED;".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(!outcome.is_error, "{}", outcome.text);

        let written = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines[19_999], "let value_19999 = EDITED;");
        assert_eq!(lines[19_998], "let value_19998 = 19998;");
        assert_eq!(lines.len(), 30_000);
    }

    #[tokio::test]
    async fn invalid_utf8_falls_back_to_lossy_decoding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("bad.txt");
        // A lone 0xFF byte is not valid UTF-8; decoding must not fail the call.
        std::fs::write(&file, b"alpha\n\xffbeta\n").unwrap();

        let lossy = String::from_utf8_lossy(b"alpha\n\xffbeta\n").into_owned();
        let s = scheme();
        let input = HashlineEditInput {
            file_path: "bad.txt".to_owned(),
            edits: vec![HashlineOp::Replace {
                anchor: anchor_for(&lossy, 1, s),
                end_anchor: None,
                content: "ALPHA".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, s).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .starts_with("ALPHA\n")
        );
    }
}
