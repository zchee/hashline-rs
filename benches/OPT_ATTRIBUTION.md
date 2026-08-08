# W0 Attribution Baseline — limit-optimization plan

Wave 0 artifact of `.omc/plans/2026-08-08-limit-optimization.md`. No
production code changed in this wave; `tests/alloc_probe.rs` (measurement
harness) is the only addition.

## Provenance

| Item | Value |
|---|---|
| Commit (both hosts) | `ccd5321` |
| macOS host | Apple M3 Max, Darwin 27.0.0 arm64 |
| Linux host | Intel Xeon Platinum 8481C, x86_64, via `ssh debian-13-trixie.gaudiy-platform` |
| Toolchain | `nightly-2026-08-07` (macOS explicit; Linux via rust-toolchain.toml) |
| Flags | `RUSTFLAGS` unset via `env -u RUSTFLAGS`; committed bench profile (`lto = "fat"`, `codegen-units = 1`) |
| Runner | `cargo bench --bench hashline`, full suite, strictly sequential, host otherwise idle |
| Alloc probe | `cargo test --test alloc_probe -- --nocapture` (counting global allocator; realloc counts grown bytes only) |

Attribution method: the suite already contains phase-isolating groups
(`phase2_snapshot` = full snapshot construction incl. UTF-8+NUL+offsets+hash;
`phase2_offsets` = line-offset materialization; `phase2_version` = identity
hash alone), so per-phase cost is derived by cell algebra against the wired
medians, cross-checked with the code-verified pass inventory
(src/edit/mod.rs, src/read.rs, src/grep.rs). `xctrace` is available on the
macOS host but was not needed: the isolating cells bound every phase tightly
enough to place the waves' targets.

## Criterion medians at ccd5321

| Bench ID | macOS arm64 | Linux x86_64 |
|---|---:|---:|
| `dispatch/read_300_lines` (warm) | 26.61 µs | 38.13 µs |
| `dispatch/read_300_lines_cold` | 46.29 µs | 42.62 µs |
| `dispatch/edit_single_op_300_lines` (e2e full) | 10.30 ms | 126.60 µs |
| `wired_read/full_10k` (6 cursor pages) | 4.62 ms | 8.82 ms |
| `wired_read/window_2k_of_100k_warm` | 123.27 µs | 154.92 µs |
| `wired_read/window_2k_of_100k_cold` | 1.60 ms | 2.47 ms |
| `wired_read/cursor_page_50k` | 4.17 ms | 8.22 ms |
| `wired_edit/single_op_50k_apply` | 4.17 ms | 8.15 ms |
| `wired_edit/batch_8ops_50k_apply` | 4.02 ms | 8.16 ms |
| `wired_edit/single_op_50k_e2e_full` | 15.71 ms | 10.79 ms |
| `wired_edit/batch_8ops_50k_e2e_full` | 15.55 ms | 10.79 ms |
| `wired_edit/conflict_50k` | 4.43 ms | 8.77 ms |
| `wired_grep/dense_file_capped` | 1.71 ms | 2.03 ms |
| `wired_write/create_10k_e2e_full` | 9.95 ms | 951.98 µs |
| `wired_write/replace_10k_e2e_full` | 10.32 ms | 986.71 µs |
| `wired_write/create_conflict` | 310.57 µs | 498.40 µs |
| `wired_glob/tree_recursive_rs` | 9.13 ms | 5.20 ms |
| `grep/rare_literal` (2,000-file tree) | 14.34 ms | 4.68 ms |
| `grep/common_literal` | 4.39 ms | 1.41 ms |
| `grep/anchored_regex` | 13.21 ms | 3.80 ms |
| `grep_large_file/rare_literal` | 1.54 ms | 2.21 ms |
| `grep_large_file/common_literal` | 2.81 ms | 4.62 ms |
| `grep_large_file/anchored_regex` | 1.77 ms | 2.66 ms |
| `phase2_snapshot/10000/candidate_snapshot` | 30.12 µs | 45.13 µs |
| `phase2_snapshot/50000/candidate_snapshot` | 152.03 µs | 300.70 µs |
| `phase2_validation/50000/safe_snapshot` | 171.56 µs | 321.59 µs |
| `phase2_validation/50000/simd_validated_unchecked_snapshot` | 156.35 µs | 296.29 µs |
| `phase2_version/multimegabyte/xxh3_128_with_seed` | 57.79 µs | 109.39 µs |
| `phase2_version/multimegabyte/blake3_128` | 1.12 ms | 406.25 µs |
| `phase2_offsets/construction_50k/full_u64` | 476.95 µs | 610.59 µs |
| `phase2_offsets/cold_window_2k_of_100k/full_u64` | 996.47 µs | 1.24 ms |

Raw logs: macOS `scratchpad/bench-w0-macos.log` (session dir), Linux
`~/rust/src/github.com/zchee/hashline-rs/bench-w0.log` on the Linux host.

## Allocation shape (alloc probe, macOS, current-thread runtime)

| Scenario | file bytes | allocated | ratio | alloc calls |
|---|---:|---:|---:|---:|
| wired edit, single op, 50k lines | 2,027,780 | 10,941,031 | **5.40x** | 31 |
| wired read, full 10k via cursor | 387,780 | 1,761,526 | **4.54x** | 148 |

## Attribution (macOS algebra; Linux ratios agree within noted spans)

### Edit, single op, 50k lines (~2.0 MB)

| Phase | macOS | Linux | Basis |
|---|---:|---:|---|
| Full snapshot construction (UTF-8+NUL+offsets+hash) | ~152 µs | ~301 µs | `phase2_snapshot/50000` |
| Identity hash alone (~2 MB) | ~58 µs | ~109 µs | `phase2_version/multimegabyte` |
| Offsets alone (50k) | ~477 µs | ~611 µs | `phase2_offsets/construction_50k` |
| **Apply CPU total** | **4.17 ms** | **8.15 ms** | `wired_edit/single_op_50k_apply` |
| ⇒ reference validation + splice overhead | **~3.5 ms (≈85%)** | **~7.2 ms (≈88%)** | apply − construction |
| Persist (+ stamped re-read) = e2e − apply | ~11.5 ms | ~2.6 ms | macOS is F_FULLFSYNC-class-bound |

The apply path spends ~27x the cost of a complete snapshot construction on
reference validation/splicing. W2's specialized-engine floor estimate:
position validation via offsets (~0.5 ms worst case) + splice memcpy
(~0.1 ms) + response snapshot via validated-bytes (~0.06 ms hash + line
count) ≈ **0.7–1.0 ms macOS**, making the ≥3x target conservative.

W1's copy elimination targets the 5.40x alloc ratio: source `to_vec` (1.0x)
+ `applied.clone()` (1.0x) + post-persist disk re-read (1.0x) are pure waste
on top of the unavoidable ~2x (applied buffer + response text).

### Read, full 10k via cursor (6 pages, 388 KB in / 580 KB out)

| Phase | macOS | Linux | Basis |
|---|---:|---:|---|
| Snapshot construction 10k | ~30 µs | ~45 µs | `phase2_snapshot/10000` |
| **Full pagination total** | **4.62 ms** | **8.82 ms** | `wired_read/full_10k` |
| ⇒ per-page cursor reference validation + render | ~765 µs/page | ~1.46 ms/page | (total − construction) / 6 |

Cursor validation re-derives line boundaries per page through the reference
path (src/read.rs:96-105); offsets binary search plus render should land
near the render floor (~1–1.5 ms macOS for 580 KB of output). W2's ≥2x
target is conservative.

### Grep tree (2,000 files, rare literal): 14.34 ms macOS / 4.68 ms Linux

Dense single-file capped search costs 1.71 / 2.03 ms, so the tree run is
dominated by per-file overheads (open/stat/read + classify + snapshot/hash +
render for matching files); the 3x macOS-vs-Linux divergence is
filesystem/syscall cost, not search CPU. Discovery modes
(`files_with_matches`/`count`) pay full content-mode construction today and
discard it (src/grep.rs search_file always builds Snapshot + body); W3
removes snapshot/offsets/render/hash from those modes entirely.

### fsync-bound ceilings (unmovable before W5)

macOS e2e: edit 300-line 10.30 ms, write create 9.95 ms — fsync-policy
bound; the same operations on Linux are 126.60 µs / 951.98 µs. Only the W5
durability knob moves the macOS numbers; no CPU target is set against them.

## Wave targets restated against this baseline

| Wave | Metric | Baseline (macOS / Linux) | Target |
|---|---|---|---|
| W1 | edit 50k alloc ratio / calls (probe) | 5.40x / 31 | ≤3.5x / ≤20 |
| W1 | Linux `dispatch/edit_single_op_300_lines` | 126.60 µs | ≤90 µs |
| W2 | `wired_edit/single_op_50k_apply` | 4.17 / 8.15 ms | ≥3x both hosts (macOS ≤1.35 ms) |
| W2 | `wired_read/full_10k` | 4.62 / 8.82 ms | ≥2x both hosts |
| W2 | `wired_read/cursor_page_50k` | 4.17 / 8.22 ms | ≥2x both hosts |
| W2 | `wired_edit/conflict_50k` | 4.43 / 8.77 ms | within ±2% |
| W3 | tree discovery modes vs content baseline | 14.34/4.39/13.21 ms (macOS) | ≥2x |
| W3 | tree content mode | same | ≥1.2x |
| W3 | `grep_large_file/*` | 1.54–2.81 / 2.21–4.62 ms | ≥1.15x |
| W4 | `dispatch/read_300_lines_cold` | 46.29 / 42.62 µs | ≥1.4x macOS (≤33 µs) |
| W4 | `wired_read/window_2k_of_100k_cold` | 1.60 / 2.47 ms | ≥1.3x |
| W5 | macOS `dispatch/edit_single_op_300_lines` (barrier) | 10.30 ms (full) | ≤1.5 ms; full unchanged ±2% |
| W6 | any two wired benches | — | ≥5% or drop-with-note |
| All | every untargeted wired bench | — | within ±2% per host |
