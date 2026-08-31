# v2 redesign performance evidence (archival)

Hash-pinned evidence for the 2026-07/08 "incompatible max performance
redesign" (phases 0-2), extracted verbatim from the local-only `.omx/`
workspace so the provenance chain behind `benches/V2_RESULTS.md` and
`benches/V2_BASELINE.md` does not depend on a single machine. The layout
mirrors the original `.omx/`-relative paths.

## Verifying

`SHA256SUMS` is the pin ledger: every digest was carried over verbatim
from the fail-closed phase evaluators that gated the redesign. Verify
the whole set with:

```sh
cd benches/evidence/v2 && sha256sum -c SHA256SUMS
```

## What is (and is not) here

Tracked: the plan document, the phase 0/1 evaluations, ledgers,
exit-gate audits, state files, the phase 0 transfer bundle, the
independent auditor artifact, and the `manifest.json` /
`SHA256SUMS.json` ledgers of the two canonical phase 0 runs.

Not tracked: the raw benchmark/profile run payloads under
`goals/performance/hashline-v2-phase0/artifacts/<platform>/runs/<id>/`
(3,418 files per platform; 65MB for macos-arm64, 605MB for linux-amd64).
They remain only in the local `.omx/` archive, but their per-file
expected hashes are inside the tracked `SHA256SUMS.json` ledgers, so
the payloads stay verifiable wherever a copy exists.

## Tooling

The capture/evaluation harness that produced and originally verified
this evidence (`benches/support/{phase0,phase1,phase2,phase2_capture}.py`
and tests) was archival after Phase 8 (29ffc1e) deleted the bench
targets it drove, and was removed from the working tree once this
directory became self-verifying. The full harness is preserved in git
history; the last tree containing it is commit 2f3d612.

Key provenance constants formerly held by those scripts:

| Constant | Value |
|---|---|
| Plan baseline commit | `f3a2f3f41076fc48f3aa4836eda873b21f7a6be6` |
| Phase 0 capture commit | `6afe83059de218d71d4161fb36848d849c9da0a6` |
| Phase 2 base commit | `690fb307d460fb95bc3b6c62884359e7d262932a` |
| Phase 2 gates | snapshot speedup >= 4.0x; u32 metadata <= 4.0 bytes/line; unsafe speedup >= 1.05x |
| Canonical run entry counts | 3,418 files per platform (65,141,104 B macos-arm64; 605,377,617 B linux-amd64) |
