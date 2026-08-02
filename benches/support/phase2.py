"""Fail-closed evaluator for the incompatible-v2 Phase 2 snapshot core."""

from __future__ import annotations

import argparse
import math
import os
import platform
import re
import shlex
import statistics
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath
from typing import cast

if __package__ in {None, ""}:
    _ = sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import benches.support.phase1 as phase1_evaluator

SCHEMA_VERSION = 1
PHASE0_COMMIT = "6afe83059de218d71d4161fb36848d849c9da0a6"
BASE_COMMIT = "690fb307d460fb95bc3b6c62884359e7d262932a"
PLAN_PATH = ".omx/plans/2026-07-31-incompatible-max-performance-redesign.md"
PLAN_SHA256 = "db00bf029f184811b79ab709df064a3fb9b23a9ab64562e28432e43ca8a41a6f"
PHASE1_ROOT = Path(".omx/goals/performance/hashline-v2-phase1")
PHASE2_AUDIT = "phase2-exit-gate-audit.json"
PHASE2_DECISIONS = "phase2-decisions.json"
REQUIRED_PLATFORMS = ("macos-arm64", "linux-amd64")
REQUIRED_SNAPSHOT_SIZES = (10_000, 50_000)
MINIMUM_SNAPSHOT_SPEEDUP = 4.0
MAXIMUM_U32_METADATA_BYTES_PER_LINE = 4.0
MINIMUM_UNSAFE_SPEEDUP = 1.05

IMMUTABLE_PHASE1_FILES = {
    ".omx/goals/performance/hashline-v2-phase1/evaluation.json": "09781dfc625e1a89d3a12552a8540728bf6bd72664e7b71b640ac0d1309a9434",
    ".omx/goals/performance/hashline-v2-phase1/evaluator.md": "7262a3750d7d9f7c8efc9844edb03d3ddbb222efbf2a5d98389a62e116227d23",
    ".omx/goals/performance/hashline-v2-phase1/ledger.jsonl": "93d370b6f5e4d0bd2c62eab2187a91c41a6c656b1f1672801618c2cc5fd427bf",
    ".omx/goals/performance/hashline-v2-phase1/phase1-exit-gate-audit.json": "d1c272e5ace351a4be7d9e9921fe7dea595f63afd663348ba6c28ecd405e6610",
    ".omx/goals/performance/hashline-v2-phase1/state.json": "63287aaf40b0e4b1e8d28451b6c47f97a2a1eec8faefafaa5e2793ad53299c34",
    ".omx/goals/performance/hashline-v2-phase1/evaluations/"
    "20260731T155151Z-690fb307d460/evaluation.json": "09781dfc625e1a89d3a12552a8540728bf6bd72664e7b71b640ac0d1309a9434",
    ".omx/artifacts/"
    "claude-act-as-an-independent-read-only-phase-1-exit-gate-auditor-fo-"
    "2026-07-31T16-01-34-938Z.md": "24c5a9a900c47d84a8509afea9be7751609c4e32a4ad588e4dc009fcd5643182",
}

ALLOWED_CHANGED_PATHS = frozenset(
    {
        "Cargo.lock",
        "Cargo.toml",
        "benches/hashline.rs",
        "benches/support/phase2.py",
        "benches/support/phase2_capture.py",
        "benches/support/test_phase2.py",
        "src/lib.rs",
        "src/snapshot.rs",
        "src/util.rs",
    }
)
REQUIRED_CHANGED_PATHS = frozenset(
    {
        "Cargo.toml",
        "benches/hashline.rs",
        "benches/support/phase2.py",
        "benches/support/phase2_capture.py",
        "benches/support/test_phase2.py",
        "src/lib.rs",
        "src/snapshot.rs",
        "src/util.rs",
    }
)
FORBIDDEN_PHASE3_PATHS = frozenset({"src/read.rs", "src/render.rs", "src/server.rs"})
PHASE0_TRACKED_PATHS = (
    "benches/BASELINE.md",
    "benches/V2_BASELINE.md",
    "benches/support/phase0.py",
    "benches/support/phase0_resources.rs",
    "benches/support/phase0_workloads.rs",
    "benches/support/test_phase0.py",
)
PHASE1_TRACKED_PATHS = (
    "benches/support/phase1.py",
    "benches/support/test_phase1.py",
    "docs/protocol-v2.md",
    "src/protocol.rs",
)

QUALITY_COMMANDS = (
    (
        "phase2_evaluator_tests",
        (
            "python3",
            "-B",
            "-m",
            "unittest",
            "discover",
            "-s",
            "benches/support",
            "-p",
            "test_phase2.py",
        ),
    ),
    ("fmt", ("cargo", "fmt", "--all", "--", "--check")),
    ("build_all", ("cargo", "build", "--all-targets", "--all-features")),
    (
        "build_no_default",
        ("cargo", "build", "--all-targets", "--no-default-features"),
    ),
    (
        "clippy_all",
        (
            "cargo",
            "clippy",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ),
    ),
    (
        "clippy_no_default",
        (
            "cargo",
            "clippy",
            "--all-targets",
            "--no-default-features",
            "--",
            "-D",
            "warnings",
        ),
    ),
    ("test_all", ("cargo", "test", "--all-targets", "--all-features")),
    (
        "test_no_default",
        ("cargo", "test", "--all-targets", "--no-default-features"),
    ),
    ("doc_test", ("cargo", "test", "--doc", "--all-features")),
    ("doc", ("cargo", "doc", "--no-deps", "--all-features")),
    (
        "miri_snapshot",
        (
            "cargo",
            "miri",
            "test",
            "--lib",
            "--no-default-features",
            "snapshot::tests::miri_validated_text_round_trip",
        ),
    ),
)


class EvaluationError(RuntimeError):
    """Raised when a Phase 2 invariant or evidence contract fails."""


@dataclass(frozen=True)
class CommandRecord:
    """Durable record for one evaluator subprocess."""

    name: str
    command: list[str]
    exit_code: int
    elapsed_seconds: float
    stdout_path: str
    stdout_sha256: str
    stderr_path: str
    stderr_sha256: str


def require(condition: bool, message: str) -> None:
    """Fail the evaluator when condition is false."""

    if not condition:
        raise EvaluationError(message)


def sha256_file(path: Path) -> str:
    """Return the SHA-256 digest of a file without loading it into memory."""

    return phase1_evaluator.sha256_file(path)


def read_json_object(path: Path) -> dict[str, object]:
    """Read a UTF-8 JSON object."""

    return phase1_evaluator.read_json_object(path)


def write_json(path: Path, value: object) -> None:
    """Atomically write deterministic, human-readable JSON."""

    phase1_evaluator.write_json(path, value)


def object_value(value: object, label: str) -> dict[str, object]:
    """Require a JSON object."""

    if not isinstance(value, dict) or not all(isinstance(key, str) for key in value):
        raise EvaluationError(f"{label} must be a string-keyed JSON object")
    return cast(dict[str, object], value)


def list_value(value: object, label: str) -> list[object]:
    """Require a JSON array."""

    if not isinstance(value, list):
        raise EvaluationError(f"{label} must be a JSON array")
    return cast(list[object], value)


def string_value(value: object, label: str) -> str:
    """Require a non-empty JSON string."""

    if not isinstance(value, str) or not value:
        raise EvaluationError(f"{label} must be a non-empty string")
    return value


def finite_number(value: object, label: str) -> float:
    """Require one finite positive JSON number."""

    if (
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(float(value))
        or float(value) <= 0.0
    ):
        raise EvaluationError(f"{label} must be a finite positive number")
    return float(value)


def git_text(*arguments: str) -> str:
    """Run Git and return stripped UTF-8 stdout."""

    result = subprocess.run(
        ("git", *arguments),
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def verify_phase1_evidence() -> dict[str, object]:
    """Replay every immutable Phase 1 digest and terminal PASS assertion."""

    files = [
        phase1_evaluator.verify_immutable_file(path, digest)
        for path, digest in IMMUTABLE_PHASE1_FILES.items()
    ]
    evaluation = read_json_object(PHASE1_ROOT / "evaluation.json")
    audit = read_json_object(PHASE1_ROOT / "phase1-exit-gate-audit.json")
    state = read_json_object(PHASE1_ROOT / "state.json")

    require(evaluation.get("status") == "pass", "Phase 1 evaluation is not PASS")
    require(audit.get("status") == "pass", "Phase 1 independent audit is not PASS")
    require(state.get("status") == "complete", "Phase 1 OMX state is not complete")
    validation = object_value(state.get("lastValidation"), "Phase 1 lastValidation")
    require(validation.get("status") == "pass", "Phase 1 last validation is not PASS")
    evidence = string_value(validation.get("evidence"), "Phase 1 validation evidence")
    require(
        BASE_COMMIT in evidence, "Phase 1 validation does not name the exact candidate"
    )
    require(
        PHASE0_COMMIT in evidence, "Phase 1 validation does not name the Phase 0 base"
    )
    return {"immutable_files": files, "status": "pass"}


def verify_repository_scope() -> dict[str, object]:
    """Verify exact base ancestry, cleanliness, signature, and Phase 2-only scope."""

    root = Path(git_text("rev-parse", "--show-toplevel")).resolve()
    require(root == Path.cwd().resolve(), f"wrong repository root: {root}")

    head = git_text("rev-parse", "HEAD")
    require(head != BASE_COMMIT, "Phase 2 has no candidate commit")
    ancestor = subprocess.run(
        ("git", "merge-base", "--is-ancestor", BASE_COMMIT, head),
        check=False,
    )
    require(ancestor.returncode == 0, f"{BASE_COMMIT} is not an ancestor of {head}")

    dirty = git_text("status", "--porcelain=v1", "--untracked-files=all")
    require(not dirty, f"worktree is not clean:\n{dirty}")

    changed = {
        line
        for line in git_text(
            "diff", "--name-only", f"{BASE_COMMIT}..{head}"
        ).splitlines()
        if line
    }
    require(
        changed <= ALLOWED_CHANGED_PATHS,
        f"Phase 2 changed unrelated paths: {sorted(changed - ALLOWED_CHANGED_PATHS)!r}",
    )
    require(
        REQUIRED_CHANGED_PATHS <= changed,
        f"Phase 2 required paths are absent: {sorted(REQUIRED_CHANGED_PATHS - changed)!r}",
    )
    require(
        not (changed & FORBIDDEN_PHASE3_PATHS),
        f"Phase 3 paths changed: {sorted(changed & FORBIDDEN_PHASE3_PATHS)!r}",
    )

    protected = (*PHASE0_TRACKED_PATHS, *PHASE1_TRACKED_PATHS)
    protected_diff = git_text(
        "diff",
        "--name-only",
        f"{BASE_COMMIT}..{head}",
        "--",
        *protected,
    )
    require(
        not protected_diff, f"frozen Phase 0/1 tracked paths changed:\n{protected_diff}"
    )

    signature = git_text("log", "-1", "--format=%G?")
    require(
        signature in {"G", "U"},
        f"candidate commit signature is not good: {signature}",
    )
    return {
        "base_commit": BASE_COMMIT,
        "candidate_commit": head,
        "signature_status": signature,
        "changed_paths": sorted(changed),
        "dirty": False,
    }


def production_source() -> str:
    """Return the non-test portion of the snapshot module."""

    source = Path("src/snapshot.rs").read_text(encoding="utf-8")
    return source.split("#[cfg(test)]", maxsplit=1)[0]


def verify_source_contract() -> dict[str, object]:
    """Reject unchecked conversions and audit any benchmark-gated unsafe path."""

    source = production_source()
    complete_source = Path("src/snapshot.rs").read_text(encoding="utf-8")
    util_source = Path("src/util.rs").read_text(encoding="utf-8")
    lib_source = Path("src/lib.rs").read_text(encoding="utf-8")

    required_tokens = (
        "pub struct ValidatedText",
        "pub struct Snapshot",
        "enum LineOffsets",
        "OnceLock",
        "xxh3_128_with_seed",
        "memchr_iter",
        "try_from",
        "checked_add",
    )
    missing = [token for token in required_tokens if token not in source]
    require(not missing, f"snapshot source is missing required mechanisms: {missing!r}")
    require("pub mod snapshot;" in lib_source, "snapshot module is not exported")
    require(
        "process_random_seed" in util_source,
        "process-scoped random seed helper is absent",
    )

    numeric_casts = re.findall(
        r"\bas\s+(?:u(?:8|16|32|64|128|size)|i(?:8|16|32|64|128|size))\b",
        source,
    )
    require(not numeric_casts, f"unchecked numeric casts remain: {numeric_casts!r}")
    require(".unwrap()" not in source, "production snapshot source contains unwrap()")
    require(
        ".expect(" not in source,
        "production snapshot source contains an unchecked expect() conversion",
    )

    production_unsafe = len(re.findall(r"\bunsafe\b", source))
    version_engines = {
        name: name in source for name in ("gxhash128", "xxh3_128_with_seed", "blake3")
    }
    require(
        sum(version_engines.values()) == 1 and version_engines["xxh3_128_with_seed"],
        f"production version matrix is not reduced to XXH3-128: {version_engines!r}",
    )
    require(
        "fixed" not in source.lower() or "fixed-capacity" in source.lower(),
        "snapshot source still describes a fixed version seed",
    )

    test_names = set(re.findall(r"\bfn (phase2_[a-z0-9_]+)\s*\(", complete_source))
    required_tests = {
        "phase2_validated_text_rejects_invalid_utf8_and_nul",
        "phase2_snapshot_id_is_process_scoped_and_content_stable",
        "phase2_stable_read_retries_once_without_returning_mixed_bytes",
        "phase2_stable_read_rejects_two_mutated_attempts",
        "phase2_offsets_are_lazy_u32_and_u64_checked",
        "phase2_boundaries_and_ranges_match_reference",
        "phase2_integer_overflow_paths_fail_closed",
        "phase2_concurrent_read_mutation_never_returns_mixed_snapshot",
    }
    missing_tests = sorted(required_tests - test_names)
    require(
        not missing_tests,
        f"required Phase 2 regression tests are absent: {missing_tests!r}",
    )
    return {
        "production_unsafe_tokens": production_unsafe,
        "unchecked_numeric_casts": len(numeric_casts),
        "version_engines": version_engines,
        "required_regression_tests": sorted(required_tests),
    }


def validate_relative_path(path_text: str) -> PurePosixPath:
    """Reject absolute or parent-traversing artifact references."""

    path = PurePosixPath(path_text)
    require(not path.is_absolute(), f"absolute artifact path: {path_text}")
    require(".." not in path.parts, f"parent traversal in artifact path: {path_text}")
    return path


def canonical_run(
    goal_root: Path, platform_name: str, head: str
) -> tuple[Path, dict[str, object]]:
    """Resolve and authenticate one platform's canonical Phase 2 run."""

    platform_root = goal_root / "artifacts" / platform_name
    latest_path = platform_root / "latest.json"
    latest = read_json_object(latest_path)
    relative = validate_relative_path(
        string_value(latest.get("run"), f"{platform_name} run")
    )
    run_root = platform_root / relative
    require(run_root.is_dir(), f"canonical run is missing: {run_root}")

    manifest_path = run_root / "manifest.json"
    checksums_path = run_root / "SHA256SUMS.json"
    require(
        sha256_file(manifest_path)
        == string_value(
            latest.get("manifest_sha256"), f"{platform_name} manifest digest"
        ),
        f"{platform_name} canonical manifest digest changed",
    )
    require(
        sha256_file(checksums_path)
        == string_value(
            latest.get("checksums_sha256"), f"{platform_name} checksum digest"
        ),
        f"{platform_name} canonical checksum digest changed",
    )
    checksum_result = phase1_evaluator.verify_checksum_manifest(run_root)
    manifest = read_json_object(manifest_path)
    require(manifest.get("status") == "pass", f"{platform_name} capture is not PASS")
    require(manifest.get("phase") == "Phase 2", f"{platform_name} phase label is wrong")
    require(
        manifest.get("platform") == platform_name, f"{platform_name} platform mismatch"
    )
    require(
        manifest.get("base_commit") == BASE_COMMIT, f"{platform_name} base mismatch"
    )
    require(
        manifest.get("candidate_commit") == head, f"{platform_name} candidate mismatch"
    )
    require(
        manifest.get("plan_sha256") == PLAN_SHA256, f"{platform_name} plan mismatch"
    )

    environment = object_value(
        manifest.get("environment"), f"{platform_name} environment"
    )
    require(
        environment.get("candidate_dirty") is False,
        f"{platform_name} candidate was dirty",
    )
    require(environment.get("base_dirty") is False, f"{platform_name} base was dirty")
    require(
        environment.get("benchmark_rustflags") == {},
        f"{platform_name} benchmark compiler flags were not empty",
    )
    require(
        environment.get("load_gate") == "pass",
        f"{platform_name} host load gate did not pass",
    )
    require(
        isinstance(environment.get("commands"), dict),
        f"{platform_name} exact environment commands are absent",
    )
    return run_root, {
        "latest": latest_path.as_posix(),
        "run_root": run_root.as_posix(),
        "manifest_sha256": sha256_file(manifest_path),
        "checksums_sha256": sha256_file(checksums_path),
        "checksum_result": checksum_result,
        "manifest": manifest,
    }


def estimate_bounds(run: dict[str, object], label: str) -> tuple[float, float, float]:
    """Return point, lower, and upper nanoseconds for one raw Criterion run."""

    estimate = object_value(run.get("estimate"), f"{label} estimate")
    require(
        estimate.get("estimate_kind") == "absolute", f"{label} estimate is relative"
    )
    point = finite_number(estimate.get("point_estimate_ns"), f"{label} point estimate")
    interval = object_value(estimate.get("confidence_interval_ns"), f"{label} interval")
    lower = finite_number(interval.get("lower_bound"), f"{label} lower bound")
    upper = finite_number(interval.get("upper_bound"), f"{label} upper bound")
    require(lower <= point <= upper, f"{label} estimate lies outside its interval")
    return point, lower, upper


def verify_pair(
    pair: dict[str, object],
    size: int,
    run_root: Path,
    head: str,
) -> dict[str, object]:
    """Recompute one exact-commit interleaved snapshot speedup."""

    require(pair.get("size") == size, f"snapshot pair size mismatch for {size}")
    require(
        pair.get("corpus_line_count") == size,
        f"snapshot pair corpus line count mismatch for {size}",
    )
    string_value(pair.get("corpus_sha256"), f"{size} corpus digest")
    rounds_value = pair.get("rounds")
    require(
        isinstance(rounds_value, int)
        and not isinstance(rounds_value, bool)
        and rounds_value >= 3,
        f"{size} pair has fewer than 3 rounds",
    )
    rounds = cast(int, rounds_value)
    runs = [
        object_value(value, f"{size} run")
        for value in list_value(pair.get("runs"), f"{size} runs")
    ]
    require(len(runs) == rounds * 2, f"{size} pair run count is not 2 * rounds")

    expected_order: list[str] = []
    for round_index in range(rounds):
        expected_order.extend(
            ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")
        )
    actual_order = [run.get("variant") for run in runs]
    require(
        actual_order == expected_order,
        f"{size} pair is not interleaved: {actual_order!r}",
    )

    base_points: list[float] = []
    base_lowers: list[float] = []
    candidate_points: list[float] = []
    candidate_uppers: list[float] = []
    for sequence, run in enumerate(runs, start=1):
        label = f"{size} run {sequence}"
        require(run.get("sequence") == sequence, f"{label} sequence mismatch")
        variant = string_value(run.get("variant"), f"{label} variant")
        expected_commit = BASE_COMMIT if variant == "base" else head
        require(run.get("commit") == expected_commit, f"{label} exact commit mismatch")
        raw = validate_relative_path(
            string_value(run.get("raw_path"), f"{label} raw path")
        )
        raw_root = run_root / raw
        require(raw_root.is_dir(), f"{label} raw artifact directory is missing")
        raw_estimate = raw_root / "criterion_new" / "estimates.json"
        require(raw_estimate.is_file(), f"{label} raw Criterion estimate is missing")

        point, lower, upper = estimate_bounds(run, label)
        raw_summary = phase1_evaluator.read_json_object(raw_estimate)
        raw_median = object_value(raw_summary.get("median"), f"{label} raw median")
        raw_point = finite_number(
            raw_median.get("point_estimate"), f"{label} raw point"
        )
        require(
            math.isclose(point, raw_point, rel_tol=0.0, abs_tol=0.0),
            f"{label} summary differs from raw Criterion output",
        )
        if variant == "base":
            base_points.append(point)
            base_lowers.append(lower)
        else:
            candidate_points.append(point)
            candidate_uppers.append(upper)

    point_speedup = statistics.median(base_points) / statistics.median(candidate_points)
    conservative_speedup = statistics.median(base_lowers) / statistics.median(
        candidate_uppers
    )
    require(
        conservative_speedup >= MINIMUM_SNAPSHOT_SPEEDUP,
        f"{size} snapshot speedup {conservative_speedup:.4f}x is below "
        f"{MINIMUM_SNAPSHOT_SPEEDUP:.1f}x",
    )
    summary = object_value(pair.get("summary"), f"{size} summary")
    reported = finite_number(
        summary.get("conservative_speedup"), f"{size} reported speedup"
    )
    require(
        math.isclose(reported, conservative_speedup, rel_tol=1e-12),
        f"{size} reported speedup differs from evaluator recomputation",
    )
    return {
        "size": size,
        "base_median_ns": statistics.median(base_points),
        "candidate_median_ns": statistics.median(candidate_points),
        "point_speedup": point_speedup,
        "conservative_speedup": conservative_speedup,
    }


def verify_representation_results(
    results: dict[str, object],
    platform_name: str,
) -> dict[str, object]:
    """Require every offset representation and the U32 memory gate."""

    rows = [
        object_value(value, f"{platform_name} representation")
        for value in list_value(
            results.get("representations"), "representation results"
        )
    ]
    names = {string_value(row.get("name"), "representation name") for row in rows}
    required = {
        "full_u32",
        "full_u64",
        "sparse_128",
        "sparse_256",
        "sparse_512",
        "rank_select_bitmap",
    }
    require(
        required <= names,
        f"{platform_name} representation results missing {required - names!r}",
    )
    for row in rows:
        finite_number(
            row.get("construction_ns"), f"{platform_name} representation construction"
        )
        finite_number(
            row.get("cold_window_ns"), f"{platform_name} representation cold window"
        )
        finite_number(
            row.get("bytes_per_line"), f"{platform_name} representation memory"
        )

    selected = object_value(
        results.get("selected_representation"), "selected representation"
    )
    require(
        selected.get("name") == "lazy_full_u32_u64",
        f"{platform_name} selected offset representation changed",
    )
    u32_bytes = finite_number(
        selected.get("u32_bytes_per_line"),
        f"{platform_name} selected U32 bytes per line",
    )
    require(
        u32_bytes <= MAXIMUM_U32_METADATA_BYTES_PER_LINE,
        f"{platform_name} U32 metadata is {u32_bytes:.4f} bytes per line",
    )
    require(
        selected.get("u64_fallback") is True,
        f"{platform_name} selected offsets lack checked U64 fallback",
    )
    return {"alternatives": sorted(names), "u32_bytes_per_line": u32_bytes}


def verify_version_results(
    results: dict[str, object],
    platform_name: str,
) -> dict[str, object]:
    """Require a complete raw-byte version bench-off and one portable selection."""

    rows = [
        object_value(value, f"{platform_name} version candidate")
        for value in list_value(results.get("version_candidates"), "version candidates")
    ]
    names = {string_value(row.get("name"), "version candidate name") for row in rows}
    required = {"gxhash128", "xxh3_128_with_seed", "blake3_128"}
    require(names == required, f"{platform_name} version matrix changed: {names!r}")
    for row in rows:
        finite_number(row.get("short_ns"), f"{platform_name} short version time")
        finite_number(
            row.get("multimegabyte_ns"), f"{platform_name} large version time"
        )
        require(
            isinstance(row.get("cross_target"), bool),
            f"{platform_name} version cross-target flag is absent",
        )
    selected = string_value(results.get("selected_version"), "selected version")
    require(
        selected == "xxh3_128_with_seed",
        f"{platform_name} selected version function is {selected}",
    )
    return {"candidates": sorted(names), "selected": selected}


def verify_unsafe_result(
    results: dict[str, object],
    platform_name: str,
    production_unsafe_tokens: int,
) -> dict[str, object]:
    """Enforce the 5 percent unsafe adoption rule."""

    unsafe_result = object_value(
        results.get("unsafe_validation"), "unsafe validation result"
    )
    adopted = unsafe_result.get("adopted")
    require(
        isinstance(adopted, bool), f"{platform_name} unsafe adoption flag is absent"
    )
    speedup = finite_number(
        unsafe_result.get("conservative_speedup"),
        f"{platform_name} unsafe conservative speedup",
    )
    if adopted:
        require(
            speedup >= MINIMUM_UNSAFE_SPEEDUP,
            f"{platform_name} adopted unsafe path improves only {speedup:.4f}x",
        )
        require(
            production_unsafe_tokens > 0,
            "unsafe decision says adopted but source is safe",
        )
        require(
            unsafe_result.get("miri") == "pass", "adopted unsafe path lacks Miri PASS"
        )
    else:
        require(
            production_unsafe_tokens == 0,
            "safe decision conflicts with production unsafe",
        )
        require(
            speedup < MINIMUM_UNSAFE_SPEEDUP,
            f"{platform_name} rejected unsafe path measured {speedup:.4f}x",
        )
    return {"adopted": adopted, "conservative_speedup": speedup}


def verify_profiles(run_root: Path, platform_name: str) -> dict[str, object]:
    """Require raw and symbolized base/candidate snapshot profiles."""

    summary = read_json_object(run_root / "profiles" / "summary.json")
    profiles = [
        object_value(value, f"{platform_name} profile")
        for value in list_value(summary.get("profiles"), "profile records")
    ]
    require(
        len(profiles) == 2, f"{platform_name} must have base and candidate profiles"
    )
    variants = {profile.get("variant") for profile in profiles}
    require(
        variants == {"base", "candidate"},
        f"{platform_name} profile variants are incomplete",
    )
    for profile_record in profiles:
        variant = string_value(profile_record.get("variant"), "profile variant")
        require(
            profile_record.get("symbolized") is True,
            f"{platform_name} {variant} not symbolized",
        )
        raw_path = run_root / validate_relative_path(
            string_value(profile_record.get("raw_path"), f"{variant} raw profile")
        )
        report_path = run_root / validate_relative_path(
            string_value(profile_record.get("report_path"), f"{variant} profile report")
        )
        require(raw_path.is_file(), f"{platform_name} {variant} raw profile is missing")
        require(
            report_path.is_file(),
            f"{platform_name} {variant} profile report is missing",
        )
        hits = list_value(profile_record.get("symbol_hits"), f"{variant} symbol hits")
        require(
            bool(hits),
            f"{platform_name} {variant} profile has no workload symbol",
        )
    return {"profiles": profiles}


def verify_platform_run(
    goal_root: Path,
    platform_name: str,
    head: str,
    production_unsafe_tokens: int,
) -> dict[str, object]:
    """Verify benchmarks, memory, decisions, profiles, and raw coverage for one host."""

    run_root, canonical = canonical_run(goal_root, platform_name, head)
    results = read_json_object(run_root / "benchmarks" / "results.json")
    require(
        results.get("status") == "pass",
        f"{platform_name} benchmark results are not PASS",
    )
    pairs = {
        cast(int, pair.get("size")): pair
        for pair in (
            object_value(value, f"{platform_name} snapshot pair")
            for value in list_value(results.get("snapshot_pairs"), "snapshot pairs")
        )
        if isinstance(pair.get("size"), int)
    }
    require(
        set(pairs) == set(REQUIRED_SNAPSHOT_SIZES),
        f"{platform_name} snapshot sizes changed: {sorted(pairs)!r}",
    )
    speedups = [
        verify_pair(pairs[size], size, run_root, head)
        for size in REQUIRED_SNAPSHOT_SIZES
    ]
    representations = verify_representation_results(results, platform_name)
    versions = verify_version_results(results, platform_name)
    unsafe_result = verify_unsafe_result(
        results, platform_name, production_unsafe_tokens
    )
    profiles = verify_profiles(run_root, platform_name)
    manifest = object_value(canonical["manifest"], f"{platform_name} manifest")
    return {
        "canonical": canonical,
        "corpora": object_value(manifest.get("corpora"), f"{platform_name} corpora"),
        "speedups": speedups,
        "representations": representations,
        "versions": versions,
        "unsafe": unsafe_result,
        "profiles": profiles,
    }


def verify_cross_platform(
    goal_root: Path,
    head: str,
    production_unsafe_tokens: int,
) -> dict[str, object]:
    """Verify both architectures used byte-identical corpora and passed every gate."""

    platforms = {
        name: verify_platform_run(goal_root, name, head, production_unsafe_tokens)
        for name in REQUIRED_PLATFORMS
    }
    first = object_value(platforms[REQUIRED_PLATFORMS[0]]["corpora"], "first corpora")
    second = object_value(platforms[REQUIRED_PLATFORMS[1]]["corpora"], "second corpora")
    require(
        first == second, "macOS/arm64 and Linux/amd64 corpora are not byte-identical"
    )
    return {"platforms": platforms, "corpora_equal": True}


def verify_decisions(
    goal_root: Path,
    head: str,
    platform_results: dict[str, object],
) -> dict[str, object]:
    """Require all Phase 2 choices, rejections, and rollback conditions to be frozen."""

    path = goal_root / PHASE2_DECISIONS
    decisions = read_json_object(path)
    require(decisions.get("status") == "pass", "Phase 2 decision record is not PASS")
    require(decisions.get("candidate_commit") == head, "decision candidate mismatch")
    require(decisions.get("base_commit") == BASE_COMMIT, "decision base mismatch")
    require(decisions.get("plan_sha256") == PLAN_SHA256, "decision plan mismatch")
    require(
        decisions.get("unresolved_decisions") == 0, "Phase 2 has unresolved decisions"
    )

    version = object_value(decisions.get("version_function"), "version decision")
    require(
        version.get("selected") == "xxh3_128_with_seed",
        "version decision is not frozen",
    )
    require(
        version.get("process_scoped_seed") is True, "version seed is not process scoped"
    )
    require(
        version.get("production_implementations") == 1,
        "production version matrix remains",
    )

    offsets = object_value(decisions.get("offsets"), "offset decision")
    require(
        offsets.get("selected") == "lazy_full_u32_u64", "offset decision is not frozen"
    )
    require(
        offsets.get("u32_bytes_per_line") == 4, "U32 offset metadata is not 4 bytes"
    )
    require(offsets.get("u64_fallback") is True, "U64 fallback decision is absent")

    unsafe_decision = object_value(decisions.get("unsafe"), "unsafe decision")
    unsafe_adopted_value = unsafe_decision.get("adopted")
    require(isinstance(unsafe_adopted_value, bool), "unsafe decision flag is absent")
    unsafe_adopted = cast(bool, unsafe_adopted_value)
    unsafe_threshold = finite_number(
        unsafe_decision.get("threshold"),
        "unsafe decision threshold",
    )
    require(
        unsafe_threshold == MINIMUM_UNSAFE_SPEEDUP,
        f"unsafe decision threshold changed: {unsafe_threshold}",
    )

    rollback = list_value(decisions.get("rollback_no_go"), "rollback/no-go decisions")
    require(len(rollback) >= 3, "rollback/no-go record is incomplete")
    rollback_by_experiment: dict[str, dict[str, object]] = {}
    for index, row in enumerate(rollback):
        record = object_value(row, f"rollback record {index}")
        experiment = string_value(
            record.get("experiment"),
            f"rollback experiment {index}",
        )
        require(
            experiment not in rollback_by_experiment,
            f"duplicate rollback experiment: {experiment}",
        )
        require(
            record.get("decision") in {"adopt", "reject", "rollback"},
            f"rollback decision {index} is invalid",
        )
        string_value(record.get("reason"), f"rollback reason {index}")
        rollback_by_experiment[experiment] = record

    artifacts = list_value(decisions.get("artifact_paths"), "decision artifact paths")
    require(len(artifacts) >= 4, "decision record lacks raw artifact paths")
    for value in artifacts:
        referenced = Path(string_value(value, "decision artifact path"))
        require(referenced.exists(), f"decision artifact is missing: {referenced}")

    platform_object = object_value(
        platform_results.get("platforms"), "platform evidence"
    )
    require(
        set(platform_object) == set(REQUIRED_PLATFORMS),
        "decision validation lacks one required platform",
    )

    platform_adoption: set[bool] = set()
    for platform_name, value in platform_object.items():
        platform = object_value(value, f"{platform_name} platform evidence")
        measured = object_value(
            platform.get("unsafe"),
            f"{platform_name} unsafe evidence",
        )
        measured_adopted = measured.get("adopted")
        require(
            isinstance(measured_adopted, bool),
            f"{platform_name} unsafe evidence lacks adoption flag",
        )
        platform_adoption.add(cast(bool, measured_adopted))
    require(
        len(platform_adoption) == 1,
        "required platforms disagree on unsafe adoption",
    )
    require(
        platform_adoption.pop() == unsafe_adopted,
        "unsafe decision disagrees with authenticated platform evidence",
    )

    unsafe_rollback = rollback_by_experiment.get(
        "unchecked validated-string conversion"
    )
    if unsafe_rollback is None:
        raise EvaluationError("unsafe rollback/no-go record is absent")
    expected_unsafe_action = "adopt" if unsafe_adopted else "reject"
    require(
        unsafe_rollback.get("decision") == expected_unsafe_action,
        "unsafe rollback/no-go action disagrees with measured adoption",
    )
    return {"path": path.as_posix(), "sha256": sha256_file(path), "record": decisions}


def verify_independent_audit(goal_root: Path, head: str) -> dict[str, object]:
    """Require a PASS audit tied to the exact candidate and raw advisor artifact."""

    path = goal_root / PHASE2_AUDIT
    audit = read_json_object(path)
    require(audit.get("status") == "pass", "independent Phase 2 audit is not PASS")
    require(audit.get("phase") == "Phase 2", "independent audit phase mismatch")
    require(
        audit.get("candidate_commit") == head, "independent audit candidate mismatch"
    )
    require(audit.get("base_commit") == BASE_COMMIT, "independent audit base mismatch")
    require(
        audit.get("unresolved_decisions") == 0, "independent audit found open decisions"
    )

    advisor_path = Path(string_value(audit.get("advisor_path"), "advisor path"))
    require(
        advisor_path.is_file(),
        f"independent advisor artifact is missing: {advisor_path}",
    )
    advisor_sha = string_value(audit.get("advisor_sha256"), "advisor digest")
    require(
        sha256_file(advisor_path) == advisor_sha, "independent advisor digest changed"
    )
    preaudit_path = Path(string_value(audit.get("preaudit_path"), "preaudit path"))
    require(preaudit_path.is_file(), f"preaudit artifact is missing: {preaudit_path}")
    preaudit_sha = string_value(audit.get("preaudit_sha256"), "preaudit digest")
    require(sha256_file(preaudit_path) == preaudit_sha, "preaudit digest changed")
    preaudit = read_json_object(preaudit_path)
    require(preaudit.get("status") == "pass", "audited preaudit evaluation is not PASS")
    return {
        "path": path.as_posix(),
        "sha256": sha256_file(path),
        "advisor_path": advisor_path.as_posix(),
        "advisor_sha256": advisor_sha,
        "preaudit_path": preaudit_path.as_posix(),
        "preaudit_sha256": preaudit_sha,
    }


def command_environment() -> tuple[dict[str, str], list[str]]:
    """Return a deterministic correctness environment without ambient compiler flags."""

    environment = os.environ.copy()
    removed: list[str] = []
    for name in ("RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        if name in environment:
            removed.append(name)
            del environment[name]
    environment["CARGO_TERM_COLOR"] = "never"
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["LC_ALL"] = "C"
    environment["LANG"] = "C"
    return environment, removed


def run_quality_command(
    artifact_root: Path,
    name: str,
    command: tuple[str, ...],
    environment: dict[str, str],
) -> CommandRecord:
    """Run one quality gate and persist complete stdout and stderr."""

    command_root = artifact_root / "commands"
    command_root.mkdir(parents=True, exist_ok=True)
    stdout_path = command_root / f"{name}.stdout.log"
    stderr_path = command_root / f"{name}.stderr.log"

    started = time.monotonic()
    result = subprocess.run(
        command,
        check=False,
        capture_output=True,
        env=environment,
    )
    elapsed = time.monotonic() - started
    stdout_path.write_bytes(result.stdout)
    stderr_path.write_bytes(result.stderr)
    record = CommandRecord(
        name=name,
        command=list(command),
        exit_code=result.returncode,
        elapsed_seconds=elapsed,
        stdout_path=stdout_path.as_posix(),
        stdout_sha256=sha256_file(stdout_path),
        stderr_path=stderr_path.as_posix(),
        stderr_sha256=sha256_file(stderr_path),
    )
    require(
        result.returncode == 0,
        f"quality command failed ({name}): {shlex.join(command)}; see {stderr_path}",
    )
    return record


def environment_record(removed_ambient_flags: list[str]) -> dict[str, object]:
    """Capture exact evaluator host and toolchain provenance."""

    def output(command: tuple[str, ...]) -> str:
        result = subprocess.run(command, check=True, capture_output=True, text=True)
        return result.stdout.strip()

    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": sys.version,
        "rustc": output(("rustc", "--version", "--verbose")),
        "cargo": output(("cargo", "--version", "--verbose")),
        "removed_ambient_flags": removed_ambient_flags,
    }


def evaluate(goal_root: Path, require_audit: bool) -> Path:
    """Run every Phase 2 exit gate and return the durable evaluation path."""

    head_hint = git_text("rev-parse", "--short=12", "HEAD")
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    kind = "evaluations" if require_audit else "preaudits"
    artifact_root = goal_root / kind / f"{timestamp}-{head_hint}"
    artifact_root.mkdir(parents=True, exist_ok=False)
    evaluation_path = artifact_root / "evaluation.json"
    top_level_path = goal_root / (
        "evaluation.json" if require_audit else "preaudit.json"
    )
    commands: list[dict[str, object]] = []
    result: dict[str, object] = {
        "schema_version": SCHEMA_VERSION,
        "phase": "Phase 2",
        "status": "fail",
        "audit_required": require_audit,
        "started_at": datetime.now(UTC).isoformat(),
        "artifact_root": artifact_root.as_posix(),
        "evaluator_command": " ".join(shlex.quote(argument) for argument in sys.argv),
        "commands": commands,
    }

    try:
        result["phase0_evidence"] = phase1_evaluator.verify_phase0_evidence()
        result["phase1_evidence"] = verify_phase1_evidence()
        repository = verify_repository_scope()
        result["repository"] = repository
        source_contract = verify_source_contract()
        result["source_contract"] = source_contract
        head = string_value(repository.get("candidate_commit"), "candidate commit")
        platform_results = verify_cross_platform(
            goal_root,
            head,
            cast(int, source_contract["production_unsafe_tokens"]),
        )
        result["performance_evidence"] = platform_results
        result["decisions"] = verify_decisions(goal_root, head, platform_results)
        if require_audit:
            result["independent_audit"] = verify_independent_audit(goal_root, head)

        environment, removed = command_environment()
        result["environment"] = environment_record(removed)
        for name, command in QUALITY_COMMANDS:
            record = run_quality_command(artifact_root, name, command, environment)
            commands.append(cast(dict[str, object], asdict(record)))
        result["status"] = "pass"
    except (
        EvaluationError,
        OSError,
        subprocess.SubprocessError,
        ValueError,
        KeyError,
    ) as error:
        result["error"] = str(error)
    finally:
        result["completed_at"] = datetime.now(UTC).isoformat()
        write_json(evaluation_path, result)
        write_json(top_level_path, result)

    failure = result.get("error", "unknown failure")
    require(
        result["status"] == "pass",
        f"Phase 2 evaluator FAIL: {failure}; see {evaluation_path}",
    )
    return evaluation_path


def parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""

    command_parser = argparse.ArgumentParser(description=__doc__)
    subparsers = command_parser.add_subparsers(dest="command", required=True)
    for name in ("preaudit", "evaluate"):
        subparser = subparsers.add_parser(name)
        subparser.add_argument("--goal-root", type=Path, required=True)
    return command_parser


def main() -> int:
    """Run the requested Phase 2 evaluator stage."""

    arguments = parser().parse_args()
    command = cast(str, arguments.command)
    goal_root = cast(Path, arguments.goal_root)
    if command == "preaudit":
        path = evaluate(goal_root, require_audit=False)
        print(f"INFO Phase 2 preaudit PASS: {path}")
        return 0
    if command == "evaluate":
        path = evaluate(goal_root, require_audit=True)
        print(f"INFO Phase 2 evaluator PASS: {path}")
        return 0
    raise AssertionError(f"unhandled command: {command}")


if __name__ == "__main__":
    raise SystemExit(main())
