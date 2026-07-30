//! Anchor scheme abstraction and implementations.
//!
//! Three schemes are provided:
//!
//! - [`ContentOnly`]: content-only line hash. Simplest, weakest freshness —
//!   edits above a line do not invalidate its anchor.
//!
//! - [`ChunkFingerprint`]: local line hash + fixed-size chunk fingerprint.
//!   Edits invalidate only anchors within the affected chunk. Recommended
//!   default.
//!
//! - [`CheckpointChain`]: local line hash + checkpoint-derived fingerprint
//!   computed from the nearest preceding checkpoint. Strongest freshness
//!   detection at the cost of more anchor churn after edits.
//!
//! All schemes share the same whitespace-normalized local line hash from
//! [`crate::hash::line_hash`].

use std::fmt;

use crate::hash::{self, DEFAULT_HASH_LEN};

/// Split file content into lines suitable for anchor generation.
///
/// Strips trailing newlines from each line (matching the convention used by
/// [`AnchorScheme::generate_anchors`]). The returned `Vec<&str>` has one entry
/// per logical line: `"hello\n"` has 2 lines (line 1 = `"hello"`, line 2 = `""`).
pub fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return vec![""];
    }

    let mut lines: Vec<&str> = content.lines().collect();

    // `str::lines()` does not yield a trailing empty entry for content ending
    // with '\n'. Add one to match the 1-based line numbering convention.
    if content.ends_with('\n') {
        lines.push("");
    }

    lines
}

/// Trait for pluggable anchor generation and validation schemes.
///
/// Implementations generate anchors for file lines and validate anchors
/// against current file content.
pub trait AnchorScheme: fmt::Debug + Send + Sync {
    /// Machine-readable name for this scheme (e.g. `"content_only_v1"`).
    fn name(&self) -> &str;

    /// Number of lowercase letters in the local line hash component.
    fn hash_len(&self) -> usize;

    /// Generate anchors for all lines in a file.
    ///
    /// `lines` is a slice of the file's lines (without trailing newlines).
    /// Returns one `Anchor` per line, in order.
    fn generate_anchors(&self, lines: &[&str]) -> Vec<Anchor>;

    /// Validate a parsed anchor against current file content.
    ///
    /// `anchor` is the anchor to validate. `lines` is the current file
    /// content split by line. Returns the validation result.
    fn validate(&self, anchor: &ParsedAnchor, lines: &[&str]) -> ValidationResult;

    /// Search for a shifted anchor within a bounded window around the
    /// original line number.
    ///
    /// Returns [`ShiftResult::Found`] if exactly one nearby line validates
    /// under this scheme, [`ShiftResult::Ambiguous`] if multiple candidates
    /// match, and [`ShiftResult::NotFound`] if none match.
    fn find_shifted(
        &self,
        anchor: &ParsedAnchor,
        lines: &[&str],
        search_radius: usize,
    ) -> ShiftResult;
}

/// A rendered anchor for a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// 1-based line number.
    pub line: usize,
    /// Encoded local line hash (e.g. `"abc"`).
    pub local: String,
    /// Optional contextual fingerprint (e.g. `"rst"` for chunk/checkpoint).
    pub context: Option<String>,
}

impl Anchor {
    /// Render this anchor as a string suitable for output.
    ///
    /// Format: `"LINE:LOCAL"` or `"LINE:LOCAL:CONTEXT"`.
    pub fn render(&self) -> String {
        match &self.context {
            Some(ctx) => format!("{}:{}:{}", self.line, self.local, ctx),
            None => format!("{}:{}", self.line, self.local),
        }
    }
}

impl fmt::Display for Anchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// A parsed anchor extracted from model input.
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

/// Default search radius for shifted-anchor recovery (±15 lines).
pub const DEFAULT_SEARCH_RADIUS: usize = 15;

/// Content-only line hash scheme.
///
/// Anchor format: `LINE:LOCAL` (e.g. `22:abc`).
/// Validates only the normalized content of the specified line. Edits above
/// the line do not invalidate its anchor. Weakest freshness semantics.
#[derive(Debug, Clone)]
pub struct ContentOnly {
    hash_len: usize,
}

impl ContentOnly {
    /// Create with default hash length (3 letters).
    pub fn new() -> Self {
        Self {
            hash_len: DEFAULT_HASH_LEN,
        }
    }

    /// Create with a custom hash length.
    ///
    /// # Panics
    ///
    /// Panics if `hash_len` is not in `1..=4`.
    pub fn with_hash_len(hash_len: usize) -> Self {
        assert!(
            hash_len > 0 && hash_len <= 4,
            "hash_len must be 1..=4, got {hash_len}"
        );
        Self { hash_len }
    }
}

impl Default for ContentOnly {
    fn default() -> Self {
        Self::new()
    }
}

impl AnchorScheme for ContentOnly {
    fn name(&self) -> &str {
        "content_only_v1"
    }

    fn hash_len(&self) -> usize {
        self.hash_len
    }

    fn generate_anchors(&self, lines: &[&str]) -> Vec<Anchor> {
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let h = hash::line_hash(line);
                Anchor {
                    line: i + 1,
                    local: hash::encode_hash(h, self.hash_len),
                    context: None,
                }
            })
            .collect()
    }

    fn validate(&self, anchor: &ParsedAnchor, lines: &[&str]) -> ValidationResult {
        let idx = anchor.line.checked_sub(1).unwrap_or(usize::MAX);
        if idx >= lines.len() {
            return ValidationResult::OutOfRange;
        }

        let expected_local = hash::encode_hash(hash::line_hash(lines[idx]), self.hash_len);
        if anchor.local == expected_local {
            ValidationResult::Valid
        } else {
            ValidationResult::Stale
        }
    }

    fn find_shifted(
        &self,
        anchor: &ParsedAnchor,
        lines: &[&str],
        search_radius: usize,
    ) -> ShiftResult {
        find_shifted_generic(self, anchor, lines, search_radius)
    }
}

/// Default chunk size for [`ChunkFingerprint`] when constructed directly.
pub const DEFAULT_CHUNK_SIZE: usize = 16;

/// Chunk-fingerprinted line anchor scheme.
///
/// Anchor format: `LINE:LOCAL:CHUNK` (e.g. `22:abc:rst`).
/// `LOCAL` is the normalized line hash. `CHUNK` is a fingerprint of the
/// fixed-size chunk containing this line. Edits invalidate anchors only
/// within the affected chunk.
#[derive(Debug, Clone)]
pub struct ChunkFingerprint {
    hash_len: usize,
    chunk_size: usize,
}

impl ChunkFingerprint {
    /// Create with default parameters (3-letter hash, 16-line chunks).
    pub fn new() -> Self {
        Self {
            hash_len: DEFAULT_HASH_LEN,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Create with custom parameters.
    ///
    /// # Panics
    ///
    /// Panics if `hash_len` is not in `1..=4` or `chunk_size` is 0.
    pub fn with_params(hash_len: usize, chunk_size: usize) -> Self {
        assert!(
            hash_len > 0 && hash_len <= 4,
            "hash_len must be 1..=4, got {hash_len}"
        );
        assert!(chunk_size > 0, "chunk_size must be > 0");
        Self {
            hash_len,
            chunk_size,
        }
    }

    /// Compute the chunk fingerprint for the chunk containing `line_idx` (0-based).
    fn chunk_fingerprint(&self, lines: &[&str], line_idx: usize) -> String {
        let chunk_start = (line_idx / self.chunk_size) * self.chunk_size;
        let chunk_end = (chunk_start + self.chunk_size).min(lines.len());

        // Hash all normalized lines in the chunk together.
        let mut combined: u32 = hash::fnv1a_32(b"chunk");
        for line in &lines[chunk_start..chunk_end] {
            let lh = hash::line_hash(line);
            combined ^= lh;
            combined = combined.wrapping_mul(16_777_619);
        }
        hash::encode_hash(combined, self.hash_len)
    }
}

impl Default for ChunkFingerprint {
    fn default() -> Self {
        Self::new()
    }
}

impl AnchorScheme for ChunkFingerprint {
    fn name(&self) -> &str {
        "chunk_v1"
    }

    fn hash_len(&self) -> usize {
        self.hash_len
    }

    fn generate_anchors(&self, lines: &[&str]) -> Vec<Anchor> {
        // Pre-compute chunk fingerprints to avoid redundant work.
        let num_chunks = lines.len().div_ceil(self.chunk_size);
        let mut chunk_fps: Vec<String> = Vec::with_capacity(num_chunks);
        for chunk_idx in 0..num_chunks {
            let start = chunk_idx * self.chunk_size;
            let end = (start + self.chunk_size).min(lines.len());

            let mut combined: u32 = hash::fnv1a_32(b"chunk");
            for line in &lines[start..end] {
                let lh = hash::line_hash(line);
                combined ^= lh;
                combined = combined.wrapping_mul(16_777_619);
            }
            chunk_fps.push(hash::encode_hash(combined, self.hash_len));
        }

        lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let h = hash::line_hash(line);
                let chunk_idx = i / self.chunk_size;
                Anchor {
                    line: i + 1,
                    local: hash::encode_hash(h, self.hash_len),
                    context: Some(chunk_fps[chunk_idx].clone()),
                }
            })
            .collect()
    }

    fn validate(&self, anchor: &ParsedAnchor, lines: &[&str]) -> ValidationResult {
        let idx = anchor.line.checked_sub(1).unwrap_or(usize::MAX);
        if idx >= lines.len() {
            return ValidationResult::OutOfRange;
        }

        // Validate local line hash.
        let expected_local = hash::encode_hash(hash::line_hash(lines[idx]), self.hash_len);
        if anchor.local != expected_local {
            return ValidationResult::Stale;
        }

        // Chunk-fingerprinted scheme requires context — reject truncated anchors
        // that omit the chunk fingerprint, as they would silently weaken
        // validation to content-only semantics.
        let Some(ref expected_ctx) = anchor.context else {
            return ValidationResult::Stale;
        };
        let actual_ctx = self.chunk_fingerprint(lines, idx);
        if *expected_ctx != actual_ctx {
            return ValidationResult::Stale;
        }

        ValidationResult::Valid
    }

    fn find_shifted(
        &self,
        anchor: &ParsedAnchor,
        lines: &[&str],
        search_radius: usize,
    ) -> ShiftResult {
        find_shifted_generic(self, anchor, lines, search_radius)
    }
}

/// Default checkpoint interval for [`CheckpointChain`].
pub const DEFAULT_CHECKPOINT_INTERVAL: usize = 32;

/// Checkpoint-chained line anchor scheme.
///
/// Anchor format: `LINE:LOCAL:CKPT` (e.g. `22:abc:rst`).
/// `LOCAL` is the normalized line hash. `CKPT` is a fingerprint derived from
/// chaining all line hashes from the nearest preceding checkpoint to this
/// line. Strongest freshness detection but more anchor churn after edits.
#[derive(Debug, Clone)]
pub struct CheckpointChain {
    hash_len: usize,
    checkpoint_interval: usize,
}

impl CheckpointChain {
    /// Create with default parameters (3-letter hash, 32-line checkpoints).
    pub fn new() -> Self {
        Self {
            hash_len: DEFAULT_HASH_LEN,
            checkpoint_interval: DEFAULT_CHECKPOINT_INTERVAL,
        }
    }

    /// Create with custom parameters.
    ///
    /// # Panics
    ///
    /// Panics if `hash_len` is not in `1..=4` or `checkpoint_interval` is 0.
    pub fn with_params(hash_len: usize, checkpoint_interval: usize) -> Self {
        assert!(
            hash_len > 0 && hash_len <= 4,
            "hash_len must be 1..=4, got {hash_len}"
        );
        assert!(checkpoint_interval > 0, "checkpoint_interval must be > 0");
        Self {
            hash_len,
            checkpoint_interval,
        }
    }

    /// Compute the checkpoint-chained fingerprint for `line_idx` (0-based).
    ///
    /// Chains line hashes from the nearest checkpoint boundary up to and
    /// including `line_idx`.
    fn checkpoint_fingerprint(&self, lines: &[&str], line_idx: usize) -> String {
        let checkpoint_start = (line_idx / self.checkpoint_interval) * self.checkpoint_interval;

        let mut chain: u32 = hash::fnv1a_32(b"ckpt");
        for line in &lines[checkpoint_start..=line_idx] {
            let lh = hash::line_hash(line);
            chain ^= lh;
            chain = chain.wrapping_mul(16_777_619);
        }
        hash::encode_hash(chain, self.hash_len)
    }
}

impl Default for CheckpointChain {
    fn default() -> Self {
        Self::new()
    }
}

impl AnchorScheme for CheckpointChain {
    fn name(&self) -> &str {
        "checkpoint_v1"
    }

    fn hash_len(&self) -> usize {
        self.hash_len
    }

    fn generate_anchors(&self, lines: &[&str]) -> Vec<Anchor> {
        let mut anchors = Vec::with_capacity(lines.len());
        let mut chain: u32 = hash::fnv1a_32(b"ckpt");

        for (i, line) in lines.iter().enumerate() {
            // Reset chain at checkpoint boundaries.
            if i % self.checkpoint_interval == 0 {
                chain = hash::fnv1a_32(b"ckpt");
            }

            let lh = hash::line_hash(line);
            chain ^= lh;
            chain = chain.wrapping_mul(16_777_619);

            anchors.push(Anchor {
                line: i + 1,
                local: hash::encode_hash(lh, self.hash_len),
                context: Some(hash::encode_hash(chain, self.hash_len)),
            });
        }

        anchors
    }

    fn validate(&self, anchor: &ParsedAnchor, lines: &[&str]) -> ValidationResult {
        let idx = anchor.line.checked_sub(1).unwrap_or(usize::MAX);
        if idx >= lines.len() {
            return ValidationResult::OutOfRange;
        }

        // Validate local line hash.
        let expected_local = hash::encode_hash(hash::line_hash(lines[idx]), self.hash_len);
        if anchor.local != expected_local {
            return ValidationResult::Stale;
        }

        // Checkpoint-chained scheme requires context — reject truncated anchors
        // that omit the checkpoint fingerprint, as they would silently weaken
        // validation to content-only semantics.
        let Some(ref expected_ctx) = anchor.context else {
            return ValidationResult::Stale;
        };
        let actual_ctx = self.checkpoint_fingerprint(lines, idx);
        if *expected_ctx != actual_ctx {
            return ValidationResult::Stale;
        }

        ValidationResult::Valid
    }

    fn find_shifted(
        &self,
        anchor: &ParsedAnchor,
        lines: &[&str],
        search_radius: usize,
    ) -> ShiftResult {
        find_shifted_generic(self, anchor, lines, search_radius)
    }
}

/// Generic shifted-anchor recovery used by all scheme implementations.
///
/// Searches `±search_radius` lines around the anchor's original position for
/// a line whose local hash matches. For schemes with contextual components,
/// the contextual fingerprint is recomputed at each candidate position and
/// also compared.
///
/// This function avoids per-candidate allocations: it computes the local hash
/// inline (no [`ParsedAnchor`] cloning) and only evaluates the contextual
/// fingerprint when the cheap local-hash check passes.
fn find_shifted_generic(
    scheme: &dyn AnchorScheme,
    anchor: &ParsedAnchor,
    lines: &[&str],
    search_radius: usize,
) -> ShiftResult {
    let orig_idx = anchor.line.saturating_sub(1);
    let start = orig_idx.saturating_sub(search_radius);
    let end = (orig_idx + search_radius + 1).min(lines.len());
    let hash_len = scheme.hash_len();

    let mut candidates: Vec<usize> = Vec::new();

    for idx in start..end {
        // Skip the original line — it already failed validation.
        if idx == orig_idx {
            continue;
        }

        // Cheap check: does the local line hash match?
        let local = hash::encode_hash(hash::line_hash(lines[idx]), hash_len);
        if local != anchor.local {
            continue;
        }

        // If the anchor carries context, validate via the full scheme
        // (which recomputes the contextual fingerprint at this position).
        // For context-free anchors (ContentOnly) this is skipped entirely.
        if anchor.context.is_some() {
            let probe = ParsedAnchor {
                line: idx + 1,
                local,
                context: anchor.context.clone(),
            };
            if scheme.validate(&probe, lines) != ValidationResult::Valid {
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lines() -> Vec<&'static str> {
        vec![
            "import React from 'react';",
            "",
            "export function App() {",
            "  return <div>Hello</div>;",
            "}",
        ]
    }

    fn all_schemes() -> Vec<Box<dyn AnchorScheme>> {
        vec![
            Box::new(ContentOnly::new()),
            Box::new(ChunkFingerprint::new()),
            Box::new(CheckpointChain::new()),
        ]
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
            local: "abc".to_owned(),
            context: None,
        };
        assert_eq!(a.render(), "5:abc");
        assert_eq!(a.to_string(), "5:abc");
    }

    #[test]
    fn anchor_render_with_context() {
        let a = Anchor {
            line: 22,
            local: "abc".to_owned(),
            context: Some("rst".to_owned()),
        };
        assert_eq!(a.render(), "22:abc:rst");
    }

    #[test]
    fn generate_anchors_one_per_line() {
        let lines = sample_lines();
        for scheme in all_schemes() {
            let anchors = scheme.generate_anchors(&lines);
            assert_eq!(anchors.len(), lines.len(), "scheme {}", scheme.name());
            for (i, anchor) in anchors.iter().enumerate() {
                assert_eq!(anchor.line, i + 1);
                assert_eq!(anchor.local.len(), scheme.hash_len());
            }
        }
    }

    #[test]
    fn content_only_has_no_context() {
        let anchors = ContentOnly::new().generate_anchors(&sample_lines());
        assert!(anchors.iter().all(|a| a.context.is_none()));
    }

    #[test]
    fn chunk_and_checkpoint_have_context() {
        let lines = sample_lines();
        for scheme in [
            Box::new(ChunkFingerprint::new()) as Box<dyn AnchorScheme>,
            Box::new(CheckpointChain::new()),
        ] {
            let anchors = scheme.generate_anchors(&lines);
            assert!(
                anchors.iter().all(|a| a.context.is_some()),
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn validate_generated_anchors_valid() {
        let lines = sample_lines();
        for scheme in all_schemes() {
            for anchor in scheme.generate_anchors(&lines) {
                let parsed = ParsedAnchor {
                    line: anchor.line,
                    local: anchor.local,
                    context: anchor.context,
                };
                assert_eq!(
                    scheme.validate(&parsed, &lines),
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
        let lines = sample_lines();
        for scheme in all_schemes() {
            let parsed = ParsedAnchor {
                line: 999,
                local: "abc".to_owned(),
                context: None,
            };
            assert_eq!(
                scheme.validate(&parsed, &lines),
                ValidationResult::OutOfRange,
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn validate_stale_on_content_change() {
        let original = sample_lines();
        for scheme in all_schemes() {
            let anchors = scheme.generate_anchors(&original);
            let mut modified = original.clone();
            modified[2] = "export function Renamed() {";
            let parsed = ParsedAnchor {
                line: anchors[2].line,
                local: anchors[2].local.clone(),
                context: anchors[2].context.clone(),
            };
            assert_eq!(
                scheme.validate(&parsed, &modified),
                ValidationResult::Stale,
                "scheme {}",
                scheme.name()
            );
        }
    }

    #[test]
    fn chunk_rejects_context_free_anchor() {
        let lines = sample_lines();
        let scheme = ChunkFingerprint::new();
        let anchors = scheme.generate_anchors(&lines);
        let parsed = ParsedAnchor {
            line: anchors[0].line,
            local: anchors[0].local.clone(),
            context: None, // truncated anchor
        };
        assert_eq!(scheme.validate(&parsed, &lines), ValidationResult::Stale);
    }

    #[test]
    fn checkpoint_rejects_context_free_anchor() {
        let lines = sample_lines();
        let scheme = CheckpointChain::new();
        let anchors = scheme.generate_anchors(&lines);
        let parsed = ParsedAnchor {
            line: anchors[0].line,
            local: anchors[0].local.clone(),
            context: None,
        };
        assert_eq!(scheme.validate(&parsed, &lines), ValidationResult::Stale);
    }

    #[test]
    fn chunk_edit_within_chunk_invalidates_neighbors() {
        // With chunk_size=4, editing line 1 changes the fingerprint of all
        // lines in chunk 0 (lines 1-4) but not chunk 1 (lines 5+).
        let lines: Vec<&str> = vec!["a1", "b2", "c3", "d4", "e5", "f6", "g7", "h8"];
        let scheme = ChunkFingerprint::with_params(3, 4);
        let anchors = scheme.generate_anchors(&lines);

        let modified: Vec<&str> = vec!["CHANGED", "b2", "c3", "d4", "e5", "f6", "g7", "h8"];

        // Line 2 is in the edited chunk → stale.
        let in_chunk = ParsedAnchor {
            line: anchors[1].line,
            local: anchors[1].local.clone(),
            context: anchors[1].context.clone(),
        };
        assert_eq!(
            scheme.validate(&in_chunk, &modified),
            ValidationResult::Stale
        );

        // Line 6 is in the other chunk → still valid.
        let other_chunk = ParsedAnchor {
            line: anchors[5].line,
            local: anchors[5].local.clone(),
            context: anchors[5].context.clone(),
        };
        assert_eq!(
            scheme.validate(&other_chunk, &modified),
            ValidationResult::Valid
        );
    }

    #[test]
    fn find_shifted_found_after_insertion() {
        let original = vec!["alpha one", "beta two", "gamma three"];
        for scheme in all_schemes() {
            let anchors = scheme.generate_anchors(&original);
            // Insert a line at the top → "beta two" shifts from line 2 to 3.
            let modified = vec!["inserted", "alpha one", "beta two", "gamma three"];
            let parsed = ParsedAnchor {
                line: anchors[1].line,
                local: anchors[1].local.clone(),
                context: anchors[1].context.clone(),
            };
            match scheme.find_shifted(&parsed, &modified, DEFAULT_SEARCH_RADIUS) {
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
        let scheme = ContentOnly::new();
        let original = vec!["unique", "dup", "x"];
        let anchors = scheme.generate_anchors(&original);
        // Two copies of "dup" near the original location, neither at the
        // original line index.
        let modified = vec!["dup", "changed", "dup", "x"];
        let parsed = ParsedAnchor {
            line: anchors[1].line,
            local: anchors[1].local.clone(),
            context: None,
        };
        match scheme.find_shifted(&parsed, &modified, DEFAULT_SEARCH_RADIUS) {
            ShiftResult::Ambiguous { candidates } => assert_eq!(candidates, vec![1, 3]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn find_shifted_not_found() {
        let scheme = ContentOnly::new();
        let original = vec!["only line"];
        let anchors = scheme.generate_anchors(&original);
        let modified = vec!["completely different"];
        let parsed = ParsedAnchor {
            line: anchors[0].line,
            local: anchors[0].local.clone(),
            context: None,
        };
        assert_eq!(
            scheme.find_shifted(&parsed, &modified, DEFAULT_SEARCH_RADIUS),
            ShiftResult::NotFound
        );
    }

    #[test]
    fn find_shifted_respects_radius() {
        let scheme = ContentOnly::new();
        let target = "needle line";
        let mut modified: Vec<&str> = vec!["filler"; 40];
        modified.push(target);

        let anchors = scheme.generate_anchors(&[target]);
        let parsed = ParsedAnchor {
            line: 1,
            local: anchors[0].local.clone(),
            context: None,
        };
        // Target is at line 41, far outside radius 5 from line 1.
        assert_eq!(
            scheme.find_shifted(&parsed, &modified, 5),
            ShiftResult::NotFound
        );
        // Wide radius finds it.
        assert_eq!(
            scheme.find_shifted(&parsed, &modified, 50),
            ShiftResult::Found { new_line: 41 }
        );
    }

    #[test]
    fn anchors_stable_across_reindent() {
        // Whitespace-normalized hashing: re-indenting a line keeps its local
        // hash stable under every scheme.
        for scheme in all_schemes() {
            let original = vec!["fn main() {", "    let x = 1;", "}"];
            let reindented = vec!["fn main() {", "        let x = 1;", "}"];
            let a = scheme.generate_anchors(&original);
            let b = scheme.generate_anchors(&reindented);
            assert_eq!(a[1].local, b[1].local, "scheme {}", scheme.name());
        }
    }
}
