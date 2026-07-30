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
use hashline::index::{FileIndex, split_lines};
use hashline::read::format_hashline_content;
use hashline::scheme::{Anchor, Scheme};
use hashline::util::Workspace;
use memchr::{memchr_iter, memchr3_iter};

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

/// Share of lines that are ~2 KiB long in the long-line-heavy corpus.
const LONG_LINE_PERCENT: u32 = 85;

/// Generate a deterministic long-line-heavy corpus (a minified-bundle shape):
/// [`LONG_LINE_PERCENT`] of the lines are ~2 KiB, the rest are ordinary
/// code-like lines.
///
/// The realistic corpus ([`generate_corpus`]) already carries the ~0.5 % long
/// -line tail real source files have; this one inverts the distribution so the
/// hash bench-off can see the bulk-throughput end of the range too.
fn generate_long_line_corpus(num_lines: usize, seed: u32) -> String {
    let mut rng = Xorshift32::new(seed);
    let mut out = String::with_capacity(num_lines * 1_800);
    for i in 0..num_lines {
        if rng.next_range(100) < LONG_LINE_PERCENT {
            let word = IDENTIFIERS[rng.next_range(IDENTIFIERS.len() as u32) as usize];
            let n = rng.next_range(1000);
            out.push_str(&format!("const {word}_{i}={};", word.repeat(250)));
            out.push_str(&format!("//{n}"));
        } else {
            out.push_str(&generate_line(&mut rng, i));
        }
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
    let scheme = SchemeConfig::default()
        .build_scheme()
        .expect("build scheme");

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
        group.bench_function(label, |b| {
            b.iter(|| black_box(run_grep(&ws, &input, scheme)));
        });
    }
    group.finish();
}

/// Line-splitting cost of a partial index, against the floor a pure
/// newline-count scan sets.
///
/// A partial index only needs *hashes* for the spans its caller declared, but
/// it still has to know the file's line count. `count_lines_only` is the
/// irreducible part of that; the gap between it and `new_partial_one_span` is
/// what materializing the untouched lines costs.
fn bench_index_partial(c: &mut Criterion) {
    let content = generate_corpus(50_000, 0x1DEC_0000);
    let span = 24_000..24_064;

    let mut group = c.benchmark_group("index");
    group.sample_size(50);

    group.bench_function("new_partial_one_span_50k", |b| {
        b.iter(|| {
            let index = FileIndex::new_partial(black_box(&content), std::slice::from_ref(&span));
            black_box(index.len())
        });
    });

    group.bench_function("count_lines_only_50k", |b| {
        b.iter(|| black_box(memchr_iter(b'\n', black_box(content.as_bytes())).count()));
    });

    // `Iterator::count` on `memchr_iter` is a specialized popcount loop that
    // never materializes a position. Visiting each newline position is a
    // different — and much more expensive — operation, and it is the one a
    // line index actually needs.
    group.bench_function("visit_newlines_50k", |b| {
        b.iter(|| {
            let mut last = 0usize;
            for pos in memchr_iter(b'\n', black_box(content.as_bytes())) {
                last = pos;
            }
            black_box(last)
        });
    });

    group.bench_function("full_new_50k", |b| {
        b.iter(|| {
            let index = FileIndex::new(black_box(&content));
            black_box(index.len())
        });
    });

    group.finish();
}

/// Reimplementation of the production fused normalize+FNV loop
/// (`hashline::hash::line_hash`), kept here as the bench-off's variant (a)
/// reference so the baseline cell measures the same code shape the crate ships
/// rather than a cross-crate call.
fn fused_fnv(line: &str) -> u32 {
    const FNV_OFFSET: u32 = 2_166_136_261;
    const FNV_PRIME: u32 = 16_777_619;

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

/// Variant (b): branchy normalization into a reusable scratch buffer.
///
/// Byte-for-byte the same output as the fused loop's hash input: trim, then
/// collapse every run of ASCII whitespace to one space.
fn normalize_branchy(line: &str, scratch: &mut Vec<u8>) {
    scratch.clear();
    let mut prev_ws = false;
    for byte in line.trim().bytes() {
        if byte.is_ascii_whitespace() {
            if !prev_ws {
                scratch.push(b' ');
                prev_ws = true;
            }
        } else {
            scratch.push(byte);
            prev_ws = false;
        }
    }
}

/// Variant (c): segment-scan normalization driven by `memchr3`.
///
/// Copies whole non-whitespace segments and writes one space between them,
/// so the per-byte branch of variant (b) is replaced by a SIMD search plus a
/// `memcpy` per segment.
///
/// The scan looks for space, tab, and CR — the ASCII whitespace bytes that
/// occur inside real source lines. `str::trim` has already removed any leading
/// or trailing whitespace (including form feed), so the only input on which
/// this would diverge from variants (a)/(b) is a line with an *interior* form
/// feed. `assert_normalization_agrees` proves the corpora contain none; a
/// production adoption of this variant would need to handle that byte too.
fn normalize_segments(line: &str, scratch: &mut Vec<u8>) {
    scratch.clear();
    let trimmed = line.trim().as_bytes();
    let mut start = 0usize;
    let mut wrote = false;
    for pos in memchr3_iter(b' ', b'\t', b'\r', trimmed) {
        if pos > start {
            if wrote {
                scratch.push(b' ');
            }
            scratch.extend_from_slice(&trimmed[start..pos]);
            wrote = true;
        }
        start = pos + 1;
    }
    if start < trimmed.len() {
        if wrote {
            scratch.push(b' ');
        }
        scratch.extend_from_slice(&trimmed[start..]);
    }
}

/// Variant (c) made exact for every possible input.
///
/// `u8::is_ascii_whitespace` matches five bytes; `memchr3` searches three. This
/// adds the missing two (`\n`, form feed) as a cheap up-front rejection: if
/// either occurs, the branchy path — which is exact by construction — takes
/// over. Real source lines contain neither, so the guard's whole cost is one
/// extra `memchr2` pass over the line, which is precisely what this cell
/// measures against the unguarded `c_segments` cells.
fn normalize_segments_guarded(line: &str, scratch: &mut Vec<u8>) {
    if memchr::memchr2(b'\n', 0x0C, line.trim().as_bytes()).is_some() {
        normalize_branchy(line, scratch);
        return;
    }
    normalize_segments(line, scratch);
}

/// Variant (c) guarded against form feed only.
///
/// Lines produced by `split_lines` cannot contain `\n`, so a `FileIndex` only
/// has to rule out form feed. Measures whether halving the guard's byte
/// alphabet buys anything over [`normalize_segments_guarded`].
fn normalize_segments_ff_guarded(line: &str, scratch: &mut Vec<u8>) {
    if memchr::memchr(0x0C, line.trim().as_bytes()).is_some() {
        normalize_branchy(line, scratch);
        return;
    }
    normalize_segments(line, scratch);
}

/// Prove every normalization variant feeds the hash the same bytes.
///
/// Run once per corpus (not per iteration): variants (b) and (c) must produce
/// identical buffers, and FNV over that buffer must equal the fused loop's
/// result — which is what makes the matrix cells comparable at all.
fn assert_normalization_agrees(lines: &[&str], label: &str) {
    let mut branchy = Vec::with_capacity(4_096);
    let mut segments = Vec::with_capacity(4_096);
    for (idx, line) in lines.iter().enumerate() {
        normalize_branchy(line, &mut branchy);
        normalize_segments(line, &mut segments);
        assert_eq!(branchy, segments, "{label} line {idx}: variants (b) vs (c)");
        assert_eq!(
            fnv1a_32(&branchy),
            fused_fnv(line),
            "{label} line {idx}: normalized bytes vs fused loop"
        );
    }
    // The crate's own `line_hash` is no longer FNV on AES-enabled targets, so
    // it is not comparable cell-for-cell here; `src/hash.rs` owns the tests
    // proving the shipped hasher consumes exactly these normalized bytes.
}

/// Phase 6 hash bench-off: normalization strategy × hash function.
///
/// Measurement only — nothing here is wired into the crate. Each cell hashes
/// every line of a 10,000-line corpus, so a cell's median divided by 10,000 is
/// its ns/line.
///
/// Variant (a) (fused normalize+hash in one pass) exists only for a streaming
/// byte-at-a-time hash, so it pairs with FNV alone; `gxhash32` and
/// `rapidhash` are block hashes and structurally require the two-pass
/// normalize-then-hash shape of variants (b) and (c). The (b)/(c) + FNV cells
/// are the controls that separate the two-pass penalty from the hash's own
/// speed.
fn bench_hash_matrix(c: &mut Criterion) {
    let realistic = generate_corpus(10_000, 0x4A54_0001);
    let long_line = generate_long_line_corpus(10_000, 0x4A54_0002);

    for (corpus_label, content) in [("realistic", &realistic), ("long_line", &long_line)] {
        let lines = split_lines(content);
        assert_normalization_agrees(&lines, corpus_label);

        let mut group = c.benchmark_group(format!("hash_matrix/{corpus_label}"));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(6));

        group.bench_function("a_fused+fnv", |b| {
            b.iter(|| {
                let mut acc = 0u32;
                for line in &lines {
                    acc ^= fused_fnv(black_box(line));
                }
                black_box(acc)
            });
        });

        for (norm_label, normalize) in [
            ("b_branchy", normalize_branchy as fn(&str, &mut Vec<u8>)),
            ("c_segments", normalize_segments),
            ("c_guarded", normalize_segments_guarded),
            ("c_ff_guarded", normalize_segments_ff_guarded),
        ] {
            group.bench_function(format!("{norm_label}+fnv"), |b| {
                let mut scratch = Vec::with_capacity(8_192);
                b.iter(|| {
                    let mut acc = 0u32;
                    for line in &lines {
                        normalize(black_box(line), &mut scratch);
                        acc ^= fnv1a_32(&scratch);
                    }
                    black_box(acc)
                });
            });

            group.bench_function(format!("{norm_label}+gxhash32"), |b| {
                let mut scratch = Vec::with_capacity(8_192);
                b.iter(|| {
                    let mut acc = 0u32;
                    for line in &lines {
                        normalize(black_box(line), &mut scratch);
                        acc ^= gxhash::gxhash32(&scratch, 0);
                    }
                    black_box(acc)
                });
            });

            group.bench_function(format!("{norm_label}+rapidhash"), |b| {
                let mut scratch = Vec::with_capacity(8_192);
                b.iter(|| {
                    let mut acc = 0u32;
                    for line in &lines {
                        normalize(black_box(line), &mut scratch);
                        acc ^= rapidhash::v3::rapidhash_v3(&scratch) as u32;
                    }
                    black_box(acc)
                });
            });
        }

        group.finish();
    }
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
    bench_grep_large_file,
    bench_index_partial,
    bench_hash_matrix,
    bench_dispatch,
);
criterion_main!(benches);
