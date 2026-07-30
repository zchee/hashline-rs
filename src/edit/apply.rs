//! Core edit-application logic for `hashline_edit`.
//!
//! Validates anchors against the pre-edit file snapshot, detects overlapping
//! edits, sorts operations bottom-up, and applies them. Returns a fresh-anchor
//! snippet of the edited region.

use super::range_policy;
use super::types::{
    HashlineEditError, HashlineEditErrorKind, HashlineEditOutput, HashlineEditsApplied, HashlineOp,
};
use crate::read::format_hashline_content;
use crate::scheme::{
    Anchor, AnchorScheme, DEFAULT_SEARCH_RADIUS, ParsedAnchor, ShiftResult, ValidationResult,
    split_lines,
};

const SNIPPET_CONTEXT: usize = 3;

/// Generate a scheme-appropriate format label and example anchor for error messages.
///
/// Probes the scheme with a single-line sample to determine whether it uses
/// a context hash (e.g. `"22:abc:rst"`) or only a local hash (e.g. `"22:abc"`).
fn anchor_format_hint(scheme: &dyn AnchorScheme) -> (&'static str, String) {
    let len = scheme.hash_len().clamp(1, 4);
    let hash = &"abcd"[..len];
    let has_context = scheme
        .generate_anchors(&["x"])
        .first()
        .is_some_and(|a| a.context.is_some());
    if has_context {
        let ctx = &"rstu"[..len];
        ("LINE:HASH1:HASH2", format!("22:{hash}:{ctx}"))
    } else {
        ("LINE:HASH", format!("22:{hash}"))
    }
}

/// Format an anchor's local+context as `"local:ctx"` or `"local"`.
fn anchor_suffix(a: &Anchor) -> String {
    match &a.context {
        Some(ctx) => format!("{}:{ctx}", a.local),
        None => a.local.clone(),
    }
}

/// Check whether any line in `content` starts with an anchor prefix
/// (e.g. `"22:abc:rst→..."` or `"axy:edj->..."`).
/// Returns the first offending line (1-based) if found.
fn detect_anchor_prefix_in_content(content: &str) -> Option<usize> {
    for (idx, line) in content.lines().enumerate() {
        let s = line.trim_start();
        if let Some((before, _)) = s.split_once('\u{2192}')
            && before.len() <= 25
            && before.contains(':')
            && !before.contains(' ')
        {
            return Some(idx + 1);
        }
        if let Some((before, _)) = s.split_once("->")
            && before.len() <= 25
            && before.contains(':')
            && !before.contains(' ')
        {
            return Some(idx + 1);
        }
    }
    None
}

fn anchor_content_error(op_label: &str, content: &str, line_num: usize) -> HashlineEditError {
    // Build a small context snippet (up to 3 lines around the offending line).
    let lines: Vec<&str> = content.lines().collect();
    let ctx_start = line_num.saturating_sub(1).saturating_sub(1); // 1 line before (0-based)
    let ctx_end = (line_num + 1).min(lines.len()); // 1 line after
    let context: String = (ctx_start..ctx_end)
        .map(|i| {
            let marker = if i + 1 == line_num { ">>>" } else { "   " };
            format!("{marker} line {}: {}", i + 1, lines[i])
        })
        .collect::<Vec<_>>()
        .join("\n");

    HashlineEditError {
        error: HashlineEditErrorKind::InvalidInput,
        message: format!(
            "{op_label} content contains anchor prefixes (e.g. \"22:abc:rst\u{2192}\") \
             copied from hashline_read output. The first offending line is line {line_num}. \
             Strip the anchor prefixes and the \u{2192} separator from every line, \
             keeping only the actual file content, then retry."
        ),
        context: Some(context),
        context_start_line: Some(ctx_start + 1),
        shifted_anchor: None,
    }
}

/// Format `"LINE:SUFFIX→CONTENT"`.
fn render_anchored_line(a: &Anchor, content: &str) -> String {
    format!("{}:{}→{content}", a.line, anchor_suffix(a))
}

/// A validated, resolved edit operation ready for application.
/// All line indices are 0-based.
#[derive(Debug)]
struct ResolvedOp {
    /// Original index in the input batch (for stable ordering).
    original_idx: usize,
    /// Start line (0-based, inclusive).
    start: usize,
    /// End line (0-based, exclusive). For insert_after, start == end (insertion point).
    end: usize,
    /// Replacement lines (empty = delete).
    new_lines: Vec<String>,
}

/// Result of [`apply_edits`]: the output to return to the caller, plus the new
/// file content on success (to be written to disk by the tool layer).
pub struct ApplyResult {
    /// The structured output (success or error).
    pub output: HashlineEditOutput,
    /// The new file content string. `Some` only when `output` is
    /// `EditsApplied`; `None` on error.
    pub new_content: Option<String>,
}

/// Apply a batch of hashline edit operations to file content.
///
/// Validates all anchors against `content` before applying any edits.
/// Returns both the structured output and the new file content (if
/// successful), so the caller can write to disk without re-deriving the
/// content through a separate code path.
pub fn apply_edits(content: &str, ops: &[HashlineOp], scheme: &dyn AnchorScheme) -> ApplyResult {
    let lines = split_lines(content);

    if ops.len() == 1
        && let HashlineOp::Write {
            content: new_content,
        } = &ops[0]
    {
        if let Some(line_num) = detect_anchor_prefix_in_content(new_content) {
            return ApplyResult {
                output: HashlineEditOutput::Error(anchor_content_error(
                    "write",
                    new_content,
                    line_num,
                )),
                new_content: None,
            };
        }
        return ApplyResult {
            output: build_write_result(new_content, scheme),
            new_content: Some(new_content.clone()),
        };
    }

    let mut resolved: Vec<ResolvedOp> = Vec::with_capacity(ops.len());

    for (idx, op) in ops.iter().enumerate() {
        match resolve_op(op, idx, &lines, scheme) {
            Ok(r) => resolved.push(r),
            Err(mut e) => {
                if ops.len() > 1 {
                    let op_label = match op {
                        HashlineOp::Replace { .. } => "replace",
                        HashlineOp::InsertAfter { .. } => "insert_after",
                        HashlineOp::Write { .. } => "write",
                    };
                    e.message = format!(
                        "Edit {}/{} ({op_label}): {}\n\n\
                         This batch contained {} edits. \
                         Because this anchor failed validation, \
                         none of the edits were applied. \
                         Retry all {} edits with fresh anchors, \
                         not just the failed one.",
                        idx + 1,
                        ops.len(),
                        e.message,
                        ops.len(),
                        ops.len(),
                    );
                }
                return ApplyResult {
                    output: HashlineEditOutput::Error(e),
                    new_content: None,
                };
            }
        }
    }

    if let Some(mut err) = check_overlaps(&resolved) {
        if ops.len() > 1 {
            err.message = format!(
                "{}\n\nThis batch contained {} edits. \
                 Because of the overlap, none were applied. \
                 Fix the overlapping ranges and retry all edits.",
                err.message,
                ops.len(),
            );
        }
        return ApplyResult {
            output: HashlineEditOutput::Error(err),
            new_content: None,
        };
    }

    let mut warnings: Vec<String> = Vec::new();
    for op in &resolved {
        if let Some(w) = range_policy::range_warning(op.start, op.end) {
            warnings.push(w);
        }
    }

    // Bottom-up + reverse-original-idx: preserves request order for same-position ops.
    resolved.sort_by(|a, b| {
        b.start
            .cmp(&a.start)
            .then(b.original_idx.cmp(&a.original_idx))
    });

    let mut result_lines: Vec<String> = lines.iter().map(|s| (*s).to_owned()).collect();

    // Collect each edit's affected region in post-edit coordinates by
    // tracking cumulative line-count shifts. Ops are sorted bottom-up for
    // splicing — iterate in reverse to get top-down order for region tracking.
    let mut edit_regions: Vec<(usize, usize)> = Vec::with_capacity(resolved.len());
    let mut cumulative_shift: isize = 0;

    for op in resolved.iter().rev() {
        let shifted_start = op.start.checked_add_signed(cumulative_shift).unwrap_or(0);
        let replaced = op.end - op.start;
        let inserted = op.new_lines.len();
        edit_regions.push((shifted_start, shifted_start + inserted));
        cumulative_shift += inserted as isize - replaced as isize;
    }

    for op in &resolved {
        result_lines.splice(op.start..op.end, op.new_lines.iter().cloned());
    }

    let new_content = result_lines.join("\n");
    let total_new_lines = split_lines(&new_content).len();

    // Sort edit regions top-down and merge nearby ones.
    edit_regions.sort_by_key(|r| r.0);
    let snippet = build_snippet(&new_content, &edit_regions, total_new_lines, scheme);
    let snippet_start_line = edit_regions
        .first()
        .map_or(1, |r| r.0.saturating_sub(SNIPPET_CONTEXT) + 1);

    ApplyResult {
        output: HashlineEditOutput::EditsApplied(HashlineEditsApplied {
            applied: ops.len(),
            scheme: scheme.name().to_owned(),
            snippet_start_line,
            snippet,
            warnings,
        }),
        new_content: Some(new_content),
    }
}

/// Maximum total snippet lines before switching to per-region snippets.
///
/// When the contiguous range from first to last edit exceeds this, we show
/// individual ±[`SNIPPET_CONTEXT`] windows separated by `... N lines not shown ...`.
const MAX_CONTIGUOUS_SNIPPET: usize = 80;

/// Build the snippet output for a batch of edits.
///
/// If all edits fall within [`MAX_CONTIGUOUS_SNIPPET`] lines of each other,
/// returns a single contiguous snippet. Otherwise, returns per-edit-region
/// snippets separated by gap markers.
fn build_snippet(
    new_content: &str,
    edit_regions: &[(usize, usize)],
    total_new_lines: usize,
    scheme: &dyn AnchorScheme,
) -> String {
    let (Some(first), Some(last)) = (edit_regions.first(), edit_regions.last()) else {
        return String::new();
    };

    let global_start = first.0.saturating_sub(SNIPPET_CONTEXT);
    let global_end = (last.1 + SNIPPET_CONTEXT).min(total_new_lines);

    // If the span is small enough, emit one contiguous snippet.
    if global_end - global_start <= MAX_CONTIGUOUS_SNIPPET {
        return format_hashline_content(
            new_content,
            Some(global_start + 1),
            Some(global_end - global_start),
            scheme,
        );
    }

    // Merge overlapping/adjacent regions (with context).
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for &(start, end) in edit_regions {
        let ctx_start = start.saturating_sub(SNIPPET_CONTEXT);
        let ctx_end = (end + SNIPPET_CONTEXT).min(total_new_lines);
        if let Some(last_region) = merged.last_mut()
            && ctx_start <= last_region.1
        {
            last_region.1 = last_region.1.max(ctx_end);
            continue;
        }
        merged.push((ctx_start, ctx_end));
    }

    // Build per-region snippets separated by gap markers.
    let mut parts: Vec<String> = Vec::new();
    let mut prev_end: usize = 0;

    for (i, &(start, end)) in merged.iter().enumerate() {
        if i > 0 {
            let gap = start.saturating_sub(prev_end);
            parts.push(format!("... {gap} lines not shown ..."));
        } else if start > 0 {
            parts.push(format!("... {start} lines not shown ..."));
        }

        parts.push(format_hashline_content(
            new_content,
            Some(start + 1),
            Some(end - start),
            scheme,
        ));
        prev_end = end;
    }

    if prev_end < total_new_lines {
        let remaining = total_new_lines - prev_end;
        parts.push(format!("... {remaining} lines not shown ..."));
    }

    parts.join("\n")
}

/// Resolve a single [`HashlineOp`] into a `ResolvedOp`, validating anchors.
fn resolve_op(
    op: &HashlineOp,
    original_idx: usize,
    lines: &[&str],
    scheme: &dyn AnchorScheme,
) -> Result<ResolvedOp, HashlineEditError> {
    match op {
        HashlineOp::Replace {
            anchor,
            end_anchor,
            content,
        } => {
            let start = validate_anchor(anchor, lines, scheme)?;
            let end = match end_anchor {
                Some(ea) => {
                    let e = validate_anchor(ea, lines, scheme)?;
                    if e < start {
                        return Err(HashlineEditError::new(
                            HashlineEditErrorKind::InvalidInput,
                            format!(
                                "end_anchor line {} is before start anchor line {}.",
                                e + 1,
                                start + 1
                            ),
                        ));
                    }
                    e + 1 // exclusive end
                }
                None => start + 1, // single line
            };

            if let Some(line_num) = detect_anchor_prefix_in_content(content) {
                return Err(anchor_content_error("replace", content, line_num));
            }
            let new_lines: Vec<String> = if content.is_empty() {
                vec![] // delete
            } else {
                content.lines().map(str::to_owned).collect()
            };

            Ok(ResolvedOp {
                original_idx,
                start,
                end,
                new_lines,
            })
        }

        HashlineOp::InsertAfter { anchor, content } => {
            let insert_at = if anchor == "0:" {
                0
            } else if anchor == "EOF" {
                // Insert at the actual end of file content. If the file ends
                // with '\n', split_lines produces a synthetic trailing empty
                // line — insert before it rather than after it.
                let len = lines.len();
                if len > 1 && lines[len - 1].is_empty() {
                    len - 1
                } else {
                    len
                }
            } else {
                let line = validate_anchor(anchor, lines, scheme)?;
                line + 1
            };

            if let Some(line_num) = detect_anchor_prefix_in_content(content) {
                return Err(anchor_content_error("insert_after", content, line_num));
            }
            let new_lines: Vec<String> = if content.is_empty() {
                vec![String::new()] // blank line
            } else {
                content.lines().map(str::to_owned).collect()
            };

            Ok(ResolvedOp {
                original_idx,
                start: insert_at,
                end: insert_at, // insertion: start == end
                new_lines,
            })
        }

        HashlineOp::Write { .. } => {
            // Write ops should be handled before reaching here.
            Err(HashlineEditError::new(
                HashlineEditErrorKind::InvalidInput,
                "Write op must be the only operation in a batch. \
                 Either use write alone (to replace the entire file) or use \
                 replace/insert_after ops without write."
                    .to_owned(),
            ))
        }
    }
}

/// Try to recover a [`ParsedAnchor`] from a hash-only string like `"ab:cd"`
/// (no line number).
///
/// Generates anchors for the file and returns `Some` only if exactly one
/// line's suffix matches, avoiding ambiguity.
fn recover_anchor_by_suffix(
    suffix: &str,
    lines: &[&str],
    scheme: &dyn AnchorScheme,
) -> Option<ParsedAnchor> {
    let anchors = scheme.generate_anchors(lines);
    let mut matches = anchors
        .iter()
        .filter(|a| match (&a.context, suffix.split_once(':')) {
            (Some(ctx), Some((local, sfx_ctx))) => a.local == local && ctx.as_str() == sfx_ctx,
            (None, None) => a.local == suffix,
            _ => false,
        });
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(ParsedAnchor {
        line: first.line,
        local: first.local.clone(),
        context: first.context.clone(),
    })
}

/// Validate an anchor string against file content.
///
/// Returns the 0-based line index on success, or a structured error.
fn validate_anchor(
    anchor_str: &str,
    lines: &[&str],
    scheme: &dyn AnchorScheme,
) -> Result<usize, HashlineEditError> {
    // Strip trailing arrow + content that the model copies from hashline_read
    // output (e.g. `22:abc:rst→code` or `22:abc:rst->code`).
    let anchor_str = anchor_str
        .split_once('\u{2192}')
        .or_else(|| anchor_str.split_once("->"))
        .map_or(anchor_str, |(pre, _)| pre);

    let parsed = match ParsedAnchor::parse(anchor_str) {
        Some(p) => p,
        None => {
            // Recovery: the model sometimes drops the line number, sending
            // just "ab:cd" instead of "22:ab:cd". Try matching the hash suffix
            // against generated anchors — accept if exactly one line matches.
            if let Some(recovered) = recover_anchor_by_suffix(anchor_str, lines, scheme) {
                tracing::debug!(
                    anchor = anchor_str,
                    recovered_line = recovered.line,
                    "recovered malformed anchor by suffix match"
                );
                recovered
            } else {
                let (fmt, ex) = anchor_format_hint(scheme);
                return Err(HashlineEditError::new(
                    HashlineEditErrorKind::InvalidInput,
                    format!(
                        "Malformed anchor: \"{anchor_str}\". \
                         Expected format: \"{fmt}\" (e.g. \"{ex}\")."
                    ),
                ));
            }
        }
    };

    match scheme.validate(&parsed, lines) {
        ValidationResult::Valid => Ok(parsed.line - 1), // 0-based

        ValidationResult::OutOfRange => Err(HashlineEditError::new(
            HashlineEditErrorKind::AnchorNotFound,
            format!(
                "Line {} is out of range (file has {} lines).",
                parsed.line,
                lines.len()
            ),
        )),

        ValidationResult::Stale => {
            let shift = scheme.find_shifted(&parsed, lines, DEFAULT_SEARCH_RADIUS);
            let anchors = scheme.generate_anchors(lines);

            // Wider context for recovery (±5 lines).
            let recovery_ctx = 5;
            let ctx_start = parsed.line.saturating_sub(1).saturating_sub(recovery_ctx);
            let ctx_end = (parsed.line + recovery_ctx).min(lines.len());

            let context: String = (ctx_start..ctx_end)
                .map(|i| render_anchored_line(&anchors[i], lines[i]))
                .collect::<Vec<_>>()
                .join("\n");

            let (shifted_anchor, error_kind, message) = match shift {
                ShiftResult::Found { new_line } => {
                    let fresh = format!("{}:{}", new_line, anchor_suffix(&anchors[new_line - 1]));
                    let msg = format!(
                        "Anchor stale at line {}. \
                         Content appears to have shifted to line {new_line}. \
                         Retry with anchor \"{fresh}\".",
                        parsed.line
                    );
                    (Some(fresh), HashlineEditErrorKind::AnchorStale, msg)
                }
                ShiftResult::Ambiguous { candidates } => {
                    let msg = format!(
                        "Anchor stale at line {}. Multiple candidates at lines {:?}. \
                         Use the fresh anchors from the context below to retry your edit.",
                        parsed.line, candidates,
                    );
                    (None, HashlineEditErrorKind::AmbiguousAnchor, msg)
                }
                ShiftResult::NotFound => {
                    let msg = format!(
                        "Anchor stale at line {}. \
                         Use the fresh anchors from the context below to retry your edit.",
                        parsed.line,
                    );
                    (None, HashlineEditErrorKind::AnchorStale, msg)
                }
            };

            Err(HashlineEditError {
                error: error_kind,
                message,
                context: Some(context),
                context_start_line: Some(ctx_start + 1),
                shifted_anchor,
            })
        }
    }
}

fn check_overlaps(ops: &[ResolvedOp]) -> Option<HashlineEditError> {
    let mut ranges: Vec<(usize, usize, usize)> = ops
        .iter()
        .filter(|op| op.start != op.end)
        .map(|op| (op.start, op.end, op.original_idx))
        .collect();
    ranges.sort_by_key(|r| r.0);

    // Replacement vs replacement overlap.
    for window in ranges.windows(2) {
        if window[0].1 > window[1].0 {
            return Some(overlap_error(
                window[0].0,
                window[0].1,
                window[0].2,
                window[1].0,
                window[1].1,
                window[1].2,
            ));
        }
    }

    // Insertion vs replacement overlap: reject if the insertion point falls
    // strictly inside a replacement span (start <= insert_at < end).
    for op in ops {
        if op.start != op.end {
            continue; // not an insertion
        }
        let insert_at = op.start;
        for &(rs, re, r_idx) in &ranges {
            if rs <= insert_at && insert_at < re {
                return Some(overlap_error(
                    rs,
                    re,
                    r_idx,
                    insert_at,
                    insert_at,
                    op.original_idx,
                ));
            }
        }
    }

    None
}

fn overlap_error(
    a_start: usize,
    a_end: usize,
    a_idx: usize,
    b_start: usize,
    b_end: usize,
    b_idx: usize,
) -> HashlineEditError {
    let describe = |start: usize, end: usize, idx: usize| {
        if start == end {
            format!("edit #{} (insertion at line {})", idx + 1, start + 1)
        } else {
            format!("edit #{} (lines {}-{})", idx + 1, start + 1, end)
        }
    };
    HashlineEditError::new(
        HashlineEditErrorKind::OverlappingEdits,
        format!(
            "Overlapping edits: {} and {}.",
            describe(a_start, a_end, a_idx),
            describe(b_start, b_end, b_idx)
        ),
    )
}

fn build_write_result(new_content: &str, scheme: &dyn AnchorScheme) -> HashlineEditOutput {
    let total = split_lines(new_content).len();
    let snippet_end = (SNIPPET_CONTEXT * 2).min(total);
    let snippet = format_hashline_content(new_content, Some(1), Some(snippet_end), scheme);

    HashlineEditOutput::EditsApplied(HashlineEditsApplied {
        applied: 1,
        scheme: scheme.name().to_owned(),
        snippet_start_line: 1,
        snippet,
        warnings: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SchemeConfig, SchemeKind};

    fn scheme() -> Box<dyn AnchorScheme> {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn content_only() -> Box<dyn AnchorScheme> {
        SchemeConfig {
            kind: SchemeKind::ContentOnly,
            ..Default::default()
        }
        .build_scheme()
        .unwrap()
    }

    /// Render the anchor of `line` (1-based) for `content` under `scheme`.
    fn anchor_for(content: &str, line: usize, scheme: &dyn AnchorScheme) -> String {
        let lines = split_lines(content);
        let anchors = scheme.generate_anchors(&lines);
        anchors[line - 1].render()
    }

    fn replace(anchor: &str, content: &str) -> HashlineOp {
        HashlineOp::Replace {
            anchor: anchor.to_owned(),
            end_anchor: None,
            content: content.to_owned(),
        }
    }

    fn expect_applied(result: &ApplyResult) -> &HashlineEditsApplied {
        match &result.output {
            HashlineEditOutput::EditsApplied(a) => a,
            HashlineEditOutput::Error(e) => panic!("expected success, got error: {}", e.message),
        }
    }

    fn expect_error(result: &ApplyResult) -> &HashlineEditError {
        match &result.output {
            HashlineEditOutput::Error(e) => e,
            HashlineEditOutput::EditsApplied(_) => panic!("expected error, got success"),
        }
    }

    #[test]
    fn replace_single_line() {
        let content = "let a = 1;\nlet b = 2;\nlet c = 3;\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, &*s);
        let result = apply_edits(content, &[replace(&anchor, "let b = 42;")], &*s);

        let applied = expect_applied(&result);
        assert_eq!(applied.applied, 1);
        assert_eq!(
            result.new_content.as_deref(),
            Some("let a = 1;\nlet b = 42;\nlet c = 3;\n")
        );
        assert!(applied.snippet.contains("let b = 42;"));
    }

    #[test]
    fn replace_range_inclusive() {
        let content = "one\ntwo\nthree\nfour\nfive\n";
        let s = scheme();
        let start = anchor_for(content, 2, &*s);
        let end = anchor_for(content, 4, &*s);
        let op = HashlineOp::Replace {
            anchor: start,
            end_anchor: Some(end),
            content: "MERGED".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("one\nMERGED\nfive\n"));
    }

    #[test]
    fn replace_with_empty_content_deletes() {
        let content = "keep\ndelete me\nkeep too\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, &*s);
        let result = apply_edits(content, &[replace(&anchor, "")], &*s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("keep\nkeep too\n"));
    }

    #[test]
    fn replace_multiline_content() {
        let content = "a\nb\nc\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, &*s);
        let result = apply_edits(content, &[replace(&anchor, "x\ny\nz")], &*s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("a\nx\ny\nz\nc\n"));
    }

    #[test]
    fn end_anchor_before_start_rejected() {
        let content = "one\ntwo\nthree\n";
        let s = scheme();
        let op = HashlineOp::Replace {
            anchor: anchor_for(content, 3, &*s),
            end_anchor: Some(anchor_for(content, 1, &*s)),
            content: "x".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("before start anchor"));
    }

    #[test]
    fn insert_after_line() {
        let content = "first\nsecond\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, &*s);
        let op = HashlineOp::InsertAfter {
            anchor,
            content: "inserted".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);

        expect_applied(&result);
        assert_eq!(
            result.new_content.as_deref(),
            Some("first\ninserted\nsecond\n")
        );
    }

    #[test]
    fn insert_after_bof() {
        let content = "body\n";
        let s = scheme();
        let op = HashlineOp::InsertAfter {
            anchor: "0:".to_owned(),
            content: "header".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("header\nbody\n"));
    }

    #[test]
    fn insert_after_eof_with_trailing_newline() {
        let content = "body\n";
        let s = scheme();
        let op = HashlineOp::InsertAfter {
            anchor: "EOF".to_owned(),
            content: "footer".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);

        expect_applied(&result);
        // Inserted before the synthetic trailing empty line → keeps final newline.
        assert_eq!(result.new_content.as_deref(), Some("body\nfooter\n"));
    }

    #[test]
    fn insert_after_eof_without_trailing_newline() {
        let content = "body";
        let s = scheme();
        let op = HashlineOp::InsertAfter {
            anchor: "EOF".to_owned(),
            content: "footer".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("body\nfooter"));
    }

    #[test]
    fn insert_after_empty_content_adds_blank_line() {
        let content = "a\nb\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, &*s);
        let op = HashlineOp::InsertAfter {
            anchor,
            content: String::new(),
        };
        let result = apply_edits(content, &[op], &*s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("a\n\nb\n"));
    }

    #[test]
    fn write_replaces_entire_file() {
        let content = "old\n";
        let s = scheme();
        let op = HashlineOp::Write {
            content: "brand\nnew\n".to_owned(),
        };
        let result = apply_edits(content, &[op], &*s);

        let applied = expect_applied(&result);
        assert_eq!(applied.snippet_start_line, 1);
        assert_eq!(result.new_content.as_deref(), Some("brand\nnew\n"));
    }

    #[test]
    fn write_must_be_sole_op() {
        let content = "a\nb\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, &*s);
        let ops = [
            replace(&anchor, "x"),
            HashlineOp::Write {
                content: "y".to_owned(),
            },
        ];
        let result = apply_edits(content, &ops, &*s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("only operation"));
    }

    #[test]
    fn batch_edits_applied_bottom_up() {
        let content = "l1\nl2\nl3\nl4\nl5\n";
        let s = scheme();
        // Both anchors validated against the same pre-edit snapshot.
        let a1 = anchor_for(content, 1, &*s);
        let a4 = anchor_for(content, 4, &*s);
        let ops = [replace(&a1, "L1a\nL1b"), replace(&a4, "L4x")];
        let result = apply_edits(content, &ops, &*s);

        let applied = expect_applied(&result);
        assert_eq!(applied.applied, 2);
        assert_eq!(
            result.new_content.as_deref(),
            Some("L1a\nL1b\nl2\nl3\nL4x\nl5\n")
        );
    }

    #[test]
    fn overlapping_replacements_rejected() {
        let content = "a\nb\nc\nd\n";
        let s = scheme();
        let op1 = HashlineOp::Replace {
            anchor: anchor_for(content, 1, &*s),
            end_anchor: Some(anchor_for(content, 3, &*s)),
            content: "x".to_owned(),
        };
        let op2 = replace(&anchor_for(content, 2, &*s), "y");
        let result = apply_edits(content, &[op1, op2], &*s);

        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::OverlappingEdits);
        assert!(err.message.contains("none were applied"));
    }

    #[test]
    fn insertion_inside_replacement_rejected() {
        let content = "a\nb\nc\nd\n";
        let s = scheme();
        let op1 = HashlineOp::Replace {
            anchor: anchor_for(content, 1, &*s),
            end_anchor: Some(anchor_for(content, 3, &*s)),
            content: "x".to_owned(),
        };
        let op2 = HashlineOp::InsertAfter {
            anchor: anchor_for(content, 1, &*s),
            content: "y".to_owned(),
        };
        let result = apply_edits(content, &[op1, op2], &*s);
        assert_eq!(
            expect_error(&result).error,
            HashlineEditErrorKind::OverlappingEdits
        );
    }

    #[test]
    fn adjacent_edits_allowed() {
        let content = "a\nb\nc\nd\n";
        let s = scheme();
        let ops = [
            replace(&anchor_for(content, 1, &*s), "A"),
            replace(&anchor_for(content, 2, &*s), "B"),
        ];
        let result = apply_edits(content, &ops, &*s);
        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("A\nB\nc\nd\n"));
    }

    #[test]
    fn stale_anchor_reports_shift_suggestion() {
        let original = "alpha\nbeta\ngamma\n";
        let s = content_only();
        let anchor = anchor_for(original, 2, &*s); // "beta" at line 2

        // A line was inserted above → "beta" now at line 3.
        let current = "inserted\nalpha\nbeta\ngamma\n";
        let result = apply_edits(current, &[replace(&anchor, "BETA")], &*s);

        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AnchorStale);
        assert!(err.message.contains("shifted to line 3"), "{}", err.message);
        let suggested = err.shifted_anchor.as_deref().unwrap();
        assert!(suggested.starts_with("3:"), "suggested: {suggested}");
        assert!(err.context.is_some());

        // The suggested anchor must actually work on retry.
        let retry = apply_edits(current, &[replace(suggested, "BETA")], &*s);
        expect_applied(&retry);
        assert_eq!(
            retry.new_content.as_deref(),
            Some("inserted\nalpha\nBETA\ngamma\n")
        );
    }

    #[test]
    fn stale_anchor_ambiguous_candidates() {
        let s = content_only();
        let original = "x\ndup\ny\n";
        let anchor = anchor_for(original, 2, &*s);
        // "dup" now appears at lines 1 and 3, original line 2 changed.
        let current = "dup\nchanged\ndup\n";
        let result = apply_edits(current, &[replace(&anchor, "z")], &*s);

        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AmbiguousAnchor);
        assert!(err.message.contains("Multiple candidates"));
    }

    #[test]
    fn out_of_range_anchor() {
        let content = "only\n";
        let s = scheme();
        let result = apply_edits(content, &[replace("99:abc:rst", "x")], &*s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AnchorNotFound);
        assert!(err.message.contains("out of range"));
    }

    #[test]
    fn malformed_anchor_rejected_with_hint() {
        let content = "a\nb\n";
        let s = scheme();
        let result = apply_edits(content, &[replace("not-an-anchor", "x")], &*s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("Malformed anchor"));
        assert!(err.message.contains("LINE:HASH1:HASH2"));
    }

    #[test]
    fn anchor_with_copied_arrow_suffix_accepted() {
        let content = "let a = 1;\nlet b = 2;\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, &*s);
        let sloppy = format!("{anchor}\u{2192}let a = 1;");
        let result = apply_edits(content, &[replace(&sloppy, "let a = 9;")], &*s);
        expect_applied(&result);
        assert_eq!(
            result.new_content.as_deref(),
            Some("let a = 9;\nlet b = 2;\n")
        );
    }

    #[test]
    fn line_number_free_anchor_recovered_when_unique() {
        let content = "unique line here\nother content\n";
        let s = scheme();
        let full = anchor_for(content, 1, &*s);
        // Drop the leading "1:" — suffix like "abc:rst".
        let suffix = full.split_once(':').unwrap().1;
        let result = apply_edits(content, &[replace(suffix, "REPLACED")], &*s);
        expect_applied(&result);
        assert_eq!(
            result.new_content.as_deref(),
            Some("REPLACED\nother content\n")
        );
    }

    #[test]
    fn replace_content_with_anchor_prefixes_rejected() {
        let content = "a\nb\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, &*s);
        let bad = "1:abc:rst\u{2192}let x = 1;";
        let result = apply_edits(content, &[replace(&anchor, bad)], &*s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("anchor prefixes"));
    }

    #[test]
    fn write_content_with_anchor_prefixes_rejected() {
        let s = scheme();
        let op = HashlineOp::Write {
            content: "10:abc:rst->code here".to_owned(),
        };
        let result = apply_edits("old\n", &[op], &*s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
    }

    #[test]
    fn batch_failure_message_mentions_batch() {
        let content = "a\nb\nc\n";
        let s = scheme();
        let good = anchor_for(content, 1, &*s);
        let ops = [replace(&good, "x"), replace("2:zzz:zzz", "y")];
        let result = apply_edits(content, &ops, &*s);
        let err = expect_error(&result);
        assert!(err.message.contains("Edit 2/2"), "{}", err.message);
        assert!(err.message.contains("none of the edits were applied"));
    }

    #[test]
    fn medium_range_edit_warns() {
        let content: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let s = scheme();
        let op = HashlineOp::Replace {
            anchor: anchor_for(&content, 5, &*s),
            end_anchor: Some(anchor_for(&content, 14, &*s)),
            content: "condensed".to_owned(),
        };
        let result = apply_edits(&content, &[op], &*s);
        let applied = expect_applied(&result);
        assert_eq!(applied.warnings.len(), 1);
        assert!(applied.warnings[0].contains("medium range"));
    }

    #[test]
    fn snippet_contains_fresh_valid_anchors() {
        let content = "one\ntwo\nthree\nfour\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, &*s);
        let result = apply_edits(content, &[replace(&anchor, "TWO")], &*s);

        let applied = expect_applied(&result);
        let new_content = result.new_content.as_deref().unwrap();
        let new_lines = split_lines(new_content);

        // Every anchor in the snippet must validate against the new content.
        for line in applied.snippet.lines() {
            let anchor_part = line.split('\u{2192}').next().unwrap();
            let parsed = ParsedAnchor::parse(anchor_part).unwrap();
            assert_eq!(
                s.validate(&parsed, &new_lines),
                ValidationResult::Valid,
                "snippet anchor {anchor_part} must be fresh"
            );
        }
    }

    #[test]
    fn distant_edits_produce_gap_markers() {
        let content: String = (1..=200).map(|i| format!("line number {i}\n")).collect();
        let s = scheme();
        let ops = [
            replace(&anchor_for(&content, 5, &*s), "EDIT-A"),
            replace(&anchor_for(&content, 180, &*s), "EDIT-B"),
        ];
        let result = apply_edits(&content, &ops, &*s);
        let applied = expect_applied(&result);
        assert!(
            applied.snippet.contains("lines not shown"),
            "{}",
            applied.snippet
        );
        assert!(applied.snippet.contains("EDIT-A"));
        assert!(applied.snippet.contains("EDIT-B"));
    }

    #[test]
    fn same_position_ops_preserve_request_order() {
        let content = "target\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, &*s);
        let ops = [
            HashlineOp::InsertAfter {
                anchor: anchor.clone(),
                content: "first".to_owned(),
            },
            HashlineOp::InsertAfter {
                anchor,
                content: "second".to_owned(),
            },
        ];
        let result = apply_edits(content, &ops, &*s);
        expect_applied(&result);
        assert_eq!(
            result.new_content.as_deref(),
            Some("target\nfirst\nsecond\n")
        );
    }
}
