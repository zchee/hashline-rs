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

//! Shared deterministic corpus and offset workloads for the benches.
//!
//! Nothing in this module is linked into the shipping library or binary.

use memchr::memchr_iter;

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

/// Count logical lines without materializing positions.
pub fn logical_line_count(content: &str) -> usize {
    memchr_iter(b'\n', content.as_bytes()).count() + 1
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
