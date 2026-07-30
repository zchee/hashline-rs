# Incompatible v2 Phase 0 Baseline

This file records the reproducibility contract for Phase 0 of
`.omx/plans/2026-07-31-incompatible-max-performance-redesign.md`. That plan is
the sole source of truth. This document must not be used to authorize Phase 1.

## Status

The harness is ready for canonical capture. Performance results are not accepted
until both platform artifact sets pass:

```sh
python3 benches/support/phase0.py evaluate \
  --goal-root .omx/goals/performance/hashline-v2-phase0
```

## Immutable provenance

| Item | Exact value |
|---|---|
| Plan baseline | `f3a2f3f41076fc48f3aa4836eda873b21f7a6be6` |
| Phase 0 harness commit | Recorded in every platform `manifest.json` |
| Local architecture | `aarch64-apple-darwin` |
| Comparison architecture | `x86_64-unknown-linux-gnu` |
| External Rust corpus | `tokio-rs/tokio@adc2ae7af2caaea83985fbdfbc7884c159c486f2` |
| Ambient compiler flags | `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` removed |
| Benchmark profile | Repository `bench` profile as-is |
| Symbol profile delta | `debug=2`, `strip=none`; optimization flags unchanged |

## Paired measurement contract

Each lower-bound target has a current base function and an incompatible-v2
prototype candidate in the same optimized binary and on the same deterministic
corpus. The capture harness runs at least three rounds in alternating order:

```text
round 0: base -> candidate
round 1: candidate -> base
round 2: base -> candidate
```

Criterion median point estimates and 95 percent confidence intervals are
preserved for every individual run. This same-binary design prevents compiler
or link-layout changes from being confused with an algorithmic lower bound.

The suite covers:

- normalized current index versus raw per-line hashing;
- normalized current index versus gxhash128, XXH3-128, and BLAKE3-128 plus line
  count at 10k and 50k lines;
- current partial index versus sparse positional selection;
- current full index versus `Vec<u32>` and `Vec<u64>` line offsets;
- current anchor rendering versus position-only full and window rendering;
- current full read versus versioned positional full read;
- current one/eight-op edit versus byte-range splice;
- current direct persistence versus same-directory temporary write and rename.

## Filesystem, memory, and profile controls

| Evidence | macOS/arm64 | Linux/amd64 |
|---|---|---|
| Cold file read | `fcntl(F_NOCACHE)=1` on timed descriptor | `posix_fadvise(POSIX_FADV_DONTNEED)` before timed read |
| Warm file read | Full pre-read before timed descriptor | Full pre-read before timed descriptor |
| Allocation counts | Bench-only counting global allocator | Same implementation |
| Peak RSS | `/usr/bin/time -l` | `/usr/bin/time -v` |
| Symbolized stacks | `sample`, 5 seconds, 1 ms | `perf record -g --call-graph dwarf` |
| Profile build | Bench profile with debug symbols, no stripping | Same |

Cold and warm samples are never mixed. Base and candidate use the same cache
policy and alternate within each state.

## Canonical capture commands

macOS/arm64:

```sh
python3 benches/support/phase0.py capture \
  --goal-root .omx/goals/performance/hashline-v2-phase0 \
  --external-repo /Users/zchee/rust/src/github.com/tokio-rs/tokio \
  --rounds 3 \
  --filesystem-samples 12 \
  --profile-seconds 8
```

Linux/amd64 uses the same script, candidate commit, Tokio commit, round count,
sample count, profile duration, and ambient-flag policy. Only the paths and
platform-specific cache/profile tools differ.

## Raw artifact layout

```text
.omx/goals/performance/hashline-v2-phase0/
+-- artifacts/
|   +-- macos-arm64/
|   |   +-- latest.json
|   |   +-- runs/<timestamp>-<candidate>/
|   +-- linux-amd64/
|       +-- latest.json
|       +-- runs/<timestamp>-<candidate>/
+-- evaluation.json
```

Each immutable run contains environment and corpus manifests, the clean exact
baseline run, the full candidate run, interleaved pair estimates, cold/warm
samples, allocation/RSS records, four symbolized profiles, quality-command
logs, raw Criterion trees, and `SHA256SUMS.json`.

## Phase boundary

This harness adds benchmark-only prototypes and evidence tooling. It does not
change the shipping protocol, anchors, read/edit/grep implementation, or any
production module. A failed evaluator blocks Phase 1.
