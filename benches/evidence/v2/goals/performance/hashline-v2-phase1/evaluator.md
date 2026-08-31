# Performance Evaluator: hashline-v2-phase1

## Objective
Freeze and fully verify the hashline v2 protocol and reference model defined by Phase 1 of .omx/plans/2026-07-31-incompatible-max-performance-redesign.md, without starting Phase 2 or modifying Phase 0 evidence

## Evaluator Command
```sh
RUSTUP_TOOLCHAIN=nightly-2026-07-31 PYTHONDONTWRITEBYTECODE=1 python3 -B benches/support/phase1.py evaluate --goal-root .omx/goals/performance/hashline-v2-phase1
```

## Pass/Fail Contract
PASS only when the authoritative plan and Phase 0 evidence hashes are unchanged; v2 SnapshotId, position/range, UTF-8, conflict, pagination, restart, size, line-ending, empty-file, schema, CLI, error-taxonomy, and reference-apply semantics are frozen with executable examples and at least one realistic test per rule; all Phase 1-owned unit, property/differential, schema, CLI, fmt, build, clippy -D warnings, test, and doc gates pass; no unresolved semantic choice or Phase 2 implementation is present; and an exact commit/command/artifact audit is complete

This evaluator must exist and produce concrete pass/fail evidence before the performance goal can be completed.
