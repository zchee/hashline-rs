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
//! Wire rendering for v2 positional lines and the transitional v1 anchor format.
//!
//! Phase 3 production reads use [`render_snapshot_page`]. The v1
//! [`render_range`] path remains only for edit/grep until those tools migrate.

use std::fmt::Write as _;
use std::ops::Range;

use crate::index::FileIndex;
use crate::protocol::{
    PageCursor, Position, SnapshotHeader, render_read_line,
};
use crate::scheme::Scheme;
use crate::snapshot::{Snapshot, SnapshotError};

/// Separator between a v1 line's anchor and its content.
pub(crate) const CONTENT_SEPARATOR: char = '\u{2192}';

/// Number of decimal digits needed for `value`, at least 1.
fn decimal_digits(value: usize) -> usize {
    value.checked_ilog10().unwrap_or(0) as usize + 1
}

/// Bytes of anchor and separator overhead to reserve per rendered v1 line.
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
/// newline-separated v1 `ANCHOR→CONTENT` lines, appending to `out`.
///
/// Kept for edit/grep until Phase 4/5 migrate them off [`FileIndex`].
pub(crate) fn render_range(
    index: &FileIndex<'_>,
    scheme: Scheme,
    range: Range<usize>,
    out: &mut String,
) {
    let end = range.end.min(index.len());
    let start = range.start.min(end);

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

/// Display content of one logical line: strip a trailing LF and one preceding CR.
fn display_line_content(text: &str, start: usize, end: usize) -> &str {
    let mut content_end = end;
    let bytes = text.as_bytes();
    if content_end > start && bytes[content_end - 1] == b'\n' {
        content_end -= 1;
        if content_end > start && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }
    }
    // Line starts and ASCII terminators are UTF-8 boundaries by construction.
    &text[start..content_end]
}

/// Estimate decimal digit width for line and byte numbers up to `value`.
fn u64_digits(value: u64) -> usize {
    if value == 0 {
        1
    } else {
        value.ilog10() as usize + 1
    }
}

/// Render one v2 read page: header, `LINE@BYTE|CONTENT` lines, optional cursor footer.
///
/// Lines are 1-based inclusive from `start_line` for at most `limit` logical lines.
/// When more lines remain after the page, a [`PageCursor`] footer is appended.
///
/// # Errors
///
/// Returns [`SnapshotError`] when lazy offsets cannot be materialized or a line
/// start is missing for an in-range line number.
pub(crate) fn render_snapshot_page(
    snapshot: &Snapshot,
    path: &str,
    start_line: u64,
    limit: u16,
) -> Result<String, SnapshotError> {
    let line_count = snapshot.line_count();
    let header = SnapshotHeader::new(
        path.to_owned(),
        snapshot.id(),
        line_count,
        snapshot.byte_len(),
    )
    .map_err(SnapshotError::from)?;

    // Materialize offsets once before the page walk so capacity and slicing are O(1).
    let _ = snapshot.materialize_offsets()?;

    let start_line = start_line.max(1);
    if start_line > line_count {
        // Caller reports beyond-EOF; still return header-only success body only
        // when the page is empty because the file is empty at line 1.
        let mut out = header.render();
        if line_count == 1 && snapshot.byte_len() == 0 && start_line == 1 {
            out.push('\n');
            out.push_str(&render_read_line(
                Position::new(1, 0).expect("line 1 is valid"),
                "",
            ));
        }
        return Ok(out);
    }

    let limit = u64::from(limit.max(1));
    let end_line_inclusive = start_line
        .saturating_add(limit)
        .saturating_sub(1)
        .min(line_count);
    let page_lines = end_line_inclusive.saturating_sub(start_line).saturating_add(1);

    let text = snapshot.text();
    // Capacity: header + per-line "L@B|content\n" with digit upper bounds.
    let max_line_digits = u64_digits(end_line_inclusive);
    let max_byte_digits = u64_digits(snapshot.byte_len());
    let per_line = max_line_digits + 1 + max_byte_digits + 1 + 1; // L @ B | \n
    let mut capacity = header.render().len() + 1;
    capacity = capacity.saturating_add(
        usize::try_from(page_lines).unwrap_or(usize::MAX).saturating_mul(per_line),
    );
    // Content bytes: span from first line start through last line end.
    if let (Some(first), Some(last_start)) = (
        snapshot.line_start(start_line)?,
        snapshot.line_start(end_line_inclusive)?,
    ) {
        let last_end = if end_line_inclusive < line_count {
            snapshot
                .line_start(end_line_inclusive + 1)?
                .unwrap_or(snapshot.byte_len())
        } else {
            snapshot.byte_len()
        };
        let first_u = usize::try_from(first).unwrap_or(0);
        let last_u = usize::try_from(last_end).unwrap_or(text.len());
        capacity = capacity.saturating_add(last_u.saturating_sub(first_u));
        let _ = last_start;
    }
    if end_line_inclusive < line_count {
        capacity = capacity.saturating_add(96); // cursor footer budget
    }

    let mut out = String::with_capacity(capacity);
    out.push_str(&header.render());

    for line in start_line..=end_line_inclusive {
        let start_byte = snapshot
            .line_start(line)?
            .ok_or(SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?;
        let end_byte = if line < line_count {
            snapshot
                .line_start(line + 1)?
                .ok_or(SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?
        } else {
            snapshot.byte_len()
        };
        let start_u =
            usize::try_from(start_byte).map_err(|_| SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?;
        let end_u =
            usize::try_from(end_byte).map_err(|_| SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?;
        let content = display_line_content(text, start_u, end_u);
        let position = Position::new(line, start_byte)
            .map_err(|_| SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?;
        out.push('\n');
        // render_read_line allocates; write directly to avoid per-line String.
        write!(out, "{position}|{content}").expect("writing to String cannot fail");
    }

    if end_line_inclusive < line_count {
        let next_line = end_line_inclusive + 1;
        let next_byte = snapshot
            .line_start(next_line)?
            .ok_or(SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?;
        let next = Position::new(next_line, next_byte)
            .map_err(|_| SnapshotError::Offsets(crate::snapshot::OffsetError::AddressInvariant))?;
        let cursor = PageCursor {
            snapshot: snapshot.id(),
            next,
        };
        out.push('\n');
        out.push_str(&cursor.render_footer());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SnapshotId;

    #[test]
    fn decimal_digits_counts_digits() {
        assert_eq!(decimal_digits(0), 1);
        assert_eq!(decimal_digits(9), 1);
        assert_eq!(decimal_digits(10), 2);
        assert_eq!(decimal_digits(120), 3);
    }

    #[test]
    fn display_line_content_strips_crlf() {
        assert_eq!(display_line_content("a\r\nb", 0, 3), "a");
        assert_eq!(display_line_content("a\nb", 0, 2), "a");
        assert_eq!(display_line_content("ab", 0, 2), "ab");
    }

    #[test]
    fn snapshot_page_empty_file() {
        let snap = Snapshot::from_bytes(Vec::new()).expect("empty");
        let page = render_snapshot_page(&snap, "empty.txt", 1, 2000).expect("render");
        assert!(page.starts_with("[hashline-v2 snapshot="));
        assert!(page.contains("lines=1 bytes=0 path=\"empty.txt\""));
        assert!(page.contains("\n1@0|"));
        assert!(!page.contains("[hashline-v2 next"));
        let _ = SnapshotId::from_u128(0); // keep import warm for clarity
    }

    #[test]
    fn snapshot_page_pagination_footer() {
        let body = (1..=5).map(|i| format!("line{i}\n")).collect::<String>();
        let snap = Snapshot::from_bytes(body.into_bytes()).expect("snap");
        let page = render_snapshot_page(&snap, "t.rs", 1, 2).expect("render");
        assert!(page.contains("\n1@0|line1"));
        assert!(page.contains("\n2@"));
        assert!(page.contains("[hashline-v2 next snapshot="));
        assert!(page.contains("position=3@"));
    }

    #[test]
    fn snapshot_pages_concatenate_like_single_window() {
        // No trailing newline → line_count equals numbered content lines.
        let body = (1..=10)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let snap = Snapshot::from_bytes(body.into_bytes()).expect("snap");
        assert_eq!(snap.line_count(), 10);
        let full = render_snapshot_page(&snap, "p.rs", 1, 10).expect("full");
        let p1 = render_snapshot_page(&snap, "p.rs", 1, 4).expect("p1");
        let p2 = render_snapshot_page(&snap, "p.rs", 5, 4).expect("p2");
        let p3 = render_snapshot_page(&snap, "p.rs", 9, 4).expect("p3");

        fn bodies(page: &str) -> Vec<&str> {
            page.lines()
                .filter(|line| !line.starts_with("[hashline-v2"))
                .collect()
        }
        let mut combined = bodies(&p1);
        combined.extend(bodies(&p2));
        combined.extend(bodies(&p3));
        assert_eq!(combined, bodies(&full));
    }
}
