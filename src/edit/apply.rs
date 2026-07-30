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
//! Core edit-application logic for `hashline_edit`.
//!
//! Validates anchors against the pre-edit file snapshot, detects overlapping
//! edits, sorts operations bottom-up, and applies them. Returns a fresh-anchor
//! snippet of the edited region.

use std::ops::Range;

use super::range_policy;
use super::types::{
    HashlineEditError, HashlineEditErrorKind, HashlineEditOutput, HashlineEditsApplied, HashlineOp,
};
use crate::index::{FileIndex, split_lines};
use crate::render::{CONTENT_SEPARATOR, render_range};
use crate::scheme::{DEFAULT_SEARCH_RADIUS, ParsedAnchor, Scheme, ShiftResult, ValidationResult};

const SNIPPET_CONTEXT: usize = 3;

/// Lines of fresh-anchor context rendered around a stale anchor.
const RECOVERY_CONTEXT: usize = 5;

/// Lines hashed on each side of an anchor's target in the pre-edit index.
///
/// A single anchor reaches three ways: [`Scheme::validate`] reads the target
/// line, [`Scheme::find_shifted`] scans `±DEFAULT_SEARCH_RADIUS`, and the
/// stale-anchor error renders `±RECOVERY_CONTEXT` lines of fresh anchors. The
/// widest of those is the shift search, and the sum bounds all three, so a
/// window of this radius around the target is the most any one anchor can
/// touch.
const ANCHOR_HASH_PADDING: usize = DEFAULT_SEARCH_RADIUS + RECOVERY_CONTEXT;

/// Generate a scheme-appropriate format label and example anchor for error messages.
fn anchor_format_hint(scheme: Scheme) -> (&'static str, String) {
    let len = scheme.hash_len().clamp(1, 4);
    let hash = &"abcd"[..len];
    if scheme.has_context() {
        let ctx = &"rstu"[..len];
        ("LINE:HASH1:HASH2", format!("22:{hash}:{ctx}"))
    } else {
        ("LINE:HASH", format!("22:{hash}"))
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

/// Strip the `→CONTENT` (or `->CONTENT`) tail that models copy verbatim from
/// `hashline_read` output, leaving the bare anchor.
fn strip_anchor_suffix(anchor_str: &str) -> &str {
    anchor_str
        .split_once(CONTENT_SEPARATOR)
        .or_else(|| anchor_str.split_once("->"))
        .map_or(anchor_str, |(pre, _)| pre)
}

/// Split replacement content into the lines it contributes to the file.
///
/// `str::lines` strips a `\r` only when a `\n` follows it, so content ending in
/// a bare `\r` keeps it — and the join about to happen would turn that into a
/// `\r\n` terminator, leaving one CRLF line in a file this path otherwise
/// rewrites entirely with `\n` terminators. Caller-supplied text is the one
/// place that is ours to normalize, so it is normalized here, the same way the
/// join normalizes the terminators of every line it rewrites.
///
/// This is a policy choice, not the correctness fix: the spliced vector is
/// reconciled with `split_lines(new_content)` in [`apply_edits`], which also
/// covers the `\r`s that come from the file rather than from an op.
fn content_lines(content: &str) -> Vec<&str> {
    content
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect()
}

/// The hash spans needed to anchor `regions`.
///
/// Each region is expanded to the block boundaries its scheme needs, so a
/// snippet of a 50,000-line file hashes tens of lines rather than all of them.
/// The line count is not always known here, so the spans are computed against
/// an unbounded file and the `FileIndex` constructor clamps them.
fn snippet_spans(regions: &[Range<usize>], scheme: Scheme) -> Vec<Range<usize>> {
    regions
        .iter()
        .map(|region| scheme.required_hash_span(region.clone(), usize::MAX))
        .collect()
}

/// Index `content` for rendering `regions`, hashing only the lines those
/// regions' anchors depend on.
///
/// For the edit path prefer [`FileIndex::from_lines_partial`] with the spliced
/// line vector — it is already the post-edit split, so re-splitting is waste.
fn snippet_index<'a>(content: &'a str, regions: &[Range<usize>], scheme: Scheme) -> FileIndex<'a> {
    FileIndex::new_partial(content, &snippet_spans(regions, scheme))
}

/// Bytes `lines` contribute to a newline-joined string: their content plus one
/// separator each.
fn line_bytes(lines: &[&str]) -> usize {
    lines.iter().map(|line| line.len() + 1).sum()
}

/// Separate snippet parts with `'\n'`, matching a `join("\n")` over them:
/// nothing before the first part, one newline before every later one.
fn push_part_separator(out: &mut String, written: &mut bool) {
    if *written {
        out.push('\n');
    } else {
        *written = true;
    }
}

/// Append `"... {count} lines not shown ..."` to `out`.
fn push_gap_marker(out: &mut String, count: usize) {
    let mut buf = itoa::Buffer::new();
    out.push_str("... ");
    out.push_str(buf.format(count));
    out.push_str(" lines not shown ...");
}

/// Build the pre-edit index for `ops`, hashing only the lines their anchors
/// can reach.
///
/// Every anchor that parses contributes the `±ANCHOR_HASH_PADDING` window
/// around its target, expanded to scheme block boundaries; overlapping windows
/// merge inside [`FileIndex::new_partial`]. An anchor that does not parse falls
/// through to [`recover_anchor_by_suffix`], which must compare against every
/// line in the file to detect ambiguity, so one unparseable anchor forces a
/// full index — a rare recovery path, paid for only when it is taken.
///
/// Unlike the read and grep paths this one splits every line up front
/// ([`FileIndex::from_lines_partial`]): the splice rebuilds the whole line
/// vector, so a partial index would only have to materialize it again.
fn pre_edit_index<'a>(content: &'a str, ops: &[HashlineOp], scheme: Scheme) -> FileIndex<'a> {
    let mut spans: Vec<Range<usize>> = Vec::with_capacity(ops.len() + 1);

    for op in ops {
        let anchors: [Option<&str>; 2] = match op {
            HashlineOp::Replace {
                anchor, end_anchor, ..
            } => [Some(anchor.as_str()), end_anchor.as_deref()],
            // `"0:"` and `"EOF"` resolve from the line count alone.
            HashlineOp::InsertAfter { anchor, .. } if anchor == "0:" || anchor == "EOF" => {
                [None, None]
            }
            HashlineOp::InsertAfter { anchor, .. } => [Some(anchor.as_str()), None],
            HashlineOp::Write { .. } => [None, None],
        };

        for anchor in anchors.into_iter().flatten() {
            let Some(parsed) = ParsedAnchor::parse(strip_anchor_suffix(anchor)) else {
                return FileIndex::new(content);
            };
            // `ParsedAnchor::parse` rejects line 0, so this cannot underflow.
            let target = parsed.line - 1;
            let window = target.saturating_sub(ANCHOR_HASH_PADDING)
                ..target.saturating_add(ANCHOR_HASH_PADDING).saturating_add(1);
            spans.push(scheme.required_hash_span(window, usize::MAX));
        }
    }

    FileIndex::from_lines_partial(split_lines(content), &spans)
}

/// A validated, resolved edit operation ready for application.
/// All line indices are 0-based.
#[derive(Debug)]
struct ResolvedOp<'a> {
    /// Original index in the input batch (for stable ordering).
    original_idx: usize,
    /// Start line (0-based, inclusive).
    start: usize,
    /// End line (0-based, exclusive). For insert_after, start == end (insertion point).
    end: usize,
    /// Replacement lines, borrowed from the operation's content (empty =
    /// delete).
    new_lines: Vec<&'a str>,
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
pub fn apply_edits(content: &str, ops: &[HashlineOp], scheme: Scheme) -> ApplyResult {
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

    // Hashes only what the batch's anchors can reach, so validating a handful
    // of anchors in a large file costs a handful of windows, not the file.
    let index = pre_edit_index(content, ops, scheme);

    let mut resolved: Vec<ResolvedOp<'_>> = Vec::with_capacity(ops.len());

    for (idx, op) in ops.iter().enumerate() {
        match resolve_op(op, idx, &index, scheme) {
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

    // Every unchanged line stays a borrow of the original content; only the
    // pointer table is rebuilt, never the file text.
    let mut result_lines: Vec<&str> = index.lines().to_vec();

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

    // Joining the lines costs `sum(len) + count - 1` bytes, which differs from
    // the input only by what the edits add and remove. `content.len()` bounds
    // the join of the *unedited* lines from above (a `\r\n` terminator becomes
    // a bare `\n`), so this never under-reserves and the join never reallocates.
    let mut spliced_bytes = content.len();
    for op in &resolved {
        spliced_bytes += line_bytes(&op.new_lines);
        spliced_bytes = spliced_bytes.saturating_sub(line_bytes(&index.lines()[op.start..op.end]));
    }

    for op in &resolved {
        result_lines.splice(op.start..op.end, op.new_lines.iter().copied());
    }

    // Deleting every line empties the vector, but the resulting empty file
    // still has one (empty) line. Restoring it keeps `result_lines` exactly
    // equal to `split_lines(new_content)`, which is the precondition
    // `FileIndex::from_lines_partial` relies on.
    if result_lines.is_empty() {
        result_lines.push("");
    }

    let total_new_lines = result_lines.len();

    let mut new_content = String::with_capacity(spliced_bytes);
    for (i, line) in result_lines.iter().enumerate() {
        if i > 0 {
            new_content.push('\n');
        }
        new_content.push_str(line);
    }

    // Joining with `\n` and splitting again is not quite the identity: a line
    // ending in `\r` just gained a `\r\n` terminator, and `split_lines` strips
    // exactly one `\r` from it. A file line can arrive that way — one the file
    // wrote as `\r\r\n`, or an unterminated last line — and then the vector no
    // longer equals `split_lines(new_content)`, the precondition
    // `FileIndex::from_lines_partial` relies on, so the snippet would render
    // an anchor hashed over text a later read does not produce.
    //
    // Re-splitting to find out would cost the split this path exists to skip,
    // so the equivalent is applied directly: every line but the last drops one
    // trailing `\r`, which is exactly what a re-split would have done. The
    // last one keeps it — nothing follows it, so the join reproduced it byte
    // for byte. `new_content` is already built and is not touched: this
    // corrects the bookkeeping, never the file.
    if let Some((_, above)) = result_lines.split_last_mut() {
        for line in above {
            if let Some(stripped) = line.strip_suffix('\r') {
                *line = stripped;
            }
        }
    }

    // Sort edit regions top-down and merge nearby ones.
    edit_regions.sort_by_key(|r| r.0);
    // The splice result *is* the post-edit line vector, so the snippet is
    // anchored straight off it — no second split of the new content.
    let snippet = build_snippet(result_lines, &edit_regions, total_new_lines, scheme);
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
    new_lines: Vec<&str>,
    edit_regions: &[(usize, usize)],
    total_new_lines: usize,
    scheme: Scheme,
) -> String {
    let (Some(first), Some(last)) = (edit_regions.first(), edit_regions.last()) else {
        return String::new();
    };

    let global_start = first.0.saturating_sub(SNIPPET_CONTEXT);
    let global_end = (last.1 + SNIPPET_CONTEXT).min(total_new_lines);

    // If the span is small enough, emit one contiguous snippet.
    if global_end - global_start <= MAX_CONTIGUOUS_SNIPPET {
        let region = global_start..global_end;
        let spans = snippet_spans(std::slice::from_ref(&region), scheme);
        let index = FileIndex::from_lines_partial(new_lines, &spans);
        let mut out = String::new();
        render_range(&index, scheme, region, &mut out);
        return out;
    }

    // Merge overlapping/adjacent regions (with context).
    let mut merged: Vec<Range<usize>> = Vec::new();
    for &(start, end) in edit_regions {
        let ctx_start = start.saturating_sub(SNIPPET_CONTEXT);
        let ctx_end = (end + SNIPPET_CONTEXT).min(total_new_lines);
        if let Some(last_region) = merged.last_mut()
            && ctx_start <= last_region.end
        {
            last_region.end = last_region.end.max(ctx_end);
            continue;
        }
        merged.push(ctx_start..ctx_end);
    }

    // One index for every region: each is hashed once, none of the file
    // between them is hashed at all.
    let index = FileIndex::from_lines_partial(new_lines, &snippet_spans(&merged, scheme));

    // Build per-region snippets separated by gap markers. Parts are joined
    // with '\n', including empty ones from regions that clamp away.
    let mut out = String::new();
    let mut written = false;
    let mut prev_end: usize = 0;

    for (i, region) in merged.iter().enumerate() {
        let (start, end) = (region.start, region.end);
        if i > 0 {
            push_part_separator(&mut out, &mut written);
            push_gap_marker(&mut out, start.saturating_sub(prev_end));
        } else if start > 0 {
            push_part_separator(&mut out, &mut written);
            push_gap_marker(&mut out, start);
        }

        push_part_separator(&mut out, &mut written);
        render_range(&index, scheme, region.clone(), &mut out);
        prev_end = end;
    }

    if prev_end < total_new_lines {
        push_part_separator(&mut out, &mut written);
        push_gap_marker(&mut out, total_new_lines - prev_end);
    }

    out
}

/// Resolve a single [`HashlineOp`] into a `ResolvedOp`, validating anchors.
fn resolve_op<'a>(
    op: &'a HashlineOp,
    original_idx: usize,
    index: &FileIndex<'_>,
    scheme: Scheme,
) -> Result<ResolvedOp<'a>, HashlineEditError> {
    match op {
        HashlineOp::Replace {
            anchor,
            end_anchor,
            content,
        } => {
            let start = validate_anchor(anchor, index, scheme)?;
            let end = match end_anchor {
                Some(ea) => {
                    let e = validate_anchor(ea, index, scheme)?;
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
            let new_lines: Vec<&str> = if content.is_empty() {
                Vec::new() // delete
            } else {
                content_lines(content)
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
                let len = index.len();
                if len > 1 && index.line(len - 1).is_some_and(str::is_empty) {
                    len - 1
                } else {
                    len
                }
            } else {
                let line = validate_anchor(anchor, index, scheme)?;
                line + 1
            };

            if let Some(line_num) = detect_anchor_prefix_in_content(content) {
                return Err(anchor_content_error("insert_after", content, line_num));
            }
            let new_lines: Vec<&str> = if content.is_empty() {
                vec![""] // blank line
            } else {
                content_lines(content)
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
/// line's suffix matches, avoiding ambiguity. Detecting a second match is what
/// makes the result trustworthy, so the scan cannot stop early and `index`
/// must be a full [`FileIndex`] — [`pre_edit_index`] builds one whenever a
/// batch contains an anchor that might land here.
fn recover_anchor_by_suffix(
    suffix: &str,
    index: &FileIndex<'_>,
    scheme: Scheme,
) -> Option<ParsedAnchor> {
    let mut matches = scheme.anchors_for_range(index, 0..index.len()).filter(|a| {
        match (a.context, suffix.split_once(':')) {
            (Some(ctx), Some((local, sfx_ctx))) => a.local == local && ctx == sfx_ctx,
            (None, None) => a.local == suffix,
            _ => false,
        }
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(ParsedAnchor::from(first))
}

/// Validate an anchor string against file content.
///
/// Returns the 0-based line index on success, or a structured error.
fn validate_anchor(
    anchor_str: &str,
    index: &FileIndex<'_>,
    scheme: Scheme,
) -> Result<usize, HashlineEditError> {
    // Strip trailing arrow + content that the model copies from hashline_read
    // output (e.g. `22:abc:rst→code` or `22:abc:rst->code`).
    let anchor_str = strip_anchor_suffix(anchor_str);

    let parsed = match ParsedAnchor::parse(anchor_str) {
        Some(p) => p,
        None => {
            // Recovery: the model sometimes drops the line number, sending
            // just "ab:cd" instead of "22:ab:cd". Try matching the hash suffix
            // against generated anchors — accept if exactly one line matches.
            if let Some(recovered) = recover_anchor_by_suffix(anchor_str, index, scheme) {
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

    match scheme.validate(&parsed, index) {
        ValidationResult::Valid => Ok(parsed.line - 1), // 0-based

        ValidationResult::OutOfRange => Err(HashlineEditError::new(
            HashlineEditErrorKind::AnchorNotFound,
            format!(
                "Line {} is out of range (file has {} lines).",
                parsed.line,
                index.len()
            ),
        )),

        ValidationResult::Stale => {
            let shift = scheme.find_shifted(&parsed, index, DEFAULT_SEARCH_RADIUS);

            // Wider context for recovery (±5 lines).
            let ctx_start = parsed
                .line
                .saturating_sub(1)
                .saturating_sub(RECOVERY_CONTEXT);
            let ctx_end = (parsed.line + RECOVERY_CONTEXT).min(index.len());

            let mut context = String::new();
            render_range(index, scheme, ctx_start..ctx_end, &mut context);

            let (shifted_anchor, error_kind, message) = match shift {
                ShiftResult::Found { new_line } => {
                    // `find_shifted` only reports lines it read out of `index`.
                    let fresh = scheme
                        .anchor_at(index, new_line - 1)
                        .expect("find_shifted reported an indexed line")
                        .render();
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

fn check_overlaps(ops: &[ResolvedOp<'_>]) -> Option<HashlineEditError> {
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

fn build_write_result(new_content: &str, scheme: Scheme) -> HashlineEditOutput {
    // `render_range` clamps to the file, so a short file needs no line count.
    let region = 0..SNIPPET_CONTEXT * 2;
    let index = snippet_index(new_content, std::slice::from_ref(&region), scheme);
    let mut snippet = String::new();
    render_range(&index, scheme, region, &mut snippet);

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
    use crate::testutil::corpus;

    fn scheme() -> Scheme {
        SchemeConfig::default().build_scheme().unwrap()
    }

    fn content_only() -> Scheme {
        SchemeConfig {
            kind: SchemeKind::ContentOnly,
            ..Default::default()
        }
        .build_scheme()
        .unwrap()
    }

    fn checkpoint() -> Scheme {
        SchemeConfig {
            kind: SchemeKind::Checkpoint,
            ..Default::default()
        }
        .build_scheme()
        .unwrap()
    }

    /// Render the anchor of `line` (1-based) for `content` under `scheme`.
    fn anchor_for(content: &str, line: usize, scheme: Scheme) -> String {
        let index = FileIndex::new(content);
        scheme
            .anchor_at(&index, line - 1)
            .expect("line within file")
            .render()
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
        let anchor = anchor_for(content, 2, s);
        let result = apply_edits(content, &[replace(&anchor, "let b = 42;")], s);

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
        let start = anchor_for(content, 2, s);
        let end = anchor_for(content, 4, s);
        let op = HashlineOp::Replace {
            anchor: start,
            end_anchor: Some(end),
            content: "MERGED".to_owned(),
        };
        let result = apply_edits(content, &[op], s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("one\nMERGED\nfive\n"));
    }

    #[test]
    fn replace_with_empty_content_deletes() {
        let content = "keep\ndelete me\nkeep too\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, s);
        let result = apply_edits(content, &[replace(&anchor, "")], s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("keep\nkeep too\n"));
    }

    #[test]
    fn replace_multiline_content() {
        let content = "a\nb\nc\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, s);
        let result = apply_edits(content, &[replace(&anchor, "x\ny\nz")], s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("a\nx\ny\nz\nc\n"));
    }

    #[test]
    fn end_anchor_before_start_rejected() {
        let content = "one\ntwo\nthree\n";
        let s = scheme();
        let op = HashlineOp::Replace {
            anchor: anchor_for(content, 3, s),
            end_anchor: Some(anchor_for(content, 1, s)),
            content: "x".to_owned(),
        };
        let result = apply_edits(content, &[op], s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("before start anchor"));
    }

    #[test]
    fn insert_after_line() {
        let content = "first\nsecond\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, s);
        let op = HashlineOp::InsertAfter {
            anchor,
            content: "inserted".to_owned(),
        };
        let result = apply_edits(content, &[op], s);

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
        let result = apply_edits(content, &[op], s);

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
        let result = apply_edits(content, &[op], s);

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
        let result = apply_edits(content, &[op], s);

        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("body\nfooter"));
    }

    #[test]
    fn insert_after_empty_content_adds_blank_line() {
        let content = "a\nb\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, s);
        let op = HashlineOp::InsertAfter {
            anchor,
            content: String::new(),
        };
        let result = apply_edits(content, &[op], s);

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
        let result = apply_edits(content, &[op], s);

        let applied = expect_applied(&result);
        assert_eq!(applied.snippet_start_line, 1);
        assert_eq!(result.new_content.as_deref(), Some("brand\nnew\n"));
    }

    #[test]
    fn write_must_be_sole_op() {
        let content = "a\nb\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, s);
        let ops = [
            replace(&anchor, "x"),
            HashlineOp::Write {
                content: "y".to_owned(),
            },
        ];
        let result = apply_edits(content, &ops, s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("only operation"));
    }

    #[test]
    fn batch_edits_applied_bottom_up() {
        let content = "l1\nl2\nl3\nl4\nl5\n";
        let s = scheme();
        // Both anchors validated against the same pre-edit snapshot.
        let a1 = anchor_for(content, 1, s);
        let a4 = anchor_for(content, 4, s);
        let ops = [replace(&a1, "L1a\nL1b"), replace(&a4, "L4x")];
        let result = apply_edits(content, &ops, s);

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
            anchor: anchor_for(content, 1, s),
            end_anchor: Some(anchor_for(content, 3, s)),
            content: "x".to_owned(),
        };
        let op2 = replace(&anchor_for(content, 2, s), "y");
        let result = apply_edits(content, &[op1, op2], s);

        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::OverlappingEdits);
        assert!(err.message.contains("none were applied"));
    }

    #[test]
    fn insertion_inside_replacement_rejected() {
        let content = "a\nb\nc\nd\n";
        let s = scheme();
        let op1 = HashlineOp::Replace {
            anchor: anchor_for(content, 1, s),
            end_anchor: Some(anchor_for(content, 3, s)),
            content: "x".to_owned(),
        };
        let op2 = HashlineOp::InsertAfter {
            anchor: anchor_for(content, 1, s),
            content: "y".to_owned(),
        };
        let result = apply_edits(content, &[op1, op2], s);
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
            replace(&anchor_for(content, 1, s), "A"),
            replace(&anchor_for(content, 2, s), "B"),
        ];
        let result = apply_edits(content, &ops, s);
        expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some("A\nB\nc\nd\n"));
    }

    #[test]
    fn stale_anchor_reports_shift_suggestion() {
        let original = "alpha\nbeta\ngamma\n";
        let s = content_only();
        let anchor = anchor_for(original, 2, s); // "beta" at line 2

        // A line was inserted above → "beta" now at line 3.
        let current = "inserted\nalpha\nbeta\ngamma\n";
        let result = apply_edits(current, &[replace(&anchor, "BETA")], s);

        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AnchorStale);
        assert!(err.message.contains("shifted to line 3"), "{}", err.message);
        let suggested = err.shifted_anchor.as_deref().unwrap();
        assert!(suggested.starts_with("3:"), "suggested: {suggested}");
        assert!(err.context.is_some());

        // The suggested anchor must actually work on retry.
        let retry = apply_edits(current, &[replace(suggested, "BETA")], s);
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
        let anchor = anchor_for(original, 2, s);
        // "dup" now appears at lines 1 and 3, original line 2 changed.
        let current = "dup\nchanged\ndup\n";
        let result = apply_edits(current, &[replace(&anchor, "z")], s);

        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AmbiguousAnchor);
        assert!(err.message.contains("Multiple candidates"));
    }

    #[test]
    fn out_of_range_anchor() {
        let content = "only\n";
        let s = scheme();
        let result = apply_edits(content, &[replace("99:abc:rst", "x")], s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AnchorNotFound);
        assert!(err.message.contains("out of range"));
    }

    #[test]
    fn malformed_anchor_rejected_with_hint() {
        let content = "a\nb\n";
        let s = scheme();
        let result = apply_edits(content, &[replace("not-an-anchor", "x")], s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
        assert!(err.message.contains("Malformed anchor"));
        assert!(err.message.contains("LINE:HASH1:HASH2"));
    }

    #[test]
    fn anchor_with_copied_arrow_suffix_accepted() {
        let content = "let a = 1;\nlet b = 2;\n";
        let s = scheme();
        let anchor = anchor_for(content, 1, s);
        let sloppy = format!("{anchor}\u{2192}let a = 1;");
        let result = apply_edits(content, &[replace(&sloppy, "let a = 9;")], s);
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
        let full = anchor_for(content, 1, s);
        // Drop the leading "1:" — suffix like "abc:rst".
        let suffix = full.split_once(':').unwrap().1;
        let result = apply_edits(content, &[replace(suffix, "REPLACED")], s);
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
        let anchor = anchor_for(content, 1, s);
        let bad = "1:abc:rst\u{2192}let x = 1;";
        let result = apply_edits(content, &[replace(&anchor, bad)], s);
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
        let result = apply_edits("old\n", &[op], s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::InvalidInput);
    }

    #[test]
    fn batch_failure_message_mentions_batch() {
        let content = "a\nb\nc\n";
        let s = scheme();
        let good = anchor_for(content, 1, s);
        let ops = [replace(&good, "x"), replace("2:zzz:zzz", "y")];
        let result = apply_edits(content, &ops, s);
        let err = expect_error(&result);
        assert!(err.message.contains("Edit 2/2"), "{}", err.message);
        assert!(err.message.contains("none of the edits were applied"));
    }

    #[test]
    fn medium_range_edit_warns() {
        let content: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let s = scheme();
        let op = HashlineOp::Replace {
            anchor: anchor_for(&content, 5, s),
            end_anchor: Some(anchor_for(&content, 14, s)),
            content: "condensed".to_owned(),
        };
        let result = apply_edits(&content, &[op], s);
        let applied = expect_applied(&result);
        assert_eq!(applied.warnings.len(), 1);
        assert!(applied.warnings[0].contains("medium range"));
    }

    #[test]
    fn snippet_contains_fresh_valid_anchors() {
        let content = "one\ntwo\nthree\nfour\n";
        let s = scheme();
        let anchor = anchor_for(content, 2, s);
        let result = apply_edits(content, &[replace(&anchor, "TWO")], s);

        let applied = expect_applied(&result);
        let new_content = result.new_content.as_deref().unwrap();
        let new_index = FileIndex::new(new_content);

        // Every anchor in the snippet must validate against the new content.
        for line in applied.snippet.lines() {
            let anchor_part = line.split('\u{2192}').next().unwrap();
            let parsed = ParsedAnchor::parse(anchor_part).unwrap();
            assert_eq!(
                s.validate(&parsed, &new_index),
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
            replace(&anchor_for(&content, 5, s), "EDIT-A"),
            replace(&anchor_for(&content, 180, s), "EDIT-B"),
        ];
        let result = apply_edits(&content, &ops, s);
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
        let anchor = anchor_for(content, 1, s);
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
        let result = apply_edits(content, &ops, s);
        expect_applied(&result);
        assert_eq!(
            result.new_content.as_deref(),
            Some("target\nfirst\nsecond\n")
        );
    }

    // ── partial pre-edit index: span coverage ────────────────────────────────

    /// Render the fresh-anchor context a stale anchor at 1-based `line` must
    /// produce, derived independently from a **full** index.
    ///
    /// Returns the context text and its 1-based start line.
    fn reference_context(content: &str, line: usize, scheme: Scheme) -> (String, usize) {
        let index = FileIndex::new(content);
        let ctx_start = line.saturating_sub(1).saturating_sub(RECOVERY_CONTEXT);
        let ctx_end = (line + RECOVERY_CONTEXT).min(index.len());
        let text = scheme
            .anchors_for_range(&index, ctx_start..ctx_end)
            .map(|a| format!("{a}\u{2192}{}", index.line(a.line - 1).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n");
        (text, ctx_start + 1)
    }

    /// Assert that a stale-anchor error rendered off the partial pre-edit index
    /// matches what a full index would have produced.
    fn assert_stale_error_matches_full_index(
        current: &str,
        line: usize,
        scheme: Scheme,
        err: &HashlineEditError,
    ) {
        let (expected_ctx, expected_start) = reference_context(current, line, scheme);
        assert_eq!(err.context.as_deref(), Some(expected_ctx.as_str()));
        assert_eq!(err.context_start_line, Some(expected_start));

        // A suggested anchor must be the real anchor of the shifted line.
        if let Some(ref suggested) = err.shifted_anchor {
            let index = FileIndex::new(current);
            let parsed = ParsedAnchor::parse(suggested).expect("suggested anchor parses");
            assert_eq!(
                scheme.validate(&parsed, &index),
                ValidationResult::Valid,
                "suggested anchor {suggested} must be fresh"
            );
        }
    }

    /// Every stale-anchor recovery path — validate, `find_shifted`, and the
    /// ±[`RECOVERY_CONTEXT`] context render — must stay inside the padded spans
    /// the partial pre-edit index hashes, for every scheme and at every block
    /// boundary. Anything short of that panics in `FileIndex`, so reaching the
    /// assertions at all is half the test; the other half is that the rendered
    /// error is byte-identical to a full index's.
    #[test]
    fn padded_spans_cover_validate_shift_and_context() {
        // Chunk boundaries fall every 8 lines and checkpoints every 32 under
        // the default config, so these targets sit on, just before, and just
        // after both kinds of boundary.
        const TARGETS: &[usize] = &[1, 7, 8, 9, 16, 17, 31, 32, 33, 64, 65, 96, 129, 160, 195];
        let mut stale_paths = 0usize;

        for s in [scheme(), content_only(), checkpoint()] {
            for seed in 0..6u32 {
                let original = corpus(200, 0x57A1_0000 + seed, true);
                let total = FileIndex::new(&original).len();

                for &target in TARGETS {
                    if target > total {
                        continue;
                    }
                    let anchor = anchor_for(&original, target, s);

                    // (a) Content shifted down by one line.
                    let shifted = format!("// shift marker\n{original}");
                    // (b) The anchored line itself rewritten in place.
                    let mut lines: Vec<&str> = FileIndex::new(&original).lines().to_vec();
                    lines[target - 1] = "@@ rewritten sentinel line @@";
                    let rewritten = lines.join("\n");

                    for current in [shifted, rewritten] {
                        let result = apply_edits(&current, &[replace(&anchor, "X")], s);
                        // A corpus line can coincidentally re-validate; only the
                        // stale outcomes carry the recovery context.
                        if let HashlineEditOutput::Error(ref err) = result.output
                            && err.context.is_some()
                        {
                            stale_paths += 1;
                            assert_stale_error_matches_full_index(&current, target, s, err);
                        }
                    }
                }
            }
        }

        // Guard against the loop silently never reaching the stale branch.
        assert!(
            stale_paths > 100,
            "only {stale_paths} stale paths exercised"
        );
    }

    #[test]
    fn ambiguous_stale_anchor_at_chunk_boundary_renders_context() {
        let s = content_only();
        let original: String = (0..80).map(|i| format!("line {i}\n")).collect();
        // 1-based line 65 is a chunk boundary (0-based 64) under 8-line chunks.
        let target = 65;
        let anchor = anchor_for(&original, target, s);

        let mut lines: Vec<&str> = FileIndex::new(&original).lines().to_vec();
        let original_text = lines[target - 1];
        // Two nearby duplicates of the anchored text, and the line itself
        // rewritten, so `find_shifted` sees exactly two candidates.
        lines[target - 3] = original_text;
        lines[target + 1] = original_text;
        lines[target - 1] = "@@ rewritten @@";
        let current = lines.join("\n");

        let result = apply_edits(&current, &[replace(&anchor, "X")], s);
        let err = expect_error(&result);
        assert_eq!(err.error, HashlineEditErrorKind::AmbiguousAnchor);
        assert!(
            err.message.contains("Multiple candidates"),
            "{}",
            err.message
        );
        assert_stale_error_matches_full_index(&current, target, s, err);
    }

    #[test]
    fn suffix_recovery_falls_back_to_full_index() {
        // The match sits far outside any window a padded anchor span would
        // cover, so recovery only works if the missing line number forced a
        // full index.
        let content: String = (0..400).map(|i| format!("distinct line {i};\n")).collect();
        for s in [scheme(), content_only(), checkpoint()] {
            let full = anchor_for(&content, 301, s);
            // Drop the leading "301:", leaving just the hash suffix.
            let suffix = full.split_once(':').expect("anchor has a line number").1;

            let result = apply_edits(&content, &[replace(suffix, "RECOVERED")], s);
            expect_applied(&result);
            let new_content = result.new_content.expect("applied");
            assert_eq!(
                new_content.lines().nth(300),
                Some("RECOVERED"),
                "scheme {}",
                s.name()
            );
            // Every other line is untouched.
            assert_eq!(new_content.lines().nth(299), Some("distinct line 299;"));
            assert_eq!(new_content.lines().nth(301), Some("distinct line 301;"));
        }
    }

    // ── splice and capacity ─────────────────────────────────────────────────

    #[test]
    fn batch_splice_handles_adjacent_and_distant_regions() {
        let content: String = (1..=200).map(|i| format!("line number {i}\n")).collect();
        let s = scheme();

        // Adjacent edits: their ±SNIPPET_CONTEXT regions overlap and merge.
        let adjacent = [
            replace(&anchor_for(&content, 20, s), "A1\nA2"),
            replace(&anchor_for(&content, 22, s), "B1"),
            HashlineOp::InsertAfter {
                anchor: anchor_for(&content, 24, s),
                content: "C1\nC2".to_owned(),
            },
        ];
        let result = apply_edits(&content, &adjacent, s);
        let applied = expect_applied(&result);
        let new_content = result.new_content.as_deref().expect("applied");
        let lines: Vec<&str> = new_content.lines().collect();
        assert_eq!(
            &lines[18..27],
            &[
                "line number 19",
                "A1",
                "A2",
                "line number 21",
                "B1",
                "line number 23",
                "line number 24",
                "C1",
                "C2",
            ]
        );
        // +1 line from the two-line replace, +2 from the insert.
        assert_eq!(lines.len(), 200 + 3);
        // One merged region, so no gap markers.
        assert!(
            !applied.snippet.contains("lines not shown"),
            "{}",
            applied.snippet
        );

        // Distant edits: separate regions, gap markers between them.
        let distant = [
            replace(&anchor_for(&content, 5, s), "EDIT-A"),
            replace(&anchor_for(&content, 180, s), "EDIT-B"),
        ];
        let result = apply_edits(&content, &distant, s);
        let applied = expect_applied(&result);
        let snippet = &applied.snippet;
        assert!(snippet.contains("... 1 lines not shown ..."), "{snippet}");
        assert!(
            snippet.contains("EDIT-A") && snippet.contains("EDIT-B"),
            "{snippet}"
        );
        // Every anchor in a gapped snippet still validates against the result.
        let new_content = result.new_content.as_deref().expect("applied");
        let new_index = FileIndex::new(new_content);
        for line in snippet.lines().filter(|l| !l.starts_with("... ")) {
            let anchor_part = line.split('\u{2192}').next().expect("anchor prefix");
            let parsed = ParsedAnchor::parse(anchor_part).expect("snippet anchor parses");
            assert_eq!(s.validate(&parsed, &new_index), ValidationResult::Valid);
        }
    }

    #[test]
    fn splice_preserves_crlf_normalization_and_line_count() {
        // The corpus mixes `\r`-terminated lines, so the pre-edit join is
        // shorter than the file — the case the capacity bound must survive.
        let content = corpus(60, 0xC12F_0001, true);
        let s = scheme();
        let result = apply_edits(&content, &[replace(&anchor_for(&content, 30, s), "X")], s);
        let produced = result.new_content.expect("applied");

        let mut expected: Vec<&str> = FileIndex::new(&content).lines().to_vec();
        expected[29] = "X";
        assert_eq!(produced, expected.join("\n"));
    }

    #[test]
    fn splice_capacity_never_under_reserves() {
        for seed in 0..8u32 {
            for &trailing in &[true, false] {
                let content = corpus(120, 0x5CA9_0000 + seed, trailing);
                let joined = FileIndex::new(&content).lines().join("\n");
                // The load-bearing bound: joining a file's own lines never
                // exceeds its byte length, because a `\r\n` terminator
                // collapses to a bare `\n`.
                assert!(joined.len() <= content.len(), "seed {seed}");

                let s = scheme();
                let target = 40usize;
                let replacement = "REPLACED\nWITH\nTHREE LINES";
                let op = replace(&anchor_for(&content, target, s), replacement);
                let produced = apply_edits(&content, &[op], s)
                    .new_content
                    .expect("applied");

                let removed = line_bytes(&FileIndex::new(&content).lines()[target - 1..target]);
                let inserted = line_bytes(&replacement.lines().collect::<Vec<_>>());
                let reserved = content.len() + inserted - removed;

                assert!(
                    reserved >= produced.len(),
                    "reserved {reserved} < produced {} (seed {seed})",
                    produced.len()
                );
                // And it over-reserves by exactly the terminator bytes the join
                // drops — never more.
                assert_eq!(reserved - produced.len(), content.len() - joined.len());
            }
        }
    }

    /// Assert every rendered snippet line is byte-identical to what the read
    /// path produces for that absolute line number.
    ///
    /// The read path splits `new_content` itself, so this is a direct
    /// differential check of `from_lines_partial`'s precondition: if the
    /// spliced line vector ever diverged from `split_lines(new_content)`, the
    /// anchors or line numbers here would disagree.
    ///
    /// Split on `'\n'` rather than with `lines()`: the snippet's parts are
    /// joined with a bare `'\n'`, and `lines()` would strip a trailing `'\r'`
    /// off each one — precisely the divergence this guard exists to catch.
    fn assert_snippet_matches_read_path(new_content: &str, snippet: &str, scheme: Scheme) {
        let mut checked = 0usize;
        for line in snippet.split('\n').filter(|l| !l.starts_with("... ")) {
            let anchor_part = line.split('\u{2192}').next().expect("anchor prefix");
            let line_no: usize = anchor_part
                .split(':')
                .next()
                .expect("line number")
                .parse()
                .expect("numeric line");
            let expected =
                crate::read::format_hashline_content(new_content, Some(line_no), Some(1), scheme);
            assert_eq!(line, expected, "snippet line {line_no}");
            checked += 1;
        }
        assert!(checked > 0, "no snippet lines checked");
    }

    /// The spliced line vector must equal `split_lines(new_content)` for every
    /// op shape, with and without a trailing newline — otherwise adopting it as
    /// the snippet index would misnumber anchors.
    #[test]
    fn spliced_lines_match_a_fresh_split_of_the_new_content() {
        for &trailing in &[true, false] {
            for seed in 0..4u32 {
                let mut content = corpus(120, 0x5911_0000 + seed, trailing);
                if !trailing && content.ends_with('\n') {
                    content.pop();
                }
                let total = FileIndex::new(&content).len();

                for s in [scheme(), content_only(), checkpoint()] {
                    let single = vec![replace(&anchor_for(&content, 40, s), "SINGLE")];
                    let multiline = vec![replace(&anchor_for(&content, 40, s), "ONE\nTWO\nTHREE")];
                    let range = vec![HashlineOp::Replace {
                        anchor: anchor_for(&content, 30, s),
                        end_anchor: Some(anchor_for(&content, 36, s)),
                        content: "COLLAPSED".to_owned(),
                    }];
                    let delete = vec![replace(&anchor_for(&content, 50, s), "")];
                    let insert = vec![HashlineOp::InsertAfter {
                        anchor: anchor_for(&content, 20, s),
                        content: "INSERTED\nLINES".to_owned(),
                    }];
                    let batch = vec![
                        replace(&anchor_for(&content, 10, s), "B1"),
                        replace(&anchor_for(&content, 60, s), "B2\nB2b"),
                        HashlineOp::InsertAfter {
                            anchor: anchor_for(&content, 100, s),
                            content: "B3".to_owned(),
                        },
                    ];
                    let eof = vec![HashlineOp::InsertAfter {
                        anchor: "EOF".to_owned(),
                        content: "TAIL".to_owned(),
                    }];
                    let bof = vec![HashlineOp::InsertAfter {
                        anchor: "0:".to_owned(),
                        content: "HEAD".to_owned(),
                    }];
                    let delete_all = vec![HashlineOp::Replace {
                        anchor: anchor_for(&content, 1, s),
                        end_anchor: Some(anchor_for(&content, total, s)),
                        content: String::new(),
                    }];

                    for ops in [
                        single, multiline, range, delete, insert, batch, eof, bof, delete_all,
                    ] {
                        let result = apply_edits(&content, &ops, s);
                        let applied = expect_applied(&result);
                        let new_content = result.new_content.as_deref().expect("applied");

                        // The snippet's own line numbering is the observable
                        // consequence of the precondition holding.
                        assert_snippet_matches_read_path(new_content, &applied.snippet, s);
                    }
                }
            }
        }
    }

    /// Replacement content ending in a bare `\r` must not carry the CR into
    /// the spliced line vector.
    ///
    /// `str::lines` strips a `\r` only ahead of a `\n`, so a trailing one
    /// survives; joining the lines then makes it part of a `\r\n` terminator
    /// that a re-split drops, and the spliced vector stops matching
    /// `split_lines(new_content)` — the snippet renders `→TWO\r` where a
    /// follow-up read renders `→TWO`.
    ///
    /// Both sides are compared as raw bytes: iterating either through
    /// `lines()` would strip exactly the byte under test.
    #[test]
    fn trailing_carriage_return_in_replacement_never_reaches_the_snippet() {
        let content = "one\ntwo\nthree\n";
        let s = scheme();
        for (replacement, expected_content) in [
            ("TWO\r", "one\nTWO\nthree\n"),
            ("TWO\r\r", "one\nTWO\nthree\n"),
            ("TWO\r\nTHREE\r", "one\nTWO\nTHREE\nthree\n"),
        ] {
            let anchor = anchor_for(content, 2, s);
            let result = apply_edits(content, &[replace(&anchor, replacement)], s);
            let applied = expect_applied(&result);
            let new_content = result.new_content.as_deref().expect("applied");
            assert_eq!(new_content, expected_content, "replacement {replacement:?}");

            // The snippet covers the whole (short) file here, so the read path
            // renders exactly the same window.
            let total = FileIndex::new(new_content).len();
            let expected =
                crate::read::format_hashline_content(new_content, Some(1), Some(total), s);
            assert_eq!(
                applied.snippet.as_bytes(),
                expected.as_bytes(),
                "replacement {replacement:?}: snippet diverged from the read path"
            );
            assert!(
                !applied.snippet.contains('\r'),
                "replacement {replacement:?}: CR leaked into {:?}",
                applied.snippet
            );
        }

        // `insert_after` parses its content through the same split, so it
        // carries the same hazard.
        let op = HashlineOp::InsertAfter {
            anchor: anchor_for(content, 2, s),
            content: "MIDDLE\r".to_owned(),
        };
        let result = apply_edits(content, &[op], s);
        let applied = expect_applied(&result);
        let new_content = result.new_content.as_deref().expect("applied");
        assert_eq!(new_content, "one\ntwo\nMIDDLE\nthree\n");
        let total = FileIndex::new(new_content).len();
        let expected = crate::read::format_hashline_content(new_content, Some(1), Some(total), s);
        assert_eq!(applied.snippet.as_bytes(), expected.as_bytes());
    }

    /// The same divergence from the other direction: a `\r` the file itself
    /// carried into the splice rather than one an op introduced.
    ///
    /// `split_lines` leaves a trailing `\r` on exactly two kinds of line — a
    /// final line with no newline after it, and a line the file wrote as
    /// `\r\r\n` — and either one breaks the precondition as soon as it stops
    /// being the last element of the spliced vector. These bytes belong to the
    /// file, not to the request, so the fix reconciles the line vector and
    /// leaves the written content exactly as it was.
    #[test]
    fn carriage_returns_from_the_file_never_reach_the_snippet() {
        let s = scheme();

        // (a) Unterminated last line ending in `\r`, with an insert appended
        //     after it: the `\r` stops being last and becomes a terminator.
        let content = "one\ntwo\nthree\r";
        let op = HashlineOp::InsertAfter {
            anchor: "EOF".to_owned(),
            content: "TAIL".to_owned(),
        };
        let result = apply_edits(content, &[op], s);
        let applied = expect_applied(&result);
        let new_content = result.new_content.as_deref().expect("applied");
        // The file's own `\r` survives, now as the line's terminator.
        assert_eq!(new_content, "one\ntwo\nthree\r\nTAIL");
        assert_snippet_matches_read_path(new_content, &applied.snippet, s);
        assert!(!applied.snippet.contains('\r'), "{:?}", applied.snippet);

        // (b) An interior line the file wrote as `\r\r\n`, so one `\r` outlives
        //     the terminator strip and is part of the line's content.
        let content = "one\r\r\ntwo\n";
        let result = apply_edits(content, &[replace(&anchor_for(content, 2, s), "TWO")], s);
        let applied = expect_applied(&result);
        let new_content = result.new_content.as_deref().expect("applied");
        // Line 1 is untouched: its content `"one\r"` is preserved and only its
        // terminator is normalized, exactly as every other line's is.
        assert_eq!(new_content, "one\r\nTWO\n");
        assert_snippet_matches_read_path(new_content, &applied.snippet, s);
        assert!(!applied.snippet.contains('\r'), "{:?}", applied.snippet);
    }

    #[test]
    fn deleting_every_line_yields_an_empty_file() {
        // The splice can empty the line vector, but a file still has one
        // (empty) line — the snippet must render it rather than nothing.
        let content = "only\n";
        let s = scheme();
        let op = HashlineOp::Replace {
            anchor: anchor_for(content, 1, s),
            end_anchor: Some(anchor_for(content, 2, s)),
            content: String::new(),
        };
        let result = apply_edits(content, &[op], s);
        let applied = expect_applied(&result);
        assert_eq!(result.new_content.as_deref(), Some(""));
        assert!(applied.snippet.ends_with('\u{2192}'), "{}", applied.snippet);
    }
}
