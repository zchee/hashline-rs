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
//! The `ANCHOR→CONTENT` wire format, rendered in exactly one place.
//!
//! `read` renders windows of a file and `edit` renders
//! post-edit snippets and stale-anchor context. Those are the same bytes by
//! contract — a model compares an edit snippet against a later read and must
//! not see them disagree — so both go through [`render_range`] rather than
//! through two implementations kept in step by comment.
//!
//! `grep` is deliberately not a caller: its lines carry `:` for a
//! match and `-` for context instead of the separator here, so it is a
//! different format rather than the same one rendered elsewhere.

use std::ops::Range;

use crate::index::FileIndex;
use crate::scheme::Scheme;

/// Separator between a line's anchor and its content.
pub(crate) const CONTENT_SEPARATOR: char = '\u{2192}';

/// Number of decimal digits needed for `value`, at least 1.
fn decimal_digits(value: usize) -> usize {
    value.checked_ilog10().unwrap_or(0) as usize + 1
}

/// Bytes of anchor and separator overhead to reserve per rendered line, for a
/// range whose highest line number is `last_line`.
pub(crate) fn per_line_overhead(scheme: Scheme, last_line: usize) -> usize {
    let hash_len = scheme.hash_len();
    let context = if scheme.has_context() {
        1 + hash_len
    } else {
        0
    };
    // "LINE" ':' LOCAL [':' CONTEXT] CONTENT_SEPARATOR '\n'
    decimal_digits(last_line) + 1 + hash_len + context + CONTENT_SEPARATOR.len_utf8() + 1
}

/// Render the 0-based half-open line range `range` of `index` as
/// newline-separated `ANCHOR→CONTENT` lines, appending to `out`.
///
/// The range is clamped to the index, so an out-of-range range renders nothing
/// and appends no separator. Reserving up front is the point of the pass over
/// `range` that precedes the render loop: the caller's buffer grows once.
///
/// # Panics
///
/// Panics if `index` is a partial [`FileIndex`] that does not hash everything
/// `range`'s anchors depend on — expand each rendered range through
/// [`Scheme::required_hash_span`] when building the index.
pub(crate) fn render_range(
    index: &FileIndex<'_>,
    scheme: Scheme,
    range: Range<usize>,
    out: &mut String,
) {
    let end = range.end.min(index.len());
    let start = range.start.min(end);

    // Line by line rather than a slice of the whole-file line vector, so a
    // partial index never has to materialize the lines it skipped.
    let content_bytes: usize = (start..end)
        .filter_map(|idx| index.line(idx))
        .map(str::len)
        .sum();
    out.reserve(content_bytes + (end - start) * per_line_overhead(scheme, end));

    let mut first = true;
    for (offset, anchor) in scheme.anchors_for_range(index, start..end).enumerate() {
        if first {
            first = false;
        } else {
            out.push('\n');
        }
        anchor.render_into(out);
        out.push(CONTENT_SEPARATOR);
        out.push_str(index.line(start + offset).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_digits_counts_digits() {
        assert_eq!(decimal_digits(0), 1);
        assert_eq!(decimal_digits(9), 1);
        assert_eq!(decimal_digits(10), 2);
        assert_eq!(decimal_digits(999), 3);
        assert_eq!(decimal_digits(1_000), 4);
    }

    /// The reserved overhead must cover the widest anchor a range can render,
    /// for every scheme shape — it is what keeps the render loop from growing
    /// the caller's buffer mid-write.
    #[test]
    fn per_line_overhead_covers_the_widest_rendered_anchor() {
        let content: String = (1..=120).map(|i| format!("line {i}\n")).collect();
        let index = FileIndex::new(&content);
        for scheme in [
            Scheme::content_only(1),
            Scheme::content_only(4),
            Scheme::chunk(3, 8),
            Scheme::checkpoint(4, 32),
        ] {
            let end = index.len();
            let budget = per_line_overhead(scheme, end);
            for line in 0..end {
                let anchor = scheme.anchor_at(&index, line).expect("line within file");
                let rendered = anchor.render().len() + CONTENT_SEPARATOR.len_utf8() + 1;
                assert!(
                    rendered <= budget,
                    "scheme {} line {line}: rendered {rendered} > budget {budget}",
                    scheme.name()
                );
            }
        }
    }

    /// An out-of-range range renders nothing and leaves the buffer untouched.
    #[test]
    fn out_of_range_renders_nothing() {
        let index = FileIndex::new("a\nb\n");
        let mut out = String::from("kept");
        render_range(&index, Scheme::content_only(3), 100..200, &mut out);
        assert_eq!(out, "kept");
    }
}
