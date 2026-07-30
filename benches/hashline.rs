//! Criterion benchmarks for hashline's hot paths.
//!
//! These are the Phase 0 baselines for the max-performance optimization plan
//! (`.omc/plans/2026-07-30-max-performance-optimization.md`): every later
//! phase is measured against the numbers this harness records in
//! `benches/BASELINE.md`. Corpus generation is fully deterministic (a small
//! inline xorshift32 PRNG, no `rand` and no system time) so results are
//! reproducible across runs and machines.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use hashline::HashlineServer;
use hashline::config::{SchemeConfig, SchemeKind};
use hashline::edit::HashlineOp;
use hashline::edit::apply::apply_edits;
use hashline::grep::{HashlineGrepInput, run_grep};
use hashline::hash::{encode_hash, fnv1a_32, line_hash};
use hashline::index::FileIndex;
use hashline::read::format_hashline_content;
use hashline::scheme::{Anchor, Scheme};
use hashline::util::Workspace;

/// Deterministic xorshift32 PRNG for reproducible synthetic corpora.
///
/// Not cryptographic and not `rand` — a self-contained generator is enough
/// to produce varied, reproducible code-like fixtures without a new
/// dependency.
struct Xorshift32(u32);

impl Xorshift32 {
    /// Create a generator seeded with `seed` (zero is remapped to a nonzero
    /// constant, since an all-zero xorshift state never advances).
    fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    /// Next pseudo-random `u32`.
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Next pseudo-random value in `0..bound`.
    fn next_range(&mut self, bound: u32) -> u32 {
        self.next_u32() % bound
    }
}

/// Identifier pool for synthetic code-like lines.
const IDENTIFIERS: &[&str] = &[
    "value", "index", "buffer", "result", "config", "handler", "state", "count", "items", "cursor",
    "reader", "writer", "context", "target", "source", "delta",
];

/// Keyword pool for synthetic code-like lines (includes `fn` at zero
/// indentation for the grep `^`-anchored regex fixture).
const KEYWORDS: &[&str] = &[
    "let", "fn", "if", "for", "while", "return", "match", "struct", "impl", "pub",
];

/// Generate one deterministic code-like line.
///
/// The line number is embedded in an identifier so every generated line is
/// content-unique regardless of indentation — this keeps anchor/find-shifted
/// benchmarks free of incidental hash collisions between distinct lines.
fn generate_line(rng: &mut Xorshift32, line_no: usize) -> String {
    // Occasional blank line.
    if rng.next_range(37) == 0 {
        return String::new();
    }
    // Occasional long line (~2 KiB), simulating a long literal or comment.
    if rng.next_range(211) == 0 {
        let word = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
        return format!("// {}", word.repeat(300));
    }

    let depth = rng.next_range(5) as usize;
    let indent = "    ".repeat(depth);
    let kw = KEYWORDS[rng.next_range(KEYWORDS.len() as u32) as usize];
    let ident = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
    let ident2 = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
    let n = rng.next_range(1000);
    format!("{indent}{kw} {ident}_{line_no} = {ident2}({n});")
}

/// Generate a deterministic code-like corpus of `num_lines` lines, joined
/// with `\n` and ending in a trailing newline (matching typical source
/// files).
fn generate_corpus(num_lines: usize, seed: u32) -> String {
    let mut rng = Xorshift32::new(seed);
    let mut out = String::with_capacity(num_lines * 24);
    for i in 0..num_lines {
        out.push_str(&generate_line(&mut rng, i));
        out.push('\n');
    }
    out
}

/// Render the anchor string for a specific 1-based line under `scheme`.
fn anchor_at(index: &FileIndex<'_>, scheme: Scheme, line_1based: usize) -> String {
    scheme
        .anchor_at(index, line_1based - 1)
        .expect("line within file")
        .render()
}

/// Nearest non-blank 1-based line index to `target`, scanning outward.
///
/// The synthetic corpus occasionally emits blank lines; anchor and edit
/// benchmarks need a stable, content-bearing target line.
fn nearest_nonblank(lines: &[&str], target: usize) -> usize {
    let target = target.clamp(1, lines.len());
    if !lines[target - 1].is_empty() {
        return target;
    }
    for offset in 1..lines.len() {
        if let Some(idx) = target.checked_sub(offset)
            && idx >= 1
            && !lines[idx - 1].is_empty()
        {
            return idx;
        }
        let idx = target + offset;
        if idx <= lines.len() && !lines[idx - 1].is_empty() {
            return idx;
        }
    }
    target
}

/// `line_hash` / `encode_hash` microbenches on a short (~40 B) and a long
/// (~2 KiB) line.
fn bench_line_hash(c: &mut Criterion) {
    let short_line = "    let value_42 = compute(123, next_state);";
    let long_line: String = format!("// {}", "x".repeat(2_000));

    let mut group = c.benchmark_group("line_hash");
    group.bench_function("short_line_40b", |b| {
        b.iter(|| black_box(line_hash(black_box(short_line))));
    });
    group.bench_function("long_line_2kb", |b| {
        b.iter(|| black_box(line_hash(black_box(&long_line))));
    });
    group.bench_function("encode_hash", |b| {
        let h = fnv1a_32(b"sample content for the encode_hash benchmark");
        b.iter(|| black_box(encode_hash(black_box(h), 3)));
    });
    group.finish();
}

/// Anchor generation over 1k / 10k / 100k-line synthetic files, for each of the
/// three anchor schemes.
///
/// Measures the same logical operation as the Phase 0 baseline: build the
/// per-request line/hash index and render every line's anchor. The index build
/// (line splitting plus line hashing) is inside the timed region — the Phase 0
/// `generate_anchors` baseline hashed every line too, so the comparison stays
/// apples-to-apples (this version additionally pays for the line splitting).
fn bench_generate_anchors(c: &mut Criterion) {
    let mut group = c.benchmark_group("generate_anchors");
    group.sample_size(30);

    for &size in &[1_000usize, 10_000, 100_000] {
        let content = generate_corpus(size, 0x5EED_0000_u32.wrapping_add(size as u32));

        for kind in [
            SchemeKind::ContentOnly,
            SchemeKind::Chunk,
            SchemeKind::Checkpoint,
        ] {
            let scheme = SchemeConfig {
                kind,
                ..Default::default()
            }
            .build_scheme()
            .expect("build scheme");
            let label = format!("{kind:?}/{size}_lines");
            group.bench_function(label, |b| {
                b.iter(|| {
                    let index = FileIndex::new(black_box(&content));
                    let anchors: Vec<Anchor> =
                        scheme.anchors_for_range(&index, 0..index.len()).collect();
                    black_box(anchors)
                });
            });
        }
    }
    group.finish();
}

/// `format_hashline_content`: a full 10k-line read, and a 2,000-line window
/// of a 100k-line file.
fn bench_format_hashline_content(c: &mut Criterion) {
    let scheme = SchemeConfig::default()
        .build_scheme()
        .expect("build scheme");
    let content_10k = generate_corpus(10_000, 0xA11C_E000);
    let content_100k = generate_corpus(100_000, 0xB0B0_B0B0);

    let mut group = c.benchmark_group("format_hashline_content");
    group.sample_size(30);

    group.bench_function("full_read_10k_lines", |b| {
        b.iter(|| black_box(format_hashline_content(&content_10k, None, None, scheme)));
    });

    group.bench_function("window_2k_of_100k_lines", |b| {
        b.iter(|| {
            black_box(format_hashline_content(
                &content_100k,
                Some(50_000),
                Some(2_000),
                scheme,
            ))
        });
    });

    group.finish();
}

/// `apply_edits`: a single-op edit and an 8-op batch on a 50k-line file, plus
/// the stale-anchor error path (an anchor whose target shifted by one line,
/// exercising `find_shifted` and full-file error-context rendering).
fn bench_apply_edits(c: &mut Criterion) {
    let scheme = SchemeConfig::default()
        .build_scheme()
        .expect("build scheme");
    let content = generate_corpus(50_000, 0xED17_0001);
    let index = FileIndex::new(&content);
    let lines = index.lines();

    let single_target = nearest_nonblank(lines, 25_000);
    let single_ops = vec![HashlineOp::Replace {
        anchor: anchor_at(&index, scheme, single_target),
        end_anchor: None,
        content: "REPLACED SINGLE LINE".to_owned(),
    }];

    let batch_ops: Vec<HashlineOp> = (1..=8u32)
        .map(|i| {
            let target = nearest_nonblank(lines, i as usize * 6_000);
            HashlineOp::Replace {
                anchor: anchor_at(&index, scheme, target),
                end_anchor: None,
                content: format!("REPLACED BATCH LINE {i}"),
            }
        })
        .collect();

    let stale_target = nearest_nonblank(lines, 25_000);
    let stale_anchor = anchor_at(&index, scheme, stale_target);
    // Prepend one line so the anchor's target content shifts down by exactly
    // one line — validate_anchor sees Stale, then find_shifted + full-file
    // context rendering run (F3's repeated-pass path).
    let shifted_content = format!("// shift marker\n{content}");
    let stale_ops = vec![HashlineOp::Replace {
        anchor: stale_anchor,
        end_anchor: None,
        content: "should not apply".to_owned(),
    }];

    let mut group = c.benchmark_group("apply_edits");
    group.sample_size(30);

    group.bench_function("single_op_50k_lines", |b| {
        b.iter(|| black_box(apply_edits(black_box(&content), &single_ops, scheme)));
    });

    group.bench_function("batch_8ops_50k_lines", |b| {
        b.iter(|| black_box(apply_edits(black_box(&content), &batch_ops, scheme)));
    });

    group.bench_function("stale_anchor_error_path_50k_lines", |b| {
        b.iter(|| black_box(apply_edits(black_box(&shifted_content), &stale_ops, scheme)));
    });

    group.finish();
}

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

/// Build a `HashlineGrepInput` searching for `pattern` with default options.
fn grep_input(pattern: &str) -> HashlineGrepInput {
    HashlineGrepInput {
        pattern: pattern.to_owned(),
        path: None,
        glob: None,
        ignore_case: None,
        after_context: None,
        before_context: None,
        context: None,
        max_matches: None,
    }
}

/// `run_grep` over the ~2,000-file fixture: a rare literal, a common
/// literal, and a `^`-anchored regex.
fn bench_grep(c: &mut Criterion) {
    let root = grep_fixture_root();
    let ws = Workspace::new(root.to_path_buf(), false);
    let scheme = SchemeConfig::default()
        .build_scheme()
        .expect("build scheme");

    let mut group = c.benchmark_group("grep");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(6));

    for (label, pattern) in [
        ("rare_literal", GREP_RARE_TOKEN),
        ("common_literal", GREP_COMMON_TOKEN),
        ("anchored_regex", "^fn "),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| black_box(run_grep(&ws, &grep_input(pattern), scheme)));
        });
    }
    group.finish();
}

/// End-to-end `HashlineServer::dispatch` bench: a realistic 300-line file
/// through the read path, and a single-op edit — answers the plan's
/// pre-mortem question of whether hot paths are already sub-millisecond at
/// realistic file sizes.
fn bench_dispatch(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for dispatch bench");

    let tmp = tempfile::TempDir::new().expect("tempdir for dispatch bench");
    let content = generate_corpus(300, 0xD159_A7C0);
    let file_path = tmp.path().join("sample.rs");
    std::fs::write(&file_path, &content).expect("write dispatch fixture");

    let server = HashlineServer::new(tmp.path().to_path_buf(), SchemeConfig::default())
        .expect("server construction");

    let scheme = SchemeConfig::default()
        .build_scheme()
        .expect("build scheme");
    let index = FileIndex::new(&content);
    let edit_target = nearest_nonblank(index.lines(), 150);
    let edit_anchor = anchor_at(&index, scheme, edit_target);

    let mut group = c.benchmark_group("dispatch");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(5));

    let read_args = serde_json::json!({"path": "sample.rs"});
    group.bench_function("read_300_lines", |b| {
        b.to_async(&rt).iter(|| {
            let args = read_args.clone();
            let server = &server;
            async move { black_box(server.dispatch("hashline_read", args).await) }
        });
    });

    let edit_args = serde_json::json!({
        "file_path": "sample.rs",
        "edits": [{"op": "replace", "anchor": edit_anchor, "content": "EDITED LINE"}],
    });
    group.bench_function("edit_single_op_300_lines", |b| {
        b.to_async(&rt).iter_batched(
            || std::fs::write(&file_path, &content).expect("reset dispatch fixture"),
            |()| {
                let args = edit_args.clone();
                let server = &server;
                async move { black_box(server.dispatch("hashline_edit", args).await) }
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_line_hash,
    bench_generate_anchors,
    bench_format_hashline_content,
    bench_apply_edits,
    bench_grep,
    bench_dispatch,
);
criterion_main!(benches);
