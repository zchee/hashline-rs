# hashline-rs Incompatible Maximum-Performance Redesign Plan

- Status: ready for execution
- Mode: /plan --deliberate --direct
- Date: 2026-07-31
- Repository: /Users/zchee/rust/src/github.com/zchee/hashline-rs
- Baseline: main at f3a2f3f41076fc48f3aa4836eda873b21f7a6be6
- Compatibility policy: no source, CLI, wire-format, anchor-format, or persisted-anchor compatibility is required
- Delivery policy: this document plans the work only; it does not authorize implementation

## 1. Outcome

Replace the current stateless, whitespace-normalized, per-line hash protocol with a
versioned file-snapshot protocol whose line references carry positions, not content
hashes. The new protocol must keep the cross-tool safety property -- a reference
emitted by read or grep either edits the exact snapshot it names or is rejected --
while eliminating the normalization, line-hash arrays, contextual fingerprint
schemes, stale-anchor rescans, and whole-line pointer tables that now set the CPU
floor.

The target is not a local micro-optimization. It is a new fast-path architecture:

```
file bytes
   |
   +--> validate UTF-8 once --> SnapshotId (raw whole-file 128-bit hash)
   |                              |
   |                              +--> bounded snapshot cache
   |
   +--> newline count / lazy u32 offsets
                                  |
            +---------------------+----------------------+
            |                     |                      |
          read                  edit                   grep
     positional refs       versioned byte ranges    direct sink render
```

## 2. Requirements Summary

1. Optimize all three MCP tools, not only the hash microbenchmark:
   `hashline_read`, `hashline_edit`, and `hashline_grep`.
2. Backward compatibility is explicitly out of scope. The implementation may remove
   the three scheme choices, change CLI flags and JSON schemas, rotate every anchor,
   delete public types, and replace dependencies.
3. Preserve correctness under concurrent or external file changes. A stale snapshot
   must fail closed whenever pre-commit checks observe a mismatch, and same-server
   writes must be serialized. Document the irreducible final TOCTOU window for a
   noncooperating external writer because portable filesystems do not expose a
   content-version compare-and-swap primitive.
4. Optimize algorithmic work first, data layout second, I/O and runtime scheduling
   third, and compiler/profile tuning last.
5. Keep memory bounded under large repositories and concurrent grep workers.
6. Benchmark sequentially, with the shipping profile, without ambient `RUSTFLAGS`
   or `target-cpu=native`, as required by the repository Rust instructions.
7. No partial migration remains in the final tree. Once all tools use the v2
   snapshot protocol, remove the v1 scheme/hash/index implementation and its CLI.
8. Keep stdout reserved for MCP transport; all phase timing and cache telemetry goes
   to tracing on stderr.

## 3. Current Architecture and Measured Evidence

### 3.1 Already-landed optimization work

This plan starts after a substantial optimization campaign, not before it:

- `FileIndex` already shares line hashes within a request and supports partial
  spans (src/index.rs:320-331, src/index.rs:382-433).
- The gxhash path still materializes normalized bytes per line and hashes the
  scratch buffer (src/hash.rs:148-168, src/hash.rs:281-289).
- Read hashes only the requested scheme span (src/read.rs:57-65,
  src/read.rs:79-83).
- Rendering already writes anchors into one output string, but it first walks the
  range to sum content lengths, then walks it again to render
  (src/render.rs:51-78).
- Edit still splits the entire pre-edit file because it rebuilds a
  `Vec<&str>`, clones that pointer table, splices it, and builds a new full
  output string (src/edit/apply.rs:179-210, src/edit/apply.rs:334-382).
- Grep already searches whole haystacks with ripgrep's engine, but reads every file
  into a fresh vector, validates all bytes as UTF-8, records every match line,
  builds a partial index, and only then renders (src/grep.rs:205-238,
  src/grep.rs:255-285).
- Directory grep deliberately over-collects up to `max_matches * 50` before
  stopping workers (src/grep.rs:501-518).
- Every read and edit CPU phase uses `spawn_blocking`, including small files, to
  isolate partial-index panics (src/read.rs:165-184,
  src/edit/mod.rs:119-136).
- Server dispatch deserializes through `serde_json::Value` and wraps the complete
  response in one `String` content block (src/server.rs:319-357).
- The release profile already uses fat LTO and one codegen unit
  (Cargo.toml:67-70). The previous campaign measured and rejected mimalloc and
  `panic = "abort"` (benches/BASELINE.md:239-271).

These facts invalidate repeating the old plan's allocator swap, another contextual
hash implementation, or another full-file/partial-index pass. The remaining ceiling
is the protocol and data model.

### 3.2 Live Criterion measurements at the baseline commit

Environment: Apple M3 Max, aarch64-apple-darwin, Rust 1.99.0-nightly
(2026-07-29), `cargo bench` as-is, sequential. Point estimates are the middle
of Criterion's reported confidence interval.

| Workload | Live point estimate |
|---|---:|
| `line_hash/short_line_40b` | 46.688 ns |
| `line_hash/long_line_2kb` | 1.7185 us |
| `generate_anchors/ContentOnly/10000_lines` | 588.12 us |
| `generate_anchors/Chunk/10000_lines` | 604.52 us |
| `generate_anchors/Checkpoint/10000_lines` | 600.19 us |
| `format_hashline_content/full_read_10k_lines` | 887.19 us |
| `format_hashline_content/window_2k_of_100k_lines` | 271.85 us |
| `index/new_partial_one_span_50k` | 55.264 us |
| `index/count_lines_only_50k` | 46.834 us |
| `index/visit_newlines_50k` | 409.33 us |
| `index/full_new_50k` | 2.9260 ms |
| `apply_edits/single_op_50k_lines` | 997.18 us |
| `apply_edits/batch_8ops_50k_lines` | 1.0297 ms |
| `apply_edits/stale_anchor_error_path_50k_lines` | 440.84 us |
| `grep_large_file/rare_literal` | 253.57 us |
| `grep_large_file/common_literal` | 3.4830 ms |
| `grep_large_file/anchored_regex` | 1.3485 ms |
| `grep/rare_literal` (2,000 files) | 14.079 ms |
| `grep/common_literal` (2,000 files) | 14.944 ms |
| `grep/anchored_regex` (2,000 files) | 14.421 ms |
| `dispatch/read_300_lines` | 45.938 us |
| `dispatch/edit_single_op_300_lines` | 27.280 us |

The run showed small 1-7 percent movements versus Criterion's saved baseline on
many rows. Those are not attributed to source changes: HEAD and the worktree were
unchanged. Acceptance gates must compare interleaved base/candidate builds, not a
single historical delta.

### 3.3 Isolated incompatible-design probe

A repository-external release-mode probe used the current crate plus `gxhash`,
`memchr`, and `itoa`. It was deleted after the run. Its corpus was code-like,
included one approximately 2 KiB line per 211 lines, and measured medians over 21
batches. The probe is directional evidence, not a replacement for Criterion.

| Operation | Corpus | Median |
|---|---:|---:|
| Current `FileIndex::new` | 10k lines / 570,588 B | 448.844 us |
| Raw per-line `gxhash32`, no normalization | same | 76.192 us |
| Position-only full render | same | 245.541 us |
| SIMD newline count | 50k lines / 2,891,297 B | 57.142 us |
| Materialize `Vec<u32>` line offsets | same | 356.557 us |
| Raw whole-file token plus line count | same | 88.272 us |
| One byte-range splice into a new vector | same | 38.469 us |

Conclusions:

1. Removing whitespace normalization makes the paired per-line hash path about
   5.9x faster on the same probe corpus.
2. A single raw whole-file version token plus line count is dramatically cheaper
   than building hashes for every line.
3. Materializing all newline positions remains much more expensive than counting
   them, matching the existing 46.8 us versus 409 us Criterion evidence.
4. Full read speed will eventually be limited by output construction and copying:
   the probe's position-only render already costs 245.5 us.
5. Edit CPU can be reduced by an order of magnitude if an already-versioned byte
   range replaces line-vector reconstruction.

A five-second macOS `sample` capture was attempted on the optimized Criterion
binary. Fat LTO plus symbol stripping prevented useful Rust symbol attribution,
but the visible native frames contained substantial `memmove` activity. The
benchmark decomposition and paired probe are therefore the primary attribution
evidence; the implementation phase must add symbolized profiling artifacts rather
than infer from stripped addresses.

## 4. RALPLAN-DR Deliberation Summary

### 4.1 Principles

1. One snapshot identity per file state; never repeat identity material on every
   line.
2. Never normalize or hash bytes unless the v2 correctness contract requires it.
3. Position lookup is lazy and compact; pay for a full offset table only when the
   request or cache reuse justifies it.
4. Fail closed on every observed mismatch and serialize same-server writes; state
   the residual noncooperating-writer TOCTOU limit explicitly.
5. Every optional complexity -- cache, mmap, unsafe conversion, PGO, CPU-specific
   artifact -- is benchmark-gated and removable.

### 4.2 Top decision drivers

| Priority | Driver | Why it dominates |
|---:|---|---|
| 1 | Large-file read/edit CPU latency | Current per-line normalization and pointer tables dominate CPU even after six optimization waves. |
| 2 | Cold and warm repository grep separately | Cold grep is open/read/walk bound; warm grep can benefit from reuse, so one blended number would mislead. |
| 3 | Snapshot correctness under mutation | Removing local/context hashes is acceptable only if version validation is stronger and simpler than the old stale-anchor recovery. |

### 4.3 Viable options

#### Option A: Raw stateless line anchors

Keep the current stateless model and three tool shapes, but hash exact raw line
bytes, store line starts as compact offsets, and delete whitespace normalization.

Pros:

- Lowest migration risk.
- Paired probe says the core per-line hash can improve about 5.9x.
- Anchors remain independently verifiable without server cache state.
- Existing stale-shift recovery can be retained in simplified form.

Cons:

- Still computes and stores one hash per line.
- Still repeats local hash material in every rendered line.
- Still needs contextual fingerprints or accepts ambiguity after line movement.
- Edit still needs a line-to-byte mapping and stale-anchor search.
- Cannot reach the output/protocol floor on full reads.

Expected ceiling: roughly 2-3x full-read CPU and 3-6x index construction, with
smaller edit and grep gains.

#### Option B: Versioned snapshots plus positional references (recommended)

Emit one 128-bit snapshot version per file section. Each rendered line carries a
human line number and byte start offset. Edit requests send the snapshot version
once and operate on byte ranges. A raw whole-file version validates the file state;
no local line hash, chunk fingerprint, checkpoint chain, or whitespace
normalization exists.

Pros:

- Removes the measured dominant work rather than accelerating it.
- Makes validation O(1) after snapshot validation and each edit position O(1).
- Lets edit splice exact byte slices without building `Vec<&str>`.
- Lets grep render directly from searcher callbacks without a partial index.
- Produces the smallest anchor wire format among safe choices.
- Makes the cache a transparent accelerator, not a correctness requirement.

Cons:

- Complete wire/CLI/API break.
- Automatic stale-line relocation is replaced by fail-fast reread/retry.
- Requires careful file-race detection and an exact byte-offset contract.
- Cold requests still must read and version the file; output copying remains.
- The model must carry a section version into edit input.

Expected ceiling: 2.5-4x full-read CPU, 5-10x edit CPU before filesystem write,
1.3-3x large-file grep depending on match density, and larger warm-cache gains.

#### Option C: Resident repository index and OS-specific zero-copy paths

Build Option B around a long-lived watched repository index, memory maps above a
size threshold, and optionally maintain a search pre-index for repeated literals.

Pros:

- Highest warm-read and repeated-grep ceiling.
- Can avoid most repeated reads, UTF-8 validation, newline scans, and regex scans.
- Enables repo-scale query latency largely independent of file count for indexed
  literals.

Cons:

- Highest memory cost and invalidation complexity.
- File watchers overflow and coalesce events; correctness still needs stamp checks.
- mmap of concurrently truncated files can fault; it is unacceptable as an
  unconditional live-edit path.
- General regex indexing is not free; a trigram index helps only part of the query
  space.
- Much higher implementation and operational cost than the measured cold-path
  gains justify today.

Expected ceiling: very large warm gains, limited cold gains, and the worst
cost-effectiveness unless production traces prove repeated scans dominate.

### 4.4 Decision

Choose Option B as the required architecture. Add bounded snapshot reuse from
Option C only after the uncached v2 path is correct and measured. Keep mmap,
filesystem watchers, and search pre-indexing as independent gates; do not make them
prerequisites. Option A is the fallback only if a v2 MCP client contract cannot be
changed, which conflicts with this request's explicit compatibility waiver.

## 5. V2 Protocol Contract

The exact delimiters may change during Phase 1, but the semantics may not.

Example read/grep section:

```
[src/lib.rs#7d9c3af08e1b4f6c9a2d1137f68582a1 lines=641]
21@418|mod render;
22@430|mod scheme;
```

Example edit input:

```json
{
  "file_path": "src/lib.rs",
  "version": "7d9c3af08e1b4f6c9a2d1137f68582a1",
  "edits": [
    {
      "op": "replace",
      "start": "21@418",
      "end": "23@442",
      "content": "mod protocol;\nmod snapshot;"
    }
  ]
}
```

Contract rules:

1. `version` is a per-process-seeded 128-bit hash of the exact file bytes. It is
   not persisted across server restarts and is not a security digest.
2. A position is `LINE@BYTE_OFFSET`. The line number is for humans and diagnostics;
   the byte offset is authoritative after the version matches.
3. Offsets are UTF-8 byte offsets into the exact snapshot, never character indices.
4. `start` is inclusive and `end` is exclusive. Both must be valid line
   boundaries in the named snapshot.
5. A missing, evicted, or stale version causes a structured conflict that includes a
   fresh version and a small fresh context window. It never triggers fuzzy
   relocation.
6. Grep emits one version per matching file section, so any listed position can be
   edited directly.
7. CRLF and a final unterminated line are preserved byte-for-byte outside replaced
   ranges. Replacement bytes are exactly the request content; the server does not
   silently normalize line endings.
8. An empty file has one logical line at byte offset zero, matching the existing
   user-facing convention.
9. Files larger than the selected offset representation use a `u64` offset table;
   no truncating cast is permitted.
10. The text path rejects binary/NUL input and invalid UTF-8 by default. Lossy
    mutation is removed because it cannot preserve byte offsets reliably.

## 6. Target Data Model

Planned new modules:

- `src/protocol.rs`: v2 request/response types, parsing, and conflict errors.
- `src/snapshot.rs`: validated text, file stamp, snapshot ID, line count, and lazy
  positions.
- `src/cache.rs`: bounded snapshot and compiled-matcher reuse.
- `src/persist.rs`: byte-range application and atomic replacement.
- Existing `src/read.rs`, `src/render.rs`, `src/grep.rs`, `src/server.rs`,
  and `src/edit/` become thin consumers of those deep modules.

Target shapes:

```rust
struct SnapshotId(u128);

struct FileStamp {
    file_id: FileId,
    len: u64,
    modified_ns: u128,
    changed_ns: Option<u128>,
}

enum LineOffsets {
    U32(Vec<u32>),
    U64(Vec<u64>),
}

struct Snapshot {
    text: std::sync::Arc<String>,
    id: SnapshotId,
    stamp: FileStamp,
    line_count: usize,
    offsets: std::sync::OnceLock<LineOffsets>,
}

struct Position {
    line: usize,
    byte: u64,
}
```

Important invariants:

- `Snapshot.text` owns exact validated bytes. On the optimized validation path,
  `simdutf8` validates first and one narrowly-audited constructor converts the
  owned vector without a second scalar validation.
- `SnapshotId` hashes the raw bytes once. Benchmark `gxhash128`,
  `xxh3_128`, and BLAKE3 on short and multi-megabyte files. Select the fastest
  candidate meeting the accidental-collision and cross-target requirements.
- The non-cryptographic candidate uses a random per-process seed, making deliberate
  precomputed collisions impractical for this local session protocol.
- `line_count` uses the specialized SIMD newline count and is available without
  materializing positions.
- Full positions are lazy. Cold small-window reads use a sparse/blockwise selector;
  repeated random access or edit-heavy snapshots materialize one compact table.
- Output capacity comes from byte offsets in O(1); delete the pre-render sum pass at
  src/render.rs:62-66.
- Cache entries are immutable `Arc<Snapshot>` values. Mutation creates a new
  snapshot and atomically replaces the cache entry.

## 7. Implementation Plan

Each phase has a hard exit gate. A failed gate stops later phases; do not hide a
regression by averaging unrelated workloads.

### Phase 0: Reproducible v2 baselines and lower-bound benches

Files:

- `benches/hashline.rs`
- new `benches/V2_BASELINE.md`
- optional benchmark-only helpers under `benches/support/`

Work:

1. Preserve every current driver benchmark until the v2 replacement exists.
2. Add paired prototype benches on identical corpora:
   - normalized current index;
   - raw per-line hash;
   - whole-file 128-bit version plus line count;
   - sparse line selection;
   - `Vec<u32>` and `Vec<u64>` offset construction;
   - position-only rendering;
   - byte-range splice for 1 and 8 edits;
   - atomic temp-write/rename, measured separately from CPU splice.
3. Add cold and warm filesystem fixtures. Report page-cache state explicitly; do
   not compare a cold base to a warm candidate.
4. Add allocation counts and peak RSS for full read, 50k-line edit, and tree grep.
5. Add real-repository fixtures sampled from this repository and one large external
   Rust repository, recorded by commit hash rather than copied into tests.
6. Build a symbolized bench profile without changing optimization flags and capture
   Time Profiler stacks for full read, edit, rare grep, and common grep.
7. Record local arm64 and the documented Linux x86_64 host separately.

Exit gate:

- Baseline artifact includes commands, HEAD, dirty state, rustc/cargo versions, CPU,
  corpus hashes, point estimates, confidence intervals, allocation counts, and raw
  Criterion output locations.
- Every v2 target below has a paired base/candidate measurement on the same run.

### Phase 1: Freeze the v2 protocol and reference model

Files:

- new `docs/protocol-v2.md`
- new `src/protocol.rs`
- `src/edit/types.rs`
- `src/read.rs` input type
- `src/grep.rs` input type
- `src/server.rs` tool schemas
- new property/reference tests

Work:

1. Specify snapshot header, position grammar, byte-range semantics, error taxonomy,
   line-ending behavior, empty files, maximum file size, and restart behavior.
2. Replace scheme/hash CLI choices with the v2 contract in a feature branch; do not
   retain compatibility aliases in the final binary.
3. Implement strict parsers with no allocation for valid position tokens.
4. Build a slow, obviously-correct test reference that applies v2 byte ranges to a
   byte vector.
5. Add arbitrary valid-UTF-8 and CRLF differential tests comparing optimized and
   reference results.
6. Decide invalid UTF-8 policy now. Recommended: reject it for read/edit; grep may
   search bytes but must reject or escape invalid matched output rather than alter
   offsets.

Exit gate:

- The protocol document has executable examples and every rule above has at least
  one test.
- No unresolved semantic choice remains before hot-path implementation.

### Phase 2: Snapshot core and version selection

Files:

- new `src/snapshot.rs`
- `src/util.rs`
- `src/lib.rs`
- `Cargo.toml` / `Cargo.lock`
- `benches/hashline.rs`

Work:

1. Implement `ValidatedText` as the only constructor for cached text.
2. Benchmark and select a 128-bit raw-byte version function. Keep one implementation,
   not a production matrix. Remove fixed line-hash seeds.
3. Read through one file descriptor and capture metadata before and after the read.
   Retry once or return a concurrent-modification error if the stamp changes.
4. Compute version and newline count with at most two streaming passes over resident
   bytes. Prototype a fused loop only if it beats separate vectorized primitives.
5. Implement lazy `U32` offsets and `U64` fallback. Benchmark:
   - full position vector;
   - sparse checkpoints every 128/256/512 lines;
   - a newline rank/select bitmap.
   Select the simplest representation meeting the cold-window and memory gates.
6. Expose O(1) boundary validation and range slicing once offsets exist.
7. Add an audit comment and Miri test for any unsafe validated-string conversion.
   If the unsafe path gains less than 5 percent end-to-end, keep the safe path.

Exit gate:

- Snapshot construction is at least 4x faster than the paired current full index on
  10k and 50k corpora.
- Per-line resident metadata is at most 4 bytes for files below 4 GiB.
- Concurrent mutation during read cannot produce a valid mixed snapshot.
- No unchecked integer conversion or offset overflow survives tests.

### Phase 3: Rewrite read and rendering around snapshots

Files:

- `src/read.rs`
- `src/render.rs`
- `src/server.rs`
- `src/util.rs`
- tests and benches

Work:

1. Replace `FileIndex` and `Scheme` inputs with `&Snapshot` and byte ranges.
2. Render the section version once, then `LINE@OFFSET|CONTENT` per line.
3. Reserve exact content bytes plus a bounded decimal/header estimate. Remove the
   content-length prepass at src/render.rs:62-66.
4. Use sparse selection for a cold arbitrary window and the cached offset table for
   repeated windows.
5. Return a pagination cursor containing the next line and byte position, so
   sequential pages never rescan from byte zero.
6. Remove unconditional `spawn_blocking` for small, panic-free snapshot/render
   work. Keep a measured byte threshold for CPU work that could stall the reactor.
7. Keep the 2,000-line response cap unless an end-to-end client benchmark proves a
   larger response is useful; response size is already the eventual floor.

Exit gate:

- Full 10k format: <= 350 us and >= 2.5x faster than the live 887.19 us baseline.
- Cold 2k-of-100k format: <= 175 us.
- Reused-snapshot 2k-of-100k format: <= 90 us; Phase 6 must reproduce this through
  the server cache.
- 300-line dispatch cold path does not regress beyond 45 us; warm path <= 30 us.
- Zero per-line heap allocations; at most request-output plus snapshot-scale
  allocations.
- Pagination produces exactly the same bytes as a single equivalent window.

### Phase 4: Replace line-vector edit with versioned byte-range edit

Files:

- `src/edit/types.rs`
- `src/edit/apply.rs`
- `src/edit/mod.rs`
- new `src/persist.rs`
- `src/server.rs`
- tests and benches

Work:

1. Validate the request version before parsing or applying edit ranges.
2. Validate every start/end boundary in O(1), sort operations by start offset,
   reject overlap, and copy disjoint source segments plus replacement bytes
   top-down into one exact-capacity byte vector.
3. Delete pre-edit `split_lines` and `FileIndex::from_lines_partial` at
   src/edit/apply.rs:182-210.
4. Delete `Vec<&str>` cloning/splicing at src/edit/apply.rs:334-364.
5. Build the post-edit snapshot directly from the new byte vector. Do not join or
   re-split lines.
6. Produce fresh snippet positions from the new snapshot.
7. Serialize same-path server writes with a per-path lock. Persist via a
   same-directory temporary file plus atomic rename and preserve permissions.
   Measure durability (file/parent fsync) separately; do not claim rename alone is
   durable.
8. Re-stat the destination immediately before rename and abort if file identity or
   stamp changed. Use advisory OS locking where portable, while documenting that
   noncooperating external writers can still race the final check/rename window.
9. Return the new snapshot only after persistence succeeds. Phase 6 later installs
   it into the bounded cache; Phase 4 has no cache dependency.
10. Remove fuzzy stale-anchor relocation and suffix recovery. Return one conflict
    with fresh version/context instead of scanning the file.

Exit gate:

- One-op 50k CPU apply: <= 200 us and >= 5x faster than 997.18 us.
- Eight-op 50k CPU apply: <= 250 us and >= 4x faster than 1.0297 ms.
- Version-conflict path: <= 150 us once bytes are resident.
- End-to-end filesystem edit shows >= 1.5x geomean improvement on 1 MiB, 10 MiB,
  and 100 MiB files, excluding explicit durable-fsync mode.
- Crash/race tests never leave a partially truncated destination.
- A mismatched version applies zero edits.

### Phase 5: Render grep directly from the search engine

Files:

- `src/grep.rs`
- `src/protocol.rs`
- `src/server.rs`
- tests and benches

Work:

1. Stop decoding and indexing every searched file before knowing it matches.
2. Give each grep visitor a reusable read buffer/searcher. Prefer
   `Searcher::search_path` or a reusable reader if it avoids a fresh
   `std::fs::read` allocation; verify actual grep-searcher behavior from its
   pinned source before choosing.
3. Configure before/after context in the searcher and render from sink callbacks.
   Eliminate `MatchLineSink.lines` and the second span/index pass at
   src/grep.rs:96-175 and src/grep.rs:225-285.
4. Stop a match-dense file once its per-request remaining match budget is exhausted.
   The current code scans and stores all matches before the global threshold.
5. Emit one file version header per hit section. If a strong raw version cannot be
   accumulated without rereading, compare:
   - hash while filling the reusable buffer;
   - metadata/session generation token;
   - one post-match raw hash pass.
   Choose only a path that keeps edit conflict detection sound.
6. Retain per-worker output buffers and one merge per worker. Bound over-collection
   to O(worker_count * max_matches), not `max_matches * 50`.
7. Keep final path sorting if it costs under 5 percent. Otherwise expose explicit
   `order = "path" | "discovery"` and make the faster order the documented
   default; never silently vary a supposedly deterministic mode.
8. Cache compiled regex matchers by pattern/options only if compilation is at least
   5 percent of measured repeated-query latency.

Exit gate:

- 50k-line rare literal: <= 180 us.
- 50k-line common literal capped at 200 matches: <= 1.5 ms.
- 50k-line anchored regex: <= 1.0 ms.
- Cold 2,000-file tree grep: no workload regresses by more than 5 percent from the
  14-15 ms live baseline.
- Warm repeated tree grep: >= 1.5x geomean improvement after cache Phase 6.
- Grep positions edit the exact named snapshot or fail with a version conflict.

### Phase 6: Add bounded snapshot reuse

Files:

- new `src/cache.rs`
- `src/server.rs`
- `src/read.rs`
- `src/edit/mod.rs`
- `src/grep.rs`
- `src/main.rs` / `src/config.rs`
- tests and benches

Work:

1. Add a byte-bounded, sharded cache keyed by canonical path plus `FileStamp`.
   Evaluate a small custom shard design against a mature cache crate; select by
   contention, eviction correctness, and maintenance cost, not dependency count
   alone.
2. Default capacity: 256 MiB, configurable by bytes. Oversize single files bypass
   the cache.
3. On cache hit, validate metadata from an open file handle. Server-authored writes
   advance an in-process generation and install the new snapshot directly.
4. Treat metadata as an accelerator, not the sole correctness proof for a supplied
   version. If metadata is ambiguous or changed, reread and recompute raw version.
5. Add single-flight loading per path so concurrent read/grep calls do not duplicate
   large reads and offset construction.
6. Evict immutable snapshots only after all `Arc` users finish. Never hold a
   shard lock across disk I/O, regex search, render, or await.
7. Add cache-hit/miss/eviction/bytes telemetry and per-phase duration fields.
8. Defer filesystem watchers. Add them only if stat validation is measured as a
   material warm-path cost and overflow recovery is fully tested.

Exit gate:

- Cache memory stays within configured capacity plus one in-flight oversize entry.
- Same-path concurrent misses perform one load.
- Cache stamp ABA, eviction races, rename-over-file, chmod-only changes, and
  external mutation tests pass.
- Warm read and tree-grep targets from Phases 3 and 5 pass.
- Cache-disabled performance regresses by less than 3 percent.

### Phase 7: Runtime, I/O, and compiler tuning

Files:

- `Cargo.toml`
- `.cargo/config.toml`
- `src/main.rs`
- CI/release scripts and benchmark artifacts

Work, in this order:

1. Re-tune the small-file inline versus blocking threshold after v2 removes the
   panic-prone partial index.
2. Compare Tokio current-thread and multi-thread runtimes under real MCP concurrency.
   Keep multi-thread unless current-thread improves single-call latency without
   starving notifications.
3. Build a reproducible PGO pipeline using the real read/edit/grep corpus. Keep PGO
   only with >= 5 percent geomean gain and no driver regression above 3 percent.
4. Produce separate portable and CPU-tiered release artifacts where deployment
   permits it (for example x86_64-v3). Benchmark the exact artifact that ships.
5. Evaluate mmap only for read-only, large grep files after double-stat guards.
   Reject it if end-to-end gain is < 10 percent, if truncation safety is not
   acceptable, or if small-file performance regresses.
6. Re-evaluate the allocator only if allocation profiles show pressure returned.
   The previous mimalloc gate failed; do not repeat it without new evidence.
7. Keep `panic = "unwind"` unless all model-visible recovery paths are replaced
   and a shipping-profile benchmark proves a material gain.

Exit gate:

- Each accepted tuning has its own base/candidate artifact and gate result.
- Portable arm64 and x86_64 builds pass without hidden ambient flags.
- PGO training inputs and compiler version are reproducible.
- No optional tuning is required for correctness.

### Phase 8: Delete v1 architecture and consolidate the crate

Files to remove or collapse after all callers migrate:

- `src/hash.rs` line normalization, per-line hashing, and encoded hashes.
- `src/scheme.rs` content/chunk/checkpoint schemes and stale shift logic.
- `src/index.rs` `FileIndex` and partial-hash span machinery.
- scheme-related fields in `src/config.rs` and `src/main.rs`.
- gxhash fallback/config complexity if the selected snapshot hash no longer needs it.
- v1 hash matrix benches and compatibility-only golden output.

Work:

1. Search all symbols before removal; no compatibility shim or unused feature stays.
2. Keep the old benchmark numbers as historical evidence, but mark v1 benches
   archival rather than silently changing their meaning.
3. Rewrite README tool examples, CLI documentation, and `examples/golden.rs` for
   v2.
4. Run dependency pruning and verify direct/transitive licenses.
5. Require zero warnings, zero dead code, and no duplicated renderer or version
   parser.
6. Review every unsafe block and every file-system race assumption as a dedicated
   final pass.

Exit gate:

- No `Scheme`, `FileIndex`, normalized `line_hash`, v1 anchor parser, or stale
  relocation path is reachable.
- `cargo tree` contains no dependency used only by deleted v1 code.
- Public documentation and MCP tool schemas describe only v2.

### Phase 9: Final cross-platform and adversarial verification

1. Run the complete quality matrix in Section 10.
2. Run full Criterion suites sequentially three times per architecture, interleaving
   base and candidate builds.
3. Test warm/cold page-cache scenarios separately.
4. Test real stdio MCP sessions with a spawned release binary.
5. Recheck branch, HEAD, dirty state, generated artifacts, target binary, and
   benchmark provenance before any performance claim.
6. Produce `benches/V2_RESULTS.md` with accepted/rejected experiments and raw
   artifact paths.

## 8. Acceptance Criteria

| ID | Criterion | Pass condition |
|---|---|---|
| AC1 | Snapshot build | >= 4x faster than paired current full index at 10k and 50k lines |
| AC2 | Full read format | <= 350 us and >= 2.5x vs 887.19 us |
| AC3 | Cold window read | <= 175 us for 2k of 100k |
| AC4 | Warm window read | <= 90 us for 2k of 100k |
| AC5 | One-op edit CPU | <= 200 us and >= 5x vs 997.18 us |
| AC6 | Eight-op edit CPU | <= 250 us and >= 4x vs 1.0297 ms |
| AC7 | Rare large-file grep | <= 180 us |
| AC8 | Common capped grep | <= 1.5 ms with exactly 200 reported matches |
| AC9 | Anchored regex grep | <= 1.0 ms |
| AC10 | Cold tree grep | no driver > 5 percent slower |
| AC11 | Warm tree grep | >= 1.5x geomean improvement |
| AC12 | Small dispatch | cold <= 45 us; warm <= 30 us for read 300 |
| AC13 | Allocation shape | zero per-line allocations on read/edit/grep |
| AC14 | Offset memory | <= 4 bytes per line below 4 GiB; bounded u64 fallback |
| AC15 | Cache memory | configured byte cap plus at most one in-flight oversize entry |
| AC16 | Lost-update safety | zero wrong-snapshot writes in same-server/detected external races; residual OS TOCTOU documented |
| AC17 | Cross-tool invariant | every read/grep position edits the named snapshot or conflicts |
| AC18 | Portability | arm64 macOS and x86_64 Linux full gates pass |
| AC19 | Quality | fmt, build, clippy -D warnings, tests, docs, Miri/fuzz gates pass |
| AC20 | Evidence | every claimed win has comparable raw artifacts and exact commits |

Thresholds are provisional only in one direction: Phase 0 may tighten them when its
paired prototypes prove a lower floor. It may not relax them merely to make an
implementation pass. A relaxation requires an explicit bottleneck proof and user
decision.

## 9. Pre-mortem

### Failure scenario 1: Fast cache returns a stale snapshot and an edit overwrites external work

Likely cause:

- Trusting path + size + mtime alone, missing a rename/ABA/coarse-timestamp change,
  or updating cache before atomic persistence completes.

Prevention:

- Snapshot version is computed from exact bytes.
- Read through one descriptor with before/after stamps.
- Same-server writes are serialized per path.
- The destination identity and stamp are checked again immediately before rename.
- Supplied versions are compared against a current/revalidated snapshot.
- Cache install occurs only after successful atomic persistence.
- Race tests mutate, rename, truncate, and restore same-sized files between every
  protocol step.
- The plan explicitly documents the irreducible final TOCTOU window for a
  noncooperating external writer on filesystems without content-version CAS.

Stop rule:

- Any wrong-snapshot write blocks Phase 6 and final release, regardless of speed.

### Failure scenario 2: Hash work disappears but output serialization becomes the floor

Likely cause:

- Optimizing snapshot construction while still allocating/copying the response
  multiple times through render, `ContentBlock`, and JSON serialization.

Prevention:

- Measure snapshot, render, MCP serialization, and pipe transfer separately.
- Exact output reservation from byte offsets.
- One version header per file, compact positional anchors, and bounded response size.
- Add an end-to-end stdio benchmark, not only library benches.

Stop rule:

- If full read misses AC2 while snapshot build passes AC1, profile and optimize the
  output/serialization boundary before cache or compiler tuning.

### Failure scenario 3: Grep gains on one large file but regresses on real trees

Likely cause:

- mmap/setup overhead on small files, path sorting, cache locks, over-collection,
  or double-reading matched files for versions/context.

Prevention:

- Separate single-large-file, many-small-file, match-sparse, and match-dense gates.
- Reusable per-worker buffers; no cache lock across search.
- Stop match-dense sinks at the actual remaining cap.
- Adopt mmap and deterministic sorting changes only behind independent gates.

Stop rule:

- Any cold tree driver above +5 percent rejects that grep design even if the
  single-file microbenchmark improves.

## 10. Expanded Test and Verification Plan

### 10.1 Unit tests

- Snapshot ID determinism within one process and deliberate rotation across seeds.
- Empty, single-line, final-newline, no-final-newline, CRLF, bare-CR, very long line,
  Unicode, NUL, and invalid UTF-8 inputs.
- Position parse/render round trip and strict malformed-token rejection.
- Byte boundary checks at 0, EOF, CRLF seams, multibyte UTF-8 boundaries, and
  synthetic empty final line.
- `U32`/`U64` offset equivalence and overflow boundary behavior.
- Sparse selector versus full offset table on randomized corpora.
- Batch sorting, same-position insertion order, overlap detection, delete-all, and
  replacement line-ending preservation.
- Cache weight, eviction, single-flight, and lock-free-I/O invariants.
- Grep match/context separators, exact cap, binary behavior, and pattern errors.

### 10.2 Property and differential tests

- Use arbitrary valid UTF-8 byte strings and arbitrary non-overlapping edit batches.
- Compare optimized byte-range apply against the Phase 1 reference model.
- Compare sparse line selection and rendered windows against `str::lines` semantics
  plus the explicit synthetic-final-line rule.
- Compare direct grep sink output against a slow per-line regex reference across
  pattern, case, CRLF, context, and cap combinations.
- Mutation-test the version comparison, offset boundary checks, and overlap checks;
  surviving mutations fail the gate.

### 10.3 Integration tests

- Read -> edit -> read with the same snapshot.
- Grep -> edit using a file-section version.
- Stale version after external write, truncate, rename-over, chmod, and delete/recreate.
- Cache hit, miss, eviction, cache-disabled, and oversize bypass.
- Concurrent reads of one path and concurrent edit/read conflict ordering.
- Atomic persistence error injection before write, during write, before rename, and
  after rename.
- Root confinement and symlink escape behavior remains correct through cache keys.
- Release binary with and without optional CPU-tier features.

### 10.4 End-to-end tests

- Spawn `hashline-mcp` and exchange real JSON-RPC over stdin/stdout.
- Verify `initialize`, `tools/list`, read, grep, edit, structured conflict,
  pagination, client roots, and shutdown.
- Pipe maximum-sized read/grep output through the real rmcp serializer.
- Run simultaneous notification traffic while large CPU work is dispatched; assert
  bounded notification delay.
- Kill the process at persistence fault points and verify destination-file state.

### 10.5 Observability tests

Every tool call records a tracing span with:

- tool name;
- path hash, never file contents;
- file bytes and logical lines;
- snapshot cache hit/miss/bypass;
- version, count, offset, search, render, serialize, and persist durations;
- rendered/matched line count;
- worker count and early-stop reason;
- conflict/retry reason.

Tests capture stderr and assert the fields exist while stdout contains MCP bytes
only. Benchmarks disable or fix logging so measurement does not include accidental
log volume.

### 10.6 Quality commands

Run sequentially unless a command is intrinsically parallel internally:

```sh
cargo fmt --all -- --check
cargo build --all-targets
cargo build --all-targets --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --all-targets --no-default-features
cargo doc --no-deps
cargo miri test --lib snapshot
cargo bench --bench hashline
```

Also run the repository's fuzz targets and property-test seed replay once they are
added. Do not start a second benchmark while one is active.

## 11. Risk and Mitigation Matrix

| Risk | Probability | Impact | Mitigation |
|---|---:|---:|---|
| Lost update from stale metadata/cache | Medium | Critical | Raw snapshot version, per-path lock, pre-commit restat, atomic cache install, race suite |
| Byte offset overflow on huge files | Low | High | U32/U64 enum, checked conversions, boundary tests |
| UTF-8 unsafe invariant violation | Low | Critical | Encapsulated constructor, SIMD validation, Miri/fuzz, 5 percent keep gate |
| Output serialization hides core gains | High | Medium | Phase-separated timings and real stdio benchmark |
| Cache memory blow-up | Medium | High | Byte cap, oversize bypass, immutable Arc entries, eviction tests |
| Cache lock contention in grep | Medium | Medium | Shards, no I/O under lock, cache-off comparison |
| Grep rereads matched files | Medium | Medium | Direct sink/reusable buffer/version strategy bench-off |
| mmap fault on live truncation | Medium | Critical | Optional read-only gate; reject by default |
| PGO overfits synthetic corpus | Medium | Medium | Real corpus, per-driver regression cap, cross-arch runs |
| Model omits version token | Medium | Low | Clear structured conflict plus fresh section header |
| Removal of fuzzy relocation increases retries | High | Low | Fast conflict context and pagination; correctness favored |
| Atomic rename changes metadata/permissions | Medium | High | Preserve permissions, platform integration tests |
| Historical Criterion drift | High | Medium | Interleaved paired builds, exact artifacts, no stale comparisons |

## 12. Scalability and Resource Model

- Cold read: O(file bytes) for read/version/count plus O(rendered bytes). Arbitrary
  line lookup adds sparse-select work; cached offsets make it O(rendered bytes).
- Warm read: O(rendered bytes).
- Edit CPU: O(file bytes + inserted bytes) for one exact copy; validation is O(edits
  log edits) plus O(1) boundaries. Filesystem persistence remains O(file bytes).
- Grep: O(total searched bytes) until exact cap, with bounded per-worker buffers.
- Snapshot metadata: O(lines) at 4 bytes/line below 4 GiB, allocated lazily.
- Cache: O(configured bytes), immutable sharing, bounded in-flight load duplication.
- Concurrency: no global cache lock during filesystem or search work; grep workers
  use thread-local buffers and bounded final merging.
- Backpressure: one MCP response remains bounded by existing line/match caps; large
  CPU work uses a measured blocking threshold.

## 13. ADR

### Decision

Adopt a versioned raw-byte snapshot protocol with positional line references and
byte-range edits. Remove stateless normalized line hashes and contextual schemes.

### Drivers

- The current hot path is bounded by per-line normalization and line metadata.
- The compatibility waiver permits a protocol that names a file state once.
- Correct optimistic concurrency is simpler with an exact snapshot version than
  with truncated local/context hashes and fuzzy relocation.

### Alternatives considered

- Raw stateless line anchors: simpler migration, lower ceiling.
- Resident repository/search index: highest warm ceiling, excessive initial
  complexity and invalidation risk.
- Keep v1 and add mmap/allocator/compiler flags: rejected because it optimizes below
  the measured algorithmic bottleneck.

### Why chosen

The paired probe shows the core ingredients are already feasible: 5.9x faster raw
per-line hashing, 88 us whole-file version plus count on 2.89 MB, and 38 us
byte-range splice. Option B converts those ingredients into a safety-preserving
protocol and removes entire classes of work instead of tuning constants.

### Consequences

Positive:

- Much lower CPU, memory, and anchor wire overhead.
- Fewer abstractions and dependencies after v1 deletion.
- Strong fail-closed behavior for same-server and detected external races.
- A reusable immutable snapshot becomes the deep module for all tools.

Negative:

- Every client must adopt the new schema.
- Stale edits retry instead of relocating automatically.
- Offset/version semantics become a core correctness boundary.
- Cache and atomic persistence require serious concurrency testing.

### Follow-ups

- Add bounded cache only after uncached v2 passes.
- Evaluate mmap, watcher, search index, PGO, and CPU-tiered builds independently.
- Use `$performance-goal` for execution because every phase has quantitative
  evaluators and no-go gates. For parallel implementation, combine leader-owned
  performance-goal checkpoints with a coordinated team; do not run phases past a
  failed performance or correctness gate.

## 14. Execution Ordering and Stop Rules

```
Phase 0 baselines
       |
Phase 1 protocol/reference
       |
Phase 2 snapshot core
       |
       +--> Phase 3 read ----+
       +--> Phase 4 edit ----+--> Phase 6 cache --> Phase 7 tuning
       +--> Phase 5 grep ----+          |                |
                                          +--> Phase 8 delete v1
                                                     |
                                               Phase 9 final
```

Phases 3-5 may be implemented in separate ownership lanes after Phase 2 freezes its
interfaces, but their benchmark runs remain sequential. Phase 6 begins only after
all three uncached v2 tools pass. Phase 8 begins only after every v1 caller is gone.

Hard stops:

1. Any wrong-snapshot edit.
2. Any unexplained output/protocol mismatch.
3. Any benchmark run with incomparable build flags, host load, page-cache state, or
   corpus.
4. Any cold tree-grep regression above 5 percent.
5. Any optional unsafe/mmap/cache/PGO path below its keep threshold.
6. Any phase whose acceptance gate fails; diagnose or revise that phase before
   proceeding.

## 15. Confidence Scores

| Dimension | Confidence | Basis |
|---|---:|---|
| Performance | 0.95 | Live Criterion decomposition plus paired raw-hash/version/splice probe |
| Scalability | 0.93 | Compact lazy offsets, byte-bounded immutable cache, bounded grep buffers |
| Reliability | 0.92 | Raw snapshot validation, per-path serialization, pre-commit restat, atomic replace, adversarial suite |
| Cost-effectiveness | 0.91 | Large protocol rewrite, but it deletes three complex v1 subsystems and targets measured dominant work |

All scores are at least 0.90 after adding raw-version validation, atomic persistence,
cache bounds, cold/warm split gates, and strict rejection rules. The remaining
uncertainty is empirical -- actual rmcp serialization and filesystem floors -- and
Phase 0/Phase 3 explicitly measure those before optional complexity is admitted.
