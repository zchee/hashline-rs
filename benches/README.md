# Benchmarks

`benches/hashline.rs` is the single benchmark target. It runs on
[Divan](https://github.com/nvzqz/divan) through
[`codspeed-divan-compat`](https://github.com/CodSpeedHQ/codspeed-rust), which
is declared in `Cargo.toml` as `divan = { package = "codspeed-divan-compat" }`.
One source file therefore feeds two harnesses:

- `cargo bench` runs the stock Divan walltime harness locally.
- `cargo codspeed build && cargo codspeed run` rebuilds the same bodies with
  `--cfg codspeed` and hands them to CodSpeed's instrumentation. CI does this on
  every push and pull request (`.github/workflows/codspeed.yaml`), in
  `simulation` mode.

Simulation mode counts simulated CPU work, so the fsync-bound benches
(`wired_write::*_e2e_full`, `wired_edit::*_e2e_full`, `dispatch::edit_*`) and
the directory-walk-bound ones (`grep::*`, `wired_glob::*`) report their compute
cost there, not their wall-clock latency. Latency questions belong to a local
`cargo bench` run on a quiet machine.

## Running locally

```console
# Everything.
cargo bench --bench hashline

# One module or one benchmark; the filter is a regex over the full path.
cargo bench --bench hashline -- 'wired_edit'
cargo bench --bench hashline -- '^hashline::grep::rare_literal'

# Execute every body exactly once and assert the contracts, without timing.
cargo bench --bench hashline -- --test

# Override the per-benchmark sampling that the attributes pin.
cargo bench --bench hashline -- --sample-count 10
```

Build flags matter. Never set `RUSTFLAGS` (including `target-cpu=native`) for a
measurement run, and never run two benchmark processes concurrently.

## Structure and invariants

Benchmarks are grouped by Rust module, and Divan derives the reported path from
`module_path!`. Parameterized cases use `args`, which renders as
`module::function[arg]`.

Two invariants keep the wired benchmarks honest; both are easy to break by
accident.

**`sample_size = 1` on every benchmark whose body mutates state.** Divan
generates a whole sample's inputs *before* it starts timing that sample. With
any larger sample size, all of a sample's `with_inputs` resets would run up
front and every iteration after the first would observe an already-edited file,
silently measuring the snapshot-conflict path instead of the edit path. The
affected benchmarks are the cold reads, every `_e2e_full` variant, and both
`_apply` variants.

**Each mutating benchmark owns its own fixture file.** Divan's registration
order is not source order, and under CodSpeed it differs again, so no benchmark
may depend on another having run first. `dispatch` therefore writes `read.rs`,
`edit.rs`, and `edit_barrier.rs` separately, `wired_edit` uses `single_op.rs` /
`batch_ops.rs` / `conflict.rs`, and `wired_write` uses `created.rs` /
`replaced.rs` / `occupied.rs`.

Every wired body also asserts its tool outcome, so a silent rejection can never
be recorded as a timing.

## Reading the historical baselines

The v2 redesign archive — `V2_BASELINE.md`, `V2_RESULTS.md`,
and the hash-pinned evidence set `benches/evidence/v2/` (self-verifying
via `sha256sum -c SHA256SUMS`) — lives in git history only; the last
tree containing it is commit fdfff03.

`BASELINE.md`, `V2_BASELINE_AT_HEAD.md`, `OPT_RESULTS.md`, and
`OPT_ATTRIBUTION.md` were captured with Criterion and name benchmarks in
Criterion's `group/bench` form. The measurements stand; only the names moved:

| Criterion name | Divan path |
|---|---|
| `grep/rare_literal` | `grep::rare_literal[content]` |
| `grep/rare_literal_files` | `grep::rare_literal[files]` |
| `grep_large_file/anchored_regex` | `grep_large_file::anchored_regex[content]` |
| `phase2_snapshot/50000/candidate_snapshot` | `phase2_snapshot::candidate_snapshot[50000]` |
| `phase2_validation/10000/safe_snapshot` | `phase2_validation::safe_snapshot[10000]` |
| `phase2_version/short/xxh3_128_with_seed` | `phase2_version::xxh3_128_with_seed[short]` |
| `phase2_offsets/construction_50k/sparse_128` | `phase2_offsets::construction_50k::sparse[128]` |
| `dispatch/read_300_lines` | `dispatch::read_300_lines` |
| `wired_read/full_10k` | `wired_read::full_10k` |

The corpora are unchanged: the same deterministic xorshift32 generator with the
same seeds, so a Divan number and a Criterion number describe the same work.
Divan reports median and mean over samples where Criterion reported a
confidence interval, so compare medians.
