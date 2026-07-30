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

use crate::scheme::AnchorScheme;
use crate::util::{ToolOutcome, Workspace};
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
    scheme: &dyn AnchorScheme,
) -> ToolOutcome {
    if input.edits.is_empty() {
        return ToolOutcome::error("No edit operations provided.".to_owned());
    }

    let path = match workspace.resolve(&input.file_path) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };

    let old_content = match tokio::fs::read(&path).await {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return ToolOutcome::error(format!("Failed to read {}: {e}", path.display()));
        }
    };

    let Some(old_content) = old_content else {
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
            return write_and_render("", input, &path, scheme).await;
        }
        return ToolOutcome::error(format!(
            "File not found: {}. Only a single \"write\" op can create a new file.",
            path.display()
        ));
    };

    write_and_render(&old_content, input, &path, scheme).await
}

/// Apply the edits to `old_content`, persist on success, and render text.
async fn write_and_render(
    old_content: &str,
    input: &HashlineEditInput,
    path: &Path,
    scheme: &dyn AnchorScheme,
) -> ToolOutcome {
    let result = apply::apply_edits(old_content, &input.edits, scheme);

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
    use crate::scheme::split_lines;

    fn scheme() -> Box<dyn AnchorScheme> {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn anchor_for(content: &str, line: usize, scheme: &dyn AnchorScheme) -> String {
        let lines = split_lines(content);
        scheme.generate_anchors(&lines)[line - 1].render()
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
                anchor: anchor_for(content, 2, &*s),
                end_anchor: None,
                content: "    let x = 42;".to_owned(),
            }],
        };
        let outcome = run_edit(&ws(tmp.path()), &input, &*s).await;
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
        let outcome = run_edit(&ws(tmp.path()), &input, &*s).await;
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
        let outcome = run_edit(&ws(tmp.path()), &input, &*s).await;
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
        let outcome = run_edit(&ws(tmp.path()), &input, &*s).await;
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
        let outcome = run_edit(&ws(tmp.path()), &input, &*s).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("retry your edit"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }
}
