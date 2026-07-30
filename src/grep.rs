//! `hashline_grep` — anchor-annotated content search.
//!
//! Self-contained reimplementation of ripgrep-style content search: the
//! `ignore` crate walks the tree and ripgrep's own engine (`grep-searcher`
//! driving a `grep-regex` matcher) searches each file as a single haystack, so
//! the regex engine's SIMD literal prefilters run across whole files instead of
//! being restarted per line. Match lines carry scheme-aware anchors so a
//! grep → edit workflow needs no intermediate file read.
//!
//! Output format per line: `LINE:ANCHOR:CONTENT` for matches and
//! `LINE:ANCHOR-CONTENT` for context lines (grep-style separators).
//!
//! Only the lines that are actually rendered (matches plus their context) are
//! hashed and anchored: the windows are merged first, expanded to the blocks
//! the scheme's contextual fingerprints fold over, and fed to
//! [`FileIndex::new_partial`], then [`Scheme::anchors_for_range`] runs per
//! window. A single match in a 100,000-line file therefore costs one pass of
//! line splitting plus a handful of line hashes, not a whole-file sweep.

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

use crate::index::FileIndex;
use crate::scheme::Scheme;
use crate::util::{ToolOutcome, Workspace};

/// Default cap on reported match lines.
pub const DEFAULT_MAX_MATCHES: usize = 200;

/// Bytes reserved per rendered line for its anchor and separators, on top of
/// the line's own text: line number, up to two 4-letter hashes, three
/// separators and the newline.
const ANCHOR_RENDER_OVERHEAD: usize = 16;

/// Input for the `hashline_grep` tool.
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
    /// Rendered `LINE:ANCHOR:CONTENT` lines and `--` gap markers, newline
    /// terminated, ready to be appended verbatim to the output.
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
        let idx = line_no - 1;
        let start = idx.saturating_sub(before);
        let end = idx.saturating_add(after).saturating_add(1);
        match spans.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => spans.push(start..end),
        }
    }
    spans
}

/// Search a single file, returning its anchored hit section (if any match).
fn search_file(
    path: &Path,
    rel: PathBuf,
    matcher: &RegexMatcher,
    searcher: &mut Searcher,
    before: usize,
    after: usize,
    scheme: Scheme,
) -> Option<FileHit> {
    let bytes = std::fs::read(path).ok()?;
    // Borrows for valid UTF-8, which is the overwhelmingly common case.
    let content = String::from_utf8_lossy(&bytes);

    let matched = match_lines(matcher, searcher, &content)?;
    if matched.is_empty() {
        return None;
    }

    // The rendered windows decide how much of the file needs hashing: each one
    // is expanded to the blocks its scheme's contextual fingerprints fold over,
    // and nothing else is hashed. That is what keeps a single match in a
    // 50,000-line file from paying for 50,000 line hashes. `usize::MAX` stands
    // in for the not-yet-known line count — both `required_hash_span` and
    // `new_partial` clamp to the real file, so the result is exact.
    let mut spans = included_spans(&matched, before, after);
    let hash_spans: Vec<Range<usize>> = spans
        .iter()
        .map(|span| scheme.required_hash_span(span.clone(), usize::MAX))
        .collect();
    // Anchors still come from the same FileIndex view hashline_read and
    // hashline_edit use, so a grep anchor can be passed straight to an edit.
    let index = FileIndex::new_partial(&content, &hash_spans);
    // `FileIndex` appends the synthetic trailing empty line hashline's 1-based
    // numbering needs; grep numbers lines like `str::lines()` and never renders
    // that line, so searchable lines stop one short of the index for content
    // ending in a newline.
    let line_count = if content.is_empty() || content.ends_with('\n') {
        index.len() - 1
    } else {
        index.len()
    };
    // Trim the windows now that the line count is known. Each span keeps at
    // least its match line, and the hashed blocks computed above cover a
    // superset of what is left.
    for span in &mut spans {
        span.end = span.end.min(line_count);
    }

    let rendered_lines: usize = spans.iter().map(Range::len).sum();
    let avg_line_bytes = content.len() / line_count.max(1);
    let mut body = String::with_capacity(
        rendered_lines
            .saturating_mul(avg_line_bytes.saturating_add(ANCHOR_RENDER_OVERHEAD))
            .saturating_add(spans.len().saturating_mul(3)),
    );

    // Match lines and rendered lines both ascend, so one forward cursor over
    // `matched` decides the `:`/`-` separator without a per-line lookup table.
    let mut cursor = 0usize;
    for (span_position, span) in spans.iter().enumerate() {
        if span_position > 0 {
            body.push_str("--\n");
        }
        for (offset, anchor) in scheme.anchors_for_range(&index, span.clone()).enumerate() {
            let idx = span.start + offset;
            let line_no = idx + 1;
            while matched.get(cursor).is_some_and(|&m| m < line_no) {
                cursor += 1;
            }
            anchor.render_into(&mut body);
            body.push(if matched.get(cursor) == Some(&line_no) {
                ':'
            } else {
                '-'
            });
            body.push_str(index.lines()[idx]);
            body.push('\n');
        }
    }

    Some(FileHit {
        rel,
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
    /// Anchor scheme (`Copy`, built once per server).
    scheme: Scheme,
    /// Running match total across all workers.
    total: AtomicUsize,
    /// The walk stops once `total` exceeds this.
    quit_threshold: usize,
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
            self.ctx.scheme,
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
fn assemble_output(hits: &[FileHit], max_matches: usize) -> String {
    let mut out = String::new();
    let mut shown_matches = 0usize;
    let mut shown_files = 0usize;
    let total_matches: usize = hits.iter().map(|h| h.matches).sum();
    let mut truncated = false;

    for hit in hits {
        if shown_matches >= max_matches {
            truncated = true;
            break;
        }
        let header = hit.rel.to_string_lossy();
        out.reserve(header.len() + hit.body.len() + 2);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&header);
        out.push('\n');
        out.push_str(&hit.body);
        shown_matches += hit.matches;
        shown_files += 1;
    }

    if truncated {
        out.push_str(&format!(
            "\nFound at least {total_matches} matches in {} files; \
             output capped after {shown_matches} matches in {shown_files} files. \
             Refine your pattern or lower the search scope.",
            hits.len()
        ));
    } else {
        out.push_str(&format!(
            "\nFound {total_matches} match(es) in {shown_files} file(s)."
        ));
    }

    out
}

/// Execute a `hashline_grep` request against the local filesystem.
///
/// This is a blocking function — call it via `spawn_blocking` from async
/// contexts.
pub fn run_grep(workspace: &Workspace, input: &HashlineGrepInput, scheme: Scheme) -> ToolOutcome {
    let matcher = match build_matcher(&input.pattern, input.ignore_case.unwrap_or(false)) {
        Ok(matcher) => matcher,
        Err(e) => {
            return ToolOutcome::error(format!("Invalid regex pattern \"{}\": {e}", input.pattern));
        }
    };

    let (before, after) = match input.context {
        Some(c) => (c, c),
        None => (
            input.before_context.unwrap_or(0),
            input.after_context.unwrap_or(0),
        ),
    };
    let max_matches = input.max_matches.unwrap_or(DEFAULT_MAX_MATCHES).max(1);

    let search_root = match workspace.resolve(input.path.as_deref().unwrap_or(".")) {
        Ok(path) => path,
        Err(reason) => return ToolOutcome::error(reason),
    };
    let meta = match std::fs::metadata(&search_root) {
        Ok(m) => m,
        Err(_) => {
            return ToolOutcome::error(format!("Search path not found: {}", search_root.display()));
        }
    };

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
            scheme,
        )
        .into_iter()
        .collect();
        if hits.is_empty() {
            return ToolOutcome::success("No matches found.".to_owned());
        }
        return ToolOutcome::success(assemble_output(&hits, max_matches));
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
        scheme,
        total: AtomicUsize::new(0),
        // Stop walking once we have gathered far more than the cap — enough
        // that path-sorted truncation stays deterministic for any realistic
        // layout.
        quit_threshold: max_matches.saturating_mul(50),
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
    if hits.is_empty() {
        return ToolOutcome::success("No matches found.".to_owned());
    }
    hits.sort_by(|a, b| a.rel.cmp(&b.rel));

    ToolOutcome::success(assemble_output(&hits, max_matches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SchemeConfig;

    fn scheme() -> Scheme {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn ws(root: &Path) -> Workspace {
        Workspace::new(root.to_path_buf(), false)
    }

    fn input(pattern: &str) -> HashlineGrepInput {
        HashlineGrepInput {
            pattern: pattern.to_owned(),
            path: None,
            glob: None,
            ignore_case: None,
            after_context: None,
            before_context: None,
            context: None,
            max_matches: None,
        }
    }

    /// Naive per-line reference implementation — the pre-haystack matching
    /// strategy, kept as the differential oracle for the searcher engine.
    fn reference_match_lines(content: &str, pattern: &str, ignore_case: bool) -> Vec<usize> {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .expect("reference regex builds");
        content
            .lines()
            .enumerate()
            .filter(|(_, line)| re.is_match(line))
            .map(|(i, _)| i + 1)
            .collect()
    }

    #[test]
    fn basic_match_has_anchor_and_separator() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

        let outcome = run_grep(&ws(tmp.path()), &input("beta"), scheme());
        assert!(!outcome.is_error);
        assert!(outcome.text.contains("a.rs"), "{}", outcome.text);
        // Match line: LINE:LOCAL:CTX:CONTENT — 3 colons before content.
        let match_line = outcome
            .text
            .lines()
            .find(|l| l.contains("fn beta"))
            .unwrap();
        assert!(match_line.starts_with("2:"), "{match_line}");
        assert_eq!(match_line.matches(':').count(), 3, "{match_line}");
        assert!(outcome.text.contains("Found 1 match(es) in 1 file(s)."));
    }

    #[test]
    fn context_lines_use_dash_separator() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        let mut inp = input("three");
        inp.context = Some(1);
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        let ctx_line = outcome.text.lines().find(|l| l.contains("two")).unwrap();
        assert!(ctx_line.starts_with("2:"), "{ctx_line}");
        // Context separator is '-' right before content.
        assert!(ctx_line.contains("-two"), "{ctx_line}");
        let match_line = outcome.text.lines().find(|l| l.contains("three")).unwrap();
        assert!(match_line.contains(":three"), "{match_line}");
    }

    #[test]
    fn gap_between_match_groups_marked() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content: String = (1..=30)
            .map(|i| {
                if i == 3 || i == 25 {
                    format!("NEEDLE {i}\n")
                } else {
                    format!("filler {i}\n")
                }
            })
            .collect();
        std::fs::write(tmp.path().join("g.txt"), &content).unwrap();

        let outcome = run_grep(&ws(tmp.path()), &input("NEEDLE"), scheme());
        assert!(outcome.text.contains("--"), "{}", outcome.text);
        assert!(outcome.text.contains("Found 2 match(es)"));
    }

    #[test]
    fn glob_filters_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("b.py"), "needle\n").unwrap();

        let mut inp = input("needle");
        inp.glob = Some("*.rs".to_owned());
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        assert!(outcome.text.contains("a.rs"));
        assert!(!outcome.text.contains("b.py"));
    }

    #[test]
    fn case_insensitive_search() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("c.txt"), "Mixed Case Needle\n").unwrap();

        let mut inp = input("needle");
        assert!(
            run_grep(&ws(tmp.path()), &inp, scheme())
                .text
                .contains("No matches")
        );
        inp.ignore_case = Some(true);
        assert!(
            run_grep(&ws(tmp.path()), &inp, scheme())
                .text
                .contains("Found 1 match(es)")
        );
    }

    #[test]
    fn binary_files_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), b"needle\0needle").unwrap();
        let outcome = run_grep(&ws(tmp.path()), &input("needle"), scheme());
        assert!(outcome.text.contains("No matches found."));
    }

    #[test]
    fn invalid_regex_reports_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outcome = run_grep(&ws(tmp.path()), &input("(unclosed"), scheme());
        assert!(outcome.is_error);
        assert!(outcome.text.contains("Invalid regex"));
    }

    #[test]
    fn single_file_path_searched_directly() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("only.txt"), "hit here\n").unwrap();
        let mut inp = input("hit");
        inp.path = Some("only.txt".to_owned());
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        assert!(outcome.text.contains("only.txt"), "{}", outcome.text);
        assert!(outcome.text.contains("Found 1 match(es)"));
    }

    #[test]
    fn missing_search_path_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inp = input("x");
        inp.path = Some("no/such/dir".to_owned());
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        assert!(outcome.is_error);
        assert!(outcome.text.contains("Search path not found"));
    }

    #[test]
    fn cap_reports_at_least_counts() {
        let tmp = tempfile::TempDir::new().unwrap();
        for f in 0..5 {
            let content: String = (0..10).map(|i| format!("needle {i}\n")).collect();
            std::fs::write(tmp.path().join(format!("f{f}.txt")), &content).unwrap();
        }
        let mut inp = input("needle");
        inp.max_matches = Some(15);
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        assert!(
            outcome.text.contains("at least 50 matches"),
            "{}",
            outcome.text
        );
        assert!(outcome.text.contains("capped"), "{}", outcome.text);
    }

    #[test]
    fn grep_anchor_matches_read_anchor() {
        // Anchors produced by grep must be identical to hashline_read's for
        // the same file/scheme, so they can be passed straight to edit.
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "fn main() {\n    let target = 1;\n}\n";
        std::fs::write(tmp.path().join("x.rs"), content).unwrap();

        let s = scheme();
        let outcome = run_grep(&ws(tmp.path()), &input("target"), s);
        let grep_line = outcome.text.lines().find(|l| l.starts_with("2:")).unwrap();
        // grep format: 2:local:ctx:CONTENT — extract "2:local:ctx".
        let anchor: String = grep_line
            .splitn(4, ':')
            .take(3)
            .collect::<Vec<_>>()
            .join(":");

        let read_text = crate::read::format_hashline_content(content, Some(2), Some(1), s);
        let read_anchor = read_text.split('→').next().unwrap();
        assert_eq!(anchor, read_anchor);
    }

    #[test]
    fn haystack_matching_agrees_with_per_line_reference() {
        // The searcher matches whole files at once; every pattern class whose
        // meaning could shift (line anchors, word boundaries, alternation,
        // CRLF line endings, files without a trailing newline) must still
        // produce exactly the per-line match set.
        let lf = "fn alpha() {\n    let value = 1;\n}\n\nfn beta() {\n    value += 2;\n}\n";
        let corpus: [(&str, &str); 6] = [
            ("lf", lf),
            (
                "crlf",
                "fn alpha() {\r\n    let value = 1;\r\n}\r\n\r\nfn beta() {\r\n",
            ),
            ("no_trailing_newline", "fn alpha() {\n    let value = 1;\n}"),
            ("single_line", "solitary value;"),
            ("blank_lines_only", "\n\n\n"),
            ("unicode", "let π = 3;\n// ναι — value\nfn γ() {}\n"),
        ];
        let patterns = [
            "value",
            "^fn ",
            ";$",
            r"\bvalue\b",
            "alpha|beta|gamma",
            r"fn\s+\w+",
            "^$",
            "^",
            "$",
            "[0-9]*",
            "VALUE",
        ];

        let mut searcher = build_searcher();
        for (label, content) in corpus {
            for pattern in patterns {
                for ignore_case in [false, true] {
                    let matcher = build_matcher(pattern, ignore_case).expect("matcher builds");
                    let actual = match_lines(&matcher, &mut searcher, content)
                        .expect("text content is not binary");
                    let expected = reference_match_lines(content, pattern, ignore_case);
                    assert_eq!(
                        actual, expected,
                        "corpus={label} pattern={pattern:?} ignore_case={ignore_case}"
                    );
                }
            }
        }
    }

    #[test]
    fn crlf_file_renders_stripped_lines_with_anchors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "alpha\r\nbeta target\r\ngamma\r\n";
        std::fs::write(tmp.path().join("crlf.txt"), content).unwrap();

        let mut inp = input("target$");
        inp.context = Some(1);
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        assert!(!outcome.is_error, "{}", outcome.text);
        assert!(
            outcome.text.contains("Found 1 match(es) in 1 file(s)."),
            "{}",
            outcome.text
        );
        // The carriage return is stripped exactly as `str::lines()` does, so
        // rendered content matches what hashline_read would emit.
        let match_line = outcome
            .text
            .lines()
            .find(|l| l.contains("beta target"))
            .unwrap();
        assert!(match_line.ends_with("beta target"), "{match_line:?}");
        assert!(match_line.starts_with("2:"), "{match_line}");

        // And the anchor is byte-identical to hashline_read's for line 2.
        let anchor: String = match_line
            .splitn(4, ':')
            .take(3)
            .collect::<Vec<_>>()
            .join(":");
        let read_text = crate::read::format_hashline_content(content, Some(2), Some(1), scheme());
        assert_eq!(anchor, read_text.split('→').next().unwrap());
    }

    #[test]
    fn context_windows_merge_into_minimal_spans() {
        // Overlapping and merely adjacent windows both collapse, so a `--`
        // marker is only ever emitted across a genuinely skipped line.
        assert_eq!(included_spans(&[5], 2, 2), vec![2..7]);
        assert_eq!(included_spans(&[5, 7], 1, 1), vec![3..8]);
        assert_eq!(included_spans(&[1, 3], 0, 1), vec![0..4]);
        assert_eq!(included_spans(&[1, 5], 0, 0), vec![0..1, 4..5]);
        // Windows are unbounded above; `search_file` trims them to the file.
        assert_eq!(included_spans(&[1], 5, 5), vec![0..6]);
    }

    #[test]
    fn context_past_end_of_file_renders_no_phantom_line() {
        // The trailing context of a match on the last line runs past EOF, and
        // `FileIndex` carries a synthetic trailing empty line beyond that.
        // Neither may be rendered.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("tail.txt"), "alpha\nbeta\nlast target\n").unwrap();

        let mut inp = input("target");
        inp.context = Some(4);
        let outcome = run_grep(&ws(tmp.path()), &inp, scheme());
        assert!(!outcome.is_error, "{}", outcome.text);
        let numbered: Vec<&str> = outcome
            .text
            .lines()
            .filter(|l| {
                l.split_once(':')
                    .is_some_and(|(n, _)| n.parse::<u32>().is_ok())
            })
            .collect();
        assert_eq!(numbered.len(), 3, "{numbered:?}");
        assert!(numbered[2].ends_with("last target"), "{:?}", numbered[2]);
    }

    #[test]
    fn partial_index_anchors_match_full_index_anchors() {
        // Grep hashes only the blocks its rendered windows need. Every anchor
        // it emits must still be identical to the one a whole-file index
        // produces, for every scheme shape — otherwise grep → edit breaks.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut content = String::new();
        for i in 0..200usize {
            if i % 17 == 3 {
                content.push_str(&format!("    let target_{i} = compute();\n"));
            } else {
                content.push_str(&format!("    let filler_{i} = other({i});\n"));
            }
        }
        std::fs::write(tmp.path().join("m.rs"), &content).unwrap();

        let full = FileIndex::new(&content);
        for candidate in [
            Scheme::content_only(3),
            Scheme::chunk(3, 16),
            Scheme::chunk(4, 7),
            Scheme::checkpoint(3, 32),
            Scheme::checkpoint(2, 5),
        ] {
            for context in [0usize, 3] {
                let mut inp = input("target_");
                inp.context = Some(context);
                let outcome = run_grep(&ws(tmp.path()), &inp, candidate);
                assert!(!outcome.is_error, "{}", outcome.text);

                let mut checked = 0usize;
                for line in outcome.text.lines() {
                    // Section headers, gap markers and the summary carry no
                    // leading line number.
                    let Some((number, _)) = line.split_once(':') else {
                        continue;
                    };
                    let Ok(line_no) = number.parse::<usize>() else {
                        continue;
                    };
                    let mut want = String::new();
                    candidate
                        .anchor_at(&full, line_no - 1)
                        .expect("line within file")
                        .render_into(&mut want);
                    assert!(
                        line.starts_with(&want),
                        "scheme={candidate:?} context={context} line={line:?} \
                         expected anchor prefix {want:?}"
                    );
                    checked += 1;
                }
                assert!(checked > 0, "scheme={candidate:?} rendered no anchors");
            }
        }
    }
}
