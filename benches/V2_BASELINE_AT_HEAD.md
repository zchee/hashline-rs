# v2 Wired-Path Baseline at HEAD

Wave 0 artifact of `.omc/plans/2026-08-03-v2-hotpath-max-optimization.md`.
Every number below was produced by a Criterion bench that dispatches the
production v2 engine (`HashlineServer::dispatch` -> `run_read` / `run_edit` /
`run_grep`, or the exact `apply_versioned_reference_edits` call `run_edit`
makes) and asserts the tool outcome in the bench body, so a rejection or an
unexpected error can never masquerade as a timing (AC26).

## Provenance

| Item | Value |
|---|---|
| Commit (both hosts) | `1ead4a6ff52a647fa3b4cfb8a814203c6febd98d` |
| macOS host | Apple M3 Max (16 threads), Darwin 27.0.0 arm64, 128 GiB |
| Linux host | Intel Xeon Platinum 8481C (44 threads), 6.12.100+deb13-cloud-amd64 x86_64, 172 GiB, via `ssh debian-13-trixie.gaudiy-platform` |
| rustc (both hosts) | `1.99.0-nightly (11177f223 2026-08-02)` |
| Flags | `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` explicitly unset on both hosts (the macOS shell ambiently exports `-C target-cpu=apple-m3 ...`; it is stripped for every build and measurement). Committed `.cargo/config.toml` `+aes` matrix applies as shipping build config. |
| Profile | `bench` inherits `release`: `lto = "fat"`, `codegen-units = 1`, as committed |
| Runner | `cargo bench --bench hashline -- '<anchored-group-regex>'`, one group at a time, strictly sequential; Linux ran after macOS finished |
| Raw artifacts | `benches/artifacts/v2-baseline-at-head/{darwin-arm64,linux-x86_64}/` (criterion logs, resource JSON, environment manifests) |

## Cold/warm labeling

- **warm** — the process snapshot cache holds the fixture (steady-state
  repeat read).
- **cold** — the fixture is evicted from the process snapshot cache before
  every timed iteration (`BatchSize::PerIteration`; batched setup would run
  all evictions before the first timed call). OS page cache stays warm; pure
  filesystem-cold I/O remains the phase0 `filesystem` probe's domain.
- Eviction and file resets go through canonicalized paths: the server
  canonicalizes its root, so cache keys are canonical paths and eviction via
  the raw tempdir path silently no-ops (macOS `/var` vs `/private/var`) —
  the first capture of this baseline hit exactly that and was re-measured.

## Criterion medians at HEAD

| Bench ID | State | macOS arm64 | Linux x86_64 |
|---|---|---:|---:|
| `dispatch/read_300_lines` | warm | 26.84 µs | 33.03 µs |
| `dispatch/read_300_lines_cold` | cold | 48.06 µs | 42.71 µs |
| `dispatch/edit_single_op_300_lines` | e2e, durability=full | 9.53 ms | 126.33 µs |
| `v2_read/full_10k` (6 pages via cursor) | warm | 4.67 ms | 9.60 ms |
| `v2_read/window_2k_of_100k_warm` | warm | 123.20 µs | 162.90 µs |
| `v2_read/window_2k_of_100k_cold` | cold | 1.62 ms | 2.50 ms |
| `v2_read/cursor_page_50k` (page 2) | warm | 4.25 ms | 9.04 ms |
| `v2_edit/single_op_50k_apply` | CPU apply | 4.06 ms | 8.21 ms |
| `v2_edit/batch_8ops_50k_apply` | CPU apply | 4.09 ms | 8.15 ms |
| `v2_edit/single_op_50k_e2e_full` | e2e, durability=full | 15.00 ms | 10.89 ms |
| `v2_edit/batch_8ops_50k_e2e_full` | e2e, durability=full | 16.41 ms | 10.84 ms |
| `v2_edit/conflict_50k` | conflict path | 4.37 ms | 8.75 ms |
| `v2_grep/dense_file_capped` | single dense file | 1.77 ms | 2.05 ms |
| `grep/rare_literal` (2,000-file tree) | warm page cache | 15.05 ms | 4.64 ms |
| `grep/common_literal` | warm page cache | 4.36 ms | 1.41 ms |
| `grep/anchored_regex` | warm page cache | 13.50 ms | 3.75 ms |
| `grep_large_file/rare_literal` (50k-line file) | warm page cache | 1.71 ms | 2.26 ms |
| `grep_large_file/common_literal` | warm page cache | 2.84 ms | 3.73 ms |
| `grep_large_file/anchored_regex` | warm page cache | 1.87 ms | 2.64 ms |

Sample counts and confidence intervals are in the per-group logs under the
raw artifact paths.

## Allocation shape (phase0-resources, wired scenarios, single cold shot)

| Scenario | Host | allocated_bytes | alloc calls | peak_live_bytes | output_bytes |
|---|---|---:|---:|---:|---:|
| `v2_read_full_10k` (full pagination) | macOS | 1,914,080 | 128 | 630,731 | 580,395 |
| `v2_read_full_10k` | Linux | 1,911,944 | 128 | 630,163 | 580,395 |
| `v2_edit_single_op_50k` (e2e incl. persist) | macOS | 12,264,339 | 44 | 7,277,706 | 217 |
| `v2_edit_single_op_50k` | Linux | 12,263,431 | 44 | 7,277,274 | 217 |
| `tree_grep_base` | macOS | 7,824,097 | 24,995 | 1,784,590 | 15,197 |
| `tree_grep_base` | Linux | 3,291,727 | 8,871 | 1,342,378 | 16,392 |

A one-line edit of a ~1.2 MiB file allocates ~12.26 MiB (>10x the file) in
44 calls — the plan §1.2 copy/reread pipeline made visible (Wave 2 target).

## AC2–AC12 coverage map (Wave 0 exit gate)

| AC | Target | Wired bench | Base (macOS / Linux) |
|---|---|---|---|
| AC2 | full 10k read <= 350 µs | `v2_read/full_10k` | 4.67 ms / 9.60 ms |
| AC3 | cold 2k-of-100k <= 175 µs | `v2_read/window_2k_of_100k_cold` | 1.62 ms / 2.50 ms |
| AC4 | warm 2k-of-100k <= 90 µs | `v2_read/window_2k_of_100k_warm` | 123 µs / 163 µs |
| AC5 | one-op 50k CPU apply <= 200 µs | `v2_edit/single_op_50k_apply` | 4.06 ms / 8.21 ms |
| AC6 | eight-op 50k CPU apply <= 250 µs | `v2_edit/batch_8ops_50k_apply` | 4.09 ms / 8.15 ms |
| AC7 | rare large-file grep <= 180 µs | `grep_large_file/rare_literal` | 1.71 ms / 2.26 ms |
| AC8 | capped grep <= 1.5 ms, exactly 200 | `v2_grep/dense_file_capped` | 1.77 ms / 2.05 ms, **matches=10000** |
| AC9 | anchored regex grep <= 1.0 ms | `grep_large_file/anchored_regex` | 1.87 ms / 2.64 ms |
| AC10 | cold tree grep regression bound | `grep/*` (tree) | recorded warm-page-cache steady state; paired base/candidate runs at wave gates |
| AC11 | warm tree grep >= 1.5x | `grep/*` (tree) | 15.05/4.36/13.50 ms — 4.64/1.41/3.75 ms |
| AC12 | dispatch read 300: cold <= 45 µs, warm <= 30 µs | `dispatch/read_300_lines{_cold,}` | cold 48.1/42.7 µs; warm 26.8/33.0 µs |
| AC21 | page k+1 <= 1.2x cold window | `v2_read/cursor_page_50k` | 4.25 ms vs 1.62 ms cold window (2.6x; O(file) rescan debt) |

Every AC target row above executes production code with an asserted outcome.
The base numbers are the debt the later waves must burn down; none of the
AC thresholds is claimed as passing at HEAD except warm AC12 on macOS.

## Recorded deviations and known-failing state at HEAD

1. **AC8 cap is not enforced at HEAD.** `assemble_output` stops only between
   files, so the single dense file renders all 10,000 match lines and the
   summary reports `matches=10000 truncated=false`
   (plan §1.2, src/grep.rs). `v2_grep/dense_file_capped` therefore pins the
   wired response to itself (setup verifies summary counter == rendered
   match lines) instead of asserting `matches=200`; Wave 3 tightens the
   bench to the exact-cap contract when the per-file budget lands.
2. **Pre-existing Linux-only lib test failures at the baseline** (present at
   `9407afb` before any Wave 0 change, reproduced on the Linux host):
   `persist::tests::atomic_write_detects_external_change` and
   `snapshot::tests::phase2_stable_read_rejects_two_mutated_attempts`.
   Same-length rewrites landing within one coarse file-timestamp tick
   (Debian cloud kernel, effectively ~4 ms granularity) produce identical
   `FileStamp`s, so stamp-based external-change detection misses them.
   macOS passes the same tests. This is a real platform sensitivity of the
   stamp mechanism that Waves 1–2 (cursor/stamp/persist work) must address
   or explicitly scope; it is recorded here, not silently waived.
3. `dispatch/edit_single_op_300_lines` keeps its historical ID but now
   measures a valid v2 edit (the old payload was rejected by
   `EditRequest` deserialization, so pre-Wave-0 numbers under this ID
   measured serde rejection and are not comparable).

## Exact commands

```sh
# per group, strictly sequential, both hosts (RUSTFLAGS unset):
cargo bench --bench hashline -- '^dispatch/'
cargo bench --bench hashline -- '^v2_read/'
cargo bench --bench hashline -- '^v2_edit/'
cargo bench --bench hashline -- '^v2_grep/'
cargo bench --bench hashline -- '^grep/'
cargo bench --bench hashline -- '^grep_large_file/'
# allocation shape:
cargo bench --bench phase0-resources -- measure v2_read_full_10k
cargo bench --bench phase0-resources -- measure v2_edit_single_op_50k
cargo bench --bench phase0-resources -- measure tree_grep_base
```
