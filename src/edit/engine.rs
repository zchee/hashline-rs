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
//! Optimized apply engine for the wired edit path.
//!
//! The reference model re-validates the whole source text and rebuilds the
//! line-start vector on every call (and builds a header it discards on
//! success); this engine validates positions against the snapshot's
//! materialized offsets in O(log n) each and splices once with exact
//! capacity.
//!
//! Equality with the reference model is structural, not aspirational: the
//! fast path handles only the fully-valid case, and **any** rejection falls
//! back to [`apply_versioned_reference_edits`], which reproduces the exact
//! structured error (or succeeds where the fast validator was conservative —
//! either way the observable behavior is the reference's). Debug and test
//! builds additionally re-run the reference on every fast-path success and
//! assert byte equality.

use memchr::memchr;

use crate::{
    protocol::{
        EditRequest, Position, ProtocolError, apply_versioned_reference_edits, validate_file_size,
    },
    snapshot::Snapshot,
};

/// Apply `request` against `snapshot`, matching the reference model exactly.
///
/// This is the production apply engine behind [`crate::edit::run`]; the
/// reference model stays the semantic oracle. Public so the wired benches
/// (and embedders that already hold a [`Snapshot`]) measure and use the real
/// path.
///
/// # Errors
///
/// Exactly the reference model's errors: a structured snapshot conflict on
/// mismatch, or a stable contract error for invalid positions, ranges,
/// overlaps, content, or size.
pub fn apply_edits_fast(
    snapshot: &Snapshot,
    request: &EditRequest,
) -> Result<Vec<u8>, ProtocolError> {
    match try_fast(snapshot, request) {
        Some(applied) => {
            #[cfg(debug_assertions)]
            {
                let reference =
                    apply_versioned_reference_edits(snapshot.bytes(), snapshot.id(), request);
                debug_assert!(
                    matches!(&reference, Ok(bytes) if *bytes == applied),
                    "fast apply diverged from the reference model"
                );
            }
            Ok(applied)
        }
        None => apply_versioned_reference_edits(snapshot.bytes(), snapshot.id(), request),
    }
}

/// One validated operation, mirroring the reference model's internal shape.
struct FastEdit<'a> {
    index: usize,
    start_ordinal: u64,
    end_ordinal: u64,
    start_byte: usize,
    end_byte: usize,
    content: &'a str,
}

impl FastEdit<'_> {
    const fn is_insertion(&self) -> bool {
        self.start_ordinal == self.end_ordinal
    }
}

/// Same predicate as the reference model's `edits_overlap`.
fn overlaps(left: &FastEdit<'_>, right: &FastEdit<'_>) -> bool {
    match (left.is_insertion(), right.is_insertion()) {
        (false, false) => {
            left.start_ordinal < right.end_ordinal && right.start_ordinal < left.end_ordinal
        }
        (true, false) => {
            right.start_ordinal < left.start_ordinal && left.start_ordinal < right.end_ordinal
        }
        (false, true) => {
            left.start_ordinal < right.start_ordinal && right.start_ordinal < left.end_ordinal
        }
        (true, true) => false,
    }
}

/// Resolve a position to `(ordinal, byte)` exactly as `boundary_ordinal`
/// does: an in-range line whose byte equals that line's start, or the
/// terminal boundary `(line_count + 1)@byte_len`.
fn resolve_boundary(snapshot: &Snapshot, position: Position) -> Option<(u64, usize)> {
    let line_count = snapshot.line_count();
    let line = position.line();
    let byte = position.byte();
    if line <= line_count {
        let start = snapshot.line_start(line).ok().flatten()?;
        if start != byte {
            return None;
        }
        Some((line - 1, usize::try_from(byte).ok()?))
    } else if line == line_count.checked_add(1)? && byte == snapshot.byte_len() {
        Some((line_count, usize::try_from(byte).ok()?))
    } else {
        None
    }
}

/// The fully-valid fast path; `None` means "let the reference model decide".
fn try_fast(snapshot: &Snapshot, request: &EditRequest) -> Option<Vec<u8>> {
    request.validate().ok()?;
    if request.snapshot != snapshot.id() {
        return None;
    }
    let source = snapshot.bytes();

    let mut validated: Vec<FastEdit<'_>> = Vec::with_capacity(request.edits.len());
    for (index, operation) in request.edits.iter().enumerate() {
        let content = operation.content();
        if memchr(0, content.as_bytes()).is_some() {
            return None;
        }
        let (start_ordinal, start_byte) = resolve_boundary(snapshot, operation.start())?;
        let (end_ordinal, end_byte) = resolve_boundary(snapshot, operation.end())?;
        if start_ordinal > end_ordinal {
            return None;
        }
        validated.push(FastEdit {
            index,
            start_ordinal,
            end_ordinal,
            start_byte,
            end_byte,
            content,
        });
    }

    for left in 0..validated.len() {
        for right in left + 1..validated.len() {
            if overlaps(&validated[left], &validated[right]) {
                return None;
            }
        }
    }

    let removed = validated.iter().try_fold(0_usize, |total, operation| {
        total.checked_add(operation.end_byte - operation.start_byte)
    })?;
    let added = validated.iter().try_fold(0_usize, |total, operation| {
        total.checked_add(operation.content.len())
    })?;
    let final_length = source.len().checked_sub(removed)?.checked_add(added)?;
    validate_file_size(u64::try_from(final_length).ok()?).ok()?;

    validated.sort_by_key(|operation| {
        (
            operation.start_ordinal,
            u8::from(!operation.is_insertion()),
            operation.index,
        )
    });

    let mut output = Vec::with_capacity(final_length);
    let mut cursor = 0;
    for operation in validated {
        if operation.start_byte > cursor {
            output.extend_from_slice(&source[cursor..operation.start_byte]);
            cursor = operation.start_byte;
        }
        output.extend_from_slice(operation.content.as_bytes());
        if !operation.is_insertion() {
            cursor = operation.end_byte;
        }
    }
    output.extend_from_slice(&source[cursor..]);
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EditOperation, ErrorCode, SnapshotId};

    fn pos(line: u64, byte: u64) -> Position {
        Position::new(line, byte).unwrap()
    }

    fn request(snapshot: SnapshotId, edits: Vec<EditOperation>) -> EditRequest {
        EditRequest {
            file_path: "differential.txt".into(),
            snapshot,
            edits,
        }
    }

    /// Deterministic xorshift so the differential corpus never depends on
    /// ambient randomness (reproducible failures).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: u64) -> u64 {
            let mut value = self.0;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.0 = value;
            value % bound.max(1)
        }
    }

    fn corpus(rng: &mut Rng, lines: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..lines {
            match rng.next(7) {
                0 => out.extend_from_slice(b"\n"),
                1 => out.extend_from_slice("多バイト行 ünïcode\n".as_bytes()),
                2 => out.extend_from_slice(b"crlf terminated\r\n"),
                3 => out.extend_from_slice(format!("plain line {i}\n").as_bytes()),
                4 => out.extend_from_slice("🚀 emoji + tab\t\n".as_bytes()),
                5 => out.extend_from_slice(format!("let value_{i} = {i};\n").as_bytes()),
                _ => out.extend_from_slice(b"trailing spaces   \n"),
            }
        }
        if rng.next(3) == 0 && !out.is_empty() {
            out.pop();
        }
        out
    }

    /// Random position: usually a valid boundary, sometimes deliberately off.
    fn position(rng: &mut Rng, snapshot: &Snapshot) -> Position {
        let line_count = snapshot.line_count();
        let line = 1 + rng.next(line_count + 2);
        let byte = if rng.next(5) == 0 {
            rng.next(snapshot.byte_len() + 2)
        } else if line <= line_count {
            snapshot.line_start(line).unwrap().unwrap_or(0)
        } else {
            snapshot.byte_len()
        };
        Position::new(line.min(u64::MAX - 1), byte).unwrap_or_else(|_| pos(1, 0))
    }

    #[test]
    fn differential_engine_equals_reference_over_randomized_corpora() {
        let mut rng = Rng(0x5EED_CAFE_F00D_0001);
        let mut fast_accepts = 0_u32;
        let mut rejections = 0_u32;

        for case in 0..1_000 {
            let lines = [0, 1, 2, 5, 40][usize::try_from(case % 5).unwrap()];
            let source = corpus(&mut rng, lines);
            let snapshot = Snapshot::from_bytes(source.clone()).expect("corpus is valid text");

            let snapshot_id = if rng.next(10) == 0 {
                SnapshotId::from_u128(u128::from(rng.next(u64::MAX)))
            } else {
                snapshot.id()
            };
            let mut edits = Vec::new();
            for _ in 0..(1 + rng.next(3)) {
                let a = position(&mut rng, &snapshot);
                let b = position(&mut rng, &snapshot);
                let (start, end) = if a.byte() <= b.byte() { (a, b) } else { (b, a) };
                let content = match rng.next(4) {
                    0 => String::new(),
                    1 => "replacement\n".to_owned(),
                    2 => "改行なし置換".to_owned(),
                    _ => "two\nlines\n".to_owned(),
                };
                edits.push(EditOperation::replace(start, end, content));
            }
            let request = request(snapshot_id, edits);

            let fast = apply_edits_fast(&snapshot, &request);
            let reference =
                apply_versioned_reference_edits(snapshot.bytes(), snapshot.id(), &request);

            match (&fast, &reference) {
                (Ok(fast_bytes), Ok(reference_bytes)) => {
                    fast_accepts += 1;
                    assert_eq!(fast_bytes, reference_bytes, "case {case} bytes diverged");
                }
                (Err(fast_error), Err(reference_error)) => {
                    rejections += 1;
                    assert_eq!(
                        serde_json::to_value(fast_error).unwrap(),
                        serde_json::to_value(reference_error).unwrap(),
                        "case {case} error payload diverged"
                    );
                }
                (fast, reference) => {
                    panic!("case {case} verdicts diverged: fast={fast:?} reference={reference:?}")
                }
            }
        }

        assert!(
            fast_accepts >= 100,
            "suite must exercise the fast path: {fast_accepts}"
        );
        assert!(
            rejections >= 100,
            "suite must exercise rejections: {rejections}"
        );
    }

    #[test]
    fn conflict_and_invalid_position_payloads_match_reference() {
        let snapshot = Snapshot::from_bytes(b"alpha\nbeta\ngamma\n".to_vec()).unwrap();

        let stale = request(
            SnapshotId::from_u128(7),
            vec![EditOperation::replace(pos(2, 6), pos(3, 11), "X\n".into())],
        );
        let fast = apply_edits_fast(&snapshot, &stale).unwrap_err();
        assert_eq!(fast.code, ErrorCode::SnapshotConflict);
        assert_eq!(
            serde_json::to_value(&fast).unwrap(),
            serde_json::to_value(
                apply_versioned_reference_edits(snapshot.bytes(), snapshot.id(), &stale)
                    .unwrap_err()
            )
            .unwrap()
        );

        let bad_position = request(
            snapshot.id(),
            vec![EditOperation::replace(pos(2, 7), pos(3, 11), "X\n".into())],
        );
        let fast = apply_edits_fast(&snapshot, &bad_position).unwrap_err();
        assert_eq!(fast.code, ErrorCode::InvalidPosition);
    }
}
