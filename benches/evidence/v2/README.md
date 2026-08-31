# v2 redesign performance evidence (archival)

Hash-pinned evidence for the 2026-07/08 "incompatible max performance
redesign" (phases 0-2), extracted verbatim from the local-only `.omx/`
workspace so the provenance chain recorded in `benches/support/*.py` and
`benches/V2_RESULTS.md` no longer depends on a single machine.

Every file here is pinned by SHA256 in `benches/support/phase1.py` /
`phase2.py` (`IMMUTABLE_FILES`, `IMMUTABLE_PHASE1_FILES`,
`CANONICAL_RUNS`) and was copied with its hash re-verified against those
pins. The layout mirrors the original `.omx/`-relative paths.

Not tracked here: the raw benchmark/profile run payloads under
`goals/performance/hashline-v2-phase0/artifacts/<platform>/runs/<id>/`
(3,418 files per platform; 65MB for macos-arm64, 605MB for linux-amd64).
They remain in the local `.omx/` archive only. Their `manifest.json` and
`SHA256SUMS.json` ledgers ARE tracked at the same relative locations, so
each payload file's expected hash survives even if `.omx/` is lost.

The evaluator scripts themselves are archival (see their headers): the
bench targets they drive were deleted in Phase 8 (29ffc1e), so they are
retained for provenance and do not run against current HEAD.
