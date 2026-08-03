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
//! Differential golden-output dumper for the optimization waves.
//!
//! Prints every rendered surface of the three hashline tools — anchored reads
//! (all file shapes and windows), edit success/stale/ambiguous/malformed/
//! suffix-recovery/range/batch/write paths, and grep match/context/gap/no-match
//! output — over a deterministic corpus and all 36 scheme configurations
//! (3 kinds × 3 hash lengths × 4 chunk sizes). Nothing path-dependent is
//! printed, so two builds of the crate can be diffed byte for byte.
//!
//! Usage (compare a working tree against a known-good commit):
//!
//! ```sh
//! git worktree add /tmp/hl-baseline <baseline-commit>
//! cp examples/golden.rs /tmp/hl-baseline/examples/golden.rs  # adapt the API if it changed
//! cargo run --release --example golden > /tmp/golden-new.txt
//! cargo run --release --manifest-path /tmp/hl-baseline/Cargo.toml --example golden \
//!     > /tmp/golden-old.txt
//! diff /tmp/golden-old.txt /tmp/golden-new.txt   # must be empty
//! ```
//!
//! Any optimization that is not meant to change the wire format must keep this
//! diff empty.

use hashline::config::{SchemeConfig, SchemeKind};
use hashline::edit::HashlineOp;
use hashline::edit::apply::apply_edits;
use hashline::grep::run_grep;
use hashline::protocol::GrepRequest;
use hashline::read::format_hashline_content;
use hashline::util::Workspace;

struct Xorshift32(u32);

impl Xorshift32 {
    fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn next_range(&mut self, bound: u32) -> u32 {
        self.next_u32() % bound
    }
}

const IDENTIFIERS: &[&str] = &["value", "index", "buffer", "result", "config", "handler"];
const KEYWORDS: &[&str] = &["let", "fn", "if", "for", "return", "match"];

fn corpus(num_lines: usize, seed: u32, trailing_newline: bool) -> String {
    let mut rng = Xorshift32::new(seed);
    let mut out = String::new();
    for i in 0..num_lines {
        if i > 0 {
            out.push('\n');
        }
        match rng.next_range(16) {
            0 => {}
            1 => out.push_str("    duplicated_line();"),
            2 => {
                out.push_str("// ");
                out.push_str(&"long".repeat(60));
            }
            3 => {
                out.push_str(&format!("    let value_{i} = compute();\r"));
            }
            _ => {
                let indent = "    ".repeat(rng.next_range(4) as usize);
                let kw = KEYWORDS[rng.next_range(KEYWORDS.len() as u32) as usize];
                let ident = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
                let n = rng.next_range(1_000);
                out.push_str(&format!("{indent}{kw} {ident}_{i} = other({n});"));
            }
        }
    }
    if trailing_newline {
        out.push('\n');
    }
    out
}

fn configs() -> Vec<SchemeConfig> {
    let mut out = Vec::new();
    for kind in [
        SchemeKind::Chunk,
        SchemeKind::ContentOnly,
        SchemeKind::Checkpoint,
    ] {
        for hash_len in [1usize, 3, 4] {
            for size in [1usize, 5, 8, 16] {
                out.push(SchemeConfig {
                    kind,
                    hash_len,
                    chunk_size: size,
                    checkpoint_interval: size,
                });
            }
        }
    }
    out
}

/// The anchor of 1-based `line` in `content`, harvested from read output.
///
/// A macro, not a function, so this file needs no name for the scheme type
/// (which is exactly what the refactor changes).
macro_rules! anchor_for {
    ($content:expr, $line:expr, $scheme:expr) => {
        format_hashline_content($content, Some($line), Some(1), $scheme)
            .split('\u{2192}')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
}

fn main() {
    for (ci, config) in configs().iter().enumerate() {
        let scheme = config.build_scheme().expect("build scheme");
        println!("=== config {ci} {config:?} ===");

        // --- read rendering, every file shape and window ---
        for &(seed, lines, trailing) in &[
            (1u32, 0usize, false),
            (2, 1, true),
            (3, 1, false),
            (4, 17, true),
            (5, 17, false),
            (6, 40, true),
        ] {
            let content = corpus(lines, seed, trailing);
            println!("-- read seed={seed} lines={lines} trailing={trailing}");
            println!("{}", format_hashline_content(&content, None, None, scheme));
            for &(offset, limit) in &[(1usize, 1usize), (2, 3), (7, 9), (16, 2), (17, 100)] {
                println!("-- window {offset}+{limit}");
                println!(
                    "{}",
                    format_hashline_content(&content, Some(offset), Some(limit), scheme)
                );
            }
        }

        // --- edit rendering: success, stale, malformed, suffix recovery ---
        let content = corpus(40, 7, true);
        for line in [1usize, 8, 16, 17, 33, 40] {
            let anchor = anchor_for!(&content, line, scheme);
            let result = apply_edits(
                &content,
                &[HashlineOp::Replace {
                    anchor: anchor.clone(),
                    end_anchor: None,
                    content: "REPLACED".to_owned(),
                }],
                scheme,
            );
            println!("-- edit replace line={line} anchor={anchor}");
            println!("{:?}", result.output);
            println!("{:?}", result.new_content);

            let result = apply_edits(
                &content,
                &[HashlineOp::InsertAfter {
                    anchor: anchor.clone(),
                    content: "INSERTED A\nINSERTED B".to_owned(),
                }],
                scheme,
            );
            println!("-- edit insert_after line={line}");
            println!("{:?}", result.output);
            println!("{:?}", result.new_content);

            // Suffix-only anchor (line number dropped) → recovery path.
            if let Some((_, suffix)) = anchor.split_once(':') {
                let result = apply_edits(
                    &content,
                    &[HashlineOp::Replace {
                        anchor: suffix.to_owned(),
                        end_anchor: None,
                        content: "SUFFIX RECOVERED".to_owned(),
                    }],
                    scheme,
                );
                println!("-- edit suffix-recovery line={line} suffix={suffix}");
                println!("{:?}", result.output);
            }
        }

        // Stale-anchor error path (content shifted down by one line).
        let shifted = format!("// shift marker\n{content}");
        for line in [2usize, 9, 20, 39] {
            let anchor = anchor_for!(&content, line, scheme);
            let result = apply_edits(
                &shifted,
                &[HashlineOp::Replace {
                    anchor: anchor.clone(),
                    end_anchor: None,
                    content: "SHOULD NOT APPLY".to_owned(),
                }],
                scheme,
            );
            println!("-- edit stale line={line} anchor={anchor}");
            println!("{:?}", result.output);
        }

        // Malformed anchor, out-of-range, range replace, batch, write.
        for bad in ["not-an-anchor", "99999:abc:rst", "0:abc"] {
            let result = apply_edits(
                &content,
                &[HashlineOp::Replace {
                    anchor: bad.to_owned(),
                    end_anchor: None,
                    content: "x".to_owned(),
                }],
                scheme,
            );
            println!("-- edit bad anchor {bad}");
            println!("{:?}", result.output);
        }

        let start = anchor_for!(&content, 5, scheme);
        let end = anchor_for!(&content, 25, scheme);
        let result = apply_edits(
            &content,
            &[HashlineOp::Replace {
                anchor: start,
                end_anchor: Some(end),
                content: "RANGE MERGED".to_owned(),
            }],
            scheme,
        );
        println!("-- edit range replace 5..25");
        println!("{:?}", result.output);
        println!("{:?}", result.new_content);

        let batch: Vec<HashlineOp> = [3usize, 12, 30]
            .iter()
            .map(|&line| HashlineOp::Replace {
                anchor: anchor_for!(&content, line, scheme),
                end_anchor: None,
                content: format!("BATCH {line}"),
            })
            .collect();
        let result = apply_edits(&content, &batch, scheme);
        println!("-- edit batch");
        println!("{:?}", result.output);
        println!("{:?}", result.new_content);

        let result = apply_edits(
            &content,
            &[HashlineOp::Write {
                content: "written one\nwritten two\nwritten three\n".to_owned(),
            }],
            scheme,
        );
        println!("-- edit write");
        println!("{:?}", result.output);

        // --- grep rendering ---
        let tmp = tempfile::TempDir::new().expect("tempdir");
        for i in 0..6usize {
            let dir = tmp.path().join(format!("d{}", i % 3));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let mut body = corpus(30, 100 + i as u32, true);
            if i == 2 {
                body.push_str("rare_marker_here\n");
            }
            std::fs::write(dir.join(format!("f{i}.rs")), &body).expect("write fixture");
        }
        let ws = Workspace::new(tmp.path().to_path_buf(), false);
        for (label, pattern, context) in [
            ("rare", "rare_marker_here", None),
            ("common", "value", None),
            ("anchored", "^fn ", None),
            ("ctx", "duplicated", Some(2u16)),
            ("nomatch", "zzz_no_such_token", None),
            ("dollar", ";$", None),
        ] {
            let input = GrepRequest {
                pattern: pattern.to_owned(),
                path: None,
                glob: None,
                ignore_case: false,
                after_context: None,
                before_context: None,
                context,
                max_matches: 40,
            };
            let outcome = run_grep(&ws, &input);
            println!("-- grep {label} is_error={}", outcome.is_error);
            println!("{}", outcome.text);
        }
    }
}
