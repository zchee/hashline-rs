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
//! Criterion benchmarks for hashline's hot paths.
//!
//! These are the Phase 0 baselines for the max-performance optimization plan
//! (`.omc/plans/2026-07-30-max-performance-optimization.md`): every later
//! phase is measured against the numbers this harness records in
//! `benches/BASELINE.md`. Corpus generation is fully deterministic (a small
//! inline xorshift32 PRNG, no `rand` and no system time) so results are
//! reproducible across runs and machines.

use std::{
    hint::black_box,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use hashline::{
    HashlineServer, cache,
    grep::run_grep,
    protocol::{EditRequest, GrepOutputMode, GrepRequest},
    snapshot::Snapshot,
    util::Workspace,
};
use memchr::memchr_iter;
use rmcp::model::{CallToolResult, ContentBlock};

#[path = "support/phase0_workloads.rs"]
mod phase0_workloads;

use phase0_workloads::generate_corpus;

/// Number of files in the synthetic grep fixture tree.
const GREP_FILE_COUNT: usize = 2_000;
/// Lines per file in the synthetic grep fixture tree.
const GREP_LINES_PER_FILE: usize = 40;
/// Literal present in exactly one file, for the rare-literal grep benchmark.
const GREP_RARE_TOKEN: &str = "zqxj7_rare_marker_unique";
/// Literal that occurs naturally across most files via the identifier pool,
/// for the common-literal grep benchmark.
const GREP_COMMON_TOKEN: &str = "value";

/// Lazily-built synthetic fixture tree for grep benchmarks: ~2,000 small
/// code-like files across nested directories. Built once (via `OnceLock`)
/// regardless of how many benchmark iterations run against it.
static GREP_FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();

/// Root path of the (lazily-built) grep fixture tree.
fn grep_fixture_root() -> &'static Path {
    let (_tmp, root) = GREP_FIXTURE.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir for grep fixture");
        let root = tmp.path().to_path_buf();
        for i in 0..GREP_FILE_COUNT {
            let dir = root
                .join(format!("dir_{}", i / 100))
                .join(format!("sub_{}", (i / 10) % 10));
            std::fs::create_dir_all(&dir).expect("create grep fixture dir");
            let mut content =
                generate_corpus(GREP_LINES_PER_FILE, 0x1000_u32.wrapping_add(i as u32));
            if i == GREP_FILE_COUNT / 2 {
                content.push_str(GREP_RARE_TOKEN);
                content.push('\n');
            }
            std::fs::write(dir.join(format!("file_{}.rs", i % 10)), &content)
                .expect("write grep fixture file");
        }
        (tmp, root)
    });
    root.as_path()
}

/// Build a `GrepRequest` searching for `pattern` with default options.
fn grep_input(pattern: &str) -> GrepRequest {
    GrepRequest {
        pattern: pattern.to_owned(),
        path: None,
        glob: None,
        ignore_case: false,
        after_context: None,
        before_context: None,
        context: None,
        max_matches: 200,
        output_mode: GrepOutputMode::Content,
    }
}

/// `run_grep` over the ~2,000-file fixture: a rare literal, a common
/// literal, and a `^`-anchored regex.
fn bench_grep(c: &mut Criterion) {
    let root = grep_fixture_root();
    let ws = Workspace::new(root.to_path_buf(), false);
    let mut group = c.benchmark_group("grep");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(6));

    for (label, pattern) in [
        ("rare_literal", GREP_RARE_TOKEN),
        ("common_literal", GREP_COMMON_TOKEN),
        ("anchored_regex", "^fn "),
    ] {
        let probe = run_grep(&ws, &grep_input(pattern));
        assert!(
            !probe.is_error,
            "wired tree grep must succeed: {}",
            probe.text
        );
        assert!(
            probe.text.contains("matches="),
            "wired summary missing: {}",
            probe.text
        );
        group.bench_function(label, |b| {
            b.iter(|| {
                let outcome = run_grep(&ws, &grep_input(pattern));
                assert!(!outcome.is_error, "wired tree grep must succeed");
                black_box(outcome.text.len())
            });
        });

        let mut files_input = grep_input(pattern);
        files_input.output_mode = GrepOutputMode::FilesWithMatches;
        group.bench_function(format!("{label}_files"), |b| {
            b.iter(|| {
                let outcome = run_grep(&ws, &files_input);
                assert!(!outcome.is_error, "wired tree files-mode grep must succeed");
                black_box(outcome.text.len())
            });
        });
    }
    group.finish();
}

/// Lines in the single-file grep fixture (~1.6 MB of code-like text).
const GREP_LARGE_LINES: usize = 50_000;

/// File name of the single-file grep fixture, relative to its workspace root.
const GREP_LARGE_FILE: &str = "large.rs";

/// Lazily-built single-file grep fixture: one ~1.6 MB, 50,000-line file.
///
/// The 2,000-file tree fixture is filesystem-bound (directory walk plus 2,000
/// opens dominate its wall time), so it cannot show a matching-engine or
/// anchoring win. This one file isolates exactly that: read once, search once,
/// anchor only what is rendered.
static GREP_LARGE_FIXTURE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();

/// Root path of the (lazily-built) single-file grep fixture.
fn grep_large_fixture_root() -> &'static Path {
    let (_tmp, root) = GREP_LARGE_FIXTURE.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir for large grep fixture");
        let root = tmp.path().to_path_buf();
        let mut content = generate_corpus(GREP_LARGE_LINES, 0x1A26_F11E);
        content.push_str(GREP_RARE_TOKEN);
        content.push('\n');
        std::fs::write(root.join(GREP_LARGE_FILE), &content).expect("write large grep fixture");
        (tmp, root)
    });
    root.as_path()
}

/// `run_grep` over the single 50,000-line file: a rare literal, a common
/// literal, and a `^`-anchored regex.
///
/// This is acceptance criterion 4(b)'s match-bound bench. The search path is
/// the file itself, which takes `run_grep`'s single-file short circuit — the
/// directory walker's thread-pool startup is several milliseconds and would
/// bury exactly the read/search/anchor work this bench exists to measure.
fn bench_grep_large_file(c: &mut Criterion) {
    let root = grep_large_fixture_root();
    let ws = Workspace::new(root.to_path_buf(), false);
    let mut group = c.benchmark_group("grep_large_file");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(6));

    for (label, pattern) in [
        ("rare_literal", GREP_RARE_TOKEN),
        ("common_literal", GREP_COMMON_TOKEN),
        ("anchored_regex", "^fn "),
    ] {
        let mut input = grep_input(pattern);
        input.path = Some(GREP_LARGE_FILE.to_owned());
        let probe = run_grep(&ws, &input);
        assert!(
            !probe.is_error,
            "wired large-file grep must succeed: {}",
            probe.text
        );
        assert!(
            probe.text.contains("matches="),
            "wired summary missing: {}",
            probe.text
        );
        group.bench_function(label, |b| {
            b.iter(|| {
                let outcome = run_grep(&ws, &input);
                assert!(!outcome.is_error, "wired large-file grep must succeed");
                black_box(outcome.text.len())
            });
        });

        let mut files_input = input.clone();
        files_input.output_mode = GrepOutputMode::FilesWithMatches;
        group.bench_function(format!("{label}_files"), |b| {
            b.iter(|| {
                let outcome = run_grep(&ws, &files_input);
                assert!(
                    !outcome.is_error,
                    "wired large-file files-mode grep must succeed"
                );
                black_box(outcome.text.len())
            });
        });
    }
    group.finish();
}

/// Byte offset of the 1-based logical `line` start within `content`.
fn line_start_offset(content: &str, line: u64) -> u64 {
    if line == 1 {
        return 0;
    }
    let newline_index = usize::try_from(line - 2).expect("bench line fits usize");
    let offset = memchr_iter(b'\n', content.as_bytes())
        .nth(newline_index)
        .expect("bench line within corpus")
        + 1;
    u64::try_from(offset).expect("bench offset fits u64")
}

/// Render the canonical `LINE@BYTE` boundary token for a 1-based line.
fn boundary_token(content: &str, line: u64) -> String {
    format!("{line}@{}", line_start_offset(content, line))
}

/// Process-seeded snapshot id (32-hex) for the exact bytes of `content`.
fn snapshot_id_hex(content: &str) -> String {
    Snapshot::from_bytes(content.as_bytes().to_vec())
        .expect("bench corpus is valid snapshot text")
        .id()
        .to_string()
}

/// Build edit-tool arguments replacing each 1-based line in `lines`.
fn replace_args(content: &str, path: &str, lines: &[u64]) -> serde_json::Value {
    let edits: Vec<serde_json::Value> = lines
        .iter()
        .enumerate()
        .map(|(index, &line)| {
            serde_json::json!({
                "op": "replace",
                "start": boundary_token(content, line),
                "end": boundary_token(content, line + 1),
                "content": format!("EDITED LINE {index}\n"),
            })
        })
        .collect();
    serde_json::json!({
        "file_path": path,
        "snapshot": snapshot_id_hex(content),
        "edits": edits,
    })
}

/// First text block of a dispatch result, success or error.
fn tool_text(result: &CallToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text(text)) => &text.text,
        other => panic!("dispatch returned non-text content: {other:?}"),
    }
}

/// Panic unless the dispatch outcome is a tool success; return its text.
///
/// Every wired-path bench asserts through this so a silent rejection can
/// never masquerade as a timing (plan Wave 0, AC26).
fn assert_dispatch_success(result: &CallToolResult) -> &str {
    assert_ne!(
        result.is_error,
        Some(true),
        "wired tool call failed: {result:?}"
    );
    tool_text(result)
}

/// Parse a non-terminal read page footer into (snapshot, position) tokens.
fn parse_cursor_footer(text: &str) -> Option<(String, String)> {
    let tail = &text[text.rfind('\n').map_or(0, |index| index + 1)..];
    let rest = tail.strip_prefix("[hashline next snapshot=")?;
    let (snapshot, rest) = rest.split_once(' ')?;
    let position = rest.strip_prefix("position=")?.strip_suffix(']')?;
    Some((snapshot.to_owned(), position.to_owned()))
}

/// Count rendered grep match lines (`LINE@BYTE:` grammar; context uses `-`).
fn count_grep_match_lines(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let Some((position, _)) = line.split_once(':') else {
                return false;
            };
            let Some((line_part, byte_part)) = position.split_once('@') else {
                return false;
            };
            !line_part.is_empty()
                && !byte_part.is_empty()
                && line_part.bytes().all(|byte| byte.is_ascii_digit())
                && byte_part.bytes().all(|byte| byte.is_ascii_digit())
        })
        .count()
}

/// End-to-end `HashlineServer::dispatch` bench on a realistic 300-line file:
/// the read path split into cold (snapshot-cache miss per iteration) and warm
/// (resident snapshot) states, plus a valid single-op edit. Every body
/// asserts tool success. Reset-dependent benches use
/// `BatchSize::PerIteration` because batched setup runs all resets before the
/// first timed call, which would let later iterations observe the previous
/// edit and silently measure the conflict path instead.
fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for dispatch bench");

    let tmp = tempfile::TempDir::new().expect("tempdir for dispatch bench");
    let content = generate_corpus(300, 0xD159_A7C0);
    let file_path = tmp.path().join("sample.rs");
    std::fs::write(&file_path, &content).expect("write dispatch fixture");
    // The server canonicalizes its root, so cache keys are canonical paths;
    // invalidation through the raw tempdir path (/var vs /private/var on
    // macOS) would silently no-op and time a warm read as "cold".
    let file_path = file_path
        .canonicalize()
        .expect("canonicalize dispatch fixture");

    let server = HashlineServer::new(tmp.path().to_path_buf());
    let barrier_server = HashlineServer::new(tmp.path().to_path_buf())
        .with_durability(hashline::persist::Durability::Barrier);

    let mut group = c.benchmark_group("dispatch");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(5));

    let read_args = serde_json::json!({"path": "sample.rs"});
    group.bench_function("read_300_lines", |b| {
        b.to_async(&rt).iter(|| {
            let args = read_args.clone();
            let server = &server;
            async move {
                let result = server.dispatch("read", args).await.expect("read dispatch");
                black_box(assert_dispatch_success(&result).len())
            }
        });
    });

    group.bench_function("read_300_lines_cold", |b| {
        b.to_async(&rt).iter_batched(
            || cache::process_cache().invalidate(&file_path),
            |()| {
                let args = read_args.clone();
                let server = &server;
                async move {
                    let result = server.dispatch("read", args).await.expect("read dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    let edit_args = replace_args(&content, "sample.rs", &[150]);
    group.bench_function("edit_single_op_300_lines", |b| {
        b.to_async(&rt).iter_batched(
            || std::fs::write(&file_path, &content).expect("reset dispatch fixture"),
            |()| {
                let args = edit_args.clone();
                let server = &server;
                async move {
                    let result = server.dispatch("edit", args).await.expect("edit dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    // Same edit under the barrier durability policy (R019 table): identical
    // ordering guarantees, no full fsync of temp file and parent directory.
    group.bench_function("edit_single_op_300_lines_barrier", |b| {
        b.to_async(&rt).iter_batched(
            || std::fs::write(&file_path, &content).expect("reset dispatch fixture"),
            |()| {
                let args = edit_args.clone();
                let server = &barrier_server;
                async move {
                    let result = server.dispatch("edit", args).await.expect("edit dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

/// Benchmark seed used only to compare raw version functions under identical input.
const PHASE2_VERSION_BENCH_SEED: u64 = 0x8ca7_4f91_2d63_b5e0;

#[derive(Debug)]
struct SnapshotValidationProbe {
    text: String,
    version: u128,
    line_count: usize,
    byte_len: usize,
}

fn assert_validation_probe_policy(bytes: &[u8]) {
    assert!(
        memchr::memchr(0, bytes).is_none(),
        "benchmark corpus must be NUL-free"
    );
}

fn finish_validation_probe(text: String) -> SnapshotValidationProbe {
    let version = xxhash_rust::xxh3::xxh3_128_with_seed(text.as_bytes(), PHASE2_VERSION_BENCH_SEED);
    let line_count = memchr_iter(b'\n', text.as_bytes()).count() + 1;
    let byte_len = text.len();
    SnapshotValidationProbe {
        text,
        version,
        line_count,
        byte_len,
    }
}

fn safe_snapshot_probe(bytes: Vec<u8>) -> SnapshotValidationProbe {
    assert_validation_probe_policy(&bytes);
    let text = String::from_utf8(bytes).expect("benchmark corpus must be valid UTF-8");
    finish_validation_probe(text)
}

fn unsafe_snapshot_probe(bytes: Vec<u8>) -> SnapshotValidationProbe {
    assert_validation_probe_policy(&bytes);
    simdutf8::compat::from_utf8(&bytes).expect("benchmark corpus must be valid UTF-8");
    // SAFETY: simdutf8 validated this exact owned buffer above, and no mutation
    // occurs between validation and ownership transfer.
    let text = unsafe { String::from_utf8_unchecked(bytes) };
    finish_validation_probe(text)
}

#[derive(Debug)]
struct SparseCheckpoints {
    interval: usize,
    checkpoints: Vec<u32>,
}

impl SparseCheckpoints {
    fn new(content: &str, interval: usize) -> Self {
        let mut checkpoints =
            Vec::with_capacity(phase0_workloads::logical_line_count(content) / interval + 1);
        checkpoints.push(0);
        for (line, newline) in memchr_iter(b'\n', content.as_bytes()).enumerate() {
            let next_line = line + 1;
            if next_line % interval == 0 {
                checkpoints.push(
                    u32::try_from(newline + 1).expect("Phase 2 benchmark corpus is below 4 GiB"),
                );
            }
        }
        Self {
            interval,
            checkpoints,
        }
    }

    fn select_window(&self, content: &str, start_line: usize, count: usize) -> Vec<u32> {
        if count == 0 {
            return Vec::new();
        }
        let checkpoint_index = start_line / self.interval;
        let checkpoint_line = checkpoint_index * self.interval;
        let mut position =
            usize::try_from(self.checkpoints[checkpoint_index]).expect("u32 checkpoint fits usize");
        let mut lines_to_skip = start_line - checkpoint_line;
        if lines_to_skip > 0 {
            for newline in memchr_iter(b'\n', &content.as_bytes()[position..]) {
                position += newline + 1;
                lines_to_skip -= 1;
                if lines_to_skip == 0 {
                    break;
                }
            }
        }

        let mut starts = Vec::with_capacity(count);
        starts.push(u32::try_from(position).expect("benchmark offset fits u32"));
        for newline in memchr_iter(b'\n', &content.as_bytes()[position..]).take(count - 1) {
            starts.push(u32::try_from(position + newline + 1).expect("benchmark offset fits u32"));
        }
        starts
    }

    fn resident_bytes(&self) -> usize {
        self.checkpoints.len() * std::mem::size_of::<u32>()
    }
}

const RANK_SELECT_WORD_BITS: usize = 64;
const RANK_SELECT_WORDS_PER_SUPERBLOCK: usize = 8;

#[derive(Debug)]
struct RankSelectBitmap {
    words: Vec<u64>,
    superblocks: Vec<u32>,
    line_count: usize,
}

impl RankSelectBitmap {
    fn new(content: &str) -> Self {
        let word_count = content.len() / RANK_SELECT_WORD_BITS + 1;
        let mut words = vec![0_u64; word_count];
        Self::set(&mut words, 0);
        let mut line_count = 1;
        for newline in memchr_iter(b'\n', content.as_bytes()) {
            Self::set(&mut words, newline + 1);
            line_count += 1;
        }

        let mut superblocks =
            Vec::with_capacity(words.len() / RANK_SELECT_WORDS_PER_SUPERBLOCK + 1);
        let mut rank = 0_u32;
        for block in words.chunks(RANK_SELECT_WORDS_PER_SUPERBLOCK) {
            superblocks.push(rank);
            let block_ones = block.iter().map(|word| word.count_ones()).sum::<u32>();
            rank = rank
                .checked_add(block_ones)
                .expect("benchmark line count fits u32");
        }
        Self {
            words,
            superblocks,
            line_count,
        }
    }

    fn set(words: &mut [u64], position: usize) {
        let word = position / RANK_SELECT_WORD_BITS;
        let bit = position % RANK_SELECT_WORD_BITS;
        words[word] |= 1_u64 << bit;
    }

    fn select(&self, line: usize) -> Option<usize> {
        if line >= self.line_count {
            return None;
        }
        let line_u32 = u32::try_from(line).ok()?;
        let after = self.superblocks.partition_point(|rank| *rank <= line_u32);
        let superblock = after.saturating_sub(1);
        let mut remaining = line_u32 - self.superblocks[superblock];
        let first_word = superblock * RANK_SELECT_WORDS_PER_SUPERBLOCK;
        let last_word = (first_word + RANK_SELECT_WORDS_PER_SUPERBLOCK).min(self.words.len());
        for (relative, &word) in self.words[first_word..last_word].iter().enumerate() {
            let ones = word.count_ones();
            if remaining >= ones {
                remaining -= ones;
                continue;
            }
            let mut selected = word;
            for _ in 0..remaining {
                selected &= selected - 1;
            }
            let bit =
                usize::try_from(selected.trailing_zeros()).expect("trailing-zero count fits usize");
            return Some((first_word + relative) * RANK_SELECT_WORD_BITS + bit);
        }
        None
    }

    fn select_window(&self, start_line: usize, count: usize) -> Vec<u32> {
        (start_line..start_line + count)
            .filter_map(|line| self.select(line))
            .map(|offset| u32::try_from(offset).expect("benchmark offset fits u32"))
            .collect()
    }

    fn resident_bytes(&self) -> usize {
        self.words.len() * std::mem::size_of::<u64>()
            + self.superblocks.len() * std::mem::size_of::<u32>()
    }
}

fn full_u32_window(content: &str, start_line: usize, count: usize) -> Vec<u32> {
    let offsets = phase0_workloads::offsets_u32(content);
    offsets[start_line..start_line + count].to_vec()
}

fn full_u64_window(content: &str, start_line: usize, count: usize) -> Vec<u64> {
    let offsets = phase0_workloads::offsets_u64(content);
    offsets[start_line..start_line + count].to_vec()
}

fn bench_phase2_snapshot(c: &mut Criterion) {
    let content_10k = generate_corpus(10_000, 0xB200_0010);
    let content_50k = generate_corpus(50_000, 0xB200_0050);

    for (size, content) in [(10_000usize, &content_10k), (50_000, &content_50k)] {
        let mut group = c.benchmark_group(format!("phase2_snapshot/{size}"));
        group.sample_size(30);
        group.measurement_time(Duration::from_secs(3));
        group.bench_function("candidate_snapshot", |b| {
            b.iter_batched(
                || content.as_bytes().to_vec(),
                |bytes| {
                    let snapshot =
                        Snapshot::from_bytes(black_box(bytes)).expect("valid benchmark snapshot");
                    black_box((snapshot.id(), snapshot.line_count(), snapshot.byte_len()))
                },
                BatchSize::LargeInput,
            );
        });
        group.finish();

        let mut validation = c.benchmark_group(format!("phase2_validation/{size}"));
        validation.sample_size(30);
        validation.measurement_time(Duration::from_secs(3));
        validation.bench_function("safe_snapshot", |b| {
            b.iter_batched(
                || content.as_bytes().to_vec(),
                |bytes| {
                    let probe = safe_snapshot_probe(black_box(bytes));
                    black_box((probe.text, probe.version, probe.line_count, probe.byte_len))
                },
                BatchSize::LargeInput,
            );
        });
        validation.bench_function("simd_validated_unchecked_snapshot", |b| {
            b.iter_batched(
                || content.as_bytes().to_vec(),
                |bytes| {
                    let probe = unsafe_snapshot_probe(black_box(bytes));
                    black_box((probe.text, probe.version, probe.line_count, probe.byte_len))
                },
                BatchSize::LargeInput,
            );
        });
        validation.finish();
    }
}

fn bench_phase2_version_matrix(c: &mut Criterion) {
    let short = "    let value_42 = compute(123, next_state);\n";
    let multimegabyte = generate_corpus(50_000, 0xB200_0050);

    for (label, content) in [("short", short), ("multimegabyte", multimegabyte.as_str())] {
        let mut group = c.benchmark_group(format!("phase2_version/{label}"));
        group.sample_size(30);
        group.measurement_time(Duration::from_secs(3));

        group.bench_function("xxh3_128_with_seed", |b| {
            b.iter(|| {
                black_box(xxhash_rust::xxh3::xxh3_128_with_seed(
                    black_box(content.as_bytes()),
                    PHASE2_VERSION_BENCH_SEED,
                ))
            });
        });
        group.bench_function("blake3_128", |b| {
            b.iter(|| {
                let digest = blake3::hash(black_box(content.as_bytes()));
                let prefix: [u8; 16] = digest.as_bytes()[..16]
                    .try_into()
                    .expect("BLAKE3 digest prefix is exactly 16 bytes");
                black_box(prefix)
            });
        });
        group.finish();
    }
}

fn bench_phase2_offsets(c: &mut Criterion) {
    let content_50k = generate_corpus(50_000, 0xB200_0050);
    let content_100k = generate_corpus(100_000, 0xB200_0100);

    let mut construction = c.benchmark_group("phase2_offsets/construction_50k");
    construction.sample_size(30);
    construction.measurement_time(Duration::from_secs(3));
    construction.bench_function("full_u32", |b| {
        b.iter(|| black_box(phase0_workloads::offsets_u32(black_box(&content_50k))));
    });
    construction.bench_function("full_u64", |b| {
        b.iter(|| black_box(phase0_workloads::offsets_u64(black_box(&content_50k))));
    });
    for interval in [128usize, 256, 512] {
        construction.bench_function(format!("sparse_{interval}"), |b| {
            b.iter(|| {
                let checkpoints = SparseCheckpoints::new(black_box(&content_50k), interval);
                black_box((checkpoints.resident_bytes(), checkpoints))
            });
        });
    }
    construction.bench_function("rank_select_bitmap", |b| {
        b.iter(|| {
            let bitmap = RankSelectBitmap::new(black_box(&content_50k));
            black_box((bitmap.resident_bytes(), bitmap))
        });
    });
    construction.finish();

    const START_LINE: usize = 50_000;
    const WINDOW_LINES: usize = 2_000;
    let mut cold = c.benchmark_group("phase2_offsets/cold_window_2k_of_100k");
    cold.sample_size(30);
    cold.measurement_time(Duration::from_secs(3));
    cold.bench_function("full_u32", |b| {
        b.iter(|| {
            black_box(full_u32_window(
                black_box(&content_100k),
                START_LINE,
                WINDOW_LINES,
            ))
        });
    });
    cold.bench_function("full_u64", |b| {
        b.iter(|| {
            black_box(full_u64_window(
                black_box(&content_100k),
                START_LINE,
                WINDOW_LINES,
            ))
        });
    });
    for interval in [128usize, 256, 512] {
        cold.bench_function(format!("sparse_{interval}"), |b| {
            b.iter(|| {
                let checkpoints = SparseCheckpoints::new(black_box(&content_100k), interval);
                black_box(checkpoints.select_window(&content_100k, START_LINE, WINDOW_LINES))
            });
        });
    }
    cold.bench_function("rank_select_bitmap", |b| {
        b.iter(|| {
            let bitmap = RankSelectBitmap::new(black_box(&content_100k));
            black_box(bitmap.select_window(START_LINE, WINDOW_LINES))
        });
    });
    cold.finish();
}

/// Wave 0 wired-path read benches through `HashlineServer::dispatch`: full
/// pagination of a 10k-line file (warm), the 2k window of a 100k-line file in
/// explicit cold and warm snapshot-cache states, and page 2 of a 50k-line
/// file via the cursor returned by page 1 — the pagination-cost bench the
/// tree previously lacked. Cold state is forced by evicting the fixture from
/// the process snapshot cache before every timed iteration.
fn bench_wired_read(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for wired read bench");

    let tmp = tempfile::TempDir::new().expect("tempdir for wired read bench");
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let content_10k = generate_corpus(10_000, 0x0A11_C0DE);
    std::fs::write(tmp.path().join("ten_k.rs"), &content_10k).expect("write 10k fixture");
    let content_100k = generate_corpus(100_000, 0xC0FF_EE00);
    let path_100k = tmp.path().join("hundred_k.rs");
    std::fs::write(&path_100k, &content_100k).expect("write 100k fixture");
    // Canonicalize so per-iteration invalidation hits the same key the
    // canonicalized workspace root produces (cache keys are canonical paths).
    let path_100k = path_100k.canonicalize().expect("canonicalize 100k fixture");
    let content_50k = generate_corpus(50_000, 0x50C0_5001);
    std::fs::write(tmp.path().join("fifty_k.rs"), &content_50k).expect("write 50k fixture");

    let mut group = c.benchmark_group("wired_read");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    // Walk the pagination chain once so the timed loop replays five
    // precomputed requests against a warm snapshot cache.
    let full_10k_pages = rt.block_on(async {
        let mut pages = Vec::new();
        let mut args = serde_json::json!({"path": "ten_k.rs"});
        loop {
            pages.push(args.clone());
            let result = server.dispatch("read", args).await.expect("read dispatch");
            match parse_cursor_footer(assert_dispatch_success(&result)) {
                Some((snapshot, position)) => {
                    args = serde_json::json!({
                        "path": "ten_k.rs",
                        "cursor": {"snapshot": snapshot, "next": position},
                    });
                }
                None => break,
            }
        }
        pages
    });
    // A terminated file has one trailing empty logical line, so 10k physical
    // lines are 10,001 logical lines -> six pages, not five.
    let logical_lines = memchr_iter(b'\n', content_10k.as_bytes()).count() as u64 + 1;
    let expected_pages = logical_lines.div_ceil(2_000);
    assert_eq!(
        full_10k_pages.len() as u64,
        expected_pages,
        "pagination covers every logical line exactly once"
    );

    group.bench_function("full_10k", |b| {
        b.to_async(&rt).iter(|| {
            let pages = full_10k_pages.clone();
            let server = &server;
            async move {
                let mut total = 0usize;
                for args in pages {
                    let result = server.dispatch("read", args).await.expect("read dispatch");
                    total += assert_dispatch_success(&result).len();
                }
                black_box(total)
            }
        });
    });

    let window_args = serde_json::json!({"path": "hundred_k.rs", "limit": 2_000});
    rt.block_on(async {
        let result = server
            .dispatch("read", window_args.clone())
            .await
            .expect("read dispatch");
        let text = assert_dispatch_success(&result);
        assert!(text.starts_with("[hashline snapshot="), "{text}");
    });

    group.bench_function("window_2k_of_100k_warm", |b| {
        b.to_async(&rt).iter(|| {
            let args = window_args.clone();
            let server = &server;
            async move {
                let result = server.dispatch("read", args).await.expect("read dispatch");
                black_box(assert_dispatch_success(&result).len())
            }
        });
    });

    group.bench_function("window_2k_of_100k_cold", |b| {
        b.to_async(&rt).iter_batched(
            || cache::process_cache().invalidate(&path_100k),
            |()| {
                let args = window_args.clone();
                let server = &server;
                async move {
                    let result = server.dispatch("read", args).await.expect("read dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    let cursor_args = rt.block_on(async {
        let result = server
            .dispatch(
                "read",
                serde_json::json!({"path": "fifty_k.rs", "limit": 2_000}),
            )
            .await
            .expect("read dispatch");
        let (snapshot, position) = parse_cursor_footer(assert_dispatch_success(&result))
            .expect("50k first page returns a continuation cursor");
        serde_json::json!({
            "path": "fifty_k.rs",
            "limit": 2_000,
            "cursor": {"snapshot": snapshot, "next": position},
        })
    });

    group.bench_function("cursor_page_50k", |b| {
        b.to_async(&rt).iter(|| {
            let args = cursor_args.clone();
            let server = &server;
            async move {
                let result = server.dispatch("read", args).await.expect("read dispatch");
                black_box(assert_dispatch_success(&result).len())
            }
        });
    });

    group.finish();
}

/// Wired-path edit benches on a 50k-line file. CPU-apply variants call
/// `edit::apply_edits_fast` over a per-iteration snapshot — the production
/// engine `edit::run` dispatches to — and the e2e variants dispatch real
/// edits with a per-iteration file reset. The `_full` suffix records that
/// the default durability policy fsyncs temp file and parent directory.
fn bench_wired_edit(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for wired edit bench");

    let tmp = tempfile::TempDir::new().expect("tempdir for wired edit bench");
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let content = generate_corpus(50_000, 0xED17_0002);
    let file_path = tmp.path().join("editable.rs");
    std::fs::write(&file_path, &content).expect("write editable fixture");

    let single_args = replace_args(&content, "editable.rs", &[25_000]);
    let batch_lines: Vec<u64> = (1..=8).map(|op| op * 6_000).collect();
    let batch_args = replace_args(&content, "editable.rs", &batch_lines);

    let single_request: EditRequest =
        serde_json::from_value(single_args.clone()).expect("single edit request deserializes");
    let batch_request: EditRequest =
        serde_json::from_value(batch_args.clone()).expect("batch edit request deserializes");

    let mut group = c.benchmark_group("wired_edit");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    // Production apply path (engine over snapshot offsets). A fresh Snapshot
    // per iteration mirrors the wired shape: every edit call loads its own
    // snapshot, so first-touch offset materialization is part of apply cost.
    group.bench_function("single_op_50k_apply", |b| {
        b.iter_batched(
            || Snapshot::from_bytes(content.clone().into_bytes()).expect("apply fixture snapshot"),
            |snapshot| {
                let applied = hashline::edit::apply_edits_fast(&snapshot, &single_request)
                    .expect("wired single-op apply succeeds");
                black_box(applied.len())
            },
            BatchSize::PerIteration,
        );
    });

    group.bench_function("batch_8ops_50k_apply", |b| {
        b.iter_batched(
            || Snapshot::from_bytes(content.clone().into_bytes()).expect("apply fixture snapshot"),
            |snapshot| {
                let applied = hashline::edit::apply_edits_fast(&snapshot, &batch_request)
                    .expect("wired batch apply succeeds");
                black_box(applied.len())
            },
            BatchSize::PerIteration,
        );
    });

    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(6));

    group.bench_function("single_op_50k_e2e_full", |b| {
        b.to_async(&rt).iter_batched(
            || std::fs::write(&file_path, &content).expect("reset editable fixture"),
            |()| {
                let args = single_args.clone();
                let server = &server;
                async move {
                    let result = server.dispatch("edit", args).await.expect("edit dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.bench_function("batch_8ops_50k_e2e_full", |b| {
        b.to_async(&rt).iter_batched(
            || std::fs::write(&file_path, &content).expect("reset editable fixture"),
            |()| {
                let args = batch_args.clone();
                let server = &server;
                async move {
                    let result = server.dispatch("edit", args).await.expect("edit dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));

    // Version-conflict path: a stale snapshot id must produce the structured
    // conflict and leave the file untouched, so no per-iteration reset.
    std::fs::write(&file_path, &content).expect("reset editable fixture");
    let mut stale_source = content.clone();
    stale_source.push_str("stale marker\n");
    let stale_args = replace_args(&stale_source, "editable.rs", &[25_000]);
    group.bench_function("conflict_50k", |b| {
        b.to_async(&rt).iter(|| {
            let args = stale_args.clone();
            let server = &server;
            async move {
                let result = server.dispatch("edit", args).await.expect("edit dispatch");
                assert_eq!(result.is_error, Some(true), "stale snapshot must conflict");
                let text = tool_text(&result);
                assert!(
                    text.contains("snapshot_conflict"),
                    "structured conflict expected: {text}"
                );
                black_box(text.len())
            }
        });
    });

    group.finish();
}

/// Wave 0 wired-path grep bench: a dense single file where every line
/// matches, capped at the protocol maximum. Setup asserts the AC8/AC24
/// contract once (exactly `max_matches` rendered match lines plus the
/// truncated summary); every timed iteration re-asserts the summary suffix.
fn bench_wired_grep(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for wired grep bench");

    let tmp = tempfile::TempDir::new().expect("tempdir for wired grep bench");
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let dense: String = (0..10_000)
        .map(|line| format!("let needle_hit_{line} = {line};\n"))
        .collect();
    std::fs::write(tmp.path().join("dense.rs"), &dense).expect("write dense fixture");

    let args = serde_json::json!({"pattern": "needle_hit", "path": "dense.rs", "max_matches": 200});
    // R015: the match budget is checked between files, so a single dense file
    // renders every match and reports matches=10000 truncated=false. Setup
    // pins the wired response to itself so any drift fails closed.
    let expected_summary = rt.block_on(async {
        let result = server
            .dispatch("grep", args.clone())
            .await
            .expect("grep dispatch");
        let text = assert_dispatch_success(&result);
        let rendered = count_grep_match_lines(text);
        let summary = &text[text.rfind('\n').map_or(0, |index| index + 1)..];
        assert!(
            summary.starts_with(&format!("[hashline matches={rendered} ")),
            "summary counter must equal rendered match lines: rendered={rendered} {summary}"
        );
        summary.to_owned()
    });

    let mut group = c.benchmark_group("wired_grep");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("dense_file_capped", |b| {
        b.to_async(&rt).iter(|| {
            let args = args.clone();
            let server = &server;
            let expected = &expected_summary;
            async move {
                let result = server.dispatch("grep", args).await.expect("grep dispatch");
                let text = assert_dispatch_success(&result);
                assert!(text.ends_with(expected.as_str()), "summary mismatch");
                black_box(text.len())
            }
        });
    });

    group.finish();
}

/// Wired-path write benches: exclusive create with a per-iteration unlink,
/// versioned replace with a per-iteration reset, and the already_exists
/// fail-closed path, which needs no reset because the loser never mutates.
fn bench_wired_write(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for wired write bench");

    let tmp = tempfile::TempDir::new().expect("tempdir for wired write bench");
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let content = generate_corpus(10_000, 0x0B1E_55ED);
    let create_path = tmp.path().join("created.rs");
    let create_args = serde_json::json!({
        "file_path": "created.rs",
        "content": content,
        "expect": "absent",
    });

    // Fail-closed contract once in setup: the create succeeds, a repeat
    // reports already_exists, and the destination keeps the winner's bytes.
    rt.block_on(async {
        let result = server
            .dispatch("write", create_args.clone())
            .await
            .expect("write dispatch");
        let text = assert_dispatch_success(&result);
        assert!(text.contains("\"created\": true"), "{text}");
        let repeat = server
            .dispatch("write", create_args.clone())
            .await
            .expect("write dispatch");
        assert_eq!(
            repeat.is_error,
            Some(true),
            "repeat create must fail closed"
        );
        let repeat_text = tool_text(&repeat);
        assert!(
            repeat_text.contains("already_exists"),
            "structured already_exists expected: {repeat_text}"
        );
    });

    let mut group = c.benchmark_group("wired_write");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(6));

    group.bench_function("create_10k_e2e_full", |b| {
        b.to_async(&rt).iter_batched(
            || {
                std::fs::remove_file(&create_path).expect("unlink created fixture");
                cache::process_cache().invalidate(&create_path);
            },
            |()| {
                let args = create_args.clone();
                let server = &server;
                async move {
                    let result = server
                        .dispatch("write", args)
                        .await
                        .expect("write dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    let replace_path = tmp.path().join("replaced.rs");
    std::fs::write(&replace_path, &content).expect("write replace fixture");
    let fixture_id = Snapshot::from_bytes(content.as_bytes().to_vec())
        .expect("replace fixture snapshot")
        .id();
    let mut replaced_content = content.clone();
    replaced_content.push_str("replaced tail\n");
    let replace_args = serde_json::json!({
        "file_path": "replaced.rs",
        "content": replaced_content,
        "expect": fixture_id.to_string(),
    });

    group.bench_function("replace_10k_e2e_full", |b| {
        b.to_async(&rt).iter_batched(
            || std::fs::write(&replace_path, &content).expect("reset replace fixture"),
            |()| {
                let args = replace_args.clone();
                let server = &server;
                async move {
                    let result = server
                        .dispatch("write", args)
                        .await
                        .expect("write dispatch");
                    black_box(assert_dispatch_success(&result).len())
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(2));
    group.measurement_time(Duration::from_secs(5));

    // Exclusive-create conflict path: the destination exists, so every
    // iteration fails closed without touching it — no reset required.
    std::fs::write(&create_path, "occupant\n").expect("reset conflict fixture");
    group.bench_function("create_conflict", |b| {
        b.to_async(&rt).iter(|| {
            let args = create_args.clone();
            let server = &server;
            async move {
                let result = server
                    .dispatch("write", args)
                    .await
                    .expect("write dispatch");
                assert_eq!(
                    result.is_error,
                    Some(true),
                    "existing file must fail closed"
                );
                let text = tool_text(&result);
                assert!(
                    text.contains("already_exists"),
                    "structured already_exists expected: {text}"
                );
                black_box(text.len())
            }
        });
    });

    group.finish();
}

/// Wired-path glob bench over the shared ~2,000-file grep fixture tree:
/// recursive discovery with deterministic newest-first ordering. Setup pins
/// the summary once and every timed iteration re-asserts it so silent
/// truncation or ordering drift fails closed.
fn bench_wired_glob(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for wired glob bench");

    let root = grep_fixture_root();
    let server = HashlineServer::new(root.to_path_buf());

    let args = serde_json::json!({"pattern": "**/*.rs", "max_results": 1000});
    let expected_summary = rt.block_on(async {
        let result = server
            .dispatch("glob", args.clone())
            .await
            .expect("glob dispatch");
        let text = assert_dispatch_success(&result);
        let summary = &text[text.rfind('\n').map_or(0, |index| index + 1)..];
        assert!(
            summary.starts_with("[hashline files="),
            "glob summary expected: {summary}"
        );
        summary.to_owned()
    });

    let mut group = c.benchmark_group("wired_glob");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("tree_recursive_rs", |b| {
        b.to_async(&rt).iter(|| {
            let args = args.clone();
            let server = &server;
            let expected = &expected_summary;
            async move {
                let result = server.dispatch("glob", args).await.expect("glob dispatch");
                let text = assert_dispatch_success(&result);
                assert!(text.ends_with(expected.as_str()), "summary mismatch");
                black_box(text.len())
            }
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_grep,
    bench_grep_large_file,
    bench_phase2_snapshot,
    bench_phase2_version_matrix,
    bench_phase2_offsets,
    bench_dispatch,
    bench_wired_read,
    bench_wired_edit,
    bench_wired_grep,
    bench_wired_write,
    bench_wired_glob,
);
criterion_main!(benches);
