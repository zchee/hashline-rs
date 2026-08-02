# Phase 0 Baseline — hashline-rs

Baseline measurements captured before any optimization work begins (Option B,
`.omc/plans/2026-07-30-max-performance-optimization.md`). All later phases are
measured against these numbers.

## Machine / build

| | |
|---|---|
| Date | 2026-07-30 |
| Commit | `acf9d71` + this bench harness only (no production code changed) |
| Arch | arm64 (`uname -m`) |
| CPU | Apple M3 Max |
| rustc | `rustc 1.99.0-nightly (1a833e165 2026-07-29)` |
| cargo | `cargo 1.99.0-nightly (3efb1f477 2026-07-17)` |
| Bench profile | `cargo bench` as-is: no `RUSTFLAGS`, no `target-cpu=native`, sequential (default criterion runner) |
| Total `cargo bench` wall time | 4m 43.6s (well under the ~10 min budget) |

Criterion column values are the point estimate (middle of the reported
`[low high]` confidence interval), i.e. the number on criterion's `time:` line.

## `line_hash` / `encode_hash` microbenches

| Benchmark | Median |
|---|---|
| `line_hash/short_line_40b` | 50.993 ns |
| `line_hash/long_line_2kb` | 3.2246 µs |
| `line_hash/encode_hash` | 10.693 ns |

## `generate_anchors` (FileIndex-equivalent build + full-range render)

| Scheme | 1,000 lines | 10,000 lines | 100,000 lines |
|---|---|---|---|
| `content_only` | 55.922 µs | 676.86 µs | 7.3089 ms |
| `chunk` (default) | 118.31 µs | 1.4059 ms | 14.990 ms |
| `checkpoint` | 70.277 µs | 816.54 µs | 8.6212 ms |

## `format_hashline_content`

| Benchmark | Median |
|---|---|
| `full_read_10k_lines` | 2.4406 ms |
| `window_2k_of_100k_lines` | 16.872 ms |

## `apply_edits` (50k-line file, chunk scheme)

| Benchmark | Median |
|---|---|
| `single_op_50k_lines` | 11.642 ms |
| `batch_8ops_50k_lines` | 70.652 ms |
| `stale_anchor_error_path_50k_lines` (find_shifted + full-file context render) | 8.2990 ms |

## `grep` (2,000-file synthetic tree fixture, chunk scheme)

| Benchmark | Median |
|---|---|
| `rare_literal` | 13.694 ms |
| `common_literal` | 15.175 ms |
| `anchored_regex` (`^fn `) | 14.391 ms |

Informational only at this stage (no `rg` comparison run in Phase 0 — that
comparison is an acceptance-criteria check for Phase 3).

## End-to-end dispatch (realistic 300-line file)

| Benchmark | Median |
|---|---|
| `dispatch/read_300_lines` | 102.07 µs |
| `dispatch/edit_single_op_300_lines` | 74.981 µs |

## Phase 0 exit question

**Are the hot paths at realistic sizes (300-line file) already < 1 ms?**

**Yes.** `dispatch/read_300_lines` (102 µs) and
`dispatch/edit_single_op_300_lines` (75 µs) are both roughly 10-13x under the
1 ms bar — realistic single-file tool calls are already sub-millisecond
end-to-end (including JSON dispatch, file I/O, and anchor rendering).

Per the plan's pre-mortem item 3 ("Optimized the wrong thing"), this confirms
the optimization payoff is concentrated in:

- **Large files**: `generate_anchors`/`format_hashline_content`/`apply_edits`
  scale from double-digit µs at 1k lines to double-digit ms at 100k lines
  (chunk scheme: 118 µs → 15.0 ms, essentially linear but with a large
  constant factor from the per-line `String` allocations Phase 1 removes).
  `apply_edits/batch_8ops_50k_lines` in particular (70.7 ms) is dominated by
  the repeated full-file passes described in finding F3 — each of the 8 ops'
  post-edit snippet render re-walks the whole file.
- **`grep`**, which is the one tool whose cost scales with repository
  size rather than a single file's size — all three query shapes land in the
  13-15 ms band over a 2,000-file/~85,900-line fixture even before any
  haystack-level matching (F5) is applied.

This does not by itself demand re-scoping Phase ordering away from Phases
1-2 (they still remove the dominant allocation/pass-count cost for large
single-file reads/edits), but it does confirm grep and large-file paths are
where Phases 1-3 acceptance criteria (§ Acceptance Criteria 1-4) will show
the largest wins, while realistic small-file interactive latency has no
headroom problem today.

---

# Post-wave-4 (9c425d8 + Phase 5) — hashline-rs

Full clean sequential `cargo bench` run after Phase 5 (crate-wide `simdutf8`,
block-counting partial-index line scan, fat LTO) landed on top of 9c425d8. The
Phase 0 section above is unchanged: **all deltas are always computed against
those original numbers.**

Wire output is byte-identical to the Phase 0 tree throughout — `examples/golden`
SHA-256 `f2a730abe7330245cfc10c48983cd5a9f36c5c473a4920bf4fe192e33cf5ceb0`
before and after.

## Machine / build

| | |
|---|---|
| Date | 2026-07-30 |
| Commit | `9c425d8` + Phase 5 changes |
| Arch | arm64 (`uname -m`) |
| CPU | Apple M3 Max |
| rustc | `rustc 1.99.0-nightly (1a833e165 2026-07-29)` |
| cargo | `cargo 1.99.0-nightly (3efb1f477 2026-07-17)` |
| Bench profile | `cargo bench` as-is: no `RUSTFLAGS`, no `target-cpu=native`, sequential |
| Release profile | `lto = "fat"`, `codegen-units = 1` (was `lto = "thin"` in Phase 0) |
| Global allocator | system (mimalloc evaluated and rejected — see below) |
| Total `cargo bench` wall time | 8m 01.9s (43 benchmarks, including the new Phase 5/6 groups) |

## `line_hash` / `encode_hash` microbenches

| Benchmark | Phase 0 | Now | Speedup |
|---|---|---|---|
| `line_hash/short_line_40b` | 50.993 ns | 52.009 ns | 0.98x |
| `line_hash/long_line_2kb` | 3.2246 µs | 3.3351 µs | 0.97x |
| `line_hash/encode_hash` | 10.693 ns | 1.1947 ns | 8.95x |

`line_hash` itself is untouched by every phase so far; the small deltas are run
-to-run noise. `encode_hash` is Phase 1's allocation-free `EncodedHash`.

## `generate_anchors` (FileIndex build + full-range render)

| Scheme | 1,000 lines | 10,000 lines | 100,000 lines |
|---|---|---|---|
| `content_only` | 52.561 µs (1.06x) | 646.09 µs (1.05x) | 6.9068 ms (1.06x) |
| `chunk` (default) | 54.890 µs (2.16x) | 648.53 µs (2.17x) | 6.9947 ms (2.14x) |
| `checkpoint` | 54.732 µs (1.28x) | 645.25 µs (1.27x) | 7.0492 ms (1.22x) |

All three schemes have converged onto the same number because the contextual
fingerprints are now integer folds — the remaining cost is `line_hash` and
nothing else. **Acceptance criterion 2 (chunk/10k >= 5x, i.e. <= 281.2 µs) is
NOT met at 2.17x**, and the Phase 6 matrix below shows a hash swap cannot close
the gap either (best projection 2.60x).

## `format_hashline_content`

| Benchmark | Phase 0 | Now | Speedup |
|---|---|---|---|
| `full_read_10k_lines` | 2.4406 ms | 952.70 µs | 2.56x |
| `window_2k_of_100k_lines` | 16.872 ms | 294.38 µs | **57.3x** |

Criterion 1's window clause (>= 10x) is met by a wide margin — the windowed read
improved a further 3.7x in Phase 5 (1.08 ms at wave 3a) because a 100,000-line
file's line scan no longer visits every newline position. The full-10k clause
(>= 3x) remains unmet at 2.56x: that path hashes every line, so it is bounded by
`line_hash` exactly as `generate_anchors` is.

## `apply_edits` (50k-line file, chunk scheme)

| Benchmark | Phase 0 | Now | Speedup |
|---|---|---|---|
| `single_op_50k_lines` | 11.642 ms | 964.58 µs | 12.07x |
| `batch_8ops_50k_lines` | 70.652 ms | 1.0073 ms | 70.14x |
| `stale_anchor_error_path_50k_lines` | 8.2990 ms | 447.22 µs | 18.56x |

Criterion 3 (>= 10x on both the single op and the stale path) stays met. Phase 5
is deliberately neutral here: the edit path splices the whole line vector, so
`pre_edit_index` keeps its eager split (`FileIndex::from_lines_partial`) rather
than paying for a span scan and a materialization.

## `grep` (2,000-file synthetic tree fixture, chunk scheme)

| Benchmark | Phase 0 | Now | Speedup |
|---|---|---|---|
| `rare_literal` | 13.694 ms | 14.159 ms | 0.97x |
| `common_literal` | 15.175 ms | 14.742 ms | 1.03x |
| `anchored_regex` (`^fn `) | 14.391 ms | 14.445 ms | 1.00x |

Unchanged, as expected: this fixture is filesystem-bound (directory walk plus
2,000 opens is essentially the whole wall time), which is why acceptance
criterion 4 was re-scoped to the single-file bench below.

## `grep`, single 50,000-line file (~1.6 MB) — NEW in Phase 5

Criterion 4(b)'s match-bound bench. The search path is the file itself, taking
`run_grep`'s single-file short circuit; pointing it at the containing directory
instead adds ~4 ms of walker thread-pool startup that buries the work being
measured. No Phase 0 number exists (the group is new), so the reference column
is the tree at 9c425d8 with this bench applied and no other change.

| Benchmark | At 9c425d8 | Now | Speedup |
|---|---|---|---|
| `rare_literal` | 1.3151 ms | 246.91 µs | **5.33x** |
| `common_literal` | 3.9502 ms | 3.2802 ms | 1.20x |
| `anchored_regex` (`^fn `) | 1.9404 ms | 1.3036 ms | 1.49x |

`common_literal` gains least because its cost is dominated by matching and
rendering thousands of hits, not by the two things Phase 5 removed.

## Partial-index line scan — NEW in Phase 5

| Benchmark | At 9c425d8 | Now | Speedup |
|---|---|---|---|
| `index/new_partial_one_span_50k` | 440.59 µs | 57.864 µs | **7.61x** |
| `index/count_lines_only_50k` (reference floor) | 47.339 µs | 45.973 µs | — |
| `index/visit_newlines_50k` (reference ceiling) | 406.33 µs | 405.47 µs | — |
| `index/full_new_50k` (whole-file index, unchanged path) | 3.3336 ms | 3.2884 ms | 1.01x |

The two reference rows are the measurement that drove the design and are kept as
regression guards. `count_lines_only` is `memchr_iter(b'\n', ..).count()`, a SIMD
popcount that never materializes a position; `visit_newlines` is the same
iterator with its positions actually visited. The ~8.8x gap between them — not
the cost of slicing lines, which is only ~35 µs of the original 441 µs — is what
`FileIndex::new_partial` used to pay in full. It now counts its way to each span
a 32 KiB block at a time and visits positions only inside the spans, landing
within 26% of the count-only floor.

## End-to-end dispatch (realistic 300-line file)

| Benchmark | Phase 0 | Now | Speedup |
|---|---|---|---|
| `dispatch/read_300_lines` | 102.07 µs | 44.948 µs | 2.27x |
| `dispatch/edit_single_op_300_lines` | 74.981 µs | 24.415 µs | 3.07x |

Realistic single-file tool calls were already sub-millisecond in Phase 0 and are
now 2-3x under that again, JSON dispatch and file I/O included.

## Phase 5 build/runtime decisions

**Fat LTO: adopted.** Measured against the same tree with `lto = "thin"`:
`apply_edits/single_op` -3.5%, `apply_edits/batch_8ops` -2.9%,
`format_hashline_content/full_read_10k` -1.4%, `dispatch/read_300_lines` -1.8%,
`generate_anchors/Chunk/10000` -1.9%; `grep_large_file` +1.2% to +3.5%. Small
and mixed, net mildly positive, and it is what the plan specifies.

**mimalloc: rejected, dependency removed.** Gate was >= 3% end-to-end on the
driver benches. Measured (fat LTO, mimalloc vs system allocator):

| Driver | Delta |
|---|---|
| `format_hashline_content/window_2k_of_100k` | -1.27% |
| `apply_edits/single_op_50k_lines` | **+1.27%** |
| `grep_large_file/rare_literal` | -1.34% |
| `grep_large_file/common_literal` | **+1.65%** |
| `grep_large_file/anchored_regex` | -0.34% |
| `dispatch/read_300_lines` | -1.94% |

Everything inside +/-3.5%, signs mixed, two drivers worse. The cause is
structural rather than incidental: Phases 1-4 removed per-line allocation, so
there is almost no allocator traffic left for an allocator swap to improve.

**`panic = "abort"`: not adopted.** See the deviations note in
`.omc/notepads/2026-07-30-max-performance-optimization/phase5-runtime-and-hash-benchoff.md`.
In short: Cargo ignores `panic` for the `test` and `bench` profiles, so it
cannot be measured by this harness at all (bench configuration would stop
matching ship configuration); it turns three recoverable failures into process
death (a panicking `spawn_blocking` task in read/edit, the partial-index
programmer-error panics, the poisoned-mutex `expect` in the grep collector); and
for a long-lived stdio server inside a model's edit loop, losing the process
loses the session. The only demonstrable win is binary size (4.29 MB today).

## Phase 6 hash bench-off (measurement only — nothing wired into the crate)

Normalization strategy x hash function, 10,000-line corpora, `gxhash` and
`rapidhash` as dev-dependencies only. Every cell is asserted (once per corpus,
outside the timed region) to feed the hash byte-identical normalized input, and
to agree with the shipped `line_hash` on every line.

Variant (a) is the fused normalize-and-hash single pass the crate ships. It only
exists for a streaming byte-at-a-time hash, so it pairs with FNV alone;
`gxhash32` and `rapidhash` are block hashes and structurally require the
two-pass normalize-then-hash shape of variants (b) and (c). The (b)/(c) + FNV
cells are the controls that separate the two-pass penalty from the hash's own
speed.

### REALISTIC corpus (code-like line lengths, ~0.5% of lines ~2 KB)

| Cell | Median | ns/line | vs (a)+FNV |
|---|---|---|---|
| **(a) fused + FNV** *(baseline)* | **585.73 µs** | **58.6** | **1.00x** |
| (b) branchy + FNV | 761.38 µs | 76.1 | 0.77x |
| (b) branchy + gxhash32 | 576.16 µs | 57.6 | 1.02x |
| (b) branchy + rapidhash | 579.05 µs | 57.9 | 1.01x |
| (c) memchr segments + FNV | 668.63 µs | 66.9 | 0.88x |
| **(c) memchr segments + gxhash32** | **478.85 µs** | **47.9** | **1.22x** |
| (c) memchr segments + rapidhash | 489.73 µs | 49.0 | 1.20x |

### LONG-LINE corpus (85% of lines ~2 KB, minified-bundle shape)

| Cell | Median | ns/line | vs (a)+FNV |
|---|---|---|---|
| (a) fused + FNV *(baseline)* | 20.496 ms | 2049.6 | 1.00x |
| (b) branchy + FNV | 23.657 ms | 2365.7 | 0.87x |
| (b) branchy + gxhash32 | 10.583 ms | 1058.3 | 1.94x |
| (b) branchy + rapidhash | 10.682 ms | 1068.2 | 1.92x |
| (c) memchr segments + FNV | 13.944 ms | 1394.4 | 1.47x |
| **(c) memchr segments + gxhash32** | **772.04 µs** | **77.2** | **26.5x** |
| (c) memchr segments + rapidhash | 892.06 µs | 89.2 | 23.0x |

### Projected `generate_anchors/Chunk/10000_lines` per candidate

Current total 648.53 µs, of which the baseline cell accounts for 585.73 µs; the
62.8 µs remainder is index bookkeeping and anchor rendering, which no hash
choice touches. Criterion 2 needs <= 281.2 µs.

| Cell | Projected | vs Phase 0 (1.4059 ms) | Criterion 2 |
|---|---|---|---|
| (a) fused + FNV *(today)* | 648.5 µs | 2.17x | no |
| (b) branchy + FNV | 824.2 µs | 1.71x | no |
| (b) branchy + gxhash32 | 639.0 µs | 2.20x | no |
| (b) branchy + rapidhash | 641.9 µs | 2.19x | no |
| (c) segments + FNV | 731.4 µs | 1.92x | no |
| (c) segments + gxhash32 | **541.7 µs** | **2.60x** | no |
| (c) segments + rapidhash | 552.5 µs | 2.54x | no |

Candidates do beat the fused FNV baseline on the realistic corpus, so the plan's
"report before deciding because everything lost" trigger is not hit — but the
best cell still lands 1.93x short of criterion 2. Short-line hashing is bounded
by per-line fixed costs (trim, call, buffer reset), not bulk throughput; the
long-line corpus is where these hashes show what they can do.

`gxhash32` leads `rapidhash` by 2.2% on the realistic corpus (confidence
intervals do not overlap, so the ordering is real) and by 15% on long lines.
Against that, `rapidhash` needs no `+aes`, no committed `.cargo/config.toml`, no
SIGILL startup guard, and no per-architecture `cfg` fallback.

---

# Post-Phase-6 (gxhash32 line hash) — hashline-rs

The Phase 6 swap landed: normalized lines are hashed with `gxhash32` on targets
with AES intrinsics, via the `memchr3` segment-scan normalizer. Deltas below are
against the original Phase 0 section; the "post-P5" column is the section above,
so the swap's own effect is the difference between the last two columns.

**Anchor letters change with this commit.** The golden reference is now SHA-256
`63fc336ddba8730ec67adb576c3b89e5c8a1f47d3ffea7fa90a60756924d1327`
(was `f2a730ab…`). gxhash guarantees identical output across supported platforms
within a major version, so an x86_64 build with `+aes` must reproduce that same
SHA; a build without AES uses the fused FNV-1a path and will not.

## Driver benchmarks

| Benchmark | Phase 0 | Post-P5 | Post-swap | vs Phase 0 |
|---|---|---|---|---|
| `line_hash/short_line_40b` | 50.993 ns | 52.009 ns | 45.664 ns | 1.12x |
| `line_hash/long_line_2kb` | 3.2246 µs | 3.3351 µs | 1.6753 µs | 1.92x |
| `generate_anchors/Chunk/1000` | 118.31 µs | 54.890 µs | 49.417 µs | 2.39x |
| `generate_anchors/Chunk/10000` | 1.4059 ms | 648.53 µs | 580.82 µs | **2.42x** |
| `generate_anchors/Chunk/100000` | 14.990 ms | 6.9947 ms | 6.2496 ms | 2.40x |
| `format_hashline_content/full_read_10k` | 2.4406 ms | 952.70 µs | 864.88 µs | **2.82x** |
| `format_hashline_content/window_2k_of_100k` | 16.872 ms | 294.38 µs | 269.56 µs | 62.6x |
| `apply_edits/single_op_50k` | 11.642 ms | 964.58 µs | 947.75 µs | 12.28x |
| `apply_edits/batch_8ops_50k` | 70.652 ms | 1.0073 ms | 966.65 µs | 73.1x |
| `apply_edits/stale_anchor_error_path_50k` | 8.2990 ms | 447.22 µs | 433.19 µs | 19.16x |
| `grep_large_file/rare_literal` | — | 246.91 µs | 248.06 µs | (flat) |
| `index/new_partial_one_span_50k` | — | 57.864 µs | 54.447 µs | — |
| `dispatch/read_300_lines` | 102.07 µs | 44.948 µs | 39.372 µs | 2.59x |
| `dispatch/edit_single_op_300_lines` | 74.981 µs | 24.415 µs | 21.387 µs | 3.51x |

The swap buys 9-10% on every hash-bound path (`generate_anchors` -10.1%, full
10k read -9.0%, windowed read -9.1%, end-to-end dispatch -13% to -16%) and
essentially nothing on grep, which hashes only the handful of lines it renders.
Long lines gain most: `line_hash/long_line_2kb` halves.

**Acceptance criterion 2 remains unmet**: 2.42x against a >= 5x target
(<= 281.2 µs). Criterion 1's full-10k-read clause is 2.82x against >= 3x — closer
than before the swap, still short. Criteria 1 (window), 3, and 4 stay met.

## What the exactness requirement cost

The brief required the normalized hash input to match the old
`is_ascii_whitespace` definition for **all** inputs, including form feed.
`u8::is_ascii_whitespace` matches five bytes and `memchr3` searches three, so
the two it cannot see (`\n`, form feed) must be excluded some other way.
Measured on the realistic corpus:

| Cell | Median | vs fused-FNV baseline (577.62 µs) |
|---|---|---|
| `c_segments+gxhash32` (no guard — inexact) | 481.10 µs | 1.20x |
| `c_guarded+gxhash32` (per-line `memchr2` guard) | 625.97 µs | **0.92x** |
| `c_ff_guarded+gxhash32` (per-line form-feed guard) | 623.46 µs | 0.93x |

A guard **per line** costs ~14.5 ns/line and turns the swap into an 8%
regression. The shipped design therefore establishes the same fact **per
buffer**: `FileIndex::new` and the full-coverage `new_partial` scan the content
once for form feed (`\n` cannot occur inside lines split on `\n`), and partial
indexes scan only the lines they are about to hash. That keeps exactness and
most of the win — the delivered 10% rather than the unguarded 20%.

## Anchor quality is unchanged

Anchors expose only `hash_len` letters of `mod 26` entropy, so what matters is
the low-byte distribution, not the 32-bit avalanche. Measured over 400 seeds of
40-line files (16,000 line samples), share of lines whose local anchor is unique
within its file:

| `hash_len` | FNV-1a | gxhash32 |
|---|---|---|
| 1 | 21.5% | 21.5% |
| 2 | 94.6% | 94.1% |
| 3 | 99.8% | 99.6% |

Distinct-anchor counts over 20/100/1,000/10,000-line corpora are likewise within
noise of each other (both saturate the 676-value space at `hash_len` 2). The
swap does not make ambiguous suffix recovery or false anchor validation more
likely.
