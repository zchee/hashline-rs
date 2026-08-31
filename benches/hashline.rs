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
//! Divan benchmarks for hashline's hot paths.
//!
//! These are the Phase 0 baselines for the max-performance optimization plan
//! (`.omc/plans/2026-07-30-max-performance-optimization.md`): every later
//! phase is measured against the numbers this harness records in
//! `benches/BASELINE.md`. Corpus generation is fully deterministic (a small
//! inline xorshift32 PRNG, no `rand` and no system time) so results are
//! reproducible across runs and machines.
//!
//! The `divan` dependency is [`codspeed-divan-compat`], a drop-in replacement
//! that keeps `cargo bench` on the stock Divan walltime harness while letting
//! `cargo codspeed run` measure the same benchmark bodies under CodSpeed's
//! instrumentation. The compat layer discards `sample_count` / `sample_size`
//! (it executes each body exactly once) but accepts them, so the two harnesses
//! share one source of truth.
//!
//! Every benchmark whose body mutates shared state — a file the next iteration
//! would observe as already edited, a snapshot cache entry that must be cold —
//! pins `sample_size = 1`. Divan generates a whole sample's inputs *before*
//! timing that sample, so any larger sample size would run every reset up
//! front and silently measure the conflict path instead.

use std::{
    hint::black_box,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, OnceLock},
};

use divan::Bencher;
use hashline::{
    HashlineServer, cache,
    grep::run_grep,
    persist::Durability,
    protocol::{EditRequest, GrepOutputMode, GrepRequest},
    snapshot::Snapshot,
    util::Workspace,
};
use memchr::memchr_iter;
use rmcp::model::{CallToolResult, ContentBlock};
use tempfile::TempDir;
use tokio::runtime::Runtime;

#[path = "support/phase0_workloads.rs"]
mod phase0_workloads;

use phase0_workloads::generate_corpus;

fn main() {
    divan::main();
    remove_fixture_trees();
}

/// Roots of every fixture tree created during this run.
///
/// Fixtures hang off `LazyLock`/`OnceLock` statics so a tree is built once no
/// matter how many benchmarks share it, and statics are never dropped — so
/// `TempDir`'s own cleanup never runs. `main` removes them explicitly instead.
static FIXTURE_TREES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Create a fixture tempdir and register its root for end-of-run cleanup.
fn fixture_dir() -> TempDir {
    let dir = TempDir::new().expect("bench fixture tempdir");
    FIXTURE_TREES
        .lock()
        .expect("fixture registry")
        .push(dir.path().to_path_buf());
    dir
}

/// Remove every registered fixture tree.
fn remove_fixture_trees() {
    for root in FIXTURE_TREES.lock().expect("fixture registry").drain(..) {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Shared runtime for the wired benches.
///
/// Divan has no async harness, so every wired body blocks on its future here.
/// `Runtime::block_on` costs the same constant in every variant and is orders
/// of magnitude below the dispatch work being measured.
static RUNTIME: LazyLock<Runtime> =
    LazyLock::new(|| Runtime::new().expect("tokio runtime for wired benches"));

/// Number of files in the synthetic grep fixture tree.
const GREP_FILE_COUNT: usize = 2_000;
/// Lines per file in the synthetic grep fixture tree.
const GREP_LINES_PER_FILE: usize = 40;
/// Literal present in exactly one file, for the rare-literal grep benchmark.
const GREP_RARE_TOKEN: &str = "zqxj7_rare_marker_unique";
/// Literal that occurs naturally across most files via the identifier pool,
/// for the common-literal grep benchmark.
const GREP_COMMON_TOKEN: &str = "value";
/// `^`-anchored regex exercising the non-literal matching path.
const GREP_ANCHORED_REGEX: &str = "^fn ";

/// Rendering modes every grep benchmark is run under.
const GREP_MODES: [&str; 2] = ["content", "files"];

/// Lazily-built synthetic fixture tree for grep benchmarks: ~2,000 small
/// code-like files across nested directories. Built once (via `OnceLock`)
/// regardless of how many benchmark iterations run against it.
static GREP_FIXTURE: OnceLock<(TempDir, Workspace)> = OnceLock::new();

/// Workspace over the (lazily-built) grep fixture tree.
fn grep_workspace() -> &'static Workspace {
    let (_tmp, workspace) = GREP_FIXTURE.get_or_init(|| {
        let tmp = fixture_dir();
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
        let workspace = Workspace::new(root, false);
        (tmp, workspace)
    });
    workspace
}

/// Root path of the grep fixture tree, for the glob bench that shares it.
fn grep_fixture_root() -> &'static Path {
    grep_workspace().root.as_path()
}

/// Build a `GrepRequest` for `pattern` rendered under `mode`.
fn grep_request(pattern: &str, mode: &str) -> GrepRequest {
    GrepRequest {
        pattern: pattern.to_owned(),
        path: None,
        glob: None,
        ignore_case: false,
        after_context: None,
        before_context: None,
        context: None,
        max_matches: 200,
        output_mode: match mode {
            "content" => GrepOutputMode::Content,
            "files" => GrepOutputMode::FilesWithMatches,
            other => panic!("unknown grep rendering mode: {other}"),
        },
    }
}

/// Assert the wired grep contract once, before any timing.
///
/// A silent rejection must never masquerade as a timing (plan Wave 0, AC26),
/// so the probe fails closed on an error outcome and, in content mode, on a
/// missing match summary.
fn probe_grep(workspace: &Workspace, request: &GrepRequest, mode: &str) {
    let probe = run_grep(workspace, request);
    assert!(!probe.is_error, "wired grep must succeed: {}", probe.text);
    if mode == "content" {
        assert!(
            probe.text.contains("matches="),
            "wired summary missing: {}",
            probe.text
        );
    }
}

/// Time `run_grep` over `request`, re-asserting success every iteration.
fn bench_grep(bencher: Bencher, workspace: &'static Workspace, request: GrepRequest) {
    bencher.bench_local(move || {
        let outcome = run_grep(workspace, &request);
        assert!(!outcome.is_error, "wired grep must succeed");
        black_box(outcome.text.len())
    });
}

/// `run_grep` over the ~2,000-file fixture: a rare literal, a common literal,
/// and a `^`-anchored regex, each in content and files-with-matches modes.
mod grep {
    use super::*;

    fn run(bencher: Bencher, pattern: &str, mode: &str) {
        let workspace = grep_workspace();
        let request = grep_request(pattern, mode);
        probe_grep(workspace, &request, mode);
        bench_grep(bencher, workspace, request);
    }

    #[divan::bench(args = GREP_MODES, sample_count = 20)]
    fn rare_literal(bencher: Bencher, mode: &str) {
        run(bencher, GREP_RARE_TOKEN, mode);
    }

    #[divan::bench(args = GREP_MODES, sample_count = 20)]
    fn common_literal(bencher: Bencher, mode: &str) {
        run(bencher, GREP_COMMON_TOKEN, mode);
    }

    #[divan::bench(args = GREP_MODES, sample_count = 20)]
    fn anchored_regex(bencher: Bencher, mode: &str) {
        run(bencher, GREP_ANCHORED_REGEX, mode);
    }
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
static GREP_LARGE_FIXTURE: OnceLock<(TempDir, Workspace)> = OnceLock::new();

/// Workspace over the (lazily-built) single-file grep fixture.
fn grep_large_workspace() -> &'static Workspace {
    let (_tmp, workspace) = GREP_LARGE_FIXTURE.get_or_init(|| {
        let tmp = fixture_dir();
        let root = tmp.path().to_path_buf();
        let mut content = generate_corpus(GREP_LARGE_LINES, 0x1A26_F11E);
        content.push_str(GREP_RARE_TOKEN);
        content.push('\n');
        std::fs::write(root.join(GREP_LARGE_FILE), &content).expect("write large grep fixture");
        let workspace = Workspace::new(root, false);
        (tmp, workspace)
    });
    workspace
}

/// `run_grep` over the single 50,000-line file: a rare literal, a common
/// literal, and a `^`-anchored regex.
///
/// This is acceptance criterion 4(b)'s match-bound bench. The search path is
/// the file itself, which takes `run_grep`'s single-file short circuit — the
/// directory walker's thread-pool startup is several milliseconds and would
/// bury exactly the read/search/anchor work this bench exists to measure.
mod grep_large_file {
    use super::*;

    fn run(bencher: Bencher, pattern: &str, mode: &str) {
        let workspace = grep_large_workspace();
        let mut request = grep_request(pattern, mode);
        request.path = Some(GREP_LARGE_FILE.to_owned());
        probe_grep(workspace, &request, mode);
        bench_grep(bencher, workspace, request);
    }

    #[divan::bench(args = GREP_MODES, sample_count = 30)]
    fn rare_literal(bencher: Bencher, mode: &str) {
        run(bencher, GREP_RARE_TOKEN, mode);
    }

    #[divan::bench(args = GREP_MODES, sample_count = 30)]
    fn common_literal(bencher: Bencher, mode: &str) {
        run(bencher, GREP_COMMON_TOKEN, mode);
    }

    #[divan::bench(args = GREP_MODES, sample_count = 30)]
    fn anchored_regex(bencher: Bencher, mode: &str) {
        run(bencher, GREP_ANCHORED_REGEX, mode);
    }
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

/// Dispatch one tool call on the shared runtime and return its rendered text
/// length, asserting success.
fn dispatch_len(server: &HashlineServer, tool: &str, args: serde_json::Value) -> usize {
    RUNTIME.block_on(async {
        let result = server
            .dispatch(tool, args)
            .await
            .unwrap_or_else(|error| panic!("{tool} dispatch: {error:?}"));
        assert_dispatch_success(&result).len()
    })
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

/// End-to-end `HashlineServer::dispatch` fixture on realistic 300-line files.
///
/// Read and edit get their own files so the benchmarks stay independent of the
/// order Divan happens to register them in.
struct DispatchFixture {
    _tmp: TempDir,
    server: HashlineServer,
    barrier_server: HashlineServer,
    read_path: PathBuf,
    read_args: serde_json::Value,
    content: String,
    edit_path: PathBuf,
    edit_args: serde_json::Value,
    barrier_path: PathBuf,
    barrier_args: serde_json::Value,
}

static DISPATCH_FIXTURE: LazyLock<DispatchFixture> = LazyLock::new(|| {
    let tmp = fixture_dir();
    let content = generate_corpus(300, 0xD159_A7C0);

    // The server canonicalizes its root, so cache keys are canonical paths;
    // invalidation through the raw tempdir path (/var vs /private/var on
    // macOS) would silently no-op and time a warm read as "cold".
    let write_fixture = |name: &str| {
        let path = tmp.path().join(name);
        std::fs::write(&path, &content).expect("write dispatch fixture");
        path.canonicalize().expect("canonicalize dispatch fixture")
    };

    let read_path = write_fixture("read.rs");
    let edit_path = write_fixture("edit.rs");
    let barrier_path = write_fixture("edit_barrier.rs");

    DispatchFixture {
        server: HashlineServer::new(tmp.path().to_path_buf()),
        barrier_server: HashlineServer::new(tmp.path().to_path_buf())
            .with_durability(Durability::Barrier),
        read_path,
        read_args: serde_json::json!({"path": "read.rs"}),
        edit_args: replace_args(&content, "edit.rs", &[150]),
        barrier_args: replace_args(&content, "edit_barrier.rs", &[150]),
        edit_path,
        barrier_path,
        content,
        _tmp: tmp,
    }
});

/// Wired `dispatch` benches on a 300-line file: the read path split into cold
/// (snapshot-cache miss per iteration) and warm (resident snapshot) states,
/// plus a valid single-op edit under both durability policies.
mod dispatch {
    use super::*;

    #[divan::bench(sample_count = 30)]
    fn read_300_lines(bencher: Bencher) {
        let fixture = &*DISPATCH_FIXTURE;
        bencher.bench_local(|| {
            black_box(dispatch_len(
                &fixture.server,
                "read",
                fixture.read_args.clone(),
            ))
        });
    }

    #[divan::bench(sample_size = 1, sample_count = 30)]
    fn read_300_lines_cold(bencher: Bencher) {
        let fixture = &*DISPATCH_FIXTURE;
        bencher
            .with_inputs(|| cache::process_cache().invalidate(&fixture.read_path))
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "read",
                    fixture.read_args.clone(),
                ))
            });
    }

    #[divan::bench(sample_size = 1, sample_count = 30)]
    fn edit_single_op_300_lines(bencher: Bencher) {
        let fixture = &*DISPATCH_FIXTURE;
        bencher
            .with_inputs(|| {
                std::fs::write(&fixture.edit_path, &fixture.content)
                    .expect("reset dispatch fixture");
            })
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "edit",
                    fixture.edit_args.clone(),
                ))
            });
    }

    /// The same edit under the barrier durability policy (R019 table):
    /// identical ordering guarantees, no full fsync of temp file and parent
    /// directory.
    #[divan::bench(sample_size = 1, sample_count = 30)]
    fn edit_single_op_300_lines_barrier(bencher: Bencher) {
        let fixture = &*DISPATCH_FIXTURE;
        bencher
            .with_inputs(|| {
                std::fs::write(&fixture.barrier_path, &fixture.content)
                    .expect("reset dispatch fixture");
            })
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.barrier_server,
                    "edit",
                    fixture.barrier_args.clone(),
                ))
            });
    }
}

/// Benchmark seed used only to compare raw version functions under identical input.
const PHASE2_VERSION_BENCH_SEED: u64 = 0x8ca7_4f91_2d63_b5e0;

/// Corpus sizes the Phase 2 snapshot and validation benches sweep.
const PHASE2_SIZES: [usize; 2] = [10_000, 50_000];

static CORPUS_10K: LazyLock<String> = LazyLock::new(|| generate_corpus(10_000, 0xB200_0010));
static CORPUS_50K: LazyLock<String> = LazyLock::new(|| generate_corpus(50_000, 0xB200_0050));
static CORPUS_100K: LazyLock<String> = LazyLock::new(|| generate_corpus(100_000, 0xB200_0100));

/// Deterministic Phase 2 corpus of `lines` logical lines.
fn phase2_corpus(lines: usize) -> &'static str {
    match lines {
        10_000 => &CORPUS_10K,
        50_000 => &CORPUS_50K,
        100_000 => &CORPUS_100K,
        other => panic!("no Phase 2 corpus registered for {other} lines"),
    }
}

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

/// The production `Snapshot` constructor over an owned buffer.
mod phase2_snapshot {
    use super::*;

    #[divan::bench(args = PHASE2_SIZES, sample_size = 1, sample_count = 30)]
    fn candidate_snapshot(bencher: Bencher, lines: usize) {
        let content = phase2_corpus(lines);
        bencher
            .with_inputs(|| content.as_bytes().to_vec())
            .bench_local_values(|bytes| {
                let snapshot = Snapshot::from_bytes(black_box(bytes)).expect("valid snapshot");
                black_box((snapshot.id(), snapshot.line_count(), snapshot.byte_len()))
            });
    }
}

/// Safe versus SIMD-validated UTF-8 acceptance for the same owned buffer.
mod phase2_validation {
    use super::*;

    #[divan::bench(args = PHASE2_SIZES, sample_size = 1, sample_count = 30)]
    fn safe_snapshot(bencher: Bencher, lines: usize) {
        let content = phase2_corpus(lines);
        bencher
            .with_inputs(|| content.as_bytes().to_vec())
            .bench_local_values(|bytes| {
                let probe = safe_snapshot_probe(black_box(bytes));
                black_box((probe.text, probe.version, probe.line_count, probe.byte_len))
            });
    }

    #[divan::bench(args = PHASE2_SIZES, sample_size = 1, sample_count = 30)]
    fn simd_validated_unchecked_snapshot(bencher: Bencher, lines: usize) {
        let content = phase2_corpus(lines);
        bencher
            .with_inputs(|| content.as_bytes().to_vec())
            .bench_local_values(|bytes| {
                let probe = unsafe_snapshot_probe(black_box(bytes));
                black_box((probe.text, probe.version, probe.line_count, probe.byte_len))
            });
    }
}

/// Version-function matrix over a one-line and a multi-megabyte input.
mod phase2_version {
    use super::*;

    /// Input labels the version matrix sweeps.
    const INPUTS: [&str; 2] = ["short", "multimegabyte"];

    /// One representative line, the shortest input a version function sees.
    const SHORT: &str = "    let value_42 = compute(123, next_state);\n";

    fn input(label: &str) -> &'static str {
        match label {
            "short" => SHORT,
            "multimegabyte" => phase2_corpus(50_000),
            other => panic!("unknown version-matrix input: {other}"),
        }
    }

    #[divan::bench(args = INPUTS, sample_count = 30)]
    fn xxh3_128_with_seed(bencher: Bencher, label: &str) {
        let content = input(label);
        bencher.bench_local(|| {
            black_box(xxhash_rust::xxh3::xxh3_128_with_seed(
                black_box(content.as_bytes()),
                PHASE2_VERSION_BENCH_SEED,
            ))
        });
    }

    #[divan::bench(args = INPUTS, sample_count = 30)]
    fn blake3_128(bencher: Bencher, label: &str) {
        let content = input(label);
        bencher.bench_local(|| {
            let digest = blake3::hash(black_box(content.as_bytes()));
            let prefix: [u8; 16] = digest.as_bytes()[..16]
                .try_into()
                .expect("BLAKE3 digest prefix is exactly 16 bytes");
            black_box(prefix)
        });
    }
}

/// Line-position index shapes: full materialization versus sparse checkpoints
/// versus a rank/select bitmap.
mod phase2_offsets {
    use super::*;

    /// Sparse-checkpoint intervals the sweep covers.
    const INTERVALS: [usize; 3] = [128, 256, 512];

    /// Build the whole index for a 50,000-line corpus.
    mod construction_50k {
        use super::*;

        #[divan::bench(sample_count = 30)]
        fn full_u32(bencher: Bencher) {
            let content = phase2_corpus(50_000);
            bencher.bench_local(|| black_box(phase0_workloads::offsets_u32(black_box(content))));
        }

        #[divan::bench(sample_count = 30)]
        fn full_u64(bencher: Bencher) {
            let content = phase2_corpus(50_000);
            bencher.bench_local(|| black_box(phase0_workloads::offsets_u64(black_box(content))));
        }

        #[divan::bench(args = INTERVALS, sample_count = 30)]
        fn sparse(bencher: Bencher, interval: usize) {
            let content = phase2_corpus(50_000);
            bencher.bench_local(|| {
                let checkpoints = SparseCheckpoints::new(black_box(content), interval);
                black_box((checkpoints.resident_bytes(), checkpoints))
            });
        }

        #[divan::bench(sample_count = 30)]
        fn rank_select_bitmap(bencher: Bencher) {
            let content = phase2_corpus(50_000);
            bencher.bench_local(|| {
                let bitmap = RankSelectBitmap::new(black_box(content));
                black_box((bitmap.resident_bytes(), bitmap))
            });
        }
    }

    /// Materialize a 2,000-line window at line 50,000 of a 100,000-line corpus
    /// with no index resident, which is what a first read of a large file
    /// actually pays.
    mod cold_window_2k_of_100k {
        use super::*;

        const START_LINE: usize = 50_000;
        const WINDOW_LINES: usize = 2_000;

        #[divan::bench(sample_count = 30)]
        fn full_u32(bencher: Bencher) {
            let content = phase2_corpus(100_000);
            bencher.bench_local(|| {
                black_box(full_u32_window(
                    black_box(content),
                    START_LINE,
                    WINDOW_LINES,
                ))
            });
        }

        #[divan::bench(sample_count = 30)]
        fn full_u64(bencher: Bencher) {
            let content = phase2_corpus(100_000);
            bencher.bench_local(|| {
                black_box(full_u64_window(
                    black_box(content),
                    START_LINE,
                    WINDOW_LINES,
                ))
            });
        }

        #[divan::bench(args = INTERVALS, sample_count = 30)]
        fn sparse(bencher: Bencher, interval: usize) {
            let content = phase2_corpus(100_000);
            bencher.bench_local(|| {
                let checkpoints = SparseCheckpoints::new(black_box(content), interval);
                black_box(checkpoints.select_window(content, START_LINE, WINDOW_LINES))
            });
        }

        #[divan::bench(sample_count = 30)]
        fn rank_select_bitmap(bencher: Bencher) {
            let content = phase2_corpus(100_000);
            bencher.bench_local(|| {
                let bitmap = RankSelectBitmap::new(black_box(content));
                black_box(bitmap.select_window(START_LINE, WINDOW_LINES))
            });
        }
    }
}

/// Wired-path read fixture: a 10k-line file paginated end to end, a 100k-line
/// file windowed cold and warm, and a 50k-line file resumed from a cursor.
struct WiredReadFixture {
    _tmp: TempDir,
    server: HashlineServer,
    hundred_k_path: PathBuf,
    full_10k_pages: Vec<serde_json::Value>,
    window_args: serde_json::Value,
    cursor_args: serde_json::Value,
}

static WIRED_READ_FIXTURE: LazyLock<WiredReadFixture> = LazyLock::new(|| {
    let tmp = fixture_dir();
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let content_10k = generate_corpus(10_000, 0x0A11_C0DE);
    std::fs::write(tmp.path().join("ten_k.rs"), &content_10k).expect("write 10k fixture");
    let content_100k = generate_corpus(100_000, 0xC0FF_EE00);
    let hundred_k_path = tmp.path().join("hundred_k.rs");
    std::fs::write(&hundred_k_path, &content_100k).expect("write 100k fixture");
    // Canonicalize so per-iteration invalidation hits the same key the
    // canonicalized workspace root produces (cache keys are canonical paths).
    let hundred_k_path = hundred_k_path
        .canonicalize()
        .expect("canonicalize 100k fixture");
    let content_50k = generate_corpus(50_000, 0x50C0_5001);
    std::fs::write(tmp.path().join("fifty_k.rs"), &content_50k).expect("write 50k fixture");

    // Walk the pagination chain once so the timed loop replays precomputed
    // requests against a warm snapshot cache.
    let full_10k_pages = RUNTIME.block_on(async {
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

    let window_args = serde_json::json!({"path": "hundred_k.rs", "limit": 2_000});
    RUNTIME.block_on(async {
        let result = server
            .dispatch("read", window_args.clone())
            .await
            .expect("read dispatch");
        let text = assert_dispatch_success(&result);
        assert!(text.starts_with("[hashline snapshot="), "{text}");
    });

    let cursor_args = RUNTIME.block_on(async {
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

    WiredReadFixture {
        server,
        hundred_k_path,
        full_10k_pages,
        window_args,
        cursor_args,
        _tmp: tmp,
    }
});

/// Wave 0 wired-path read benches through `HashlineServer::dispatch`: full
/// pagination of a 10k-line file (warm), the 2k window of a 100k-line file in
/// explicit cold and warm snapshot-cache states, and page 2 of a 50k-line file
/// via the cursor returned by page 1 — the pagination-cost bench the tree
/// previously lacked. Cold state is forced by evicting the fixture from the
/// process snapshot cache before every timed iteration.
mod wired_read {
    use super::*;

    #[divan::bench(sample_count = 20)]
    fn full_10k(bencher: Bencher) {
        let fixture = &*WIRED_READ_FIXTURE;
        bencher.bench_local(|| {
            let mut total = 0usize;
            for args in &fixture.full_10k_pages {
                total += dispatch_len(&fixture.server, "read", args.clone());
            }
            black_box(total)
        });
    }

    #[divan::bench(sample_count = 20)]
    fn window_2k_of_100k_warm(bencher: Bencher) {
        let fixture = &*WIRED_READ_FIXTURE;
        bencher.bench_local(|| {
            black_box(dispatch_len(
                &fixture.server,
                "read",
                fixture.window_args.clone(),
            ))
        });
    }

    #[divan::bench(sample_size = 1, sample_count = 20)]
    fn window_2k_of_100k_cold(bencher: Bencher) {
        let fixture = &*WIRED_READ_FIXTURE;
        bencher
            .with_inputs(|| cache::process_cache().invalidate(&fixture.hundred_k_path))
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "read",
                    fixture.window_args.clone(),
                ))
            });
    }

    #[divan::bench(sample_count = 20)]
    fn cursor_page_50k(bencher: Bencher) {
        let fixture = &*WIRED_READ_FIXTURE;
        bencher.bench_local(|| {
            black_box(dispatch_len(
                &fixture.server,
                "read",
                fixture.cursor_args.clone(),
            ))
        });
    }
}

/// Wired-path edit fixture on 50k-line files. Each mutating bench owns its own
/// file so the benches stay independent of registration order.
struct WiredEditFixture {
    _tmp: TempDir,
    server: HashlineServer,
    content: String,
    single_path: PathBuf,
    single_args: serde_json::Value,
    single_request: EditRequest,
    batch_path: PathBuf,
    batch_args: serde_json::Value,
    batch_request: EditRequest,
    stale_args: serde_json::Value,
}

static WIRED_EDIT_FIXTURE: LazyLock<WiredEditFixture> = LazyLock::new(|| {
    let tmp = fixture_dir();
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let content = generate_corpus(50_000, 0xED17_0002);
    let write_fixture = |name: &str| {
        let path = tmp.path().join(name);
        std::fs::write(&path, &content).expect("write editable fixture");
        path
    };
    let single_path = write_fixture("single_op.rs");
    let batch_path = write_fixture("batch_ops.rs");
    // The conflict bench never mutates its target, so it needs no reset — but
    // it does need the file to exist with the fixture bytes.
    write_fixture("conflict.rs");

    let single_args = replace_args(&content, "single_op.rs", &[25_000]);
    let batch_lines: Vec<u64> = (1..=8).map(|op| op * 6_000).collect();
    let batch_args = replace_args(&content, "batch_ops.rs", &batch_lines);

    // A stale snapshot id must produce the structured conflict and leave the
    // file untouched.
    let mut stale_source = content.clone();
    stale_source.push_str("stale marker\n");
    let stale_args = replace_args(&stale_source, "conflict.rs", &[25_000]);

    WiredEditFixture {
        single_request: serde_json::from_value(single_args.clone())
            .expect("single edit request deserializes"),
        batch_request: serde_json::from_value(batch_args.clone())
            .expect("batch edit request deserializes"),
        server,
        content,
        single_path,
        single_args,
        batch_path,
        batch_args,
        stale_args,
        _tmp: tmp,
    }
});

/// Wired-path edit benches on a 50k-line file. CPU-apply variants call
/// `edit::apply_edits_fast` over a per-iteration snapshot — the production
/// engine `edit::run` dispatches to — and the e2e variants dispatch real edits
/// with a per-iteration file reset. The `_full` suffix records that the default
/// durability policy fsyncs temp file and parent directory.
mod wired_edit {
    use super::*;

    /// Production apply path (engine over snapshot offsets). A fresh Snapshot
    /// per iteration mirrors the wired shape: every edit call loads its own
    /// snapshot, so first-touch offset materialization is part of apply cost.
    #[divan::bench(sample_size = 1, sample_count = 20)]
    fn single_op_50k_apply(bencher: Bencher) {
        let fixture = &*WIRED_EDIT_FIXTURE;
        bencher
            .with_inputs(|| {
                Snapshot::from_bytes(fixture.content.clone().into_bytes())
                    .expect("apply fixture snapshot")
            })
            .bench_local_values(|snapshot| {
                let applied = hashline::edit::apply_edits_fast(&snapshot, &fixture.single_request)
                    .expect("wired single-op apply succeeds");
                black_box(applied.len())
            });
    }

    #[divan::bench(sample_size = 1, sample_count = 20)]
    fn batch_8ops_50k_apply(bencher: Bencher) {
        let fixture = &*WIRED_EDIT_FIXTURE;
        bencher
            .with_inputs(|| {
                Snapshot::from_bytes(fixture.content.clone().into_bytes())
                    .expect("apply fixture snapshot")
            })
            .bench_local_values(|snapshot| {
                let applied = hashline::edit::apply_edits_fast(&snapshot, &fixture.batch_request)
                    .expect("wired batch apply succeeds");
                black_box(applied.len())
            });
    }

    #[divan::bench(sample_size = 1, sample_count = 10)]
    fn single_op_50k_e2e_full(bencher: Bencher) {
        let fixture = &*WIRED_EDIT_FIXTURE;
        bencher
            .with_inputs(|| {
                std::fs::write(&fixture.single_path, &fixture.content)
                    .expect("reset editable fixture");
            })
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "edit",
                    fixture.single_args.clone(),
                ))
            });
    }

    #[divan::bench(sample_size = 1, sample_count = 10)]
    fn batch_8ops_50k_e2e_full(bencher: Bencher) {
        let fixture = &*WIRED_EDIT_FIXTURE;
        bencher
            .with_inputs(|| {
                std::fs::write(&fixture.batch_path, &fixture.content)
                    .expect("reset editable fixture");
            })
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "edit",
                    fixture.batch_args.clone(),
                ))
            });
    }

    /// Version-conflict path: a stale snapshot id must produce the structured
    /// conflict and leave the file untouched, so no per-iteration reset.
    #[divan::bench(sample_count = 20)]
    fn conflict_50k(bencher: Bencher) {
        let fixture = &*WIRED_EDIT_FIXTURE;
        bencher.bench_local(|| {
            RUNTIME.block_on(async {
                let result = fixture
                    .server
                    .dispatch("edit", fixture.stale_args.clone())
                    .await
                    .expect("edit dispatch");
                assert_eq!(result.is_error, Some(true), "stale snapshot must conflict");
                let text = tool_text(&result);
                assert!(
                    text.contains("snapshot_conflict"),
                    "structured conflict expected: {text}"
                );
                black_box(text.len())
            })
        });
    }
}

/// Wired-path grep fixture: a dense single file where every line matches.
struct WiredGrepFixture {
    _tmp: TempDir,
    server: HashlineServer,
    args: serde_json::Value,
    expected_summary: String,
}

static WIRED_GREP_FIXTURE: LazyLock<WiredGrepFixture> = LazyLock::new(|| {
    let tmp = fixture_dir();
    let server = HashlineServer::new(tmp.path().to_path_buf());

    let dense: String = (0..10_000)
        .map(|line| format!("let needle_hit_{line} = {line};\n"))
        .collect();
    std::fs::write(tmp.path().join("dense.rs"), &dense).expect("write dense fixture");

    let args = serde_json::json!({"pattern": "needle_hit", "path": "dense.rs", "max_matches": 200});
    // R015: the match budget is checked between files, so a single dense file
    // renders every match and reports matches=10000 truncated=false. Setup
    // pins the wired response to itself so any drift fails closed.
    let expected_summary = RUNTIME.block_on(async {
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

    WiredGrepFixture {
        server,
        args,
        expected_summary,
        _tmp: tmp,
    }
});

/// Wave 0 wired-path grep bench: a dense single file where every line matches,
/// capped at the protocol maximum. The fixture asserts the AC8/AC24 contract
/// once (exactly the rendered match lines plus the summary); every timed
/// iteration re-asserts the summary suffix.
mod wired_grep {
    use super::*;

    #[divan::bench(sample_count = 20)]
    fn dense_file_capped(bencher: Bencher) {
        let fixture = &*WIRED_GREP_FIXTURE;
        bencher.bench_local(|| {
            RUNTIME.block_on(async {
                let result = fixture
                    .server
                    .dispatch("grep", fixture.args.clone())
                    .await
                    .expect("grep dispatch");
                let text = assert_dispatch_success(&result);
                assert!(
                    text.ends_with(fixture.expected_summary.as_str()),
                    "summary mismatch"
                );
                black_box(text.len())
            })
        });
    }
}

/// Wired-path write fixture: exclusive create, versioned replace, and the
/// already_exists fail-closed path, each against its own destination.
struct WiredWriteFixture {
    _tmp: TempDir,
    server: HashlineServer,
    content: String,
    create_path: PathBuf,
    create_args: serde_json::Value,
    replace_path: PathBuf,
    replace_args: serde_json::Value,
    conflict_args: serde_json::Value,
}

static WIRED_WRITE_FIXTURE: LazyLock<WiredWriteFixture> = LazyLock::new(|| {
    let tmp = fixture_dir();
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
    RUNTIME.block_on(async {
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

    // Exclusive-create conflict target: it exists, so every iteration fails
    // closed without touching it — no reset required, and no interference with
    // the create bench's own destination.
    std::fs::write(tmp.path().join("occupied.rs"), "occupant\n").expect("write conflict fixture");
    let conflict_args = serde_json::json!({
        "file_path": "occupied.rs",
        "content": content,
        "expect": "absent",
    });

    WiredWriteFixture {
        server,
        content,
        create_path,
        create_args,
        replace_path,
        replace_args,
        conflict_args,
        _tmp: tmp,
    }
});

/// Wired-path write benches: exclusive create with a per-iteration unlink,
/// versioned replace with a per-iteration reset, and the already_exists
/// fail-closed path, which needs no reset because the loser never mutates.
mod wired_write {
    use super::*;

    #[divan::bench(sample_size = 1, sample_count = 10)]
    fn create_10k_e2e_full(bencher: Bencher) {
        let fixture = &*WIRED_WRITE_FIXTURE;
        bencher
            .with_inputs(|| {
                std::fs::remove_file(&fixture.create_path).expect("unlink created fixture");
                cache::process_cache().invalidate(&fixture.create_path);
            })
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "write",
                    fixture.create_args.clone(),
                ))
            });
    }

    #[divan::bench(sample_size = 1, sample_count = 10)]
    fn replace_10k_e2e_full(bencher: Bencher) {
        let fixture = &*WIRED_WRITE_FIXTURE;
        bencher
            .with_inputs(|| {
                std::fs::write(&fixture.replace_path, &fixture.content)
                    .expect("reset replace fixture");
            })
            .bench_local_values(|()| {
                black_box(dispatch_len(
                    &fixture.server,
                    "write",
                    fixture.replace_args.clone(),
                ))
            });
    }

    #[divan::bench(sample_count = 20)]
    fn create_conflict(bencher: Bencher) {
        let fixture = &*WIRED_WRITE_FIXTURE;
        bencher.bench_local(|| {
            RUNTIME.block_on(async {
                let result = fixture
                    .server
                    .dispatch("write", fixture.conflict_args.clone())
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
            })
        });
    }
}

/// Wired-path glob fixture over the shared ~2,000-file grep fixture tree.
struct WiredGlobFixture {
    server: HashlineServer,
    args: serde_json::Value,
    expected_summary: String,
}

static WIRED_GLOB_FIXTURE: LazyLock<WiredGlobFixture> = LazyLock::new(|| {
    let server = HashlineServer::new(grep_fixture_root().to_path_buf());
    let args = serde_json::json!({"pattern": "**/*.rs", "max_results": 1000});
    let expected_summary = RUNTIME.block_on(async {
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

    WiredGlobFixture {
        server,
        args,
        expected_summary,
    }
});

/// Wired-path glob bench over the shared ~2,000-file grep fixture tree:
/// recursive discovery with deterministic newest-first ordering. The fixture
/// pins the summary once and every timed iteration re-asserts it so silent
/// truncation or ordering drift fails closed.
mod wired_glob {
    use super::*;

    #[divan::bench(sample_count = 20)]
    fn tree_recursive_rs(bencher: Bencher) {
        let fixture = &*WIRED_GLOB_FIXTURE;
        bencher.bench_local(|| {
            RUNTIME.block_on(async {
                let result = fixture
                    .server
                    .dispatch("glob", fixture.args.clone())
                    .await
                    .expect("glob dispatch");
                let text = assert_dispatch_success(&result);
                assert!(
                    text.ends_with(fixture.expected_summary.as_str()),
                    "summary mismatch"
                );
                black_box(text.len())
            })
        });
    }
}
