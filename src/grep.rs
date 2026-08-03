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
//! `grep` — versioned positional content search (hashline protocol v2).
//!
//! The `ignore` crate walks the tree and ripgrep's engine searches each file as
//! a single haystack. Matching files are converted into [`Snapshot`]s so each
//! hit section carries a snapshot header and `LINE@BYTE` positions that edit
//! can validate. No per-line content hashes or [`FileIndex`] are built.

use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkFinish, SinkMatch};
use ignore::{
    DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState,
    overrides::OverrideBuilder,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::protocol::{
    ErrorResponse, GrepLine, GrepLineKind, GrepRequest, GrepSummary, GrepTarget,
    GrepText, Position, ProtocolError, SnapshotHeader, classify_grep_text,
};
use crate::snapshot::Snapshot;
use crate::util::{ToolOutcome, Workspace};

/// Frozen incompatible-v2 request schema for the snapshot-bearing grep phase.
///
/// The current anchor search engine remains isolated behind HashlineGrepInput
/// until Phase 5 replaces its renderer; this type is the only v2 wire contract.
pub use crate::protocol::GrepRequest as HashlineGrepV2Input;

/// Default cap on reported match lines.
pub const DEFAULT_MAX_MATCHES: usize = 200;

/// Input for the `grep` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HashlineGrepInput {
    /// Regular expression to search for (e.g. `log.*Error`, `function\s+\w+`).
    pub pattern: String,

    /// File or directory to search in (relative to the workspace root or
    /// absolute). Defaults to the workspace root.
    #[serde(default)]
    pub path: Option<String>,

    /// Glob pattern to filter files (e.g. `*.rs`, `src/**/*.ts`).
    #[serde(default)]
    pub glob: Option<String>,

    /// Case-insensitive search.
    #[serde(default)]
    pub ignore_case: Option<bool>,

    /// Number of context lines to show after each match (like `grep -A`).
    #[serde(default)]
    pub after_context: Option<usize>,

    /// Number of context lines to show before each match (like `grep -B`).
    #[serde(default)]
    pub before_context: Option<usize>,

    /// Number of context lines to show around each match (like `grep -C`;
    /// overrides `after_context`/`before_context`).
    #[serde(default)]
    pub context: Option<usize>,

    /// Maximum number of match lines to report (default 200).
    #[serde(default)]
    pub max_matches: Option<usize>,
}

/// Per-file search result: the fully rendered hit section plus its match count.
struct FileHit {
    /// Path relative to the search root, as rendered in the section header.
    rel: PathBuf,
    /// Snapshot header for this file section.
    header: String,
    /// Rendered `LINE@BYTE:CONTENT` / `LINE@BYTE-CONTENT` lines and `--` gaps.
    body: String,
    /// Number of match lines (context lines excluded) in `body`.
    matches: usize,
}

/// Collects the 1-based line numbers of matching lines from a searcher run.
///
/// The searcher is configured without context, so every callback is a match
/// line; line numbers arrive in ascending order.
#[derive(Debug, Default)]
struct MatchLineSink {
    /// 1-based match line numbers, ascending and deduplicated.
    lines: Vec<usize>,
    /// Whether the searcher classified the content as binary and quit.
    binary: bool,
}

impl Sink for MatchLineSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        if let Some(number) = mat.line_number()
            && let Ok(number) = usize::try_from(number)
        {
            // A single match callback covers one whole line, but guard against
            // repeats so the caller can rely on a strictly ascending list.
            if self.lines.last() != Some(&number) {
                self.lines.push(number);
            }
        }
        Ok(true)
    }

    fn finish(&mut self, _searcher: &Searcher, finish: &SinkFinish) -> Result<(), io::Error> {
        self.binary = finish.binary_byte_offset().is_some();
        Ok(())
    }
}

/// Build the regex matcher backing a search.
///
/// `multi_line` makes `^`/`$` line anchors so the pattern keeps its per-line
/// meaning while matching over a whole-file haystack, and `crlf` extends those
/// anchors across `\r\n` and bans the line terminator from character classes —
/// together they reproduce "run the pattern against each `str::lines()` line"
/// semantics exactly, including on CRLF files.
fn build_matcher(pattern: &str, ignore_case: bool) -> Result<RegexMatcher, grep_regex::Error> {
    RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .multi_line(true)
        .crlf(true)
        .build(pattern)
}

/// Build a searcher configured for line-numbered, context-free matching.
///
/// Context is expanded by this module (spans are merged before anchoring), and
/// binary files are detected by the searcher itself: a NUL byte stops the
/// search and is reported through [`SinkFinish::binary_byte_offset`].
fn build_searcher() -> Searcher {
    SearcherBuilder::new()
        .line_terminator(LineTerminator::crlf())
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .bom_sniffing(false)
        .build()
}

/// Collect the 1-based line numbers of every matching line in `content`.
///
/// Returns `None` if the content was classified as binary or the search
/// failed.
fn match_lines(
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    content: &str,
) -> Option<Vec<usize>> {
    let mut sink = MatchLineSink::default();
    searcher
        .search_slice(matcher, content.as_bytes(), &mut sink)
        .ok()?;
    if sink.binary {
        return None;
    }
    Some(sink.lines)
}

/// Merge each match's `±context` window into a minimal set of ascending,
/// non-adjacent 0-based line spans.
///
/// Windows are unbounded above: the file's line count is only known once the
/// index has split it, so the caller trims the spans afterwards. Trimming
/// cannot change how they merge, because every match line is below the line
/// count and therefore below any window end that trimming would shorten.
///
/// Adjacent windows are merged as well as overlapping ones, so the gap between
/// two returned spans is always at least one unrendered line — exactly where a
/// `--` marker belongs.
fn included_spans(match_lines: &[usize], before: usize, after: usize) -> Vec<Range<usize>> {
    let mut spans: Vec<Range<usize>> = Vec::new();
    for &line_no in match_lines {
        // The sink only reports 1-based line numbers, so this never saturates;
        // it is written this way because every other index conversion here is.
        let idx = line_no.saturating_sub(1);
        let start = idx.saturating_sub(before);
        let end = idx.saturating_add(after).saturating_add(1);
        match spans.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => spans.push(start..end),
        }
    }
    spans
}

/// Search a single file, returning a positional hit section (if any match).
#[allow(clippy::too_many_arguments)]
fn search_file(
    path: &Path,
    rel: PathBuf,
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    before: usize,
    after: usize,
    target: GrepTarget,
    skipped_binary: &AtomicUsize,
    skipped_utf8: &AtomicUsize,
) -> Option<FileHit> {
    let bytes = std::fs::read(path).ok()?;
    let text = match classify_grep_text(&bytes, target) {
        Ok(GrepText::Search(text)) => text.to_owned(),
        Ok(GrepText::SkipBinary) => {
            skipped_binary.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Ok(GrepText::SkipInvalidUtf8) => {
            skipped_utf8.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Err(_) => {
            return None;
        }
    };

    let matched = match_lines(matcher, searcher, &text)?;
    if matched.is_empty() {
        return None;
    }

    let snapshot = Snapshot::from_bytes(bytes).ok()?;
    let _ = snapshot.materialize_offsets().ok()?;
    let line_count = snapshot.line_count();
    // Grep match line numbers follow str::lines()-style counts used by the
    // searcher (no synthetic trailing empty line for trailing LF).
    let searchable = if text.is_empty() || text.ends_with('\n') {
        line_count.saturating_sub(1)
    } else {
        line_count
    };
    let searchable = usize::try_from(searchable).unwrap_or(usize::MAX);

    let mut spans = included_spans(&matched, before, after);
    for span in &mut spans {
        span.end = span.end.min(searchable);
    }

    let header = SnapshotHeader::new(
        rel.to_string_lossy().into_owned(),
        snapshot.id(),
        snapshot.line_count(),
        snapshot.byte_len(),
    )
    .ok()?
    .render();

    let mut body = String::new();
    let mut cursor = 0usize;
    for (span_position, span) in spans.iter().enumerate() {
        if span_position > 0 {
            body.push_str("--\n");
        }
        for idx in span.start..span.end {
            let line_no = idx + 1;
            let line_u64 = line_no as u64;
            let start_byte = snapshot.line_start(line_u64).ok().flatten()?;
            let end_byte = if line_u64 < snapshot.line_count() {
                snapshot.line_start(line_u64 + 1).ok().flatten()?
            } else {
                snapshot.byte_len()
            };
            let start_u = usize::try_from(start_byte).ok()?;
            let end_u = usize::try_from(end_byte).ok()?;
            let mut content_end = end_u;
            let raw = snapshot.text().as_bytes();
            if content_end > start_u && raw[content_end - 1] == b'\n' {
                content_end -= 1;
                if content_end > start_u && raw[content_end - 1] == b'\r' {
                    content_end -= 1;
                }
            }
            let content = &snapshot.text()[start_u..content_end];
            while matched.get(cursor).is_some_and(|&m| m < line_no) {
                cursor += 1;
            }
            let kind = if matched.get(cursor) == Some(&line_no) {
                GrepLineKind::Match
            } else {
                GrepLineKind::Context
            };
            let position = Position::new(line_u64, start_byte).ok()?;
            let line = GrepLine {
                position,
                kind,
                content: content.to_owned(),
            };
            body.push_str(&line.render());
            body.push('\n');
        }
    }

    Some(FileHit {
        rel,
        header,
        body,
        matches: matched.len(),
    })
}

/// Read-only state shared by every worker of one grep request's walk.
struct SearchContext<'a> {
    /// Compiled pattern; `RegexMatcher` is `Sync`, so all workers share one.
    matcher: &'a RegexMatcher,
    /// Search root, used to render section headers as relative paths.
    root: &'a Path,
    /// Context lines before each match.
    before: usize,
    /// Context lines after each match.
    after: usize,
    /// Running match total across all workers.
    total: AtomicUsize,
    /// The walk stops once `total` exceeds this.
    quit_threshold: usize,
    /// Tree entries skipped as binary (NUL).
    skipped_binary: AtomicUsize,
    /// Tree entries skipped as invalid UTF-8.
    skipped_invalid_utf8: AtomicUsize,
}

/// Per-worker visitor: accumulates hits thread-locally and hands the whole
/// batch to the shared collector exactly once, when the worker finishes.
struct GrepVisitor<'a> {
    ctx: &'a SearchContext<'a>,
    collected: &'a Mutex<Vec<Vec<FileHit>>>,
    searcher: Searcher,
    hits: Vec<FileHit>,
}

impl ParallelVisitor for GrepVisitor<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            return WalkState::Continue;
        }
        let rel = entry
            .path()
            .strip_prefix(self.ctx.root)
            .unwrap_or(entry.path())
            .to_path_buf();
        if let Some(hit) = search_file(
            entry.path(),
            rel,
            self.ctx.matcher,
            &mut self.searcher,
            self.ctx.before,
            self.ctx.after,
            GrepTarget::TreeEntry,
            &self.ctx.skipped_binary,
            &self.ctx.skipped_invalid_utf8,
        ) {
            let found = self.ctx.total.fetch_add(hit.matches, Ordering::Relaxed) + hit.matches;
            self.hits.push(hit);
            if found > self.ctx.quit_threshold {
                return WalkState::Quit;
            }
        }
        WalkState::Continue
    }
}

impl Drop for GrepVisitor<'_> {
    /// Merge this worker's batch into the shared collector.
    ///
    /// The walker drops every visitor once its worker thread is done — both on
    /// normal completion and after a [`WalkState::Quit`] — so this is the
    /// single point of cross-thread synchronization per worker rather than per
    /// matching file.
    fn drop(&mut self) {
        if self.hits.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.hits);
        self.collected
            .lock()
            .expect("grep collector mutex poisoned")
            .push(batch);
    }
}

/// Builds one [`GrepVisitor`] per worker thread of the parallel walk.
struct GrepVisitorBuilder<'a> {
    ctx: &'a SearchContext<'a>,
    collected: &'a Mutex<Vec<Vec<FileHit>>>,
}

impl<'a> ParallelVisitorBuilder<'a> for GrepVisitorBuilder<'a> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 'a> {
        Box::new(GrepVisitor {
            ctx: self.ctx,
            collected: self.collected,
            searcher: build_searcher(),
            hits: Vec::new(),
        })
    }
}

/// Assemble the final output text from sorted per-file hits.
fn assemble_output(
    hits: &[FileHit],
    max_matches: usize,
    skipped_binary: u64,
    skipped_invalid_utf8: u64,
) -> String {
    let mut out = String::new();
    let mut shown_matches = 0usize;
        let mut truncated = false;

    for hit in hits {
        if shown_matches >= max_matches {
            truncated = true;
            break;
        }
        out.reserve(hit.header.len() + hit.body.len() + 2);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&hit.header);
        out.push('\n');
        out.push_str(&hit.body);
        shown_matches = shown_matches.saturating_add(hit.matches);
    }

    let summary = GrepSummary {
        matches: shown_matches as u64,
        truncated,
        skipped_binary,
        skipped_invalid_utf8,
    };
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&summary.render());
    out
}

fn protocol_outcome(error: ProtocolError) -> ToolOutcome {
    let envelope = ErrorResponse::new(error);
    match serde_json::to_string_pretty(&envelope) {
        Ok(text) => ToolOutcome::error(text),
        Err(_) => ToolOutcome::error(envelope.error.message),
    }
}

/// Execute a v2 `grep` request against the local filesystem.
///
/// Blocking — call via `spawn_blocking` from async contexts.
pub fn run_grep(workspace: &Workspace, input: &GrepRequest) -> ToolOutcome {
    if let Err(error) = input.validate() {
        return protocol_outcome(ProtocolError::from(error));
    }

    let matcher = match build_matcher(&input.pattern, input.ignore_case) {
        Ok(matcher) => matcher,
        Err(e) => {
            return protocol_outcome(ProtocolError::new(
                crate::protocol::ErrorCode::InvalidPattern,
                format!("Invalid regex pattern \"{}\": {e}", input.pattern),
            ));
        }
    };

    let (before_u16, after_u16) = input.effective_context();
    let before = usize::from(before_u16);
    let after = usize::from(after_u16);
    let max_matches = usize::from(input.max_matches.max(1));

    let search_root = match workspace.resolve(input.path.as_deref().unwrap_or(".")) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };
    let meta = match std::fs::metadata(&search_root) {
        Ok(m) => m,
        Err(_) => {
            return protocol_outcome(ProtocolError::new(
                crate::protocol::ErrorCode::NotFound,
                format!("Search path not found: {}", search_root.display()),
            ));
        }
    };

    let skipped_binary = AtomicUsize::new(0);
    let skipped_utf8 = AtomicUsize::new(0);

    // Single-file search short-circuits the walker.
    if meta.is_file() {
        let rel = search_root
            .file_name()
            .map_or_else(|| search_root.clone(), PathBuf::from);
        let mut searcher = build_searcher();
        let hits: Vec<FileHit> = search_file(
            &search_root,
            rel,
            &matcher,
            &mut searcher,
            before,
            after,
            GrepTarget::ExplicitFile,
            &skipped_binary,
            &skipped_utf8,
        )
        .into_iter()
        .collect();
        if hits.is_empty() {
            return ToolOutcome::success(assemble_output(
                &[],
                max_matches,
                skipped_binary.load(Ordering::Relaxed) as u64,
                skipped_utf8.load(Ordering::Relaxed) as u64,
            ));
        }
        return ToolOutcome::success(assemble_output(
            &hits,
            max_matches,
            skipped_binary.load(Ordering::Relaxed) as u64,
            skipped_utf8.load(Ordering::Relaxed) as u64,
        ));
    }

    let mut builder = WalkBuilder::new(&search_root);
    if let Some(ref glob) = input.glob {
        let mut overrides = OverrideBuilder::new(&search_root);
        if let Err(e) = overrides.add(glob) {
            return ToolOutcome::error(format!("Invalid glob \"{glob}\": {e}"));
        }
        match overrides.build() {
            Ok(ov) => {
                builder.overrides(ov);
            }
            Err(e) => {
                return ToolOutcome::error(format!("Invalid glob \"{glob}\": {e}"));
            }
        }
    }

    let ctx = SearchContext {
        matcher: &matcher,
        root: &search_root,
        before,
        after,
        total: AtomicUsize::new(0),
        // Bound over-collection roughly to O(workers * max_matches); 8 is a
        // typical ignore parallel fan-out upper bound on developer machines.
        quit_threshold: max_matches.saturating_mul(8),
        skipped_binary: AtomicUsize::new(0),
        skipped_invalid_utf8: AtomicUsize::new(0),
    };
    let collected: Mutex<Vec<Vec<FileHit>>> = Mutex::new(Vec::new());

    builder.build_parallel().visit(&mut GrepVisitorBuilder {
        ctx: &ctx,
        collected: &collected,
    });

    let batches = collected
        .into_inner()
        .expect("grep collector mutex poisoned");
    let mut hits: Vec<FileHit> = batches.into_iter().flatten().collect();
    hits.sort_by(|a, b| a.rel.cmp(&b.rel));

    ToolOutcome::success(assemble_output(
        &hits,
        max_matches,
        ctx.skipped_binary.load(Ordering::Relaxed) as u64,
        ctx.skipped_invalid_utf8.load(Ordering::Relaxed) as u64,
    ))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::GrepRequest;
    use crate::util::Workspace;

    fn ws(root: &std::path::Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    fn req(pattern: &str) -> GrepRequest {
        GrepRequest {
            pattern: pattern.to_owned(),
            path: None,
            glob: None,
            ignore_case: false,
            before_context: None,
            after_context: None,
            context: None,
            max_matches: 200,
        }
    }

    #[test]
    fn basic_match_emits_snapshot_header_and_position() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha\nbeta target\ngamma\n").unwrap();
        let mut input = req("target");
        input.path = Some(".".into());
        let outcome = run_grep(&ws(tmp.path()), &input);
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("[hashline-v2 snapshot="), "{}", outcome.text);
        assert!(outcome.text.contains("@"), "{}", outcome.text);
        assert!(outcome.text.contains("target"), "{}", outcome.text);
        assert!(outcome.text.contains("matches="), "{}", outcome.text);
    }

    #[test]
    fn invalid_pattern_is_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outcome = run_grep(&ws(tmp.path()), &req("(unclosed"));
        assert!(outcome.is_error, "{}", outcome.text);
    }

    #[test]
    fn included_spans_merge_windows() {
        assert_eq!(included_spans(&[5], 2, 2), vec![2..7]);
        assert_eq!(included_spans(&[5, 7], 1, 1), vec![3..8]);
    }

    #[test]
    fn no_match_summary() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing here\n").unwrap();
        let outcome = run_grep(&ws(tmp.path()), &req("zzz_no_match"));
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(outcome.text.contains("matches=0"), "{}", outcome.text);
    }
}
