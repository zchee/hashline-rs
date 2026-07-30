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
//! Deterministic corpus helpers shared by the unit tests.
//!
//! Test-only (`#[cfg(test)]`): a self-contained xorshift32 generator keeps the
//! differential corpora reproducible without adding a `rand` dependency.

/// Deterministic xorshift32 pseudo-random generator.
pub struct Xorshift32(u32);

impl Xorshift32 {
    /// Create a generator seeded with `seed` (zero is remapped, since an
    /// all-zero xorshift state never advances).
    pub fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    /// Next pseudo-random `u32`.
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Next pseudo-random value in `0..bound`.
    pub fn next_range(&mut self, bound: u32) -> u32 {
        self.next_u32() % bound
    }
}

/// Identifier pool for synthetic code-like lines.
const IDENTIFIERS: &[&str] = &[
    "value", "index", "buffer", "result", "config", "handler", "state", "count",
];

/// Keyword pool for synthetic code-like lines.
const KEYWORDS: &[&str] = &["let", "fn", "if", "for", "return", "match"];

/// Generate one deterministic line.
///
/// The mix deliberately includes blank lines, a repeated line (so
/// shifted-anchor ambiguity is reachable), a long line, and a `\r`-terminated
/// line (so joined corpora contain CRLF terminators).
fn corpus_line(rng: &mut Xorshift32, line_no: usize) -> String {
    match rng.next_range(16) {
        0 => String::new(),
        1 => "    duplicated_line();".to_owned(),
        2 => format!("// {}", "long".repeat(120)),
        3 => format!("    let value_{line_no} = compute();\r"),
        _ => {
            let indent = "    ".repeat(rng.next_range(4) as usize);
            let kw = KEYWORDS[rng.next_range(KEYWORDS.len() as u32) as usize];
            let ident = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
            let n = rng.next_range(1_000);
            format!("{indent}{kw} {ident}_{line_no} = other({n});")
        }
    }
}

/// Generate a deterministic corpus of `num_lines` `\n`-joined lines.
///
/// With `trailing_newline` the content ends in `\n`, which makes anchor line
/// counts include the synthetic trailing empty line — the edge case windowed
/// anchor generation must reproduce.
pub fn corpus(num_lines: usize, seed: u32, trailing_newline: bool) -> String {
    let mut rng = Xorshift32::new(seed);
    let mut out = String::with_capacity(num_lines * 24);
    for i in 0..num_lines {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&corpus_line(&mut rng, i));
    }
    if trailing_newline {
        out.push('\n');
    }
    out
}
