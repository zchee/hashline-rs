//! Shared hashing utilities for hashline anchor generation.
//!
//! Provides FNV-1a 32-bit hashing, whitespace-normalized line fingerprinting,
//! and the compact stack-allocated [`EncodedHash`] anchor letter encoding.
//!
//! ## Normalization policy
//!
//! Before hashing, lines are normalized: leading/trailing whitespace is trimmed
//! and internal whitespace runs are collapsed to a single ASCII space. This keeps
//! anchors stable across formatter-only edits (indentation, trailing whitespace,
//! tab/space normalization) while still distinguishing meaningful content changes
//! (e.g. `return x` vs `returnx`).

use std::fmt;

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

/// Compute a whitespace-normalized FNV-1a 32-bit fingerprint of a single line.
///
/// Normalization: `trim()` + collapse internal whitespace runs to a single
/// ASCII space. The hash is computed over the normalized byte sequence.
///
/// Returns the raw `u32` hash. Use [`encode_hash`] to convert to a compact
/// letter-based anchor encoding.
pub fn line_hash(line: &str) -> u32 {
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
