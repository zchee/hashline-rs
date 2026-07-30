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
//! `hashline_read` — anchor-annotated file reading.
//!
//! Output format: `ANCHOR→CONTENT` (e.g. `22:abc:rst→  let x = 1;`).
//!
//! A read costs one pass over the file: the content is split into lines exactly
//! once, only the lines the requested window needs for its anchors are hashed
//! ([`crate::index::FileIndex::new_partial`] plus
//! [`Scheme::required_hash_span`]), and every rendered line is appended
//! straight into one pre-sized output buffer.

use std::ops::Range;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::index::FileIndex;
use crate::render::render_range;
use crate::scheme::Scheme;
use crate::util::{ToolOutcome, Workspace, decode_utf8};

/// Maximum number of lines returned by a single read.
pub const MAX_LINES_READ: usize = 2000;

/// Bytes inspected for NUL before a file is rejected as binary.
///
/// Matches ripgrep's heuristic: text files with an embedded NUL past the first
/// 8 KiB are vanishingly rare, and scanning the whole buffer is wasted work on
/// large files.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

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
/// the scheme-generated anchor for that line. `offset` is 1-based; an absent
/// `limit` renders to the end of the file.
///
/// Only the window is anchored, and only the lines its anchors depend on are
/// hashed — a contextual fingerprint reaches beyond the window, but only as far
/// as [`Scheme::required_hash_span`] says, never across the whole file. Line
/// numbering still counts from the start of the file, so a window's anchors are
/// byte-identical to the corresponding slice of a full read.
pub fn format_hashline_content(
    file_content: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    scheme: Scheme,
) -> String {
    let window = line_window(offset, limit);
    let index = windowed_index(file_content, window.clone(), scheme);
    render_window(&index, scheme, window)
}

/// The 0-based half-open line window selected by a 1-based `offset` and a
/// `limit`.
fn line_window(offset: Option<usize>, limit: Option<usize>) -> Range<usize> {
    let start = offset.unwrap_or(1).saturating_sub(1);
    // An absent limit reads to the end; `new_partial` and `anchors_for_range`
    // clamp the open end to the file.
    let end = start.saturating_add(limit.unwrap_or(usize::MAX));
    start..end
}

/// Index `content`, hashing only the lines that anchoring `window` requires.
fn windowed_index<'a>(content: &'a str, window: Range<usize>, scheme: Scheme) -> FileIndex<'a> {
    // The line count is unknown until the content is split, so the span is
    // computed against an unbounded file; `new_partial` clamps it to the file.
    let span = scheme.required_hash_span(window, usize::MAX);
    FileIndex::new_partial(content, &[span])
}

/// Render `window` of `index` as newline-separated `ANCHOR→CONTENT` lines.
///
/// The window is clamped to the index, so an out-of-range window renders empty.
/// Shares [`render_range`] with the edit path, which is what makes an edit
/// snippet and a later read of the same lines byte-identical rather than
/// merely intended to be.
fn render_window(index: &FileIndex<'_>, scheme: Scheme, window: Range<usize>) -> String {
    let mut out = String::new();
    render_range(index, scheme, window, &mut out);
    out
}

/// A rendered window plus the file's total line count, from one index build.
struct ReadWindow {
    /// The rendered `ANCHOR→CONTENT` lines.
    text: String,
    /// Total lines in the file, including the synthetic trailing empty line
    /// that content ending in `\n` implies.
    total_lines: usize,
}

/// Split and anchor `content` once, rendering the `offset`/`limit` window.
fn read_window(content: &str, offset: usize, limit: usize, scheme: Scheme) -> ReadWindow {
    let window = line_window(Some(offset), Some(limit));
    let index = windowed_index(content, window.clone(), scheme);
    ReadWindow {
        total_lines: index.len(),
        text: render_window(&index, scheme, window),
    }
}

/// Execute a `hashline_read` request against the local filesystem.
pub async fn run_read(
    workspace: &Workspace,
    input: &HashlineReadInput,
    scheme: Scheme,
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

    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if memchr::memchr(0, sniff).is_some() {
        return ToolOutcome::error(format!(
            "{} appears to be a binary file and cannot be read as text.",
            path.display()
        ));
    }

    let size = bytes.len();
    if size == 0 {
        return ToolOutcome::success(format!("The file {} exists but is empty.", path.display()));
    }

    let offset = input.offset.unwrap_or(1);
    let effective_limit = input.limit.unwrap_or(usize::MAX).min(MAX_LINES_READ);

    // Splitting and hashing a large file would stall the reactor, so the
    // CPU-bound step always runs on a blocking thread. Every size goes through
    // it, not just the large ones: the partial index panics on a programmer
    // error, and on a blocking thread that surfaces as a failed join — one
    // tool call erroring — instead of unwinding through the reactor and taking
    // the session with it. The hop costs a few microseconds; a killed session
    // costs the conversation. The byte buffer moves into the task and the
    // decoded text borrows from it there, so validation happens there too.
    let task = tokio::task::spawn_blocking(move || {
        read_window(&decode_utf8(&bytes), offset, effective_limit, scheme)
    });
    let ReadWindow {
        mut text,
        total_lines,
    } = match task.await {
        Ok(window) => window,
        Err(e) => {
            return ToolOutcome::error(format!("Failed to read {}: {e}", path.display()));
        }
    };

    if offset > total_lines {
        return ToolOutcome::error(format!(
            "offset {offset} is beyond the end of the file ({total_lines} lines)."
        ));
    }

    let last_shown = (offset + effective_limit - 1).min(total_lines);
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
    use crate::scheme::{Anchor, DEFAULT_CHECKPOINT_INTERVAL, DEFAULT_CHUNK_SIZE};
    use crate::testutil::corpus;

    fn scheme() -> Scheme {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn ws(root: &std::path::Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    /// Pre-windowing rendering: a fully hashed index, full-range anchor
    /// generation, `skip`/`take`, and a `format!` per line. Kept as the
    /// differential reference for the windowed renderer.
    fn reference_format(
        content: &str,
        offset: Option<usize>,
        limit: Option<usize>,
        scheme: Scheme,
    ) -> String {
        let index = FileIndex::new(content);
        let skip = offset.unwrap_or(1).saturating_sub(1);
        let take = limit.unwrap_or(usize::MAX);
        scheme
            .anchors_for_range(&index, 0..index.len())
            .skip(skip)
            .take(take)
            .map(|anchor: Anchor| {
                format!(
                    "{}→{}",
                    anchor.render(),
                    index.line(anchor.line - 1).unwrap_or_default()
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    #[test]
    fn format_matches_full_index_reference_across_windows() {
        // The Phase 2 gate: rendering from a partially hashed index must be
        // byte-identical to rendering from a fully hashed one, for every
        // scheme, window position, and file shape.
        for block in [4usize, 16] {
            for seed in 0..4u32 {
                for &trailing_newline in &[true, false] {
                    let content = corpus(120, 0x2EAD_0000_u32.wrapping_add(seed), trailing_newline);
                    let total = FileIndex::new(&content).len();
                    for scheme in [
                        Scheme::content_only(3),
                        Scheme::chunk(3, block),
                        Scheme::checkpoint(3, block),
                    ] {
                        // 1-based offsets straddling block boundaries and EOF.
                        let offsets = [
                            None,
                            Some(1),
                            Some(2),
                            Some(block),
                            Some(block + 1),
                            Some(block + 2),
                            Some(2 * block + 1),
                            Some(total / 2),
                            Some(total - 1),
                            Some(total),
                            Some(total + 5),
                        ];
                        for offset in offsets {
                            for limit in [None, Some(1), Some(block), Some(block + 1), Some(total)]
                            {
                                assert_eq!(
                                    format_hashline_content(&content, offset, limit, scheme),
                                    reference_format(&content, offset, limit, scheme),
                                    "scheme {} block {block} seed {seed} \
                                     trailing_newline {trailing_newline} \
                                     offset {offset:?} limit {limit:?}",
                                    scheme.name()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn format_matches_reference_on_edge_shaped_files() {
        for content in [
            "",
            "\n",
            "\n\n",
            "a",
            "a\n",
            "a\r\nb\r\n",
            "ünïcode\nlines\n",
        ] {
            for scheme in [
                Scheme::content_only(3),
                Scheme::chunk(3, DEFAULT_CHUNK_SIZE),
                Scheme::checkpoint(3, DEFAULT_CHECKPOINT_INTERVAL),
            ] {
                for offset in [None, Some(1), Some(2), Some(3)] {
                    for limit in [None, Some(1), Some(2)] {
                        assert_eq!(
                            format_hashline_content(content, offset, limit, scheme),
                            reference_format(content, offset, limit, scheme),
                            "scheme {} content {content:?} offset {offset:?} limit {limit:?}",
                            scheme.name()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn format_reserves_exactly_one_buffer_for_the_window() {
        // `capacity() >= len()` is a String invariant and would prove nothing.
        // The reservation is recomputed here from the window the renderer was
        // asked for, then checked two ways: it covers the render (no growth
        // mid-loop), and the returned buffer's capacity is still exactly it
        // (so the single reservation really was the only allocation).
        let content = corpus(500, 0x5A1E_0001, true);
        for scheme in [
            Scheme::content_only(4),
            Scheme::chunk(3, DEFAULT_CHUNK_SIZE),
            Scheme::checkpoint(1, DEFAULT_CHECKPOINT_INTERVAL),
        ] {
            for (offset, limit) in [(None, None), (Some(1), Some(100)), (Some(400), Some(50))] {
                let out = format_hashline_content(&content, offset, limit, scheme);

                let index = FileIndex::new(&content);
                let window = line_window(offset, limit);
                let end = window.end.min(index.len());
                let start = window.start.min(end);
                let content_bytes: usize = (start..end)
                    .filter_map(|idx| index.line(idx))
                    .map(str::len)
                    .sum();
                let reserved =
                    content_bytes + (end - start) * crate::render::per_line_overhead(scheme, end);

                let label = format!("scheme {} offset {offset:?}", scheme.name());
                assert!(reserved >= 8, "{label}: fixture too small to pin capacity");
                assert!(
                    reserved >= out.len(),
                    "{label}: reserved {reserved} < rendered {}",
                    out.len()
                );
                assert_eq!(out.capacity(), reserved, "{label}: buffer was regrown");
            }
        }
    }

    #[test]
    fn line_window_translates_offset_and_limit() {
        assert_eq!(line_window(None, None), 0..usize::MAX);
        assert_eq!(line_window(Some(1), Some(10)), 0..10usize);
        assert_eq!(line_window(Some(10), Some(5)), 9..14usize);
        // A 0 offset is treated as the first line; `run_read` rejects it.
        assert_eq!(line_window(Some(0), Some(1)), 0..1usize);
        // A limit of 0 selects nothing rather than overflowing.
        assert_eq!(line_window(Some(5), Some(0)), 4..4usize);
        assert_eq!(
            line_window(Some(usize::MAX), Some(usize::MAX)).end,
            usize::MAX
        );
    }

    #[test]
    fn format_basic_file() {
        let content = "line one\nline two\nline three\n";
        let output = format_hashline_content(content, None, None, scheme());

        for line in output.lines() {
            assert!(line.contains(':'), "missing anchor separator: {line}");
            assert!(line.contains('→'), "missing content separator: {line}");
        }
    }

    #[test]
    fn format_includes_anchor_with_context() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let output = format_hashline_content(content, None, None, scheme());

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
        let output = format_hashline_content(content, Some(2), Some(2), scheme());

        let content_lines: Vec<&str> = output.lines().collect();
        assert_eq!(content_lines.len(), 2);
        assert!(content_lines[0].starts_with("2:"));
        assert!(content_lines[1].starts_with("3:"));
    }

    #[test]
    fn format_empty_file() {
        let output = format_hashline_content("", None, None, scheme());
        assert!(output.starts_with("1:"), "should contain line 1: {output}");
        assert!(output.contains('→'), "should contain arrow separator");
    }

    #[test]
    fn format_keeps_long_lines_whole() {
        let long_line = "x".repeat(5000);
        let content = format!("{long_line}\n");
        let output = format_hashline_content(&content, None, None, scheme());

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
            format_hashline_content(content, None, None, s),
            format_hashline_content(content, None, None, s)
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
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
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
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
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
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
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
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
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
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
        assert!(!outcome.is_error);
        assert!(
            outcome.text.contains("Showing lines 10-14 of 51"),
            "{}",
            outcome.text
        );
        assert!(outcome.text.lines().next().unwrap().starts_with("10:"));
    }

    #[tokio::test]
    async fn read_large_file_matches_the_direct_renderer() {
        // The index+render step always runs on a blocking thread; this is the
        // size at which that matters, so it pins that the tool's output still
        // matches what the renderer produces when called directly.
        let content = corpus(15_000, 0xB10C_0001, true);
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();

        let s = scheme();
        let input = HashlineReadInput {
            path: "big.txt".to_owned(),
            offset: Some(9_000),
            limit: Some(100),
        };
        let outcome = run_read(&ws(tmp.path()), &input, s).await;
        assert!(!outcome.is_error, "{}", outcome.text);

        let expected = format_hashline_content(&content, Some(9_000), Some(100), s);
        assert!(
            outcome.text.starts_with(&expected),
            "the tool's output diverged from the renderer called directly"
        );
        assert!(
            outcome.text.contains("Showing lines 9000-9099 of 15001"),
            "{}",
            outcome.text.lines().last().unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn read_invalid_utf8_falls_back_to_lossy() {
        let tmp = tempfile::TempDir::new().unwrap();
        // 0xFF is never valid UTF-8, but it is not NUL, so this is text.
        std::fs::write(tmp.path().join("latin.txt"), b"caf\xE9 au lait\n").unwrap();
        let input = HashlineReadInput {
            path: "latin.txt".to_owned(),
            offset: None,
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(
            outcome.text.contains('\u{FFFD}'),
            "invalid bytes should become replacement characters: {}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn read_nul_beyond_sniff_window_is_treated_as_text() {
        // Documents the sniff heuristic: only the first `BINARY_SNIFF_BYTES`
        // decide binariness, so a NUL past that point no longer rejects the
        // file. Matches ripgrep's behavior.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut content = "text line\n"
            .repeat(BINARY_SNIFF_BYTES / 10 + 16)
            .into_bytes();
        content.extend_from_slice(b"tail\0nul\n");
        std::fs::write(tmp.path().join("late-nul.txt"), &content).unwrap();

        let input = HashlineReadInput {
            path: "late-nul.txt".to_owned(),
            offset: None,
            limit: Some(1),
        };
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
        assert!(!outcome.is_error, "{}", outcome.text);

        // A NUL inside the sniff window still rejects the file.
        std::fs::write(tmp.path().join("early-nul.txt"), b"a\0b\n").unwrap();
        let input = HashlineReadInput {
            path: "early-nul.txt".to_owned(),
            offset: None,
            limit: None,
        };
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("binary"));
    }

    #[tokio::test]
    async fn read_reports_total_lines_from_the_same_index() {
        // `total_lines` in the trailer must agree with a whole-file split.
        let tmp = tempfile::TempDir::new().unwrap();
        for content in ["a\nb\nc\n", "a\nb\nc", "\n", "x"] {
            std::fs::write(tmp.path().join("t.txt"), content).unwrap();
            let input = HashlineReadInput {
                path: "t.txt".to_owned(),
                offset: Some(2),
                limit: Some(1),
            };
            let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
            let total = FileIndex::new(content).len();
            if total < 2 {
                assert!(outcome.is_error, "content {content:?}: {}", outcome.text);
                continue;
            }
            assert!(!outcome.is_error, "content {content:?}: {}", outcome.text);
            assert!(
                outcome.text.contains(&format!("of {total}.")),
                "content {content:?}: {}",
                outcome.text
            );
        }
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
        let outcome = run_read(&ws(tmp.path()), &input, scheme()).await;
        assert!(outcome.is_error);
        assert!(outcome.text.contains("beyond the end"));
    }
}
