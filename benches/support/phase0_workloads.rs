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

//! Shared deterministic workloads and incompatible-redesign lower-bound prototypes.
//!
//! Nothing in this module is linked into the shipping library or binary.

use std::{
    fmt::Write as _,
    fs::OpenOptions,
    io::{self, Write as _},
    path::Path,
};

use memchr::memchr_iter;

/// Share of lines that are about 2 KiB long in the long-line-heavy corpus.
pub const LONG_LINE_PERCENT: u32 = 85;

const IDENTIFIERS: &[&str] = &[
    "value", "index", "buffer", "result", "config", "handler", "state", "count", "items", "cursor",
    "reader", "writer", "context", "target", "source", "delta",
];

const KEYWORDS: &[&str] = &[
    "let", "fn", "if", "for", "while", "return", "match", "struct", "impl", "pub",
];

#[derive(Debug, Clone, Copy)]
struct Xorshift32(u32);

impl Xorshift32 {
    fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    fn next_u32(&mut self) -> u32 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        self.0 = value;
        value
    }

    fn next_range(&mut self, bound: u32) -> u32 {
        self.next_u32() % bound
    }
}

fn generate_line(rng: &mut Xorshift32, line_no: usize) -> String {
    if rng.next_range(37) == 0 {
        return String::new();
    }
    if rng.next_range(211) == 0 {
        let word = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
        return format!("// {}", word.repeat(300));
    }

    let depth = rng.next_range(5) as usize;
    let indent = "    ".repeat(depth);
    let keyword = KEYWORDS[rng.next_range(KEYWORDS.len() as u32) as usize];
    let identifier = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
    let argument = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
    let number = rng.next_range(1000);
    format!("{indent}{keyword} {identifier}_{line_no} = {argument}({number});")
}

/// Generate the deterministic code-like corpus used by every Phase 0 workload.
pub fn generate_corpus(num_lines: usize, seed: u32) -> String {
    let mut rng = Xorshift32::new(seed);
    let mut output = String::with_capacity(num_lines * 24);
    for line_no in 0..num_lines {
        output.push_str(&generate_line(&mut rng, line_no));
        output.push('\n');
    }
    output
}

/// Generate a deterministic corpus dominated by about 2 KiB source lines.
pub fn generate_long_line_corpus(num_lines: usize, seed: u32) -> String {
    let mut rng = Xorshift32::new(seed);
    let mut output = String::with_capacity(num_lines * 1_800);
    for line_no in 0..num_lines {
        if rng.next_range(100) < LONG_LINE_PERCENT {
            let word = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
            let number = rng.next_range(1000);
            let repeated = word.repeat(250);
            let _ = write!(output, "const {word}_{line_no}={repeated};//{number}");
        } else {
            output.push_str(&generate_line(&mut rng, line_no));
        }
        output.push('\n');
    }
    output
}

/// Count logical lines without materializing positions.
pub fn logical_line_count(content: &str) -> usize {
    memchr_iter(b'\n', content.as_bytes()).count() + 1
}

/// Hash every raw logical line without whitespace normalization.
#[cfg(feature = "gxhash")]
pub fn raw_line_hashes(content: &str) -> Vec<u32> {
    let mut hashes = Vec::with_capacity(logical_line_count(content));
    hashes.extend(
        content
            .split('\n')
            .map(|line| gxhash::gxhash32(line.as_bytes(), 0)),
    );
    hashes
}

/// Compute a gxhash 128-bit file version and logical line count in two specialized passes.
#[cfg(feature = "gxhash")]
pub fn gxhash128_and_line_count(content: &str) -> (u128, usize) {
    (
        gxhash::gxhash128(content.as_bytes(), 0),
        logical_line_count(content),
    )
}

/// Compute an XXH3 128-bit file version and logical line count.
pub fn xxh3_128_and_line_count(content: &str) -> (u128, usize) {
    (
        xxhash_rust::xxh3::xxh3_128(content.as_bytes()),
        logical_line_count(content),
    )
}

/// Compute a truncated BLAKE3 128-bit file version and logical line count.
pub fn blake3_128_and_line_count(content: &str) -> (u128, usize) {
    let digest = blake3::hash(content.as_bytes());
    let bytes: [u8; 16] = digest.as_bytes()[..16]
        .try_into()
        .expect("BLAKE3 digest always has at least 16 bytes");
    (u128::from_le_bytes(bytes), logical_line_count(content))
}

/// Materialize every logical line start using four bytes per position.
pub fn offsets_u32(content: &str) -> Vec<u32> {
    assert!(
        u32::try_from(content.len()).is_ok(),
        "u32 offset workload requires content below 4 GiB"
    );
    let mut offsets = Vec::with_capacity(logical_line_count(content));
    offsets.push(0);
    offsets.extend(memchr_iter(b'\n', content.as_bytes()).map(|position| {
        u32::try_from(position + 1).expect("content length was checked before scanning")
    }));
    offsets
}

/// Materialize every logical line start using eight bytes per position.
pub fn offsets_u64(content: &str) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(logical_line_count(content));
    offsets.push(0);
    offsets.extend(memchr_iter(b'\n', content.as_bytes()).map(|position| position as u64 + 1));
    offsets
}

/// Sparse positional selection for one requested line window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseSelection {
    /// Zero-based number of the first selected line.
    pub first_line: usize,
    /// Exact UTF-8 byte start of each selected logical line.
    pub starts: Vec<u64>,
    /// Exclusive byte boundary after the last selected logical line.
    pub end: u64,
}

/// Select only the positions needed for a zero-based line window.
pub fn sparse_select(content: &str, start_line: usize, count: usize) -> SparseSelection {
    let line_count = logical_line_count(content);
    if count == 0 || start_line >= line_count {
        return SparseSelection {
            first_line: start_line.min(line_count),
            starts: Vec::new(),
            end: content.len() as u64,
        };
    }

    let end_line = start_line.saturating_add(count).min(line_count);
    let mut starts = Vec::with_capacity(end_line - start_line);
    if start_line == 0 {
        starts.push(0);
    }

    let mut current_line = 0usize;
    let mut end = content.len() as u64;
    for newline in memchr_iter(b'\n', content.as_bytes()) {
        current_line += 1;
        if current_line >= start_line && current_line < end_line {
            starts.push((newline + 1) as u64);
        }
        if current_line == end_line {
            end = (newline + 1) as u64;
            break;
        }
    }

    SparseSelection {
        first_line: start_line,
        starts,
        end,
    }
}

fn append_positioned_line(
    output: &mut String,
    content: &str,
    line_number: usize,
    start: usize,
    next_start: usize,
) {
    let mut content_end = next_start;
    if content_end > start && content.as_bytes()[content_end - 1] == b'\n' {
        content_end -= 1;
    }
    let _ = write!(output, "{line_number}@{start}|");
    output.push_str(&content[start..content_end]);
    output.push('\n');
}

/// Render one protocol-style section with one version header and positional line references.
pub fn render_positions(content: &str, version: u128, start_line: usize, count: usize) -> String {
    let total_lines = logical_line_count(content);
    let selection = sparse_select(content, start_line, count);
    let selected_bytes = selection.end as usize
        - selection.starts.first().copied().unwrap_or(selection.end) as usize;
    let mut output = String::with_capacity(selected_bytes + selection.starts.len() * 24 + 64);
    let _ = writeln!(output, "[snapshot={version:032x} lines={total_lines}]");

    for (index, &start) in selection.starts.iter().enumerate() {
        let next_start = selection
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(selection.end) as usize;
        append_positioned_line(
            &mut output,
            content,
            selection.first_line + index + 1,
            start as usize,
            next_start,
        );
    }
    output
}

/// Render every line using a precomputed version.
pub fn render_all_positions(content: &str, version: u128) -> String {
    render_positions(content, version, 0, logical_line_count(content))
}

/// Compute the portable prototype version and render every positional line.
pub fn versioned_render_all(content: &str) -> String {
    let (version, _) = xxh3_128_and_line_count(content);
    render_all_positions(content, version)
}

/// One exact byte-range replacement used by the splice prototype.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteEdit {
    /// Inclusive byte start.
    pub start: usize,
    /// Exclusive byte end.
    pub end: usize,
    /// Exact replacement text.
    pub content: String,
}

/// Build deterministic whole-line replacement ranges for selected one-based lines.
pub fn replacement_edits(content: &str, lines: &[usize]) -> Vec<ByteEdit> {
    let offsets = offsets_u64(content);
    lines
        .iter()
        .enumerate()
        .map(|(index, &line)| {
            assert!(
                line > 0 && line <= offsets.len(),
                "replacement line is in range"
            );
            let start = offsets[line - 1] as usize;
            let end = offsets.get(line).copied().unwrap_or(content.len() as u64) as usize;
            ByteEdit {
                start,
                end,
                content: format!("REPLACED POSITIONAL LINE {index}\n"),
            }
        })
        .collect()
}

/// Apply sorted, non-overlapping byte edits with one output allocation and one copy pass.
pub fn apply_byte_edits(content: &str, edits: &[ByteEdit]) -> String {
    let inserted_bytes = edits.iter().map(|edit| edit.content.len()).sum::<usize>();
    let removed_bytes = edits
        .iter()
        .map(|edit| edit.end - edit.start)
        .sum::<usize>();
    let mut output = Vec::with_capacity(content.len() - removed_bytes + inserted_bytes);
    let mut cursor = 0usize;

    for edit in edits {
        assert!(
            cursor <= edit.start && edit.start <= edit.end && edit.end <= content.len(),
            "byte edits must be sorted, non-overlapping, and in range"
        );
        output.extend_from_slice(&content.as_bytes()[cursor..edit.start]);
        output.extend_from_slice(edit.content.as_bytes());
        cursor = edit.end;
    }
    output.extend_from_slice(&content.as_bytes()[cursor..]);

    String::from_utf8(output).expect("valid UTF-8 input and replacements remain valid UTF-8")
}

/// Replace a file directly, matching the current truncate-and-write persistence floor.
pub fn direct_write(path: &Path, content: &[u8]) -> io::Result<()> {
    std::fs::write(path, content)
}

/// Write a same-directory temporary file and atomically rename it over the destination.
pub fn atomic_temp_write(path: &Path, content: &[u8], nonce: u64) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hashline-phase0");
    let temporary = path.with_file_name(format!(".{file_name}.phase0.{nonce}.tmp"));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(content)?;
    file.flush()?;
    drop(file);

    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}
