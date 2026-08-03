# Hashline v2 Results (Ralph session 2026-08-03)

Status: **implementation of Phases 3–6 production paths complete on this host**.  
Plan: `.omx/plans/2026-07-31-incompatible-max-performance-redesign.md`  
Base HEAD before work: `0b66d10`  
Phases 0–2: immutable PASS (see `.omx/goals/performance/hashline-v2-phase*`).

## Accepted

| Item | Evidence |
|---|---|
| Phase 3 read on Snapshot + `LINE@BYTE` | `src/read.rs`, `src/render.rs`; `cargo test --lib` |
| Phase 4 versioned edit + atomic persist | `src/edit/mod.rs`, `src/persist.rs`; conflict applies zero bytes |
| Phase 5 positional grep | `src/grep.rs`; Snapshot header per file; no FileIndex on hot path |
| Phase 6 snapshot cache | Shared sharded `process_cache()`, single-flight, oversize bypass, stamp checks; used by read/edit |
| Quality | `cargo test --all-features --lib` green; `cargo clippy -D warnings` green |

## Deferred / not claimed

| Item | Reason |
|---|---|
| Phase 7 PGO / mmap / CPU-tier artifacts | Require dual-host capture harness; not run in this session |
| Phase 7 allocator re-eval | Prior mimalloc reject stands; no new allocation pressure evidence |
| Full Phase 8 deletion of `hash`/`scheme`/`index` | Still linked by archival benches (`format_hashline_content`, `apply_edits`) |
| Dual-arch Criterion gates (AC2–AC12 µs numbers) | Need paired macOS/Linux capture like Phase 0/2; not re-run here |
| Residual TOCTOU | Documented: noncooperating external writers can race final rename window after stamp re-check |

## Residual TOCTOU

Same-process writes are serialized per path. Destination stamp is re-checked immediately before rename. A noncooperating writer that replaces the file after that check and before rename can still win; portable filesystems lack content CAS. Clients must treat `snapshot_conflict` as the recovery signal.

## Follow-ups

1. Dual-host Criterion capture for AC2–AC12 gates.  
2. Phase 8: migrate archival benches off v1 FileIndex, then delete modules.  
3. Phase 7: optional PGO/mmap only with ≥5%/≥10% gates respectively.
