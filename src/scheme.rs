//! Anchor scheme selection, generation, validation, and shift recovery.
//!
//! Three schemes are provided as variants of the [`Scheme`] enum:
//!
//! - [`Scheme::ContentOnly`]: content-only line hash. Simplest, weakest
//!   freshness — edits above a line do not invalidate its anchor.
//!
//! - [`Scheme::Chunk`]: local line hash + fixed-size chunk fingerprint. Edits
//!   invalidate only anchors within the affected chunk. Recommended default.
//!
//! - [`Scheme::Checkpoint`]: local line hash + checkpoint-derived fingerprint
//!   computed from the nearest preceding checkpoint. Strongest freshness
//!   detection at the cost of more anchor churn after edits.
//!
//! All schemes share the same whitespace-normalized local line hash from
//! [`crate::hash::line_hash`], read out of a prebuilt [`FileIndex`]. `Scheme`
//! is a `Copy` enum rather than a boxed trait object so its methods inline into
//! the generation and recovery loops, and [`Anchor`] is a `Copy` value with
//! stack-encoded letters so anchors never allocate.
//!
//! [`Scheme::anchors_for_range`] is the central generation primitive: it yields
//! anchors for an arbitrary line window, with full-file generation being the
//! degenerate full-range case.

use std::fmt;
use std::ops::Range;

use crate::hash::{self, DEFAULT_HASH_LEN, EncodedHash, MAX_HASH_LEN, fold_line_hash};
use crate::index::FileIndex;

/// Default chunk size for [`Scheme::Chunk`] when constructed with defaults.
pub const DEFAULT_CHUNK_SIZE: usize = 16;

/// Default checkpoint interval for [`Scheme::Checkpoint`].
pub const DEFAULT_CHECKPOINT_INTERVAL: usize = 32;

/// Default search radius for shifted-anchor recovery (±15 lines).
pub const DEFAULT_SEARCH_RADIUS: usize = 15;

/// The pluggable anchor scheme.
///
/// Construct via [`Scheme::content_only`], [`Scheme::chunk`], or
/// [`Scheme::checkpoint`] (which validate their parameters), or from a
/// [`crate::config::SchemeConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Content-only line hash.
    ///
    /// Anchor format: `LINE:LOCAL` (e.g. `22:abc`). Validates only the
    /// normalized content of the specified line, so edits above the line do not
    /// invalidate its anchor. Weakest freshness semantics.
    ContentOnly {
        /// Number of letters in the local line hash.
        hash_len: usize,
    },

    /// Local line hash plus a fixed-size chunk fingerprint.
    ///
    /// Anchor format: `LINE:LOCAL:CHUNK` (e.g. `22:abc:rst`). `CHUNK`
    /// fingerprints the whole chunk containing the line, so edits invalidate
    /// anchors only within the affected chunk.
    Chunk {
        /// Number of letters in each hash component.
        hash_len: usize,
        /// Lines per chunk.
        chunk_size: usize,
    },

    /// Local line hash plus a checkpoint-chained fingerprint.
    ///
    /// Anchor format: `LINE:LOCAL:CKPT` (e.g. `22:abc:rst`). `CKPT` chains all
    /// line hashes from the nearest preceding checkpoint boundary through this
    /// line: strongest freshness detection, most anchor churn after edits.
    Checkpoint {
        /// Number of letters in each hash component.
        hash_len: usize,
        /// Lines between checkpoint boundaries.
        checkpoint_interval: usize,
    },
}

impl Default for Scheme {
    fn default() -> Self {
        Self::chunk(DEFAULT_HASH_LEN, DEFAULT_CHUNK_SIZE)
    }
}

impl Scheme {
    /// Create the content-only scheme.
    ///
    /// # Panics
    ///
    /// Panics if `hash_len` is not in `1..=4`.
    pub fn content_only(hash_len: usize) -> Self {
        assert_hash_len(hash_len);
        Self::ContentOnly { hash_len }
    }

    /// Create the chunk-fingerprinted scheme.
    ///
    /// # Panics
    ///
    /// Panics if `hash_len` is not in `1..=4` or `chunk_size` is 0.
    pub fn chunk(hash_len: usize, chunk_size: usize) -> Self {
        assert_hash_len(hash_len);
        assert!(chunk_size > 0, "chunk_size must be > 0");
        Self::Chunk {
            hash_len,
            chunk_size,
        }
    }

    /// Create the checkpoint-chained scheme.
    ///
    /// # Panics
    ///
    /// Panics if `hash_len` is not in `1..=4` or `checkpoint_interval` is 0.
    pub fn checkpoint(hash_len: usize, checkpoint_interval: usize) -> Self {
        assert_hash_len(hash_len);
        assert!(checkpoint_interval > 0, "checkpoint_interval must be > 0");
        Self::Checkpoint {
            hash_len,
            checkpoint_interval,
        }
    }

    /// Machine-readable name for this scheme (e.g. `"content_only_v1"`).
    pub fn name(&self) -> &'static str {
        match self {
            Self::ContentOnly { .. } => "content_only_v1",
            Self::Chunk { .. } => "chunk_v1",
            Self::Checkpoint { .. } => "checkpoint_v1",
        }
    }

    /// Number of lowercase letters in each anchor hash component.
    pub fn hash_len(&self) -> usize {
        match *self {
            Self::ContentOnly { hash_len }
            | Self::Chunk { hash_len, .. }
            | Self::Checkpoint { hash_len, .. } => hash_len,
        }
    }

    /// Whether anchors of this scheme carry a contextual fingerprint.
    pub fn has_context(&self) -> bool {
        !matches!(self, Self::ContentOnly { .. })
    }

    /// The contextual fingerprint for 0-based `idx`, or `None` for
    /// context-free schemes.
    pub fn context_fingerprint(&self, index: &FileIndex<'_>, idx: usize) -> Option<EncodedHash> {
        match *self {
            Self::ContentOnly { .. } => None,
            Self::Chunk {
                hash_len,
                chunk_size,
            } => Some(hash::encode_hash(
                index.chunk_fingerprint(idx, chunk_size),
                hash_len,
            )),
            Self::Checkpoint {
                hash_len,
                checkpoint_interval,
            } => Some(hash::encode_hash(
                index.checkpoint_fingerprint(idx, checkpoint_interval),
                hash_len,
            )),
        }
    }

    /// The anchor for 0-based `idx`, or `None` if the line is out of range.
    pub fn anchor_at(&self, index: &FileIndex<'_>, idx: usize) -> Option<Anchor> {
        let line_hash = index.hash(idx)?;
        Some(Anchor {
            line: idx + 1,
            local: hash::encode_hash(line_hash, self.hash_len()),
            context: self.context_fingerprint(index, idx),
        })
    }

    /// Generate anchors for the 0-based line range `range`.
    ///
    /// The range is clamped to the index, so `0..index.len()` yields every
    /// line's anchor and a narrower range yields exactly the corresponding
    /// slice of that sequence. Contextual fingerprints are carried forward
    /// across the iteration (one fold per chunk, one running checkpoint chain),
    /// and no anchor allocates.
    pub fn anchors_for_range<'i, 'a>(
        &self,
        index: &'i FileIndex<'a>,
        range: Range<usize>,
    ) -> AnchorIter<'i, 'a> {
        let end = range.end.min(index.len());
        AnchorIter {
            index,
            scheme: *self,
            next: range.start.min(end),
            end,
            chunk_cache: None,
            chain: None,
        }
    }

    /// Validate a parsed anchor against the indexed file content.
    pub fn validate(&self, anchor: &ParsedAnchor, index: &FileIndex<'_>) -> ValidationResult {
        let Some(idx) = anchor.line.checked_sub(1) else {
            return ValidationResult::OutOfRange;
        };
        let Some(line_hash) = index.hash(idx) else {
            return ValidationResult::OutOfRange;
        };

        // Validate the local line hash first — it is the cheap check.
        if hash::encode_hash(line_hash, self.hash_len()) != anchor.local {
            return ValidationResult::Stale;
        }

        if !self.has_context() {
            return ValidationResult::Valid;
        }

        // Context-bearing schemes require the context component — reject
        // truncated anchors that omit it, as they would silently weaken
        // validation to content-only semantics.
        let Some(ref expected_ctx) = anchor.context else {
            return ValidationResult::Stale;
        };
        match self.context_fingerprint(index, idx) {
            Some(actual) if actual == *expected_ctx => ValidationResult::Valid,
            _ => ValidationResult::Stale,
        }
    }

    /// Search for a shifted anchor within `±search_radius` lines of the
    /// anchor's recorded position.
    ///
    /// Returns [`ShiftResult::Found`] if exactly one nearby line validates
    /// under this scheme, [`ShiftResult::Ambiguous`] if several do, and
    /// [`ShiftResult::NotFound`] if none do.
    ///
    /// The scan is integer-only: candidate local hashes come straight from the
    /// index, and the contextual fingerprint is evaluated only for candidates
    /// whose local hash already matches.
    pub fn find_shifted(
        &self,
        anchor: &ParsedAnchor,
        index: &FileIndex<'_>,
        search_radius: usize,
    ) -> ShiftResult {
        let orig_idx = anchor.line.saturating_sub(1);
        let start = orig_idx.saturating_sub(search_radius);
        let end = (orig_idx + search_radius + 1).min(index.len());
        let hash_len = self.hash_len();

        let mut candidates: Vec<usize> = Vec::new();

        for idx in start..end {
            // Skip the original line — it already failed validation.
            if idx == orig_idx {
                continue;
            }

            // Cheap check: does the local line hash match? `end` is clamped to
            // the index, so every line in the window is present.
            let Some(line_hash) = index.hash(idx) else {
                break;
            };
            if hash::encode_hash(line_hash, hash_len) != anchor.local {
                continue;
            }

            // If the anchor carries context, the contextual fingerprint must
            // also match at this position. Context-free schemes ignore it, just
            // as `validate` does.
            if let Some(ref expected_ctx) = anchor.context
                && let Some(actual) = self.context_fingerprint(index, idx)
                && actual != *expected_ctx
            {
                continue;
            }

            candidates.push(idx + 1);
        }

        match candidates.len() {
            0 => ShiftResult::NotFound,
            1 => ShiftResult::Found {
                new_line: candidates[0],
            },
            _ => ShiftResult::Ambiguous { candidates },
        }
    }
}

/// Panic unless `hash_len` is a supported anchor hash length.
fn assert_hash_len(hash_len: usize) {
    assert!(
        hash_len > 0 && hash_len <= MAX_HASH_LEN,
        "hash_len must be 1..=4, got {hash_len}"
    );
}

/// A generated anchor for a single line.
///
/// `Copy` and allocation-free: both hash components are stack-encoded
/// [`EncodedHash`] values, so generating a file's anchors touches the heap only
/// for whatever collection the caller chooses to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// 1-based line number.
    pub line: usize,
    /// Encoded local line hash (e.g. `"abc"`).
    pub local: EncodedHash,
    /// Optional contextual fingerprint (e.g. `"rst"` for chunk/checkpoint).
    pub context: Option<EncodedHash>,
}

impl Anchor {
    /// Append this anchor's wire form to `out`: `"LINE:LOCAL"` or
    /// `"LINE:LOCAL:CONTEXT"`.
    ///
    /// This is the allocation-free rendering primitive; callers render straight
    /// into their output buffer.
    pub fn render_into(&self, out: &mut String) {
        let mut buf = itoa::Buffer::new();
        out.push_str(buf.format(self.line));
        out.push(':');
        self.render_suffix_into(out);
    }

    /// Append this anchor's hash components to `out`, without the line number:
    /// `"LOCAL"` or `"LOCAL:CONTEXT"`.
    pub fn render_suffix_into(&self, out: &mut String) {
        self.local.push_to(out);
        if let Some(context) = self.context {
            out.push(':');
            context.push_to(out);
        }
    }

    /// Render this anchor as a standalone string.
    ///
    /// Prefer [`Self::render_into`] when appending to an existing buffer.
    pub fn render(&self) -> String {
        // Line number (up to 20 digits) + two hash components + separators.
        let mut out = String::with_capacity(32);
        self.render_into(&mut out);
        out
    }
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.local)?;
        match self.context {
            Some(context) => write!(f, ":{context}"),
            None => Ok(()),
        }
    }
}

/// Iterator over the anchors of a line range, produced by
/// [`Scheme::anchors_for_range`].
///
/// Yields one [`Anchor`] per line in order. Contextual fingerprints are reused
/// across a chunk (chunk scheme) or extended incrementally (checkpoint scheme),
/// so a full sweep folds each line hash exactly once.
#[derive(Debug)]
pub struct AnchorIter<'i, 'a> {
    index: &'i FileIndex<'a>,
    scheme: Scheme,
    /// Next 0-based line index to yield.
    next: usize,
    /// Exclusive 0-based end index, already clamped to the index length.
    end: usize,
    /// Chunk scheme: the fingerprint of the last chunk touched.
    chunk_cache: Option<(usize, EncodedHash)>,
    /// Checkpoint scheme: the chain value of the previously yielded line.
    chain: Option<u32>,
}

impl Iterator for AnchorIter<'_, '_> {
    type Item = Anchor;

    fn next(&mut self) -> Option<Anchor> {
        if self.next >= self.end {
            return None;
        }
        let idx = self.next;
        let line_hash = self.index.hash(idx)?;
        self.next += 1;

        let hash_len = self.scheme.hash_len();
        let context = match self.scheme {
            Scheme::ContentOnly { .. } => None,
            Scheme::Chunk { chunk_size, .. } => {
                let chunk_idx = idx / chunk_size;
                let fingerprint = match self.chunk_cache {
                    Some((cached_idx, fingerprint)) if cached_idx == chunk_idx => fingerprint,
                    _ => {
                        let fingerprint = hash::encode_hash(
                            self.index.chunk_fingerprint(idx, chunk_size),
                            hash_len,
                        );
                        self.chunk_cache = Some((chunk_idx, fingerprint));
                        fingerprint
                    }
                };
                Some(fingerprint)
            }
            Scheme::Checkpoint {
                checkpoint_interval,
                ..
            } => {
                // Extend the running chain, except at a checkpoint boundary or
                // at the first line of the range (where the chain must be
                // folded from the boundary preceding it).
                let chain = match self.chain {
                    Some(previous) if !idx.is_multiple_of(checkpoint_interval) => {
                        fold_line_hash(previous, line_hash)
                    }
                    _ => self.index.checkpoint_fingerprint(idx, checkpoint_interval),
                };
                self.chain = Some(chain);
                Some(hash::encode_hash(chain, hash_len))
            }
        };

        Some(Anchor {
            line: idx + 1,
            local: hash::encode_hash(line_hash, hash_len),
            context,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.end.saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for AnchorIter<'_, '_> {}

/// A parsed anchor extracted from model input.
///
/// Unlike [`Anchor`], the hash components are owned strings: model input is not
/// guaranteed to fit the encoding (an over-long hash must validate as stale,
/// not be rejected as malformed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAnchor {
    /// 1-based line number.
    pub line: usize,
    /// Local line hash component.
    pub local: String,
    /// Optional contextual fingerprint component.
    pub context: Option<String>,
}

impl ParsedAnchor {
    /// Parse an anchor string into its components.
    ///
    /// Accepted formats:
    /// - `"22:abc"` → line=22, local="abc", context=None
    /// - `"22:abc:rst"` → line=22, local="abc", context=Some("rst")
    ///
    /// Returns `None` if the string is malformed (non-numeric line number,
    /// missing components, etc.).
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.splitn(3, ':');
        let line_str = parts.next()?;
        let local = parts.next()?;

        if line_str.is_empty() || local.is_empty() {
            return None;
        }

        let line: usize = line_str.parse().ok()?;
        if line == 0 {
            return None;
        }

        // Validate local hash: must be all lowercase ASCII letters.
        if !local.bytes().all(|b| b.is_ascii_lowercase()) {
            return None;
        }

        let context = parts.next().map(str::to_owned);
        // Validate context hash if present: must be non-empty lowercase ASCII letters.
        if let Some(ref ctx) = context
            && (ctx.is_empty() || !ctx.bytes().all(|b| b.is_ascii_lowercase()))
        {
            return None;
        }

        Some(Self {
            line,
            local: local.to_owned(),
            context,
        })
    }

    /// Render back to string form.
    pub fn render(&self) -> String {
        match &self.context {
            Some(ctx) => format!("{}:{}:{}", self.line, self.local, ctx),
            None => format!("{}:{}", self.line, self.local),
        }
    }
}

impl From<Anchor> for ParsedAnchor {
    fn from(anchor: Anchor) -> Self {
        Self {
            line: anchor.line,
            local: anchor.local.as_str().to_owned(),
            context: anchor.context.map(|ctx| ctx.as_str().to_owned()),
        }
    }
}

/// Result of validating an anchor against current file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Anchor is valid — the line content matches the expected hash.
    Valid,
    /// Anchor is stale — the line exists but its content has changed.
    Stale,
    /// Line number is out of range for the current file.
    OutOfRange,
}

/// Result of searching for a shifted anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShiftResult {
    /// Exactly one nearby line validates — the anchor shifted to this line.
    Found {
        /// 1-based line number where the anchor now validates.
        new_line: usize,
    },
    /// Multiple nearby lines validate — ambiguous recovery.
    Ambiguous {
        /// All candidate 1-based line numbers.
        candidates: Vec<usize>,
    },
    /// No nearby line validates.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::split_lines;
    use crate::testutil::corpus;

    fn sample_lines() -> Vec<&'static str> {
        vec![
            "import React from 'react';",
            "",
            "export function App() {",
            "  return <div>Hello</div>;",
            "}",
        ]
    }

    /// Join lines back into file content. `split_lines` round-trips this
    /// exactly, so line-oriented fixtures keep their indices.
    fn joined(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn all_schemes() -> [Scheme; 3] {
        [
            Scheme::content_only(DEFAULT_HASH_LEN),
            Scheme::chunk(DEFAULT_HASH_LEN, DEFAULT_CHUNK_SIZE),
            Scheme::checkpoint(DEFAULT_HASH_LEN, DEFAULT_CHECKPOINT_INTERVAL),
        ]
    }

    /// Every anchor of `index` under `scheme` (the full-range case).
    fn all_anchors(scheme: Scheme, index: &FileIndex<'_>) -> Vec<Anchor> {
        scheme.anchors_for_range(index, 0..index.len()).collect()
    }

    /// The pre-`FileIndex` text-based chunk anchor rendering, kept as a
    /// differential reference for the integer-fold implementation.
    fn reference_anchors(scheme: Scheme, lines: &[&str]) -> Vec<String> {
        let hash_len = scheme.hash_len();
        (0..lines.len())
            .map(|i| {
                let local = hash::encode_hash(hash::line_hash(lines[i]), hash_len);
                let context = match scheme {
                    Scheme::ContentOnly { .. } => None,
                    Scheme::Chunk { chunk_size, .. } => {
                        let start = (i / chunk_size) * chunk_size;
                        let end = (start + chunk_size).min(lines.len());
                        let mut combined: u32 = hash::fnv1a_32(b"chunk");
                        for line in &lines[start..end] {
                            combined ^= hash::line_hash(line);
                            combined = combined.wrapping_mul(16_777_619);
                        }
                        Some(hash::encode_hash(combined, hash_len))
                    }
                    Scheme::Checkpoint {
                        checkpoint_interval,
                        ..
                    } => {
                        let start = (i / checkpoint_interval) * checkpoint_interval;
                        let mut chain: u32 = hash::fnv1a_32(b"ckpt");
                        for line in &lines[start..=i] {
                            chain ^= hash::line_hash(line);
                            chain = chain.wrapping_mul(16_777_619);
                        }
                        Some(hash::encode_hash(chain, hash_len))
                    }
                };
                match context {
                    Some(ctx) => format!("{}:{local}:{ctx}", i + 1),
                    None => format!("{}:{local}", i + 1),
                }
            })
            .collect()
    }

    #[test]
    fn scheme_metadata() {
        let [content_only, chunk, checkpoint] = all_schemes();
        assert_eq!(content_only.name(), "content_only_v1");
        assert_eq!(chunk.name(), "chunk_v1");
        assert_eq!(checkpoint.name(), "checkpoint_v1");
        assert!(!content_only.has_context());
        assert!(chunk.has_context());
        assert!(checkpoint.has_context());
        for scheme in all_schemes() {
            assert_eq!(scheme.hash_len(), DEFAULT_HASH_LEN);
        }
        assert_eq!(Scheme::default(), Scheme::chunk(3, DEFAULT_CHUNK_SIZE));
        assert_eq!(Scheme::content_only(2).hash_len(), 2);
    }

    #[test]
    #[should_panic(expected = "hash_len must be 1..=4")]
    fn scheme_rejects_bad_hash_len() {
        let _ = Scheme::chunk(5, 8);
    }

    #[test]
    #[should_panic(expected = "chunk_size must be > 0")]
    fn scheme_rejects_zero_chunk_size() {
        let _ = Scheme::chunk(3, 0);
    }

    #[test]
    #[should_panic(expected = "checkpoint_interval must be > 0")]
    fn scheme_rejects_zero_checkpoint_interval() {
        let _ = Scheme::checkpoint(3, 0);
    }

    #[test]
    fn parse_anchor_two_parts() {
        let a = ParsedAnchor::parse("22:abc").unwrap();
        assert_eq!(a.line, 22);
        assert_eq!(a.local, "abc");
        assert!(a.context.is_none());
    }

    #[test]
    fn parse_anchor_three_parts() {
        let a = ParsedAnchor::parse("22:abc:rst").unwrap();
        assert_eq!(a.line, 22);
        assert_eq!(a.local, "abc");
        assert_eq!(a.context.as_deref(), Some("rst"));
    }

    #[test]
    fn parse_anchor_roundtrip() {
        for input in ["1:abc", "100:xyz:def", "42:ab"] {
            let parsed = ParsedAnchor::parse(input).unwrap();
            assert_eq!(parsed.render(), input);
        }
    }

    #[test]
    fn parse_anchor_rejects_malformed() {
        assert!(ParsedAnchor::parse("").is_none());
        assert!(ParsedAnchor::parse("abc").is_none());
        assert!(ParsedAnchor::parse(":abc").is_none());
        assert!(ParsedAnchor::parse("22:").is_none());
        assert!(ParsedAnchor::parse("0:abc").is_none()); // line 0 invalid
        assert!(ParsedAnchor::parse("22:ABC").is_none()); // uppercase
        assert!(ParsedAnchor::parse("22:abc:").is_none()); // empty context
        assert!(ParsedAnchor::parse("22:abc:XYZ").is_none()); // uppercase context
        assert!(ParsedAnchor::parse("abc:def").is_none()); // non-numeric line
    }

    #[test]
    fn anchor_render_without_context() {
        let a = Anchor {
            line: 5,
            local: hash::encode_hash(hash::line_hash("x"), 3),
            context: None,
        };
        let local = a.local.as_str().to_owned();
        assert_eq!(a.render(), format!("5:{local}"));
        assert_eq!(a.to_string(), a.render());
        let mut out = String::new();
        a.render_suffix_into(&mut out);
        assert_eq!(out, local);
    }

    #[test]
    fn anchor_render_with_context() {
        let a = Anchor {
            line: 22,
            local: hash::encode_hash(1, 3),
            context: Some(hash::encode_hash(2, 3)),
        };
        let expected = format!("22:{}:{}", a.local, a.context.unwrap());
        assert_eq!(a.render(), expected);
        assert_eq!(a.to_string(), expected);
        let mut out = String::from("prefix ");
        a.render_into(&mut out);
        assert_eq!(out, format!("prefix {expected}"));
    }

    #[test]
    fn anchor_is_copy_and_allocation_free() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Anchor>();
        assert_copy::<EncodedHash>();
        assert_copy::<Scheme>();
        // A `Copy` anchor cannot own a heap buffer, so generation allocates
        // nothing per line: the only heap traffic is the caller's collection.
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in all_schemes() {
            let mut iter = scheme.anchors_for_range(&index, 0..index.len());
            assert_eq!(iter.len(), index.len());
            let first = iter.next().unwrap();
            let copied = first;
            assert_eq!(copied, first);
        }
    }

    #[test]
    fn anchor_converts_to_parsed_anchor() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in all_schemes() {
            for anchor in all_anchors(scheme, &index) {
                let parsed = ParsedAnchor::from(anchor);
                assert_eq!(parsed.render(), anchor.render());
                assert_eq!(scheme.validate(&parsed, &index), ValidationResult::Valid);
            }
        }
    }

    #[test]
    fn generate_anchors_one_per_line() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in all_schemes() {
            let anchors = all_anchors(scheme, &index);
            assert_eq!(anchors.len(), index.len(), "scheme {}", scheme.name());
            for (i, anchor) in anchors.iter().enumerate() {
                assert_eq!(anchor.line, i + 1);
                assert_eq!(anchor.local.len(), scheme.hash_len());
            }
        }
    }

    #[test]
    fn content_only_has_no_context() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        let anchors = all_anchors(Scheme::content_only(DEFAULT_HASH_LEN), &index);
        assert!(anchors.iter().all(|a| a.context.is_none()));
    }

    #[test]
    fn chunk_and_checkpoint_have_context() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in [
            Scheme::chunk(DEFAULT_HASH_LEN, DEFAULT_CHUNK_SIZE),
            Scheme::checkpoint(DEFAULT_HASH_LEN, DEFAULT_CHECKPOINT_INTERVAL),
        ] {
            let anchors = all_anchors(scheme, &index);
            assert!(
                anchors.iter().all(|a| a.context.is_some()),
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn anchor_at_matches_full_range_generation() {
        for seed in 0..4u32 {
            let content = corpus(60, 0x5A5A_0000 + seed, seed % 2 == 0);
            let index = FileIndex::new(&content);
            for scheme in all_schemes() {
                let anchors = all_anchors(scheme, &index);
                for (i, expected) in anchors.iter().enumerate() {
                    assert_eq!(
                        scheme.anchor_at(&index, i).as_ref(),
                        Some(expected),
                        "scheme {} idx {i}",
                        scheme.name()
                    );
                }
                assert!(scheme.anchor_at(&index, index.len()).is_none());
            }
        }
    }

    #[test]
    fn anchors_match_text_based_reference() {
        // The integer-fold fingerprints must render byte-identically to the
        // pre-`FileIndex` text-recomputed implementation.
        for chunk_size in [1usize, 3, 8, 16] {
            for seed in 0..4u32 {
                for &trailing_newline in &[true, false] {
                    let content = corpus(64, 0xC0DE_0000 + seed, trailing_newline);
                    let index = FileIndex::new(&content);
                    let lines = index.lines();
                    for scheme in [
                        Scheme::content_only(3),
                        Scheme::chunk(3, chunk_size),
                        Scheme::checkpoint(3, chunk_size),
                    ] {
                        let rendered: Vec<String> = all_anchors(scheme, &index)
                            .iter()
                            .map(Anchor::render)
                            .collect();
                        assert_eq!(
                            rendered,
                            reference_anchors(scheme, lines),
                            "scheme {} chunk_size {chunk_size} seed {seed}",
                            scheme.name()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn windowed_generation_matches_full_range() {
        // Risk-1 gate: `anchors_for_range(w)` must equal the corresponding
        // slice of full-range generation for every scheme, window, and file
        // shape — including offsets straddling chunk boundaries.
        for chunk_size in [1usize, 4, 8, 16] {
            for seed in 0..6u32 {
                for &trailing_newline in &[true, false] {
                    let content = corpus(97, 0x1D00_0000_u32.wrapping_add(seed), trailing_newline);
                    let index = FileIndex::new(&content);
                    let total = index.len();
                    for scheme in [
                        Scheme::content_only(3),
                        Scheme::chunk(3, chunk_size),
                        Scheme::checkpoint(3, chunk_size),
                    ] {
                        let full = all_anchors(scheme, &index);
                        assert_eq!(full.len(), total);

                        let mut starts: Vec<usize> = vec![
                            0,
                            1,
                            chunk_size.saturating_sub(1),
                            chunk_size,
                            chunk_size + 1,
                            total / 2,
                            total.saturating_sub(1),
                            total,
                        ];
                        starts.extend([2 * chunk_size, 2 * chunk_size + 1]);
                        for start in starts {
                            for len in [0usize, 1, 2, chunk_size, chunk_size + 1, 30, total] {
                                let start = start.min(total);
                                let end = (start + len).min(total);
                                let window: Vec<Anchor> = scheme
                                    .anchors_for_range(&index, start..start + len)
                                    .collect();
                                assert_eq!(
                                    window,
                                    full[start..end],
                                    "scheme {} chunk_size {chunk_size} seed {seed} \
                                     trailing_newline {trailing_newline} window {start}..{}",
                                    scheme.name(),
                                    start + len
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn windowed_generation_short_final_chunk_and_empty_file() {
        // 10 content lines + synthetic trailing line = 11, so chunk_size 4
        // leaves a 3-line final chunk.
        let content: String = (0..10).map(|i| format!("line {i}\n")).collect();
        for (label, content) in [("short_final_chunk", content.as_str()), ("empty", "")] {
            let index = FileIndex::new(content);
            for scheme in [
                Scheme::content_only(3),
                Scheme::chunk(3, 4),
                Scheme::checkpoint(3, 4),
            ] {
                let full = all_anchors(scheme, &index);
                for start in 0..=index.len() {
                    let window: Vec<Anchor> = scheme
                        .anchors_for_range(&index, start..index.len())
                        .collect();
                    assert_eq!(
                        window,
                        full[start..],
                        "{label}: scheme {} start {start}",
                        scheme.name()
                    );
                }
            }
        }
    }

    #[test]
    fn windowed_generation_clamps_out_of_range() {
        let index = FileIndex::new("a\nb\nc\n");
        for scheme in all_schemes() {
            assert_eq!(scheme.anchors_for_range(&index, 100..200).count(), 0);
            assert_eq!(scheme.anchors_for_range(&index, 2..900).count(), 2);
            // A reversed range yields nothing rather than panicking.
            let (start, end) = (3usize, 1usize);
            assert_eq!(scheme.anchors_for_range(&index, start..end).count(), 0);
        }
    }

    #[test]
    fn validate_generated_anchors_valid() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in all_schemes() {
            for anchor in all_anchors(scheme, &index) {
                let parsed = ParsedAnchor::from(anchor);
                assert_eq!(
                    scheme.validate(&parsed, &index),
                    ValidationResult::Valid,
                    "scheme {} line {}",
                    scheme.name(),
                    parsed.line
                );
            }
        }
    }

    #[test]
    fn validate_out_of_range() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in all_schemes() {
            let parsed = ParsedAnchor {
                line: 999,
                local: "abc".to_owned(),
                context: None,
            };
            assert_eq!(
                scheme.validate(&parsed, &index),
                ValidationResult::OutOfRange,
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn validate_stale_on_content_change() {
        let original = sample_lines();
        let original_content = joined(&original);
        let original_index = FileIndex::new(&original_content);

        let mut modified = original.clone();
        modified[2] = "export function Renamed() {";
        let modified_content = joined(&modified);
        let modified_index = FileIndex::new(&modified_content);

        for scheme in all_schemes() {
            let anchors = all_anchors(scheme, &original_index);
            let parsed = ParsedAnchor::from(anchors[2]);
            assert_eq!(
                scheme.validate(&parsed, &modified_index),
                ValidationResult::Stale,
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn context_schemes_reject_context_free_anchor() {
        let content = joined(&sample_lines());
        let index = FileIndex::new(&content);
        for scheme in [
            Scheme::chunk(DEFAULT_HASH_LEN, DEFAULT_CHUNK_SIZE),
            Scheme::checkpoint(DEFAULT_HASH_LEN, DEFAULT_CHECKPOINT_INTERVAL),
        ] {
            let anchors = all_anchors(scheme, &index);
            let parsed = ParsedAnchor {
                line: anchors[0].line,
                local: anchors[0].local.as_str().to_owned(),
                context: None, // truncated anchor
            };
            assert_eq!(
                scheme.validate(&parsed, &index),
                ValidationResult::Stale,
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn chunk_edit_within_chunk_invalidates_neighbors() {
        // With chunk_size=4, editing line 1 changes the fingerprint of all
        // lines in chunk 0 (lines 1-4) but not chunk 1 (lines 5+).
        let original = joined(&["a1", "b2", "c3", "d4", "e5", "f6", "g7", "h8"]);
        let modified = joined(&["CHANGED", "b2", "c3", "d4", "e5", "f6", "g7", "h8"]);
        let original_index = FileIndex::new(&original);
        let modified_index = FileIndex::new(&modified);
        let scheme = Scheme::chunk(3, 4);
        let anchors = all_anchors(scheme, &original_index);

        // Line 2 is in the edited chunk → stale.
        let in_chunk = ParsedAnchor::from(anchors[1]);
        assert_eq!(
            scheme.validate(&in_chunk, &modified_index),
            ValidationResult::Stale
        );

        // Line 6 is in the other chunk → still valid.
        let other_chunk = ParsedAnchor::from(anchors[5]);
        assert_eq!(
            scheme.validate(&other_chunk, &modified_index),
            ValidationResult::Valid
        );
    }

    #[test]
    fn find_shifted_found_after_insertion() {
        let original_content = joined(&["alpha one", "beta two", "gamma three"]);
        let original_index = FileIndex::new(&original_content);
        // Insert a line at the top → "beta two" shifts from line 2 to 3.
        let modified_content = joined(&["inserted", "alpha one", "beta two", "gamma three"]);
        let modified_index = FileIndex::new(&modified_content);

        for scheme in all_schemes() {
            let anchors = all_anchors(scheme, &original_index);
            let parsed = ParsedAnchor::from(anchors[1]);
            match scheme.find_shifted(&parsed, &modified_index, DEFAULT_SEARCH_RADIUS) {
                ShiftResult::Found { new_line } => {
                    assert_eq!(new_line, 3, "scheme {}", scheme.name());
                }
                // Context-bearing schemes may fail to recover when the shift
                // also changed the contextual fingerprint region — that is
                // acceptable (NotFound), but a wrong Found is not.
                ShiftResult::NotFound => {
                    assert_ne!(scheme.name(), "content_only_v1", "content_only must find");
                }
                other => panic!("scheme {}: unexpected {other:?}", scheme.name()),
            }
        }
    }

    #[test]
    fn find_shifted_ambiguous_duplicate_lines() {
        let scheme = Scheme::content_only(DEFAULT_HASH_LEN);
        let original = joined(&["unique", "dup", "x"]);
        let original_index = FileIndex::new(&original);
        let anchors = all_anchors(scheme, &original_index);
        // Two copies of "dup" near the original location, neither at the
        // original line index.
        let modified = joined(&["dup", "changed", "dup", "x"]);
        let modified_index = FileIndex::new(&modified);
        let parsed = ParsedAnchor {
            line: anchors[1].line,
            local: anchors[1].local.as_str().to_owned(),
            context: None,
        };
        match scheme.find_shifted(&parsed, &modified_index, DEFAULT_SEARCH_RADIUS) {
            ShiftResult::Ambiguous { candidates } => assert_eq!(candidates, vec![1, 3]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn find_shifted_not_found() {
        let scheme = Scheme::content_only(DEFAULT_HASH_LEN);
        let original = joined(&["only line"]);
        let original_index = FileIndex::new(&original);
        let anchors = all_anchors(scheme, &original_index);
        let modified = joined(&["completely different"]);
        let modified_index = FileIndex::new(&modified);
        let parsed = ParsedAnchor::from(anchors[0]);
        assert_eq!(
            scheme.find_shifted(&parsed, &modified_index, DEFAULT_SEARCH_RADIUS),
            ShiftResult::NotFound
        );
    }

    #[test]
    fn find_shifted_respects_radius() {
        let scheme = Scheme::content_only(DEFAULT_HASH_LEN);
        let target = "needle line";
        let mut modified: Vec<&str> = vec!["filler"; 40];
        modified.push(target);
        let modified_content = joined(&modified);
        let modified_index = FileIndex::new(&modified_content);

        let target_content = joined(&[target]);
        let target_index = FileIndex::new(&target_content);
        let anchors = all_anchors(scheme, &target_index);
        let parsed = ParsedAnchor {
            line: 1,
            local: anchors[0].local.as_str().to_owned(),
            context: None,
        };
        // Target is at line 41, far outside radius 5 from line 1.
        assert_eq!(
            scheme.find_shifted(&parsed, &modified_index, 5),
            ShiftResult::NotFound
        );
        // Wide radius finds it.
        assert_eq!(
            scheme.find_shifted(&parsed, &modified_index, 50),
            ShiftResult::Found { new_line: 41 }
        );
    }

    #[test]
    fn anchors_stable_across_reindent() {
        // Whitespace-normalized hashing: re-indenting a line keeps its local
        // hash stable under every scheme.
        let original = joined(&["fn main() {", "    let x = 1;", "}"]);
        let reindented = joined(&["fn main() {", "        let x = 1;", "}"]);
        let original_index = FileIndex::new(&original);
        let reindented_index = FileIndex::new(&reindented);
        for scheme in all_schemes() {
            let a = all_anchors(scheme, &original_index);
            let b = all_anchors(scheme, &reindented_index);
            assert_eq!(a[1].local, b[1].local, "scheme {}", scheme.name());
        }
    }

    #[test]
    fn joined_round_trips_through_split_lines() {
        // The line-oriented fixtures above rely on this identity.
        for lines in [
            sample_lines(),
            vec!["a", "b"],
            vec!["a", ""],
            vec![""],
            vec!["", "", "x"],
        ] {
            let content = joined(&lines);
            assert_eq!(split_lines(&content), lines);
            assert_eq!(FileIndex::new(&content).lines(), lines.as_slice());
        }
    }
}
