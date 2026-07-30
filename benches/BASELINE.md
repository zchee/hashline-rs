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

## `hashline_grep` (2,000-file synthetic tree fixture, chunk scheme)

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
- **`hashline_grep`**, which is the one tool whose cost scales with repository
  size rather than a single file's size — all three query shapes land in the
  13-15 ms band over a 2,000-file/~85,900-line fixture even before any
  haystack-level matching (F5) is applied.

This does not by itself demand re-scoping Phase ordering away from Phases
1-2 (they still remove the dominant allocation/pass-count cost for large
single-file reads/edits), but it does confirm grep and large-file paths are
where Phases 1-3 acceptance criteria (§ Acceptance Criteria 1-4) will show
the largest wins, while realistic small-file interactive latency has no
headroom problem today.
