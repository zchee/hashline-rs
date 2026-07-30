//! `hashline_grep` — anchor-annotated content search.
//!
//! Self-contained reimplementation of ripgrep-style content search using the
//! `regex` and `ignore` crates. Match lines carry scheme-aware anchors so a
//! grep → edit workflow needs no intermediate file read.
//!
//! Output format per line: `LINE:ANCHOR:CONTENT` for matches and
//! `LINE:ANCHOR-CONTENT` for context lines (grep-style separators).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ignore::{WalkBuilder, WalkState, overrides::OverrideBuilder};
use regex::{Regex, RegexBuilder};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::scheme::{AnchorScheme, split_lines};
use crate::util::{ToolOutcome, resolve_path};

/// Default cap on reported match lines.
pub const DEFAULT_MAX_MATCHES: usize = 200;

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

/// One rendered line of a file section: `(line_number, is_match, text)`.
type RenderedLine = (usize, bool, String);

/// Per-file search result.
struct FileHit {
    rel: PathBuf,
    lines: Vec<RenderedLine>,
    matches: usize,
}

/// Search a single file, returning its anchored hit section (if any match).
fn search_file(
    path: &Path,
    rel: PathBuf,
    re: &Regex,
    before: usize,
    after: usize,
    scheme: &dyn AnchorScheme,
) -> Option<FileHit> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None; // binary
    }
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.lines().collect();

    let match_idxs: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| re.is_match(line))
        .map(|(i, _)| i)
        .collect();
    if match_idxs.is_empty() {
        return None;
    }

    // Anchors are generated from the full split_lines view so contextual
    // fingerprints match what hashline_read/hashline_edit compute.
    let anchors = scheme.generate_anchors(&split_lines(&content));

    let mut is_match = vec![false; lines.len()];
    let mut included = vec![false; lines.len()];
    for &m in &match_idxs {
        is_match[m] = true;
        let start = m.saturating_sub(before);
        let end = (m + after).min(lines.len() - 1);
        for flag in &mut included[start..=end] {
            *flag = true;
        }
    }

    let rendered: Vec<RenderedLine> = included
        .iter()
        .enumerate()
        .filter(|&(_, inc)| *inc)
        .map(|(i, _)| {
            let a = &anchors[i];
            let suffix = match &a.context {
                Some(ctx) => format!("{}:{ctx}", a.local),
                None => a.local.clone(),
            };
            let sep = if is_match[i] { ':' } else { '-' };
            (
                i + 1,
                is_match[i],
                format!("{}:{suffix}{sep}{}", i + 1, lines[i]),
            )
        })
        .collect();

    Some(FileHit {
        rel,
        lines: rendered,
        matches: match_idxs.len(),
    })
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
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&hit.rel.display().to_string());
        out.push('\n');

        let mut prev_line: Option<usize> = None;
        for (line_no, _, text) in &hit.lines {
            if let Some(prev) = prev_line
                && *line_no > prev + 1
            {
                out.push_str("--\n");
            }
            out.push_str(text);
            out.push('\n');
            prev_line = Some(*line_no);
        }
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
pub fn run_grep(root: &Path, input: &HashlineGrepInput, scheme: &dyn AnchorScheme) -> ToolOutcome {
    let re = match RegexBuilder::new(&input.pattern)
        .case_insensitive(input.ignore_case.unwrap_or(false))
        .build()
    {
        Ok(re) => re,
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

    let search_root = resolve_path(root, input.path.as_deref().unwrap_or("."));
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
        let hits: Vec<FileHit> = search_file(&search_root, rel, &re, before, after, scheme)
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

    let hits: Mutex<Vec<FileHit>> = Mutex::new(Vec::new());
    let total: AtomicUsize = AtomicUsize::new(0);
    // Stop walking once we have gathered far more than the cap — enough that
    // path-sorted truncation stays deterministic for any realistic layout.
    let quit_threshold = max_matches.saturating_mul(50);

    builder.build_parallel().run(|| {
        Box::new(|entry| {
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&search_root)
                .unwrap_or(entry.path())
                .to_path_buf();
            if let Some(hit) = search_file(entry.path(), rel, &re, before, after, scheme) {
                let found = total.fetch_add(hit.matches, Ordering::Relaxed) + hit.matches;
                hits.lock().expect("grep hits mutex poisoned").push(hit);
                if found > quit_threshold {
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });

    let mut hits = hits.into_inner().expect("grep hits mutex poisoned");
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

    fn scheme() -> Box<dyn AnchorScheme> {
        SchemeConfig::default().build_scheme().unwrap()
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

    #[test]
    fn basic_match_has_anchor_and_separator() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();

        let outcome = run_grep(tmp.path(), &input("beta"), &*scheme());
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
        let outcome = run_grep(tmp.path(), &inp, &*scheme());
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

        let outcome = run_grep(tmp.path(), &input("NEEDLE"), &*scheme());
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
        let outcome = run_grep(tmp.path(), &inp, &*scheme());
        assert!(outcome.text.contains("a.rs"));
        assert!(!outcome.text.contains("b.py"));
    }

    #[test]
    fn case_insensitive_search() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("c.txt"), "Mixed Case Needle\n").unwrap();

        let mut inp = input("needle");
        assert!(
            run_grep(tmp.path(), &inp, &*scheme())
                .text
                .contains("No matches")
        );
        inp.ignore_case = Some(true);
        assert!(
            run_grep(tmp.path(), &inp, &*scheme())
                .text
                .contains("Found 1 match(es)")
        );
    }

    #[test]
    fn binary_files_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bin.dat"), b"needle\0needle").unwrap();
        let outcome = run_grep(tmp.path(), &input("needle"), &*scheme());
        assert!(outcome.text.contains("No matches found."));
    }

    #[test]
    fn invalid_regex_reports_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let outcome = run_grep(tmp.path(), &input("(unclosed"), &*scheme());
        assert!(outcome.is_error);
        assert!(outcome.text.contains("Invalid regex"));
    }

    #[test]
    fn single_file_path_searched_directly() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("only.txt"), "hit here\n").unwrap();
        let mut inp = input("hit");
        inp.path = Some("only.txt".to_owned());
        let outcome = run_grep(tmp.path(), &inp, &*scheme());
        assert!(outcome.text.contains("only.txt"), "{}", outcome.text);
        assert!(outcome.text.contains("Found 1 match(es)"));
    }

    #[test]
    fn missing_search_path_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut inp = input("x");
        inp.path = Some("no/such/dir".to_owned());
        let outcome = run_grep(tmp.path(), &inp, &*scheme());
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
        let outcome = run_grep(tmp.path(), &inp, &*scheme());
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
        let outcome = run_grep(tmp.path(), &input("target"), &*s);
        let grep_line = outcome.text.lines().find(|l| l.starts_with("2:")).unwrap();
        // grep format: 2:local:ctx:CONTENT — extract "2:local:ctx".
        let anchor: String = grep_line
            .splitn(4, ':')
            .take(3)
            .collect::<Vec<_>>()
            .join(":");

        let read_text = crate::read::format_hashline_content(content, Some(2), Some(1), &*s);
        let read_anchor = read_text.split('→').next().unwrap();
        assert_eq!(anchor, read_anchor);
    }
}
