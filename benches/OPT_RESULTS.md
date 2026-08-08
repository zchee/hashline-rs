# Limit-Optimization Results (W0 → W7)

Final artifact of `.omc/plans/2026-08-08-limit-optimization.md`. Baseline
(W0) is `benches/OPT_ATTRIBUTION.md` at `ccd5321`; the "after" column is the
full sequential recapture at `f43749b` on the same two hosts, same method
(`env -u RUSTFLAGS`, suite run whole, host otherwise idle).

## Wired medians, before → after

| Bench ID | macOS W0 | macOS W7 | Linux W0 | Linux W7 |
|---|---:|---:|---:|---:|
| `dispatch/read_300_lines` (warm) | 26.61 µs | 27.24 µs | 38.13 µs | 32.73 µs |
| `dispatch/read_300_lines_cold` | 46.29 µs | 46.28 µs | 42.62 µs | 42.65 µs |
| `dispatch/edit_single_op_300_lines` (full) | 10.30 ms | 9.81 ms | 126.60 µs | **44.04 µs (2.9x)** |
| `dispatch/edit_single_op_300_lines_barrier` | — | **541 µs (19x vs full)** | — | 42.15 µs |
| `wired_read/full_10k` | 4.62 ms | **629 µs (7.3x)** | 8.82 ms | **827 µs (10.7x)** |
| `wired_read/window_2k_of_100k_warm` | 123.27 µs | 120.98 µs | 154.92 µs | 154.60 µs |
| `wired_read/window_2k_of_100k_cold` | 1.60 ms | 1.57 ms | 2.47 ms | 2.43 ms |
| `wired_read/cursor_page_50k` | 4.17 ms | **122 µs (34x)** | 8.22 ms | **158 µs (52x)** |
| `wired_edit/single_op_50k_apply` | 4.17 ms | **442 µs (9.4x)** | 8.15 ms | **690 µs (11.8x)** |
| `wired_edit/batch_8ops_50k_apply` | 4.02 ms | 445 µs | 8.16 ms | 686 µs |
| `wired_edit/single_op_50k_e2e_full` | 15.71 ms | **11.77 ms** | 10.79 ms | **2.25 ms (4.8x)** |
| `wired_edit/conflict_50k` | 4.43 ms | 4.26 ms | 8.77 ms | 8.83 ms |
| `wired_write/create_10k_e2e_full` | 9.95 ms | 10.24 ms | 951.98 µs | **664 µs (1.4x)** |
| `wired_write/replace_10k_e2e_full` | 10.32 ms | 10.65 ms | 986.71 µs | **809 µs** |
| `wired_grep/dense_file_capped` | 1.71 ms | 1.74 ms | 2.03 ms | 2.05 ms |
| `wired_glob/tree_recursive_rs` | 9.13 ms | 9.94 ms † | 5.20 ms | 5.28 ms |
| `grep/rare_literal` (2k tree) | 14.34 ms | 14.86 ms † | 4.68 ms | 4.61 ms |
| `grep/common_literal` | 4.39 ms | 4.46 ms | 1.41 ms | 1.40 ms |
| `grep/anchored_regex` | 13.21 ms | 13.63 ms † | 3.80 ms | 3.75 ms |
| `grep/*_files` (discovery, tree) | — | ≈ content | — | ≈ content |
| `grep_large_file/rare_literal` | 1.54 ms | 1.40 ms | 2.21 ms | 1.97 ms |
| `grep_large_file/rare_literal_files` | — | **868 µs (1.78x vs W0 content)** | — | **1.25 ms (1.78x)** |
| `grep_large_file/common_literal` | 2.81 ms | 2.61 ms | 4.62 ms | **3.48 ms (1.33x)** |
| `grep_large_file/common_literal_files` | — | **1.11 ms (2.53x)** | — | **1.81 ms (2.55x)** |
| `grep_large_file/anchored_regex` | 1.77 ms | 1.73 ms | 2.66 ms | 2.40 ms |
| `grep_large_file/anchored_regex_files` | — | **1.04 ms (1.71x)** | — | **1.51 ms (1.76x)** |

† macOS filesystem-walk cells (tree grep, glob) vary ±5–9% run-to-run on
**untouched code** — glob.rs and the tree-walk path changed in no wave of
this plan, and Linux shows the same cells within ±1.5%. The deltas are host
filesystem variance, not code-attributable regressions; every CPU-bound cell
is flat or improved on both hosts.

## Allocation shape (probe, release build)

| Scenario | W0 | W7 |
|---|---|---|
| wired edit, 50k lines | 5.40x file size / 31 calls | **2.10x / 23 calls** |
| wired read, full 10k | 4.54x / 148 calls | **2.48x / 133 calls** |

## Wave verdicts

| Wave | Verdict |
|---|---|
| W1 copies/hops | **Met** on alloc ratio (5.40x→2.40x, later 2.10x) and macOS e2e; calls ≤20 missed by 3 (fixed small-object allocs, not buffer copies); Linux e2e-300 unchanged at W1 (fsync-bound) — later met by W2's engine (44 µs) |
| W2 engine + offsets validation | **Met, exceeded everywhere**: apply 9.4x/11.8x, pagination 7.3x/10.7x, cursor 34x/52x; 1000-case differential + debug re-verification + Miri green; reference model untouched |
| W3 grep modes | **Met on large-file cells** (discovery 1.7–2.6x, content 1.0–1.33x); **missed-with-cause on tree cells** (walk/read/search-bound; quit_threshold already capped removable work) |
| W4 fused construction | **Dropped with attribution note**: remaining passes are <2% of any wired cell; cold reads are syscall-bound; the fusion would touch frozen validation error-precedence for sub-noise gains |
| W5 durability knob | **Met**: macOS edit-300 9.81 ms (full) vs 541 µs (barrier), default `full` pinned; Linux already 44 µs in full mode |
| W6 codegen slice | **Dropped with note** (per its explicit drop clause): no profiler-justified site promises ≥5% on two wired cells post-W5 |

## Verification inventory

- 115 lib + 1 e2e + 24 doctests green on macOS; Linux full suite green except
  the two documented pre-existing FileStamp-granularity flakes (fail at the
  W0 baseline too).
- Differential suite (engine vs frozen reference): 1000 randomized cases,
  byte- and error-payload-identical; debug builds re-run the reference on
  every fast-path success.
- Miri: unsafe engine path and validated-text round trip pass.
- Durability: three-mode persist tests green; default pinned by test and
  R019 doctest.
