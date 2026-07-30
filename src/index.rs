//! Per-request file index: line slices plus their normalized line hashes.
//!
//! Every hashline tool call splits and hashes a file's lines exactly once, into
//! a [`FileIndex`]. All downstream consumers — anchor generation, anchor
//! validation, shifted-anchor recovery, snippet rendering — read from that
//! shared index instead of re-splitting or re-hashing the content.
//!
//! Contextual fingerprints (chunk and checkpoint) are derived from the stored
//! `u32` line hashes by integer folds ([`crate::hash::fold_line_hash`]), so no
//! line text is ever hashed twice. They are computed on demand rather than
//! eagerly materialized: a fold costs `O(chunk_size)` / `O(interval)` integer
//! operations, a full-file anchor sweep amortizes to one fold per chunk (the
//! anchor iterator carries the chunk fingerprint / checkpoint chain forward),
//! and a windowed read pays only for the chunks it actually renders.
//!
//! [`FileIndex::new_partial`] takes this one step further for windowed reads:
//! it hashes only the caller-declared spans, which is what makes a 2,000-line
//! window of a 100,000-line file cost `O(window)` hashing instead of `O(file)`.
//! Reading a hash outside those spans is a programmer error and panics.
//! Line numbering and totals stay whole-file correct, but only the spans' line
//! slices are materialized: outside them the scan just counts newlines, and the
//! skipped slices are rebuilt on demand if a caller ever asks for one.

use std::cell::OnceCell;
use std::ops::Range;

use memchr::memchr_iter;

use crate::hash::{self, LineHasher, fold_line_hash};

/// Seed for chunk fingerprints — the pre-image `"chunk"` domain-separates them
/// from checkpoint chains.
const CHUNK_SEED: u32 = hash::fnv1a_32(b"chunk");

/// Seed for checkpoint chains.
const CHECKPOINT_SEED: u32 = hash::fnv1a_32(b"ckpt");

/// Assumed average line length, used only to pre-size the index vectors.
const AVG_LINE_BYTES: usize = 24;

/// Feed each anchor line of `content` to `sink`, in order.
///
/// Line semantics match [`str::lines`] — `\n` separated, with a single
/// preceding `\r` stripped — plus the synthetic trailing empty line hashline's
/// 1-based numbering requires for content ending in `\n`. Empty content yields
/// exactly one empty line.
fn for_each_line<'a>(content: &'a str, mut sink: impl FnMut(&'a str)) {
    if content.is_empty() {
        sink("");
        return;
    }

    let bytes = content.as_bytes();
    let mut start = 0usize;

    for newline in memchr_iter(b'\n', bytes) {
        let mut end = newline;
        // `str::lines()` strips exactly one `\r` immediately before the `\n`.
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        sink(&content[start..end]);
        start = newline + 1;
    }

    if start < bytes.len() {
        sink(&content[start..]);
    } else {
        // `content` ends with '\n': `str::lines()` yields no trailing entry,
        // but the 1-based line numbering convention needs one.
        sink("");
    }
}

/// Split file content into lines suitable for anchor generation.
///
/// Strips the line terminator from each line. The returned `Vec<&str>` has one
/// entry per logical line: `"hello\n"` has 2 lines (line 1 = `"hello"`,
/// line 2 = `""`), and `""` has 1 line (`""`).
pub fn split_lines(content: &str) -> Vec<&str> {
    let mut lines = Vec::with_capacity(content.len() / AVG_LINE_BYTES + 2);
    for_each_line(content, |line| lines.push(line));
    lines
}

/// Which lines of a [`FileIndex`] carry a computed hash.
#[derive(Debug, Clone)]
enum Hashed {
    /// Every line is hashed — the [`FileIndex::new`] whole-file case.
    All,
    /// Only these line-index ranges are hashed, sorted and disjoint.
    Spans(Vec<Range<usize>>),
}

/// Normalize caller-supplied hash spans: clamp to `len`, drop empty and
/// reversed ranges, sort, and merge overlapping or touching ranges.
///
/// The result is sorted and disjoint, which is what makes span membership a
/// binary search and lets a covering check compare against a single range.
fn normalize_spans(spans: &[Range<usize>], len: usize) -> Vec<Range<usize>> {
    let mut clamped: Vec<Range<usize>> = spans
        .iter()
        .filter_map(|span| {
            let start = span.start.min(len);
            let end = span.end.min(len);
            (start < end).then_some(start..end)
        })
        .collect();
    clamped.sort_unstable_by_key(|span| span.start);

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(clamped.len());
    for span in clamped {
        match merged.last_mut() {
            // Touching ranges (`span.start == last.end`) merge too: their
            // union is contiguous, so one range describes it exactly.
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// Bytes examined per step while the span scan skips toward its next span.
///
/// Counting the newlines in a block is a SIMD popcount that never materializes
/// a position; visiting each position instead costs roughly an order of
/// magnitude more per byte (measured ~46 µs vs ~406 µs over 1.6 MB). The scan
/// therefore counts its way to the next span a block at a time and pays the
/// per-position price only inside the spans. The block size bounds how far past
/// a span's first line the counting may overshoot before it switches modes.
const SKIP_BLOCK_BYTES: usize = 32 * 1024;

/// Line distance below which the scan walks newline positions instead of
/// counting a whole block.
///
/// A block count is charged in full even when the target is a few lines away,
/// so short hops — the common shape when a match-dense grep leaves many small
/// gaps between spans — are cheaper walked. The break-even point is where
/// visiting a position (~8 ns) times this many lines matches counting one block
/// (~1 µs over 32 KB).
const DIRECT_VISIT_LINES: usize = 128;

/// Number of lines from the line starting at `pos` through the end of `bytes`,
/// counted without materializing a single newline position.
///
/// `pos` at or past the end means the caller is already sitting on the
/// synthetic trailing empty line that content ending in `\n` implies.
fn lines_remaining(bytes: &[u8], pos: usize) -> usize {
    if pos >= bytes.len() {
        return 1;
    }
    // Every newline terminates a line, and whatever follows the last one is
    // one more: the file's final segment, or the synthetic empty line.
    memchr_iter(b'\n', &bytes[pos..]).count() + 1
}

/// Byte offset at which the line `count` lines after the one starting at `pos`
/// begins, or `None` if the content ends first.
fn advance_lines(bytes: &[u8], mut pos: usize, mut count: usize) -> Option<usize> {
    while count > 0 {
        if pos >= bytes.len() {
            return None;
        }
        if count <= DIRECT_VISIT_LINES {
            // Close enough that walking positions beats counting a block.
            for rel in memchr_iter(b'\n', &bytes[pos..]) {
                count -= 1;
                if count == 0 {
                    return Some(pos + rel + 1);
                }
            }
            return None;
        }
        let end = pos.saturating_add(SKIP_BLOCK_BYTES).min(bytes.len());
        let block = &bytes[pos..end];
        let newlines = memchr_iter(b'\n', block).count();
        if newlines < count {
            count -= newlines;
            pos = end;
            continue;
        }
        // The target newline is in this block, so this is the one block worth
        // paying per-position costs on.
        let mut seen = 0usize;
        for rel in memchr_iter(b'\n', block) {
            seen += 1;
            if seen == count {
                return Some(pos + rel + 1);
            }
        }
        // `newlines >= count` guarantees the loop above returned.
        return None;
    }
    Some(pos)
}

/// Split `content` into lines, materializing only the lines inside `spans`.
///
/// `spans` must already be sorted and disjoint. Returns those lines packed in
/// span order, together with the file's total line count — the only thing a
/// partial index needs to stay whole-file correct about numbering. Lines
/// outside the spans are only counted, never sliced, which is what keeps a
/// windowed read or a single grep hit from paying for a whole file's worth of
/// line splitting.
///
/// Line semantics are identical to [`for_each_line`].
fn scan_spanned_lines<'a>(content: &'a str, spans: &[Range<usize>]) -> (Vec<&'a str>, usize) {
    let wanted = spans.iter().fold(0usize, |acc, span| {
        acc.saturating_add(span.end.saturating_sub(span.start))
    });
    // An unbounded span — a read with no limit — asks for more lines than any
    // file can hold, so the file itself caps the estimate.
    let mut packed = Vec::with_capacity(wanted.min(content.len() / AVG_LINE_BYTES + 2));

    if content.is_empty() {
        // Empty content is one empty line; spans are sorted, so only the first
        // one can contain line 0.
        if spans.first().is_some_and(|span| span.start == 0) {
            packed.push("");
        }
        return (packed, 1);
    }

    let bytes = content.as_bytes();
    // Byte offset at which the line numbered `idx` starts. Spans and line
    // indices both only move forward, so `cursor` visits each span once.
    let mut pos = 0usize;
    let mut idx = 0usize;
    let mut cursor = 0usize;

    loop {
        while spans.get(cursor).is_some_and(|span| span.end <= idx) {
            cursor += 1;
        }
        let Some(span) = spans.get(cursor) else { break };

        if idx < span.start {
            let Some(next) = advance_lines(bytes, pos, span.start - idx) else {
                // The content ends before this span begins; nothing left to
                // materialize, only the line count still to settle.
                return (packed, idx + lines_remaining(bytes, pos));
            };
            idx = span.start;
            pos = next;
        }

        while idx < span.end {
            if pos >= bytes.len() {
                // Sitting on the synthetic trailing empty line.
                packed.push("");
                return (packed, idx + 1);
            }
            let Some(rel) = memchr::memchr(b'\n', &bytes[pos..]) else {
                // Final line of content that does not end in a newline.
                packed.push(&content[pos..]);
                return (packed, idx + 1);
            };
            let newline = pos + rel;
            let mut end = newline;
            // `str::lines()` strips exactly one `\r` immediately before `\n`.
            if end > pos && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            packed.push(&content[pos..end]);
            pos = newline + 1;
            idx += 1;
        }
    }

    (packed, idx + lines_remaining(bytes, pos))
}

/// Report a hash read that falls outside a partial index's hashed spans.
#[cold]
#[inline(never)]
fn unhashed_access(start: usize, end: usize, spans: &[Range<usize>]) -> ! {
    panic!(
        "hashline: partial FileIndex has no hash for 0-based lines {start}..{end} \
         (hashed spans: {spans:?}); build a full FileIndex with FileIndex::new for \
         validation paths"
    );
}

/// The lines a partial index materialized during its span scan.
///
/// Present only when the scan skipped something: an index whose spans covered
/// every line is indistinguishable from a whole-file one and stores its lines
/// directly.
#[derive(Debug, Clone)]
struct SparseLines<'a> {
    /// The content this index describes, kept so the skipped line slices can
    /// still be produced if a caller asks for one.
    content: &'a str,
    /// The hashed spans' lines, concatenated in span order.
    packed: Vec<&'a str>,
    /// Offset into `packed` where each hashed span's lines begin, parallel to
    /// the span list in [`Hashed::Spans`].
    starts: Vec<usize>,
    /// Every line of `content`, split on first demand and cached.
    full: OnceCell<Vec<&'a str>>,
}

impl<'a> SparseLines<'a> {
    /// Every line of the file, splitting the content on first use.
    ///
    /// The fallback for callers that reach outside the hashed spans. Nothing
    /// on the read, grep, or edit hot paths does — they render only what they
    /// asked to have hashed — so this stays a correctness backstop rather than
    /// a cost anyone pays routinely.
    fn materialize(&self) -> &[&'a str] {
        self.full.get_or_init(|| split_lines(self.content))
    }
}

/// A file's lines and their whitespace-normalized hashes, built once per
/// request.
///
/// Borrows the content it indexes, so no line text is copied.
#[derive(Debug, Clone)]
pub struct FileIndex<'a> {
    /// Every line, in order — empty for a partial index that materialized only
    /// its hashed spans (see `sparse`).
    lines: Vec<&'a str>,
    /// One slot per line. Slots outside the hashed spans of a partial index
    /// hold a placeholder that [`Self::assert_hashed`] refuses to hand out.
    hashes: Vec<u32>,
    /// Total line count, always at least 1.
    len: usize,
    hashed: Hashed,
    /// `Some` when only the hashed spans' lines were materialized.
    sparse: Option<SparseLines<'a>>,
}

impl<'a> FileIndex<'a> {
    /// Split and hash `content` in a single pass.
    pub fn new(content: &'a str) -> Self {
        let capacity = content.len() / AVG_LINE_BYTES + 2;
        let mut lines = Vec::with_capacity(capacity);
        let mut hashes = Vec::with_capacity(capacity);
        // One hasher for the whole buffer: its scratch is reused across lines,
        // and the normalizer choice is settled once here rather than per line.
        let mut hasher = LineHasher::for_content(content);
        for_each_line(content, |line| {
            lines.push(line);
            hashes.push(hasher.hash(line));
        });
        Self {
            len: lines.len(),
            lines,
            hashes,
            hashed: Hashed::All,
            sparse: None,
        }
    }

    /// Count every line of `content`, but hash — and materialize — only the
    /// lines in `hash_spans`.
    ///
    /// `hash_spans` holds 0-based, half-open line ranges; they are clamped to
    /// the file, sorted, and merged, so overlapping, reversed, empty, and
    /// out-of-range ranges are all accepted. [`Self::len`], [`Self::lines`],
    /// and [`Self::line`] stay whole-file correct, so a windowed read costs
    /// `O(file)` newline scanning plus `O(window)` hashing and line slicing
    /// instead of `O(file)` of both.
    ///
    /// Outside the spans only the newline positions are examined. A caller that
    /// does reach outside them — [`Self::lines`], or [`Self::line`] on a line
    /// the scan skipped — gets the correct answer, paid for by splitting the
    /// whole content once and caching it. Rendering paths ask only for lines
    /// they declared, so they never trigger it.
    ///
    /// Callers expand each rendered window to the block boundaries their
    /// scheme needs with [`crate::scheme::Scheme::required_hash_span`].
    ///
    /// # Panics
    ///
    /// [`Self::hash`], [`Self::hashes`], [`Self::chunk_fingerprint`], and
    /// [`Self::checkpoint_fingerprint`] panic when they would read a line
    /// outside `hash_spans`: anchor validation and shift recovery need
    /// whole-file hashes, so reaching for one that was never computed is a
    /// programmer error, not a runtime condition to recover from.
    pub fn new_partial(content: &'a str, hash_spans: &[Range<usize>]) -> Self {
        // The line count is unknown until the scan finishes, so the spans are
        // normalized against an unbounded file first. Clamping them afterwards
        // can only drop lines past the end, which the scan never reached, so
        // the two normalizations agree on every line that exists.
        let scan_spans = normalize_spans(hash_spans, usize::MAX);
        let (packed, len) = scan_spanned_lines(content, &scan_spans);
        let spans = normalize_spans(hash_spans, len);

        // Full coverage means the scan already materialized everything, which
        // is exactly a whole-file index — and it keeps the span checks out of
        // every later access.
        if let [only] = spans.as_slice()
            && only.start == 0
            && only.end == len
        {
            // The whole buffer is available, so the normalizer choice costs one
            // pass over it rather than one `memchr` per line.
            return Self::build(packed, spans, LineHasher::for_content(content));
        }

        // `vec![0; n]` lowers to a zeroed allocation, so the untouched slots
        // cost pages, not a hash per line.
        let mut hashes = vec![0u32; len];
        let mut starts = Vec::with_capacity(spans.len());
        let mut offset = 0usize;
        // `packed` is exactly the set of lines about to be hashed, so it is
        // also exactly what decides the normalizer.
        let mut hasher = LineHasher::for_lines(&packed);
        for span in &spans {
            starts.push(offset);
            let count = span.end - span.start;
            let hashed = &packed[offset..offset + count];
            for (slot, line) in hashes[span.clone()].iter_mut().zip(hashed) {
                *slot = hasher.hash(line);
            }
            offset += count;
        }
        debug_assert_eq!(offset, packed.len(), "span scan and span list disagree");

        Self {
            lines: Vec::new(),
            hashes,
            len,
            hashed: Hashed::Spans(spans),
            sparse: Some(SparseLines {
                content,
                packed,
                starts,
                full: OnceCell::new(),
            }),
        }
    }

    /// Index an already-split line vector, hashing only the lines in
    /// `hash_spans`.
    ///
    /// The splitting half of [`Self::new_partial`], skipped. A caller that has
    /// just built the line vector itself — the edit path, whose splice result
    /// *is* the post-edit line vector — would otherwise pay to re-split content
    /// it already knows the line structure of.
    ///
    /// `hash_spans` is normalized exactly as [`Self::new_partial`] normalizes
    /// it, and the out-of-span panic contract is identical.
    ///
    /// # Preconditions
    ///
    /// `lines` must be exactly what [`split_lines`] would produce for the
    /// content this index describes — one entry per logical line, terminators
    /// stripped, including the synthetic trailing empty line that content
    /// ending in `\n` implies. Passing anything else silently misnumbers every
    /// anchor. An empty vector is taken as the single empty line, so
    /// [`Self::len`] stays at least 1 either way.
    pub fn from_lines_partial(mut lines: Vec<&'a str>, hash_spans: &[Range<usize>]) -> Self {
        // Even empty content indexes as one empty line; keeping that true here
        // preserves the `len() >= 1` invariant the accessors rely on.
        if lines.is_empty() {
            lines.push("");
        }

        let spans = normalize_spans(hash_spans, lines.len());
        // Only the spans are hashed, so only they decide the normalizer. There
        // is no content buffer to scan in one pass here, but a partial index
        // hashes few enough lines that scanning them individually is cheap —
        // and full coverage arrives via `build` from `new_partial`, which does
        // have the buffer.
        let scannable = spans
            .iter()
            .all(|span| hash::lines_segment_scannable(&lines[span.clone()]));
        Self::build(lines, spans, LineHasher::with_segment_scan(scannable))
    }

    /// Hash the lines in `spans` with `hasher` and assemble the index.
    ///
    /// `spans` must already be normalized against `lines`, and `hasher` must
    /// have been built from exactly the lines those spans select.
    fn build(mut lines: Vec<&'a str>, spans: Vec<Range<usize>>, mut hasher: LineHasher) -> Self {
        if lines.is_empty() {
            lines.push("");
        }

        // `vec![0; n]` lowers to a zeroed allocation, so the untouched slots
        // cost pages, not a hash per line.
        let mut hashes = vec![0u32; lines.len()];
        for span in &spans {
            let range = span.clone();
            for (slot, line) in hashes[range.clone()].iter_mut().zip(&lines[range]) {
                *slot = hasher.hash(line);
            }
        }

        // Full coverage is indistinguishable from `new`, so record it as such
        // and skip the span checks entirely.
        let hashed = match spans.as_slice() {
            [only] if only.start == 0 && only.end == lines.len() => Hashed::All,
            _ => Hashed::Spans(spans),
        };
        Self {
            len: lines.len(),
            lines,
            hashes,
            hashed,
            sparse: None,
        }
    }

    /// Number of indexed lines. Always at least 1.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the index holds no lines.
    ///
    /// Always `false` — even empty content indexes as a single empty line —
    /// but provided so `len` has its conventional partner.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// All indexed lines, in order.
    ///
    /// A partial index built by [`Self::new_partial`] materialized only its
    /// hashed spans, so asking for every line splits the content once here and
    /// caches the result — correct, but the reason rendering paths read
    /// individual lines with [`Self::line`] instead.
    pub fn lines(&self) -> &[&'a str] {
        match &self.sparse {
            Some(sparse) => sparse.materialize(),
            None => &self.lines,
        }
    }

    /// All normalized line hashes, in order and parallel to [`Self::lines`].
    ///
    /// # Panics
    ///
    /// Panics if this index was built by [`Self::new_partial`] without covering
    /// every line: there is no whole-file hash slice to hand out. Read the
    /// hashed lines individually with [`Self::hash`], or build a full index
    /// with [`Self::new`].
    pub fn hashes(&self) -> &[u32] {
        if let Hashed::Spans(spans) = &self.hashed {
            unhashed_access(0, self.hashes.len(), spans);
        }
        &self.hashes
    }

    /// The line at 0-based `idx`, or `None` if out of range.
    ///
    /// On a partial index this is `O(log spans)` for a line inside a hashed
    /// span. A line outside every span is still returned correctly, at the cost
    /// of splitting the whole content once (cached thereafter).
    pub fn line(&self, idx: usize) -> Option<&'a str> {
        let Some(sparse) = &self.sparse else {
            return self.lines.get(idx).copied();
        };
        if idx >= self.len {
            return None;
        }
        if let Hashed::Spans(spans) = &self.hashed {
            // Spans are sorted and disjoint, so at most one can hold `idx`:
            // the last one starting at or before it.
            let after = spans.partition_point(|span| span.start <= idx);
            if after > 0 && idx < spans[after - 1].end {
                let span = &spans[after - 1];
                return Some(sparse.packed[sparse.starts[after - 1] + (idx - span.start)]);
            }
        }
        sparse.materialize().get(idx).copied()
    }

    /// The normalized hash of the line at 0-based `idx`, or `None` if out of
    /// range.
    ///
    /// # Panics
    ///
    /// Panics if `idx` is inside the file but outside the hashed spans of a
    /// partial index — see [`Self::new_partial`].
    pub fn hash(&self, idx: usize) -> Option<u32> {
        let hash = *self.hashes.get(idx)?;
        self.assert_hashed(idx, idx + 1);
        Some(hash)
    }

    /// Fingerprint of the fixed-size chunk containing 0-based `idx`.
    ///
    /// Folds the line hashes of the whole chunk, so every line in a chunk
    /// shares one fingerprint and any edit inside the chunk changes it.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0, or if the chunk is not fully hashed in a
    /// partial index — see [`Self::new_partial`].
    pub fn chunk_fingerprint(&self, idx: usize, chunk_size: usize) -> u32 {
        let start = (idx / chunk_size) * chunk_size;
        self.fold(CHUNK_SEED, start, start + chunk_size)
    }

    /// Checkpoint-chained fingerprint for 0-based `idx`.
    ///
    /// Folds the line hashes from the nearest preceding checkpoint boundary up
    /// to and including `idx`, so the fingerprint depends on everything since
    /// that boundary.
    ///
    /// # Panics
    ///
    /// Panics if `checkpoint_interval` is 0, or if the folded range is not
    /// fully hashed in a partial index — see [`Self::new_partial`].
    pub fn checkpoint_fingerprint(&self, idx: usize, checkpoint_interval: usize) -> u32 {
        let start = (idx / checkpoint_interval) * checkpoint_interval;
        self.fold(CHECKPOINT_SEED, start, idx + 1)
    }

    /// Panic unless every line in the non-empty range `start..end` is hashed.
    ///
    /// Spans are sorted and disjoint, so at most one of them can cover the
    /// range: the last span starting at or before `start`.
    fn assert_hashed(&self, start: usize, end: usize) {
        if let Hashed::Spans(spans) = &self.hashed {
            let after = spans.partition_point(|span| span.start <= start);
            if after == 0 || end > spans[after - 1].end {
                unhashed_access(start, end, spans);
            }
        }
    }

    /// Fold the line hashes in `start..end` (clamped to the index) into `seed`.
    fn fold(&self, seed: u32, start: usize, end: usize) -> u32 {
        let end = end.min(self.hashes.len());
        let start = start.min(end);
        if start < end {
            self.assert_hashed(start, end);
        }
        let mut acc = seed;
        for &line in &self.hashes[start..end] {
            acc = fold_line_hash(acc, line);
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Xorshift32, corpus};

    /// Pre-`FileIndex` line splitting, kept as a differential reference.
    fn reference_split_lines(content: &str) -> Vec<&str> {
        if content.is_empty() {
            return vec![""];
        }
        let mut lines: Vec<&str> = content.lines().collect();
        if content.ends_with('\n') {
            lines.push("");
        }
        lines
    }

    /// Pre-`FileIndex` text-based chunk fingerprint, kept as a differential
    /// reference for the integer-fold implementation.
    fn reference_chunk_fingerprint(lines: &[&str], line_idx: usize, chunk_size: usize) -> u32 {
        let chunk_start = (line_idx / chunk_size) * chunk_size;
        let chunk_end = (chunk_start + chunk_size).min(lines.len());

        let mut combined: u32 = hash::fnv1a_32(b"chunk");
        for line in &lines[chunk_start..chunk_end] {
            let lh = hash::line_hash(line);
            combined ^= lh;
            combined = combined.wrapping_mul(16_777_619);
        }
        combined
    }

    /// Pre-`FileIndex` text-based checkpoint fingerprint, kept as a
    /// differential reference for the integer-fold implementation.
    fn reference_checkpoint_fingerprint(lines: &[&str], line_idx: usize, interval: usize) -> u32 {
        let checkpoint_start = (line_idx / interval) * interval;

        let mut chain: u32 = hash::fnv1a_32(b"ckpt");
        for line in &lines[checkpoint_start..=line_idx] {
            let lh = hash::line_hash(line);
            chain ^= lh;
            chain = chain.wrapping_mul(16_777_619);
        }
        chain
    }

    #[test]
    fn split_lines_basic() {
        assert_eq!(split_lines("a\nb\nc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_lines_trailing_newline() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b", ""]);
    }

    #[test]
    fn split_lines_empty() {
        assert_eq!(split_lines(""), vec![""]);
    }

    #[test]
    fn split_lines_single_newline() {
        assert_eq!(split_lines("\n"), vec!["", ""]);
    }

    #[test]
    fn split_lines_crlf_strips_carriage_return() {
        assert_eq!(split_lines("a\r\nb\r\n"), vec!["a", "b", ""]);
        // A lone `\r` not followed by `\n` is content, not a terminator.
        assert_eq!(split_lines("a\rb"), vec!["a\rb"]);
        assert_eq!(split_lines("a\r"), vec!["a\r"]);
        // Only one `\r` is stripped.
        assert_eq!(split_lines("a\r\r\n"), vec!["a\r", ""]);
    }

    #[test]
    fn split_lines_matches_reference_on_fixtures() {
        let fixtures = [
            "",
            "\n",
            "\n\n",
            "a",
            "a\n",
            "a\n\n",
            "a\nb",
            "a\nb\n",
            "\na",
            "\r\n",
            "a\r\n",
            "a\r\nb",
            "a\r\nb\r\n",
            "a\r",
            "a\r\r\n",
            "ünïcode\nlines\n",
            "trailing spaces   \n\tindented\n",
        ];
        for fixture in fixtures {
            assert_eq!(
                split_lines(fixture),
                reference_split_lines(fixture),
                "fixture {fixture:?}"
            );
        }
    }

    #[test]
    fn split_lines_matches_reference_on_random_corpora() {
        for seed in 0..24u32 {
            for &trailing_newline in &[true, false] {
                let content = corpus(97, seed, trailing_newline);
                assert_eq!(
                    split_lines(&content),
                    reference_split_lines(&content),
                    "seed {seed} trailing_newline {trailing_newline}"
                );
            }
        }
    }

    #[test]
    fn index_hashes_parallel_lines() {
        let content = "alpha\n  alpha  \nbeta\n";
        let index = FileIndex::new(content);
        assert_eq!(index.lines(), split_lines(content));
        assert_eq!(index.len(), 4);
        assert!(!index.is_empty());
        for (i, line) in index.lines().iter().enumerate() {
            assert_eq!(index.hash(i), Some(hash::line_hash(line)));
        }
        // Whitespace-normalized: differently indented copies hash equal.
        assert_eq!(index.hash(0), index.hash(1));
        assert_eq!(index.line(2), Some("beta"));
        assert_eq!(index.line(4), None);
        assert_eq!(index.hash(4), None);
    }

    #[test]
    fn index_of_empty_content_has_one_line() {
        let index = FileIndex::new("");
        assert_eq!(index.len(), 1);
        assert_eq!(index.line(0), Some(""));
        assert_eq!(index.hash(0), Some(hash::line_hash("")));
    }

    #[test]
    fn chunk_fingerprint_matches_text_reference() {
        for chunk_size in [1usize, 2, 3, 7, 8, 16, 64] {
            for seed in 0..6u32 {
                for &trailing_newline in &[true, false] {
                    let content = corpus(71, seed, trailing_newline);
                    let index = FileIndex::new(&content);
                    let lines = index.lines();
                    for idx in 0..index.len() {
                        assert_eq!(
                            index.chunk_fingerprint(idx, chunk_size),
                            reference_chunk_fingerprint(lines, idx, chunk_size),
                            "chunk_size {chunk_size} seed {seed} idx {idx}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn checkpoint_fingerprint_matches_text_reference() {
        for interval in [1usize, 2, 5, 16, 32, 128] {
            for seed in 0..6u32 {
                for &trailing_newline in &[true, false] {
                    let content = corpus(71, seed, trailing_newline);
                    let index = FileIndex::new(&content);
                    let lines = index.lines();
                    for idx in 0..index.len() {
                        assert_eq!(
                            index.checkpoint_fingerprint(idx, interval),
                            reference_checkpoint_fingerprint(lines, idx, interval),
                            "interval {interval} seed {seed} idx {idx}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fingerprints_at_chunk_boundaries_and_short_final_chunk() {
        // 10 lines with chunk_size 4 → chunks [0..4), [4..8), [8..10): the
        // last chunk is short, which is where a naive fold overruns.
        let content: String = (0..10).map(|i| format!("line {i}\n")).collect();
        let index = FileIndex::new(&content); // 11 lines (synthetic trailing "")
        let lines = index.lines();
        assert_eq!(index.len(), 11);

        for chunk_size in [4usize, 8] {
            for idx in 0..index.len() {
                assert_eq!(
                    index.chunk_fingerprint(idx, chunk_size),
                    reference_chunk_fingerprint(lines, idx, chunk_size),
                    "chunk_size {chunk_size} idx {idx}"
                );
            }
            // Lines inside one chunk share a fingerprint; neighbours across a
            // boundary do not.
            let first = index.chunk_fingerprint(0, chunk_size);
            assert_eq!(index.chunk_fingerprint(chunk_size - 1, chunk_size), first);
            assert_ne!(index.chunk_fingerprint(chunk_size, chunk_size), first);
        }
    }

    #[test]
    fn fingerprints_tolerate_out_of_range_index() {
        // Defensive: a fold past the end clamps instead of panicking.
        let index = FileIndex::new("a\nb\n");
        assert_eq!(index.chunk_fingerprint(999, 8), CHUNK_SEED);
        assert_eq!(index.checkpoint_fingerprint(999, 8), CHECKPOINT_SEED);
    }

    #[test]
    fn seeds_are_domain_separated() {
        assert_eq!(CHUNK_SEED, hash::fnv1a_32(b"chunk"));
        assert_eq!(CHECKPOINT_SEED, hash::fnv1a_32(b"ckpt"));
        assert_ne!(CHUNK_SEED, CHECKPOINT_SEED);
    }

    /// Span ranges built from `(start, end)` pairs.
    ///
    /// Pairs keep reversed and single-element range literals — which
    /// `clippy::reversed_empty_ranges` and `clippy::single_range_in_vec_init`
    /// reject as likely typos — out of the tests that need them on purpose.
    fn spans(pairs: &[(usize, usize)]) -> Vec<Range<usize>> {
        pairs.iter().map(|&(start, end)| start..end).collect()
    }

    /// Build a partial index whose hashed spans come from `(start, end)` pairs.
    fn partial_index<'a>(content: &'a str, hash_spans: &[(usize, usize)]) -> FileIndex<'a> {
        FileIndex::new_partial(content, &spans(hash_spans))
    }

    /// Assert that normalizing `input` against a `len`-line file yields
    /// `expected`, both given as `(start, end)` pairs.
    fn assert_normalized(input: &[(usize, usize)], len: usize, expected: &[(usize, usize)]) {
        assert_eq!(normalize_spans(&spans(input), len), spans(expected));
    }

    #[test]
    fn normalize_spans_clamps_sorts_and_merges() {
        // Sorting plus overlap merging.
        assert_normalized(&[(10, 20), (0, 5), (15, 25)], 100, &[(0, 5), (10, 25)]);
        // Touching ranges merge into one.
        assert_normalized(&[(0, 5), (5, 9)], 100, &[(0, 9)]);
        // Containment collapses into the outer range.
        assert_normalized(&[(0, 50), (10, 20)], 100, &[(0, 50)]);
        // Clamping to the file length, including a fully out-of-range span.
        assert_normalized(&[(90, 500)], 100, &[(90, 100)]);
        assert_normalized(&[(500, 600)], 100, &[]);
        // Empty and reversed ranges are dropped rather than panicking.
        assert_normalized(&[(7, 7), (9, 3)], 100, &[]);
        assert_normalized(&[], 100, &[]);
        // A zero-length file leaves nothing to hash.
        assert_normalized(&[(0, 10)], 0, &[]);
    }

    #[test]
    fn partial_index_splits_all_lines_and_hashes_one_span() {
        let content = corpus(200, 0x9A17_0001, true);
        let full = FileIndex::new(&content);
        let partial = partial_index(&content, &[(32, 64)]);

        // Line-level state stays whole-file correct.
        assert_eq!(partial.len(), full.len());
        assert!(!partial.is_empty());
        assert_eq!(partial.lines(), full.lines());
        for idx in 0..full.len() {
            assert_eq!(partial.line(idx), full.line(idx), "idx {idx}");
        }
        assert_eq!(partial.line(full.len()), None);

        // Hashes inside the span match the full index; out of range is `None`.
        for idx in 32..64 {
            assert_eq!(partial.hash(idx), full.hash(idx), "idx {idx}");
        }
        assert_eq!(partial.hash(full.len()), None);
    }

    #[test]
    fn partial_index_with_full_coverage_equals_full_index() {
        for content in ["", "a\nb\nc\n", "one line"] {
            let full = FileIndex::new(content);
            let partial = partial_index(content, &[(0, full.len())]);
            assert_eq!(partial.lines(), full.lines());
            // Full coverage is recorded as such, so the whole-file accessors
            // work and no span check remains.
            assert_eq!(partial.hashes(), full.hashes());
            assert_eq!(
                partial.chunk_fingerprint(0, 4),
                full.chunk_fingerprint(0, 4)
            );
            assert_eq!(
                partial.checkpoint_fingerprint(0, 4),
                full.checkpoint_fingerprint(0, 4)
            );
        }
        // An over-long span is clamped to full coverage, not rejected.
        let full = FileIndex::new("a\nb\n");
        let partial = partial_index("a\nb\n", &[(0, 9_999)]);
        assert_eq!(partial.hashes(), full.hashes());
    }

    #[test]
    fn partial_index_merged_spans_cover_every_requested_line() {
        let content = corpus(120, 0x0FF5_E750, false);
        let full = FileIndex::new(&content);
        // Deliberately unsorted, overlapping, and out-of-range input.
        let partial = partial_index(&content, &[(80, 96), (0, 16), (8, 24), (400, 500)]);
        for idx in (0..24).chain(80..96) {
            assert_eq!(partial.hash(idx), full.hash(idx), "idx {idx}");
        }
    }

    #[test]
    #[should_panic(expected = "partial FileIndex has no hash for 0-based lines 64..65")]
    fn partial_index_hash_outside_span_panics() {
        let content = corpus(200, 0xDEAD_5EED, true);
        let partial = partial_index(&content, &[(32, 64)]);
        let _ = partial.hash(64);
    }

    #[test]
    #[should_panic(expected = "build a full FileIndex with FileIndex::new")]
    fn partial_index_hash_before_span_panics() {
        let content = corpus(200, 0xDEAD_5EED, true);
        let partial = partial_index(&content, &[(32, 64)]);
        let _ = partial.hash(31);
    }

    #[test]
    #[should_panic(expected = "partial FileIndex has no hash")]
    fn partial_index_chunk_fingerprint_outside_span_panics() {
        let content = corpus(200, 0xC0FF_EE00, true);
        let partial = partial_index(&content, &[(32, 64)]);
        // Chunk 4 (lines 64..80) lies entirely outside the hashed span.
        let _ = partial.chunk_fingerprint(70, 16);
    }

    #[test]
    #[should_panic(expected = "partial FileIndex has no hash")]
    fn partial_index_partially_covered_fold_panics() {
        let content = corpus(200, 0xC0FF_EE01, true);
        // The span starts mid-chunk, so chunk 2 (lines 32..48) is only
        // partially hashed — folding it must panic instead of folding
        // placeholder zeros.
        let partial = partial_index(&content, &[(36, 64)]);
        let _ = partial.chunk_fingerprint(36, 16);
    }

    #[test]
    #[should_panic(expected = "partial FileIndex has no hash")]
    fn partial_index_checkpoint_fingerprint_outside_span_panics() {
        let content = corpus(200, 0xC0FF_EE02, true);
        // The span starts after the checkpoint boundary at line 64, so the
        // chain for line 75 folds lines the partial index never hashed.
        let partial = partial_index(&content, &[(70, 96)]);
        let _ = partial.checkpoint_fingerprint(75, 32);
    }

    #[test]
    #[should_panic(expected = "build a full FileIndex with FileIndex::new")]
    fn partial_index_hashes_slice_panics() {
        let content = corpus(64, 0xBAAD_F00D, true);
        let partial = partial_index(&content, &[(0, 8)]);
        let _ = partial.hashes();
    }

    #[test]
    fn partial_index_with_no_spans_hashes_nothing() {
        let content = "a\nb\nc\n";
        let partial = partial_index(content, &[]);
        assert_eq!(partial.len(), 4);
        assert_eq!(partial.line(1), Some("b"));
        // Out of range still answers `None` rather than panicking.
        assert_eq!(partial.hash(4), None);
        // Fingerprint folds that clamp to an empty range read no hash at all.
        assert_eq!(partial.chunk_fingerprint(999, 8), CHUNK_SEED);
    }

    /// The span scan must agree with a full split on the line count for every
    /// terminator shape — it is what `len()` reports, and every anchor's line
    /// number depends on it.
    #[test]
    fn partial_index_line_count_matches_a_full_split() {
        let fixtures = [
            "",
            "\n",
            "\n\n",
            "a",
            "a\n",
            "a\n\n",
            "a\nb",
            "a\nb\n",
            "\na",
            "\r\n",
            "a\r\n",
            "a\r\nb",
            "a\r\nb\r\n",
            "a\r",
            "a\r\r\n",
            "ünïcode\nlines\n",
        ];
        let shapes: &[&[(usize, usize)]] = &[&[], &[(0, 1)], &[(1, 2)], &[(0, 9_999)]];
        for fixture in fixtures {
            let expected = split_lines(fixture).len();
            for shape in shapes {
                assert_eq!(
                    partial_index(fixture, shape).len(),
                    expected,
                    "fixture {fixture:?} shape {shape:?}"
                );
            }
        }
    }

    /// Skipping the out-of-span line slices must be invisible: every line
    /// lookup still answers exactly what a full index would, whether it lands
    /// inside a hashed span or on a line the scan never sliced.
    #[test]
    fn partial_index_line_lookups_match_a_full_index_everywhere() {
        let span_shapes: &[&[(usize, usize)]] = &[
            &[],
            &[(0, 16)],
            &[(32, 64)],
            &[(80, 96), (0, 16)],
            &[(119, 121)],
            &[(0, 9_999)],
        ];

        for seed in 0..4u32 {
            for &trailing_newline in &[true, false] {
                let content = corpus(120, 0x5A11_0000 + seed, trailing_newline);
                let full = FileIndex::new(&content);

                for shape in span_shapes {
                    let partial = partial_index(&content, shape);
                    assert_eq!(partial.len(), full.len(), "{shape:?}");

                    // In-span lines come straight from the scan's packed
                    // slices — read them first, before anything can force a
                    // full materialization.
                    for span in normalize_spans(&spans(shape), full.len()) {
                        for idx in span {
                            assert_eq!(partial.line(idx), full.line(idx), "{shape:?} idx {idx}");
                        }
                    }

                    // Out-of-span lines are recovered on demand, and the
                    // whole-file accessor still agrees line for line.
                    for idx in 0..full.len() {
                        assert_eq!(partial.line(idx), full.line(idx), "{shape:?} idx {idx}");
                    }
                    assert_eq!(partial.line(full.len()), None);
                    assert_eq!(partial.lines(), full.lines(), "{shape:?}");
                }
            }
        }
    }

    /// The scan skips toward its next span a [`SKIP_BLOCK_BYTES`] block at a
    /// time, so its seams only appear on content spanning several blocks. This
    /// corpus is a few hundred kilobytes, with spans placed near the start, in
    /// the middle, at the very end, and entirely past it.
    #[test]
    fn partial_index_spans_across_skip_blocks_match_a_full_index() {
        for &trailing_newline in &[true, false] {
            let content = corpus(4_000, 0x5B10_C000, trailing_newline);
            assert!(
                content.len() > 3 * SKIP_BLOCK_BYTES,
                "corpus must cross several skip blocks (is {} bytes)",
                content.len()
            );
            let full = FileIndex::new(&content);
            let len = full.len();

            let shapes: &[&[(usize, usize)]] = &[
                &[(1, 3)],
                &[(len / 2, len / 2 + 4)],
                &[(len - 3, len)],
                &[(1, 3), (len / 3, len / 3 + 2), (len - 2, len)],
                &[(len + 10, len + 20)],
            ];
            for shape in shapes {
                let partial = partial_index(&content, shape);
                assert_eq!(partial.len(), len, "{shape:?}");
                for span in normalize_spans(&spans(shape), len) {
                    for idx in span {
                        assert_eq!(partial.line(idx), full.line(idx), "{shape:?} idx {idx}");
                        assert_eq!(partial.hash(idx), full.hash(idx), "{shape:?} idx {idx}");
                    }
                }
                assert_eq!(partial.lines(), full.lines(), "{shape:?}");
            }
        }
    }

    /// `from_lines_partial` must be the splitting half of `new_partial`
    /// removed and nothing else: same lines, length, hashes, and full-coverage
    /// collapse, for every span shape.
    #[test]
    fn from_lines_partial_matches_new_partial() {
        let span_shapes: &[&[(usize, usize)]] = &[
            &[],
            &[(0, 16)],
            &[(32, 64)],
            &[(80, 96), (0, 16), (8, 24), (400, 500)],
            &[(0, 200)],
            &[(0, 9_999)],
            &[(7, 7), (9, 3)],
        ];

        for seed in 0..6u32 {
            for &trailing_newline in &[true, false] {
                let content = corpus(120, 0xF20D_0000 + seed, trailing_newline);
                for shape in span_shapes {
                    let hash_spans = spans(shape);
                    let split = FileIndex::new_partial(&content, &hash_spans);
                    let adopted = FileIndex::from_lines_partial(split_lines(&content), &hash_spans);

                    assert_eq!(adopted.lines(), split.lines(), "seed {seed} {shape:?}");
                    assert_eq!(adopted.len(), split.len());
                    assert!(!adopted.is_empty());
                    // Hashes agree wherever they are readable at all.
                    for span in normalize_spans(&hash_spans, split.len()) {
                        for idx in span {
                            assert_eq!(adopted.hash(idx), split.hash(idx), "idx {idx}");
                        }
                    }
                    assert_eq!(adopted.hash(split.len()), None);
                }
            }
        }
    }

    #[test]
    fn from_lines_partial_keeps_the_out_of_span_panic_contract() {
        let content = corpus(200, 0xF20D_9999, true);
        let adopted = FileIndex::from_lines_partial(split_lines(&content), &spans(&[(32, 64)]));
        // In-span reads work, out-of-range reads answer `None`...
        assert!(adopted.hash(40).is_some());
        assert_eq!(adopted.hash(adopted.len()), None);
        // ...and an in-file, out-of-span read is the loud programmer error.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| adopted.hash(64)));
        assert!(panicked.is_err(), "out-of-span read must panic");
    }

    #[test]
    fn from_lines_partial_full_coverage_collapses_to_all() {
        let content = "a\nb\nc\n";
        let lines = split_lines(content);
        let len = lines.len();
        let adopted = FileIndex::from_lines_partial(lines, &spans(&[(0, len)]));
        // Full coverage is recorded as `Hashed::All`, so the whole-file
        // accessors work rather than panicking.
        assert_eq!(adopted.hashes(), FileIndex::new(content).hashes());
    }

    #[test]
    fn from_lines_partial_treats_an_empty_vector_as_one_empty_line() {
        // The `len() >= 1` invariant holds even if a caller hands over the
        // empty vector a delete-everything splice produces.
        let adopted = FileIndex::from_lines_partial(Vec::new(), &spans(&[(0, 1)]));
        assert_eq!(adopted.len(), 1);
        assert!(!adopted.is_empty());
        assert_eq!(adopted.line(0), Some(""));
        assert_eq!(adopted.hash(0), Some(hash::line_hash("")));
        assert_eq!(FileIndex::new("").lines(), adopted.lines());
    }

    #[test]
    fn corpus_generator_is_deterministic() {
        assert_eq!(corpus(50, 7, true), corpus(50, 7, true));
        assert_ne!(corpus(50, 7, true), corpus(50, 8, true));
        let mut rng = Xorshift32::new(0);
        assert_ne!(rng.next_u32(), 0);
    }
}
