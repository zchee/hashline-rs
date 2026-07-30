//! `hashline_read` — anchor-annotated file reading.
//!
//! Output format: `ANCHOR→CONTENT` (e.g. `22:abc:rst→  let x = 1;`).

use schemars::JsonSchema;
use serde::Deserialize;

use crate::scheme::{AnchorScheme, split_lines};
use crate::util::{ToolOutcome, Workspace};

/// Maximum number of lines returned by a single read.
pub const MAX_LINES_READ: usize = 2000;

/// Input for the `hashline_read` tool.
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

/// Format file content lines with anchor annotations.
///
/// Each emitted line is formatted as `LINE:ANCHOR→CONTENT` where `ANCHOR` is
/// the scheme-generated anchor for that line. Anchors are always generated
/// from the full file content — contextual fingerprints (chunk/checkpoint)
/// span multiple lines, so windowed reads still need complete content.
pub fn format_hashline_content(
    file_content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    scheme: &dyn AnchorScheme,
) -> String {
    use std::fmt::Write as _;

    let all_lines = split_lines(file_content);
    let anchors = scheme.generate_anchors(&all_lines);

    let skip = offset.unwrap_or(1).saturating_sub(1);
    let take = limit.unwrap_or(usize::MAX);

    let mut output = String::new();
    let mut first = true;

    for (i, line) in all_lines.iter().enumerate().skip(skip).take(take) {
        if first {
            first = false;
        } else {
            output.push('\n');
        }

        let line_num = i + 1;
        let anchor_suffix = match &anchors[i].context {
            Some(ctx) => format!("{}:{ctx}", anchors[i].local),
            None => anchors[i].local.clone(),
        };
        _ = write!(&mut output, "{line_num}:{anchor_suffix}→{line}");
    }

    output
}

/// Execute a `hashline_read` request against the local filesystem.
pub async fn run_read(
    workspace: &Workspace,
    input: &HashlineReadInput,
    scheme: &dyn AnchorScheme,
) -> ToolOutcome {
    if input.offset == Some(0) {
        return ToolOutcome::error(
            "offset is 1-based; use offset=1 for the first line.".to_owned(),
        );
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
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => {
            return ToolOutcome::error(format!("{} is a directory, not a file.", path.display()));
        }
        Err(e) => {
            return ToolOutcome::error(format!("Failed to read {}: {e}", path.display()));
        }
    };

    if bytes.contains(&0) {
        return ToolOutcome::error(format!(
            "{} appears to be a binary file and cannot be read as text.",
            path.display()
        ));
    }

    let content = String::from_utf8_lossy(&bytes);
    if content.is_empty() {
        return ToolOutcome::success(format!("The file {} exists but is empty.", path.display()));
    }

    let total_lines = split_lines(&content).len();
    let offset = input.offset.unwrap_or(1);
    if offset > total_lines {
        return ToolOutcome::error(format!(
            "offset {offset} is beyond the end of the file ({total_lines} lines)."
        ));
    }

    let effective_limit = input.limit.unwrap_or(usize::MAX).min(MAX_LINES_READ);
    let output = format_hashline_content(&content, Some(offset), Some(effective_limit), scheme);

    let last_shown = (offset + effective_limit - 1).min(total_lines);
    let mut text = output;
    if offset > 1 || last_shown < total_lines {
        text.push_str(&format!(
            "\n\n(Showing lines {offset}-{last_shown} of {total_lines}. \
             Use offset/limit to read more.)"
        ));
    }

    ToolOutcome::success(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SchemeConfig;

    fn scheme() -> Box<dyn AnchorScheme> {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn ws(root: &std::path::Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    #[test]
    fn format_basic_file() {
        let content = "line one\nline two\nline three\n";
        let output = format_hashline_content(content, None, None, &*scheme());

        for line in output.lines() {
            assert!(line.contains(':'), "missing anchor separator: {line}");
            assert!(line.contains('→'), "missing content separator: {line}");
        }
    }

    #[test]
    fn format_includes_anchor_with_context() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let output = format_hashline_content(content, None, None, &*scheme());

        // Chunk scheme produces LINE:LOCAL:CONTEXT→CONTENT.
        let first_content_line = output.lines().next().unwrap();
        let before_arrow = first_content_line.split('→').next().unwrap();
        assert_eq!(
            before_arrow.matches(':').count(),
            2,
            "chunk scheme should produce 2 colons, got: {before_arrow}"
        );
    }

    #[test]
    fn format_with_offset_and_limit() {
        let content = "a\nb\nc\nd\ne\n";
        let output = format_hashline_content(content, Some(2), Some(2), &*scheme());

        let content_lines: Vec<&str> = output.lines().collect();
        assert_eq!(content_lines.len(), 2);
        assert!(content_lines[0].starts_with("2:"));
        assert!(content_lines[1].starts_with("3:"));
    }

    #[test]
    fn format_empty_file() {
        let output = format_hashline_content("", None, None, &*scheme());
        assert!(output.starts_with("1:"), "should contain line 1: {output}");
        assert!(output.contains('→'), "should contain arrow separator");
    }

    #[test]
    fn format_keeps_long_lines_whole() {
        let long_line = "x".repeat(5000);
        let content = format!("{long_line}\n");
        let output = format_hashline_content(&content, None, None, &*scheme());

        let first_line = output.lines().next().unwrap();
        let after_arrow = first_line.split('→').nth(1).unwrap();
        assert_eq!(
            after_arrow, long_line,
            "hashline must never clip line content"
        );
    }

    #[test]
    fn format_deterministic() {
        let content = "hello\nworld\n";
        let s = scheme();
        assert_eq!(
            format_hashline_content(content, None, None, &*s),
            format_hashline_content(content, None, None, &*s)
        );
    }

    #[tokio::test]
    async fn read_basic_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("test.rs"),
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let input = HashlineReadInput {
            path: "test.rs".to_owned(),
            offset: None,
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, &*scheme()).await;
        assert!(!outcome.is_error);
        assert!(outcome.text.contains('→'));
        assert!(outcome.text.contains("fn main()"));
    }

    #[tokio::test]
    async fn read_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = HashlineReadInput {
            path: "nope.txt".to_owned(),
            offset: None,
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, &*scheme()).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("File not found"));
    }

    #[tokio::test]
    async fn read_empty_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let input = HashlineReadInput {
            path: "empty.txt".to_owned(),
            offset: None,
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, &*scheme()).await;
        assert!(!outcome.is_error);
        assert!(outcome.text.contains("exists but is empty"));
    }

    #[tokio::test]
    async fn read_binary_file_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        let input = HashlineReadInput {
            path: "bin.dat".to_owned(),
            offset: None,
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, &*scheme()).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("binary"));
    }

    #[tokio::test]
    async fn read_windowed_reports_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();

        let input = HashlineReadInput {
            path: "big.txt".to_owned(),
            offset: Some(10),
            limit: Some(5),
        };
        let outcome = run_read(&ws(tmp.path()), &input, &*scheme()).await;
        assert!(!outcome.is_error);
        assert!(
            outcome.text.contains("Showing lines 10-14 of 51"),
            "{}",
            outcome.text
        );
        assert!(outcome.text.lines().next().unwrap().starts_with("10:"));
    }

    #[tokio::test]
    async fn read_offset_beyond_eof() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("small.txt"), "one\n").unwrap();
        let input = HashlineReadInput {
            path: "small.txt".to_owned(),
            offset: Some(100),
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, &*scheme()).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("beyond the end"));
    }
}
