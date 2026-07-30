//! Shared hashing utilities for hashline anchor generation.
//!
//! Provides the whitespace-normalized line fingerprint ([`line_hash`] and the
//! scratch-reusing [`LineHasher`]), FNV-1a 32-bit hashing for fingerprint
//! seeds and folds, and the compact stack-allocated [`EncodedHash`] anchor
//! letter encoding.
//!
//! ## Normalization policy
//!
//! Before hashing, lines are normalized: leading/trailing whitespace is trimmed
//! and internal whitespace runs are collapsed to a single ASCII space. This keeps
//! anchors stable across formatter-only edits (indentation, trailing whitespace,
//! tab/space normalization) while still distinguishing meaningful content changes
//! (e.g. `return x` vs `returnx`).
//!
//! The normalized byte sequence is the contract. Two normalizers produce it —
//! a byte-at-a-time loop that is exact for every input, and a `memchr3` segment
//! scan that copies whole non-whitespace runs — and they must agree byte for
//! byte on any input the segment scan is used for. See [`LineHasher`].
//!
//! ## Which hash
//!
//! Where the target has AES intrinsics (`aarch64` and `x86_64` built with
//! `target-feature=+aes`, which `.cargo/config.toml` enables for the targets
//! that do not have it by default), lines are hashed with `gxhash32` over the
//! normalized bytes. Everywhere else the portable fused FNV-1a pass is used.
//! Both paths consume identical normalized bytes; only the hash function
//! differs, so anchor letters differ between the two kinds of build. That is
//! harmless — anchors are per-session ephemeral and never persisted or
//! exchanged between machines — but it does mean a build flag change
//! invalidates anchors a running session already handed out.

use std::fmt;

use memchr::{memchr2, memchr3_iter};

/// FNV-1a 32-bit offset basis.
const FNV_OFFSET: u32 = 2_166_136_261;

/// FNV-1a 32-bit prime.
const FNV_PRIME: u32 = 16_777_619;

/// Compute FNV-1a 32-bit hash of raw bytes.
///
/// This is the low-level primitive — callers that want whitespace-normalized
/// fingerprints should use [`line_hash`] instead. It is a `const fn` so
/// fingerprint seeds can be folded at compile time.
pub const fn fnv1a_32(data: &[u8]) -> u32 {
    let mut h: u32 = FNV_OFFSET;
    let mut i = 0;
    while i < data.len() {
        h ^= data[i] as u32;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h
}

/// Whether this build hashes normalized lines with `gxhash32`.
///
/// `false` selects the portable fused FNV-1a pass. Exposed so tests and
/// diagnostics can report which path a binary was built with.
pub const BLOCK_HASH: bool = cfg!(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    target_feature = "aes"
));

/// Seed for the block line hash: the ASCII bytes of `hashlin`.
///
/// Fixed rather than random — anchors must be reproducible for a given file
/// within and across processes of the same build.
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    target_feature = "aes"
))]
const LINE_HASH_SEED: i64 = 0x0068_6173_686C_696E;

/// ASCII form feed, which `u8::is_ascii_whitespace` matches.
const FORM_FEED: u8 = 0x0C;

/// Append the normalized bytes of `line` to `scratch`, one byte at a time.
///
/// This loop *is* the normalization contract: `str::trim`, then every run of
/// ASCII whitespace collapsed to a single space. It is exact for every input
/// and is what [`normalize_segments`] is checked against.
fn normalize_branchy(line: &str, scratch: &mut Vec<u8>) {
    let mut prev_ws = false;
    for byte in line.trim().bytes() {
        if byte.is_ascii_whitespace() {
            if !prev_ws {
                scratch.push(b' ');
                prev_ws = true;
            }
        } else {
            scratch.push(byte);
            prev_ws = false;
        }
    }
}

/// Append the normalized bytes of `line` to `scratch` using a SIMD segment
/// scan: whole non-whitespace runs are copied, with one space written between
/// them.
///
/// # Correctness
///
/// `u8::is_ascii_whitespace` matches five bytes and `memchr3` searches three,
/// so this is equivalent to [`normalize_branchy`] only for lines containing
/// neither `\n` nor [`FORM_FEED`]. Establishing that is the caller's job, and
/// [`LineHasher`] is the only caller — it proves it once per buffer, because a
/// `memchr` per line costs more than the block hash saves.
fn normalize_segments(line: &str, scratch: &mut Vec<u8>) {
    let trimmed = line.trim().as_bytes();
    let mut start = 0usize;
    let mut wrote = false;
    for pos in memchr3_iter(b' ', b'\t', b'\r', trimmed) {
        if pos > start {
            if wrote {
                scratch.push(b' ');
            }
            scratch.extend_from_slice(&trimmed[start..pos]);
            wrote = true;
        }
        start = pos + 1;
    }
    if start < trimmed.len() {
        if wrote {
            scratch.push(b' ');
        }
        scratch.extend_from_slice(&trimmed[start..]);
    }
}

/// Whether every line of `lines` can be normalized by the segment scan.
pub(crate) fn lines_segment_scannable(lines: &[&str]) -> bool {
    lines
        .iter()
        .all(|line| memchr2(b'\n', FORM_FEED, line.as_bytes()).is_none())
}

/// Whether lines split from `content` on `\n` can be normalized by the segment
/// scan.
///
/// Only form feed is searched for: the caller splits on `\n`, so no resulting
/// line can contain one. This is the whole reason the fast path survives —
/// one pass over the buffer instead of one `memchr` per line.
///
/// # Preconditions
///
/// `content` must be the buffer whose lines are split on `\n`, exactly as
/// [`crate::index::split_lines`] does it.
pub(crate) fn content_segment_scannable(content: &str) -> bool {
    memchr::memchr(FORM_FEED, content.as_bytes()).is_none()
}

/// Compute a whitespace-normalized fingerprint of a single line, reusing one
/// scratch buffer across calls.
///
/// Built once per indexed buffer, so normalizing a file's lines allocates once
/// rather than once per line. Which hash it applies to the normalized bytes is
/// a build-time choice — see the module docs and [`BLOCK_HASH`].
///
/// [`Self::new`] assumes nothing about its input and is exact for any line.
/// The crate-internal constructors narrow to the faster normalizer only after
/// proving the buffer permits it.
#[derive(Debug, Clone, Default)]
pub struct LineHasher {
    /// Reusable normalized-bytes buffer. Unused on the fused FNV path, where
    /// normalization and hashing are the same pass.
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_feature = "aes"
    ))]
    scratch: Vec<u8>,
    /// Whether [`normalize_segments`] is known to be exact for this buffer.
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_feature = "aes"
    ))]
    segment_scan: bool,
}

impl LineHasher {
    /// A hasher that makes no assumptions about the lines it will be given.
    ///
    /// Exact for every input, including lines containing `\n` or form feed.
    pub fn new() -> Self {
        Self::default()
    }

    /// A hasher for lines split from `content` on `\n`.
    ///
    /// See [`content_segment_scannable`] for the precondition.
    pub(crate) fn for_content(content: &str) -> Self {
        Self::with_segment_scan(content_segment_scannable(content))
    }

    /// A hasher for an arbitrary set of already-split lines.
    pub(crate) fn for_lines(lines: &[&str]) -> Self {
        Self::with_segment_scan(lines_segment_scannable(lines))
    }

    /// A hasher whose normalizer is chosen by the caller.
    ///
    /// `segment_scan` may only be `true` when the caller has established that
    /// every line it will hash is free of `\n` and form feed; the two
    /// constructors above are the supported ways to establish it.
    #[cfg_attr(
        not(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            target_feature = "aes"
        )),
        expect(
            unused_variables,
            reason = "the fused FNV path normalizes and hashes in one pass"
        )
    )]
    pub(crate) fn with_segment_scan(segment_scan: bool) -> Self {
        #[cfg(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            target_feature = "aes"
        ))]
        return Self {
            scratch: Vec::new(),
            segment_scan,
        };
        #[cfg(not(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            target_feature = "aes"
        )))]
        return Self {};
    }

    /// Hash one line's normalized bytes.
    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_feature = "aes"
    ))]
    pub fn hash(&mut self, line: &str) -> u32 {
        self.scratch.clear();
        if self.segment_scan {
            normalize_segments(line, &mut self.scratch);
        } else {
            normalize_branchy(line, &mut self.scratch);
        }
        gxhash::gxhash32(&self.scratch, LINE_HASH_SEED)
    }

    /// Hash one line's normalized bytes.
    #[cfg(not(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_feature = "aes"
    )))]
    pub fn hash(&mut self, line: &str) -> u32 {
        fused_fnv(line)
    }
}

/// Fused normalize-and-hash single pass: FNV-1a over the normalized bytes,
/// without ever materializing them.
///
/// The portable path, and the reference the segment-scan normalization is
/// tested against. A release build that took the AES path has no other use for
/// it, so it is compiled only where it is one of those two things.
#[cfg(any(
    not(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        target_feature = "aes"
    )),
    test
))]
fn fused_fnv(line: &str) -> u32 {
    let mut h: u32 = FNV_OFFSET;
    let mut prev_ws = false;

    for byte in line.trim().bytes() {
        if byte.is_ascii_whitespace() {
            if !prev_ws {
                h ^= u32::from(b' ');
                h = h.wrapping_mul(FNV_PRIME);
                prev_ws = true;
            }
        } else {
            h ^= u32::from(byte);
            h = h.wrapping_mul(FNV_PRIME);
            prev_ws = false;
        }
    }

    h
}

/// Compute a whitespace-normalized 32-bit fingerprint of a single line.
///
/// Normalization: `trim()` + collapse internal whitespace runs to a single
/// ASCII space. The hash is computed over the normalized byte sequence and is
/// exact for any input.
///
/// Hashing many lines should build one [`LineHasher`] instead: this function
/// keeps a thread-local scratch buffer so it does not allocate per call, but
/// the thread-local access is pure overhead a loop does not need to pay.
///
/// Returns the raw `u32` hash. Use [`encode_hash`] to convert to a compact
/// letter-based anchor encoding.
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    target_feature = "aes"
))]
pub fn line_hash(line: &str) -> u32 {
    use std::cell::RefCell;

    thread_local! {
        /// Conservative by construction, so this is exact for any caller.
        static HASHER: RefCell<LineHasher> = RefCell::new(LineHasher::new());
    }
    HASHER.with(|hasher| hasher.borrow_mut().hash(line))
}

/// Compute a whitespace-normalized 32-bit fingerprint of a single line.
///
/// See the AES-enabled counterpart for the full contract; on this target the
/// normalization and the FNV-1a hash are a single fused pass, so there is no
/// scratch buffer to reuse and no reason to prefer [`LineHasher`].
#[cfg(not(all(
    any(target_arch = "aarch64", target_arch = "x86_64"),
    target_feature = "aes"
)))]
pub fn line_hash(line: &str) -> u32 {
    fused_fnv(line)
}

/// Fold one already-computed line hash into a running fingerprint accumulator.
///
/// This is the integer combining step shared by the chunk and checkpoint
/// contextual fingerprints. Folding precomputed `u32` line hashes is what lets
/// contextual fingerprints be derived without ever re-hashing line text.
#[inline]
pub const fn fold_line_hash(acc: u32, line: u32) -> u32 {
    (acc ^ line).wrapping_mul(FNV_PRIME)
}

/// Maximum number of letters in an [`EncodedHash`].
pub const MAX_HASH_LEN: usize = 4;

/// Default anchor hash length (3 lowercase letters).
pub const DEFAULT_HASH_LEN: usize = 3;

/// A compact anchor hash encoding: 1–4 lowercase ASCII letters held inline.
///
/// `EncodedHash` is `Copy` and allocation-free, so anchors can be generated,
/// compared, and rendered without touching the heap. Equality is byte-level
/// and also defined against `str`/`String`/`[u8]` so parsed anchor text can be
/// compared directly against a freshly encoded hash.
#[derive(Debug, Clone, Copy)]
pub struct EncodedHash {
    /// Encoded letters; only the first `len` bytes are significant. Trailing
    /// bytes are always zero.
    bytes: [u8; MAX_HASH_LEN],
    /// Number of significant bytes in `bytes` (always `1..=4`).
    len: u8,
}

impl EncodedHash {
    /// Number of letters in this encoding.
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether this encoding holds no letters.
    ///
    /// Always `false` for values produced by [`encode_hash`], which rejects a
    /// zero length; the method exists so `len` has its conventional partner.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The significant letters as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The significant letters as a string slice.
    pub fn as_str(&self) -> &str {
        // Every byte is an ASCII lowercase letter by construction, so the
        // significant prefix is always valid UTF-8.
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// Append the letters to `out` without allocating or re-validating UTF-8.
    ///
    /// This is the rendering primitive used by the anchor hot path.
    pub fn push_to(&self, out: &mut String) {
        for &byte in self.as_bytes() {
            out.push(byte as char);
        }
    }
}

impl PartialEq for EncodedHash {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for EncodedHash {}

impl PartialEq<str> for EncodedHash {
    fn eq(&self, other: &str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<&str> for EncodedHash {
    fn eq(&self, other: &&str) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<String> for EncodedHash {
    fn eq(&self, other: &String) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<[u8]> for EncodedHash {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_bytes() == other
    }
}

impl PartialEq<&[u8]> for EncodedHash {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_bytes() == *other
    }
}

impl PartialEq<EncodedHash> for str {
    fn eq(&self, other: &EncodedHash) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<EncodedHash> for &str {
    fn eq(&self, other: &EncodedHash) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl PartialEq<EncodedHash> for String {
    fn eq(&self, other: &EncodedHash) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl fmt::Display for EncodedHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Encode a 32-bit hash as `len` lowercase ASCII letters (a–z).
///
/// Each letter is derived from a different byte region of the hash to spread
/// entropy: letter `i` is `(hash >> (i * 8)) % 26`.
///
/// # Panics
///
/// Panics if `len` is 0 or greater than [`MAX_HASH_LEN`].
pub fn encode_hash(hash: u32, len: usize) -> EncodedHash {
    assert!(
        len > 0 && len <= MAX_HASH_LEN,
        "encode_hash: len must be 1..=4"
    );

    let mut bytes = [0u8; MAX_HASH_LEN];
    for (i, slot) in bytes[..len].iter_mut().enumerate() {
        *slot = ((hash >> (i * 8)) % 26) as u8 + b'a';
    }
    EncodedHash {
        bytes,
        len: len as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-`EncodedHash` letter encoding, kept as a differential reference:
    /// the compact encoding must stay byte-identical to it.
    fn reference_encode_hash(hash: u32, len: usize) -> String {
        let mut result = String::with_capacity(len);
        for i in 0..len {
            let byte = ((hash >> (i * 8)) % 26) as u8 + b'a';
            result.push(byte as char);
        }
        result
    }

    #[test]
    fn fnv1a_32_empty() {
        // FNV-1a of empty input is the offset basis.
        assert_eq!(fnv1a_32(b""), FNV_OFFSET);
    }

    #[test]
    fn fnv1a_32_deterministic() {
        assert_eq!(fnv1a_32(b"hello world"), fnv1a_32(b"hello world"));
    }

    #[test]
    fn fnv1a_32_different_inputs_differ() {
        assert_ne!(fnv1a_32(b"hello"), fnv1a_32(b"world"));
    }

    #[test]
    fn fnv1a_32_usable_in_const_context() {
        const SEED: u32 = fnv1a_32(b"chunk");
        assert_eq!(SEED, fnv1a_32(b"chunk"));
    }

    #[test]
    fn fold_line_hash_matches_inline_fold() {
        let mut expected: u32 = 7;
        expected ^= 0xDEAD_BEEF;
        expected = expected.wrapping_mul(FNV_PRIME);
        assert_eq!(fold_line_hash(7, 0xDEAD_BEEF), expected);
    }

    /// Normalize with the exact byte-at-a-time reference.
    fn branchy(line: &str) -> Vec<u8> {
        let mut out = Vec::new();
        normalize_branchy(line, &mut out);
        out
    }

    /// Normalize with the SIMD segment scan.
    fn segments(line: &str) -> Vec<u8> {
        let mut out = Vec::new();
        normalize_segments(line, &mut out);
        out
    }

    /// Every string built from `alphabet` up to `len` bytes long.
    fn exhaustive_strings(alphabet: &[char], len: usize) -> Vec<String> {
        let mut out = vec![String::new()];
        let mut frontier = vec![String::new()];
        for _ in 0..len {
            let mut next = Vec::new();
            for prefix in &frontier {
                for &c in alphabet {
                    let mut s = prefix.clone();
                    s.push(c);
                    next.push(s);
                }
            }
            out.extend(next.iter().cloned());
            frontier = next;
        }
        out
    }

    /// The fused pass must hash exactly the bytes the reference normalizer
    /// produces — this is what lets the two build paths share one contract.
    #[test]
    fn fused_pass_hashes_the_reference_normalization() {
        let alphabet = ['a', ' ', '\t', '\r', '\n', '\x0C'];
        for input in exhaustive_strings(&alphabet, 4) {
            assert_eq!(
                fused_fnv(&input),
                fnv1a_32(&branchy(&input)),
                "input {input:?}"
            );
        }
    }

    /// The segment scan must agree with the reference on every input free of
    /// the two whitespace bytes it cannot see. Exhaustive over every string of
    /// up to four bytes drawn from the alphabet that matters.
    #[test]
    fn segment_scan_matches_reference_where_it_is_permitted() {
        let alphabet = ['a', 'b', ' ', '\t', '\r'];
        for input in exhaustive_strings(&alphabet, 4) {
            assert!(lines_segment_scannable(&[input.as_str()]), "{input:?}");
            assert_eq!(segments(&input), branchy(&input), "input {input:?}");
        }
    }

    /// Inputs containing `\n` or form feed must be refused by the guard, so the
    /// hasher never routes them to the segment scan.
    ///
    /// Several of them genuinely would normalize differently, which is the
    /// whole reason the guard exists — this pins that the guard is what stands
    /// between those inputs and a wrong hash.
    #[test]
    fn rare_whitespace_is_refused_by_the_segment_guard() {
        let alphabet = ['a', ' ', '\n', '\x0C'];
        let mut divergences = 0usize;
        for input in exhaustive_strings(&alphabet, 4) {
            let has_rare = input.contains('\n') || input.contains('\x0C');
            assert_eq!(
                lines_segment_scannable(&[input.as_str()]),
                !has_rare,
                "input {input:?}"
            );
            if has_rare && segments(&input) != branchy(&input) {
                divergences += 1;
            }
        }
        assert!(
            divergences > 0,
            "the guard must be load-bearing, but no refused input actually diverged"
        );
    }

    /// A hasher narrowed by the content scan must agree line for line with the
    /// conservative `line_hash`, including on content carrying form feeds and
    /// CRs — the case the whole-buffer guard exists to handle.
    #[test]
    fn content_hasher_matches_line_hash_including_rare_whitespace() {
        let corpora = [
            "let x = 1;\n\tlet  y = 2;\nplain\n",
            "clean\r\nlines\r\nonly\r\n",
            "before\nmid\x0Cdle\nafter\n",
            "\x0C\nleading form feed\n",
            "",
            "no trailing newline",
        ];
        for content in corpora {
            let mut hasher = LineHasher::for_content(content);
            for line in crate::index::split_lines(content) {
                assert_eq!(hasher.hash(line), line_hash(line), "content {content:?}");
            }
        }
    }

    /// The same, for a hasher narrowed by an explicit line list.
    #[test]
    fn lines_hasher_matches_line_hash_including_rare_whitespace() {
        let lines: Vec<&str> = vec![
            "let x = 1;",
            "  spaced   out  ",
            "carriage\rreturn",
            "form\x0Cfeed",
            "embedded\nnewline",
            "",
        ];
        // The rare bytes are present, so the fast normalizer must be refused.
        assert!(!lines_segment_scannable(&lines));
        let mut hasher = LineHasher::for_lines(&lines);
        for line in &lines {
            assert_eq!(hasher.hash(line), line_hash(line), "line {line:?}");
        }

        // Without them, it must be taken — and still agree.
        let clean: Vec<&str> = vec!["let x = 1;", "  spaced   out  ", "tab\tsep"];
        assert!(lines_segment_scannable(&clean));
        let mut hasher = LineHasher::for_lines(&clean);
        for line in &clean {
            assert_eq!(hasher.hash(line), line_hash(line), "line {line:?}");
        }
    }

    /// Reusing one hasher must not let a previous line leak into the next.
    #[test]
    fn hasher_scratch_reuse_does_not_leak_between_lines() {
        let lines = ["a very long line to grow the scratch buffer", "b", ""];
        let mut hasher = LineHasher::for_lines(&lines);
        let reused: Vec<u32> = lines.iter().map(|line| hasher.hash(line)).collect();
        let fresh: Vec<u32> = lines
            .iter()
            .map(|line| LineHasher::new().hash(line))
            .collect();
        assert_eq!(reused, fresh);
    }

    #[test]
    fn line_hash_deterministic() {
        assert_eq!(line_hash("  let x = 1;  "), line_hash("  let x = 1;  "));
    }

    #[test]
    fn line_hash_whitespace_normalization_indentation() {
        // Different indentation → same hash.
        let a = line_hash("    let x = 1;");
        let b = line_hash("  let x = 1;");
        let c = line_hash("\tlet x = 1;");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn line_hash_internal_whitespace_collapsed() {
        assert_eq!(line_hash("let  x =  1;"), line_hash("let x = 1;"));
    }

    #[test]
    fn line_hash_distinguishes_content() {
        assert_ne!(line_hash("return x"), line_hash("returnx"));
        assert_ne!(line_hash("let x = 1;"), line_hash("let x = 2;"));
    }

    #[test]
    fn line_hash_empty_and_whitespace_only_equal() {
        assert_eq!(line_hash(""), line_hash("   "));
        assert_eq!(line_hash(""), line_hash("\t\t"));
    }

    #[test]
    fn encode_hash_length_and_charset() {
        for len in 1..=MAX_HASH_LEN {
            let encoded = encode_hash(0xDEAD_BEEF, len);
            assert_eq!(encoded.len(), len);
            assert!(!encoded.is_empty());
            assert_eq!(encoded.as_str().len(), len);
            assert!(encoded.as_bytes().iter().all(u8::is_ascii_lowercase));
        }
    }

    #[test]
    fn encode_hash_deterministic() {
        assert_eq!(encode_hash(42, 3), encode_hash(42, 3));
    }

    #[test]
    fn encode_hash_matches_reference_letters() {
        // Byte-identical letter mapping across the whole hash space (sampled)
        // and every supported length.
        let mut h: u32 = 0x1234_5678;
        for _ in 0..2_000 {
            for len in 1..=MAX_HASH_LEN {
                assert_eq!(
                    encode_hash(h, len).as_str(),
                    reference_encode_hash(h, len),
                    "hash {h:#x} len {len}"
                );
            }
            h = h.wrapping_mul(2_654_435_761).wrapping_add(12_345);
        }
    }

    #[test]
    fn encoded_hash_compares_against_text() {
        let encoded = encode_hash(0xDEAD_BEEF, 3);
        let text = reference_encode_hash(0xDEAD_BEEF, 3);
        assert_eq!(encoded, text.as_str());
        assert_eq!(encoded, text);
        assert_eq!(encoded, *text.as_str());
        assert_eq!(text.as_str(), encoded);
        assert_eq!(text, encoded);
        assert_eq!(encoded, text.as_bytes());
        assert_ne!(encoded, "zzz");
        // Different lengths never compare equal even on a shared prefix.
        assert_ne!(encode_hash(0xDEAD_BEEF, 2), encode_hash(0xDEAD_BEEF, 3));
    }

    #[test]
    fn encoded_hash_push_to_matches_display() {
        let encoded = encode_hash(0xFEED_FACE, 4);
        let mut out = String::from("prefix ");
        encoded.push_to(&mut out);
        assert_eq!(out, format!("prefix {encoded}"));
    }

    #[test]
    fn encoded_hash_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<EncodedHash>();
    }

    #[test]
    #[should_panic(expected = "len must be 1..=4")]
    fn encode_hash_rejects_zero_len() {
        let _ = encode_hash(42, 0);
    }

    #[test]
    #[should_panic(expected = "len must be 1..=4")]
    fn encode_hash_rejects_len_five() {
        let _ = encode_hash(42, 5);
    }
}
