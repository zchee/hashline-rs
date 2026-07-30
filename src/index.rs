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

use memchr::memchr_iter;

use crate::hash::{self, fold_line_hash};

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

/// A file's lines and their whitespace-normalized hashes, built once per
/// request.
///
/// Borrows the content it indexes, so no line text is copied.
#[derive(Debug, Clone)]
pub struct FileIndex<'a> {
    lines: Vec<&'a str>,
    hashes: Vec<u32>,
}

impl<'a> FileIndex<'a> {
    /// Split and hash `content` in a single pass.
    pub fn new(content: &'a str) -> Self {
        let capacity = content.len() / AVG_LINE_BYTES + 2;
        let mut lines = Vec::with_capacity(capacity);
        let mut hashes = Vec::with_capacity(capacity);
        for_each_line(content, |line| {
            lines.push(line);
            hashes.push(hash::line_hash(line));
        });
        Self { lines, hashes }
    }

    /// Number of indexed lines. Always at least 1.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the index holds no lines.
    ///
    /// Always `false` — even empty content indexes as a single empty line —
    /// but provided so `len` has its conventional partner.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// All indexed lines, in order.
    pub fn lines(&self) -> &[&'a str] {
        &self.lines
    }

    /// All normalized line hashes, in order and parallel to [`Self::lines`].
    pub fn hashes(&self) -> &[u32] {
        &self.hashes
    }

    /// The line at 0-based `idx`, or `None` if out of range.
    pub fn line(&self, idx: usize) -> Option<&'a str> {
        self.lines.get(idx).copied()
    }

    /// The normalized hash of the line at 0-based `idx`, or `None` if out of
    /// range.
    pub fn hash(&self, idx: usize) -> Option<u32> {
        self.hashes.get(idx).copied()
    }

    /// Fingerprint of the fixed-size chunk containing 0-based `idx`.
    ///
    /// Folds the line hashes of the whole chunk, so every line in a chunk
    /// shares one fingerprint and any edit inside the chunk changes it.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_size` is 0.
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
    /// Panics if `checkpoint_interval` is 0.
    pub fn checkpoint_fingerprint(&self, idx: usize, checkpoint_interval: usize) -> u32 {
        let start = (idx / checkpoint_interval) * checkpoint_interval;
        self.fold(CHECKPOINT_SEED, start, idx + 1)
    }

    /// Fold the line hashes in `start..end` (clamped to the index) into `seed`.
    fn fold(&self, seed: u32, start: usize, end: usize) -> u32 {
        let end = end.min(self.hashes.len());
        let start = start.min(end);
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

    #[test]
    fn corpus_generator_is_deterministic() {
        assert_eq!(corpus(50, 7, true), corpus(50, 7, true));
        assert_ne!(corpus(50, 7, true), corpus(50, 8, true));
        let mut rng = Xorshift32::new(0);
        assert_ne!(rng.next_u32(), 0);
    }
}
