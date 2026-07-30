//! Shared hashing utilities for hashline anchor generation.
//!
//! Provides FNV-1a 32-bit hashing and whitespace-normalized line fingerprinting.
//!
//! ## Normalization policy
//!
//! Before hashing, lines are normalized: leading/trailing whitespace is trimmed
//! and internal whitespace runs are collapsed to a single ASCII space. This keeps
//! anchors stable across formatter-only edits (indentation, trailing whitespace,
//! tab/space normalization) while still distinguishing meaningful content changes
//! (e.g. `return x` vs `returnx`).

/// FNV-1a 32-bit offset basis.
const FNV_OFFSET: u32 = 2_166_136_261;

/// FNV-1a 32-bit prime.
const FNV_PRIME: u32 = 16_777_619;

/// Compute FNV-1a 32-bit hash of raw bytes.
///
/// This is the low-level primitive — callers that want whitespace-normalized
/// fingerprints should use [`line_hash`] instead.
pub fn fnv1a_32(data: &[u8]) -> u32 {
    let mut h: u32 = FNV_OFFSET;
    for &byte in data {
        h ^= byte as u32;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Compute a whitespace-normalized FNV-1a 32-bit fingerprint of a single line.
///
/// Normalization: `trim()` + collapse internal whitespace runs to a single
/// ASCII space. The hash is computed over the normalized byte sequence.
///
/// Returns the raw `u32` hash. Use [`encode_hash`] to convert to a compact
/// letter-based anchor string.
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

/// Encode a 32-bit hash as `n` lowercase ASCII letters (a–z).
///
/// Each letter is derived from a different byte region of the hash to spread
/// entropy.
///
/// # Panics
///
/// Panics if `len` is 0 or greater than 4.
pub fn encode_hash(hash: u32, len: usize) -> String {
    assert!(len > 0 && len <= 4, "encode_hash: len must be 1..=4");

    let mut result = String::with_capacity(len);
    for i in 0..len {
        let byte = ((hash >> (i * 8)) % 26) as u8 + b'a';
        result.push(byte as char);
    }
    result
}

/// Default anchor hash length (3 lowercase letters).
pub const DEFAULT_HASH_LEN: usize = 3;

#[cfg(test)]
mod tests {
    use super::*;

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
        for len in 1..=4 {
            let encoded = encode_hash(0xDEAD_BEEF, len);
            assert_eq!(encoded.len(), len);
            assert!(encoded.bytes().all(|b| b.is_ascii_lowercase()));
        }
    }

    #[test]
    fn encode_hash_deterministic() {
        assert_eq!(encode_hash(42, 3), encode_hash(42, 3));
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
