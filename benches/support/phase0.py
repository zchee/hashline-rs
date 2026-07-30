"""Capture and evaluate reproducible incompatible-v2 Phase 0 evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import logging
import math
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TypeAlias, cast

LOGGER = logging.getLogger("hashline.phase0")

PLAN_BASELINE = "f3a2f3f41076fc48f3aa4836eda873b21f7a6be6"
PLAN_PATH = ".omx/plans/2026-07-31-incompatible-max-performance-redesign.md"
SCHEMA_VERSION = 1
REQUIRED_PLATFORMS = ("macos-arm64", "linux-amd64")
PROFILE_SCENARIOS = ("full_read", "edit", "rare_grep", "common_grep")
RESOURCE_SCENARIOS = (
    "full_read_base",
    "full_read_candidate",
    "edit_50k_base",
    "edit_50k_candidate",
    "tree_grep_base",
)
QUALITY_COMMANDS = (
    ("fmt", ("cargo", "fmt", "--all", "--", "--check")),
    ("build_all", ("cargo", "build", "--all-targets")),
    (
        "build_no_default",
        ("cargo", "build", "--all-targets", "--no-default-features"),
    ),
    (
        "clippy",
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
        "test_all",
        ("cargo", "test", "--all-targets", "--all-features"),
    ),
    (
        "test_no_default",
        ("cargo", "test", "--all-targets", "--no-default-features"),
    ),
    ("doc", ("cargo", "doc", "--no-deps")),
)

JsonValue: TypeAlias = (
    None | bool | int | float | str | list["JsonValue"] | dict[str, "JsonValue"]
)


@dataclass(frozen=True)
class PairSpec:
    """One Criterion base/candidate comparison."""

    name: str
    group: str
    base: str
    candidate: str

    @property
    def base_filter(self) -> str:
        """Return the full Criterion filter for the base function."""
        return f"{self.group}/{self.base}"

    @property
    def candidate_filter(self) -> str:
        """Return the full Criterion filter for the candidate function."""
        return f"{self.group}/{self.candidate}"


PAIR_SPECS = (
    PairSpec(
        "raw_lines_10k",
        "phase0_snapshot_raw/10000",
        "base_current_index",
        "candidate_raw_line_hashes",
    ),
    PairSpec(
        "raw_lines_50k",
        "phase0_snapshot_raw/50000",
        "base_current_index",
        "candidate_raw_line_hashes",
    ),
    PairSpec(
        "gxhash_version_10k",
        "phase0_snapshot_gxhash/10000",
        "base_current_index",
        "candidate_version_and_count",
    ),
    PairSpec(
        "gxhash_version_50k",
        "phase0_snapshot_gxhash/50000",
        "base_current_index",
        "candidate_version_and_count",
    ),
    PairSpec(
        "xxh3_version_10k",
        "phase0_snapshot_xxh3/10000",
        "base_current_index",
        "candidate_version_and_count",
    ),
    PairSpec(
        "xxh3_version_50k",
        "phase0_snapshot_xxh3/50000",
        "base_current_index",
        "candidate_version_and_count",
    ),
    PairSpec(
        "blake3_version_10k",
        "phase0_snapshot_blake3/10000",
        "base_current_index",
        "candidate_version_and_count",
    ),
    PairSpec(
        "blake3_version_50k",
        "phase0_snapshot_blake3/50000",
        "base_current_index",
        "candidate_version_and_count",
    ),
    PairSpec(
        "sparse_window",
        "phase0_sparse_select/window_2k_of_100k",
        "base_current_partial_index",
        "candidate_sparse_positions",
    ),
    PairSpec(
        "offsets_u32",
        "phase0_offsets/u32_50k",
        "base_current_index",
        "candidate_offsets",
    ),
    PairSpec(
        "offsets_u64",
        "phase0_offsets/u64_50k",
        "base_current_index",
        "candidate_offsets",
    ),
    PairSpec(
        "position_render_full",
        "phase0_position_render/full_10k",
        "base_current_render",
        "candidate_position_render",
    ),
    PairSpec(
        "position_render_window",
        "phase0_position_render/window_2k_of_100k",
        "base_current_render",
        "candidate_position_render",
    ),
    PairSpec(
        "versioned_full_read",
        "phase0_full_read/full_10k",
        "base_current_read",
        "candidate_versioned_read",
    ),
    PairSpec(
        "splice_one",
        "phase0_splice/one_edit_50k",
        "base_current_apply",
        "candidate_byte_splice",
    ),
    PairSpec(
        "splice_eight",
        "phase0_splice/eight_edits_50k",
        "base_current_apply",
        "candidate_byte_splice",
    ),
    PairSpec(
        "atomic_persist",
        "phase0_persist/atomic_50k",
        "base_direct_write",
        "candidate_temp_rename",
    ),
)


@dataclass
class CaptureContext:
    """Paths, environment, and immutable provenance for one capture."""

    repo: Path
    goal_root: Path
    run_root: Path
    platform_key: str
    candidate_commit: str
    phase0_parent: str
    external_repo: Path
    rounds: int
    filesystem_samples: int
    profile_seconds: int
    build_root: Path
    environment: dict[str, str]


class CaptureError(RuntimeError):
    """Raised when evidence would be incomplete or incomparable."""


def run_command(
    command: tuple[str, ...] | list[str],
    *,
    cwd: Path,
    environment: dict[str, str] | None = None,
    log_path: Path | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    """Run one command and optionally persist its exact stdout/stderr."""
    rendered = [str(item) for item in command]
    started_at = datetime.now(UTC).isoformat()
    started_ns = time.monotonic_ns()
    result = subprocess.run(
        rendered,
        cwd=cwd,
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    duration_ns = time.monotonic_ns() - started_ns
    if log_path is not None:
        write_json(
            log_path,
            {
                "schema_version": SCHEMA_VERSION,
                "command": rendered,
                "cwd": str(cwd),
                "started_at": started_at,
                "duration_ns": duration_ns,
                "exit_code": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            },
        )
    if check and result.returncode != 0:
        raise CaptureError(
            f"command failed ({result.returncode}): {' '.join(rendered)}\n"
            f"{result.stderr[-4000:]}"
        )
    return result


def output(
    command: tuple[str, ...] | list[str],
    *,
    cwd: Path,
    environment: dict[str, str] | None = None,
) -> str:
    """Return stripped stdout from a successful command."""
    return run_command(command, cwd=cwd, environment=environment).stdout.strip()


def write_json(path: Path, value: JsonValue | object) -> None:
    """Atomically write deterministic, human-readable JSON."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(f"{path.suffix}.tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")
    temporary.replace(path)


def load_json(path: Path) -> JsonValue:
    """Load one JSON artifact."""
    with path.open(encoding="utf-8") as stream:
        return cast(JsonValue, json.load(stream))


def require_object(value: JsonValue, label: str) -> dict[str, JsonValue]:
    """Require a JSON object and return it."""
    if not isinstance(value, dict):
        raise CaptureError(f"{label} must be a JSON object")
    return value


def require_list(value: JsonValue, label: str) -> list[JsonValue]:
    """Require a JSON array and return it."""
    if not isinstance(value, list):
        raise CaptureError(f"{label} must be a JSON array")
    return value


def sha256_file(path: Path) -> str:
    """Hash one file without loading it into memory."""
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def platform_key() -> str:
    """Map the live target to the two Phase 0 platform identifiers."""
    system = platform.system()
    machine = platform.machine()
    if system == "Darwin" and machine == "arm64":
        return "macos-arm64"
    if system == "Linux" and machine in {"x86_64", "amd64"}:
        return "linux-amd64"
    raise CaptureError(f"unsupported Phase 0 platform: {system}/{machine}")


def clean_environment(build_root: Path) -> dict[str, str]:
    """Create the benchmark environment with ambient compiler flags removed."""
    environment = os.environ.copy()
    environment.pop("RUSTFLAGS", None)
    environment.pop("CARGO_ENCODED_RUSTFLAGS", None)
    environment["CARGO_TARGET_DIR"] = str(build_root)
    environment["LC_ALL"] = "C"
    environment["LANG"] = "C"
    return environment


def assert_clean_repo(repo: Path) -> tuple[str, str]:
    """Require a clean candidate commit containing Phase 0-only changes."""
    status = output(
        ("git", "status", "--porcelain=v1", "--untracked-files=all"),
        cwd=repo,
    )
    if status:
        raise CaptureError(f"candidate worktree is dirty:\n{status}")

    candidate = output(("git", "rev-parse", "HEAD"), cwd=repo)
    parent = output(("git", "rev-parse", "HEAD^"), cwd=repo)
    changed = output(
        ("git", "diff-tree", "--no-commit-id", "--name-only", "-r", candidate),
        cwd=repo,
    ).splitlines()
    allowed = {
        "Cargo.lock",
        "Cargo.toml",
        "benches/hashline.rs",
        "benches/V2_BASELINE.md",
        "benches/support/phase0.py",
        "benches/support/phase0_resources.rs",
        "benches/support/phase0_workloads.rs",
        "benches/support/test_phase0.py",
    }
    unexpected = sorted(set(changed) - allowed)
    if unexpected:
        raise CaptureError(
            "candidate commit contains non-Phase 0 paths: " + ", ".join(unexpected)
        )
    if not changed:
        raise CaptureError("candidate commit has no Phase 0 implementation changes")
    return candidate, parent


def capture_environment(context: CaptureContext) -> dict[str, object]:
    """Capture exact compiler, host, load, and repository provenance."""
    repo = context.repo
    environment = context.environment
    commands = {
        "rustc": ("rustc", "-Vv"),
        "cargo": ("cargo", "-V"),
        "uname": ("uname", "-a"),
        "git_status": (
            "git",
            "status",
            "--short",
            "--branch",
            "--untracked-files=all",
        ),
        "git_worktree": ("git", "worktree", "list", "--porcelain"),
        "cpu": (
            "sh",
            "-c",
            (
                "if command -v lscpu >/dev/null 2>&1; then lscpu; "
                "else sysctl -a 2>/dev/null | "
                "grep -E 'machdep.cpu.brand_string|hw.ncpu|hw.physicalcpu'; fi"
            ),
        ),
        "os": (
            "sh",
            "-c",
            "if command -v sw_vers >/dev/null 2>&1; then sw_vers; else cat /etc/os-release; fi",
        ),
        "load": ("sh", "-c", "uptime; ps -axo state= | sort | uniq -c"),
    }
    captured: dict[str, object] = {}
    for name, command in commands.items():
        result = run_command(command, cwd=repo, environment=environment)
        captured[name] = {
            "command": list(command),
            "exit_code": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
        }

    load_average = list(os.getloadavg())
    return {
        "schema_version": SCHEMA_VERSION,
        "platform_key": context.platform_key,
        "system": platform.system(),
        "machine": platform.machine(),
        "python": sys.version,
        "cpu_count": os.cpu_count(),
        "load_average": load_average,
        "ambient_rustflags_present": bool(
            os.environ.get("RUSTFLAGS") or os.environ.get("CARGO_ENCODED_RUSTFLAGS")
        ),
        "benchmark_rustflags_present": bool(
            environment.get("RUSTFLAGS") or environment.get("CARGO_ENCODED_RUSTFLAGS")
        ),
        "candidate_commit": context.candidate_commit,
        "phase0_parent": context.phase0_parent,
        "baseline_commit": PLAN_BASELINE,
        "commands": captured,
    }


def git_corpus_manifest(repo: Path, label: str) -> dict[str, object]:
    """Hash all tracked Rust files in one exact clean repository commit."""
    commit = output(("git", "rev-parse", "HEAD"), cwd=repo)
    status = output(
        ("git", "status", "--porcelain=v1", "--untracked-files=no"),
        cwd=repo,
    )
    if status:
        raise CaptureError(f"{label} repository is dirty:\n{status}")

    paths = output(("git", "ls-files", "*.rs"), cwd=repo).splitlines()
    if not paths:
        raise CaptureError(f"{label} repository has no tracked Rust files")

    aggregate = hashlib.sha256()
    byte_count = 0
    for relative in paths:
        file_path = repo / relative
        file_hash = sha256_file(file_path)
        byte_count += file_path.stat().st_size
        aggregate.update(relative.encode())
        aggregate.update(b"\0")
        aggregate.update(file_hash.encode())
        aggregate.update(b"\n")

    return {
        "label": label,
        "path": str(repo.resolve()),
        "commit": commit,
        "rust_file_count": len(paths),
        "rust_bytes": byte_count,
        "aggregate_sha256": aggregate.hexdigest(),
    }


def build_benchmarks(context: CaptureContext) -> dict[str, Path]:
    """Build regular shipping-profile benchmark executables and locate them."""
    command = (
        "cargo",
        "bench",
        "--bench",
        "hashline",
        "--bench",
        "phase0-resources",
        "--no-run",
        "--message-format=json-render-diagnostics",
    )
    result = run_command(
        command,
        cwd=context.repo,
        environment=context.environment,
        log_path=context.run_root / "commands" / "build_benches.json",
    )
    executables: dict[str, Path] = {}
    for line in result.stdout.splitlines():
        try:
            value = require_object(
                cast(JsonValue, json.loads(line)),
                "cargo compiler artifact",
            )
        except (json.JSONDecodeError, CaptureError):
            continue
        if value.get("reason") != "compiler-artifact":
            continue
        target_value = value.get("target")
        if not isinstance(target_value, dict):
            continue
        name = target_value.get("name")
        reported_executable = value.get("executable")
        if isinstance(name, str) and isinstance(reported_executable, str):
            executables[name] = Path(reported_executable)

    for required in ("hashline", "phase0-resources"):
        built_executable = executables.get(required)
        if built_executable is None or not built_executable.is_file():
            raise CaptureError(f"cargo did not report executable for {required}")
    return executables


def copy_criterion_tree(source: Path, destination: Path) -> None:
    """Copy one raw Criterion tree after a successful canonical run."""
    if not source.is_dir():
        raise CaptureError(f"Criterion output missing: {source}")
    shutil.copytree(source, destination)


def capture_baseline(context: CaptureContext) -> dict[str, object]:
    """Run the preserved driver suite at the plan's exact clean baseline."""
    baseline_root = context.run_root / "baseline"
    with tempfile.TemporaryDirectory(prefix="hashline-phase0-base-") as temporary:
        worktree = Path(temporary) / "worktree"
        baseline_target = Path(temporary) / "target"
        run_command(
            ("git", "worktree", "add", "--detach", str(worktree), PLAN_BASELINE),
            cwd=context.repo,
            log_path=baseline_root / "worktree_add.json",
        )
        try:
            baseline_environment = clean_environment(baseline_target)
            head = output(("git", "rev-parse", "HEAD"), cwd=worktree)
            dirty = output(
                ("git", "status", "--porcelain=v1", "--untracked-files=all"),
                cwd=worktree,
            )
            if head != PLAN_BASELINE or dirty:
                raise CaptureError(
                    "baseline worktree is not the exact clean plan commit"
                )
            run = run_command(
                ("cargo", "bench", "--bench", "hashline"),
                cwd=worktree,
                environment=baseline_environment,
                log_path=baseline_root / "cargo_bench.json",
            )
            copy_criterion_tree(
                baseline_target / "criterion",
                baseline_root / "criterion",
            )
            return {
                "head": head,
                "dirty": bool(dirty),
                "command": ["cargo", "bench", "--bench", "hashline"],
                "exit_code": run.returncode,
                "raw_criterion": "baseline/criterion",
            }
        finally:
            run_command(
                ("git", "worktree", "remove", "--force", str(worktree)),
                cwd=context.repo,
                log_path=baseline_root / "worktree_remove.json",
                check=False,
            )


def capture_candidate_full(context: CaptureContext) -> dict[str, object]:
    """Run the complete candidate Criterion suite once before paired repeats."""
    result = run_command(
        ("cargo", "bench", "--bench", "hashline"),
        cwd=context.repo,
        environment=context.environment,
        log_path=context.run_root / "candidate" / "cargo_bench.json",
    )
    copy_criterion_tree(
        context.build_root / "criterion",
        context.run_root / "candidate" / "criterion",
    )
    return {
        "head": context.candidate_commit,
        "command": ["cargo", "bench", "--bench", "hashline"],
        "exit_code": result.returncode,
        "raw_criterion": "candidate/criterion",
    }


def newest_estimate(criterion_root: Path, started_ns: int) -> Path:
    """Find the one Criterion estimate updated by an exact filtered run."""
    candidates = [
        path
        for path in criterion_root.rglob("estimates.json")
        if path.stat().st_mtime_ns >= started_ns
    ]
    if not candidates:
        candidates = list(criterion_root.rglob("estimates.json"))
    if not candidates:
        raise CaptureError("filtered Criterion run produced no estimates.json")
    return max(candidates, key=lambda path: path.stat().st_mtime_ns)


def estimate_summary(path: Path) -> dict[str, object]:
    """Extract Criterion's median point estimate and confidence interval."""
    root = require_object(load_json(path), str(path))
    median = require_object(root.get("median"), f"{path} median")
    interval = require_object(
        median.get("confidence_interval"),
        f"{path} median confidence interval",
    )
    point = median.get("point_estimate")
    lower = interval.get("lower_bound")
    upper = interval.get("upper_bound")
    if not all(isinstance(item, (int, float)) for item in (point, lower, upper)):
        raise CaptureError(f"Criterion median estimate is incomplete: {path}")
    return {
        "point_estimate_ns": cast(float | int, point),
        "confidence_interval_ns": {
            "lower_bound": cast(float | int, lower),
            "upper_bound": cast(float | int, upper),
            "confidence_level": interval.get("confidence_level"),
        },
    }


def run_filtered_benchmark(
    context: CaptureContext,
    executable: Path,
    pair: PairSpec,
    variant: str,
    sequence: int,
) -> dict[str, object]:
    """Run and preserve one exact filtered Criterion benchmark."""
    benchmark_filter = pair.base_filter if variant == "base" else pair.candidate_filter
    raw_root = context.run_root / "paired" / pair.name / f"{sequence:02d}-{variant}"
    started_ns = time.time_ns()
    result = run_command(
        (str(executable), "--bench", benchmark_filter),
        cwd=context.repo,
        environment=context.environment,
        log_path=raw_root / "command.json",
    )
    estimate = newest_estimate(context.build_root / "criterion", started_ns)
    shutil.copytree(estimate.parent, raw_root / "criterion_new")
    return {
        "sequence": sequence,
        "variant": variant,
        "filter": benchmark_filter,
        "exit_code": result.returncode,
        "estimate": estimate_summary(estimate),
        "raw_path": str(raw_root.relative_to(context.run_root)),
    }


def capture_pairs(
    context: CaptureContext,
    executable: Path,
) -> list[dict[str, object]]:
    """Capture every pair with alternating base/candidate execution order."""
    pair_results: list[dict[str, object]] = []
    sequence = 0
    for pair in PAIR_SPECS:
        runs: list[dict[str, object]] = []
        for round_index in range(context.rounds):
            variants = (
                ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")
            )
            for variant in variants:
                sequence += 1
                runs.append(
                    run_filtered_benchmark(
                        context,
                        executable,
                        pair,
                        variant,
                        sequence,
                    )
                )
        pair_results.append(
            {
                "name": pair.name,
                "group": pair.group,
                "base": pair.base,
                "candidate": pair.candidate,
                "order": [run["variant"] for run in runs],
                "runs": runs,
            }
        )
    write_json(context.run_root / "paired" / "results.json", pair_results)
    return pair_results


def bootstrap_median_interval(samples: list[int]) -> dict[str, float]:
    """Return a deterministic 95 percent bootstrap interval for a median."""
    if len(samples) < 2:
        raise CaptureError("at least two filesystem samples are required")
    generator = random.Random(0x48415348)
    medians = []
    for _ in range(10_000):
        resample = [generator.choice(samples) for _ in samples]
        medians.append(statistics.median(resample))
    medians.sort()
    lower = medians[math.floor(0.025 * (len(medians) - 1))]
    upper = medians[math.ceil(0.975 * (len(medians) - 1))]
    return {
        "point_estimate_ns": float(statistics.median(samples)),
        "lower_bound_ns": float(lower),
        "upper_bound_ns": float(upper),
    }


def resource_executable(context: CaptureContext, executables: dict[str, Path]) -> Path:
    """Return the built one-shot resource probe."""
    executable = executables.get("phase0-resources")
    if executable is None:
        raise CaptureError("phase0-resources executable was not built")
    return executable


def generate_filesystem_corpus(context: CaptureContext, executable: Path) -> Path:
    """Materialize and record the exact shared 50k-line filesystem corpus."""
    corpus = context.run_root / "corpora" / "filesystem-50k.rs"
    result = run_command(
        (str(executable), "corpus", "50000", "0xf1500050", str(corpus)),
        cwd=context.repo,
        environment=context.environment,
        log_path=context.run_root / "corpora" / "filesystem_command.json",
    )
    require_object(
        cast(JsonValue, json.loads(result.stdout)),
        "filesystem corpus generator result",
    )
    return corpus


def capture_filesystem(
    context: CaptureContext,
    executable: Path,
    corpus: Path,
) -> list[dict[str, object]]:
    """Run interleaved one-shot cold/warm base/candidate filesystem probes."""
    records: list[dict[str, JsonValue]] = []
    sequence = 0
    for cache_state in ("cold", "warm"):
        for sample_index in range(context.filesystem_samples):
            variants = (
                ("base", "candidate")
                if sample_index % 2 == 0
                else ("candidate", "base")
            )
            for variant in variants:
                sequence += 1
                result = run_command(
                    (
                        str(executable),
                        "filesystem",
                        variant,
                        cache_state,
                        str(corpus),
                    ),
                    cwd=context.repo,
                    environment=context.environment,
                    log_path=(
                        context.run_root
                        / "filesystem"
                        / f"{sequence:03d}-{cache_state}-{variant}.json"
                    ),
                )
                record = require_object(
                    cast(JsonValue, json.loads(result.stdout)),
                    "filesystem probe result",
                )
                record["sequence"] = sequence
                records.append(record)

    summary: list[dict[str, object]] = []
    for cache_state in ("cold", "warm"):
        policies: set[str] = set()
        for variant in ("base", "candidate"):
            selected = [
                record
                for record in records
                if record.get("cache_state") == cache_state
                and record.get("variant") == variant
            ]
            samples = [
                cast(int, record["elapsed_ns"])
                for record in selected
                if isinstance(record.get("elapsed_ns"), int)
            ]
            policies.update(
                cast(str, record["cache_policy"])
                for record in selected
                if isinstance(record.get("cache_policy"), str)
            )
            summary.append(
                {
                    "cache_state": cache_state,
                    "variant": variant,
                    "sample_count": len(samples),
                    "samples_ns": samples,
                    "estimate": bootstrap_median_interval(samples),
                }
            )
        if len(policies) != 1:
            raise CaptureError(
                f"{cache_state} base/candidate used different cache policies: {policies}"
            )

    value = {"records": records, "summary": summary}
    write_json(context.run_root / "filesystem" / "results.json", value)
    return summary


def parse_peak_rss(stderr: str) -> tuple[int, str]:
    """Parse /usr/bin/time peak RSS into bytes on macOS or Linux."""
    if platform.system() == "Darwin":
        match = re.search(
            r"^\s*(\d+)\s+maximum resident set size", stderr, re.MULTILINE
        )
        if match is None:
            raise CaptureError("macOS time output omitted maximum resident set size")
        return int(match.group(1)), "bytes"
    match = re.search(
        r"Maximum resident set size \(kbytes\):\s*(\d+)",
        stderr,
    )
    if match is None:
        raise CaptureError("Linux time output omitted maximum resident set size")
    return int(match.group(1)) * 1024, "kibibytes converted to bytes"


def timed_resource_command(executable: Path, arguments: tuple[str, ...]) -> list[str]:
    """Build the platform-specific /usr/bin/time command."""
    if platform.system() == "Darwin":
        return ["/usr/bin/time", "-l", str(executable), *arguments]
    return ["/usr/bin/time", "-v", str(executable), *arguments]


def capture_resources(
    context: CaptureContext,
    executable: Path,
) -> list[dict[str, JsonValue]]:
    """Capture allocation counters and whole-process peak RSS."""
    records: list[dict[str, JsonValue]] = []
    scenarios: list[tuple[str, Path | None]] = [
        *[(scenario, None) for scenario in RESOURCE_SCENARIOS],
        ("real_tree_grep_base", context.repo),
        ("real_tree_grep_base", context.external_repo),
    ]
    for index, (scenario, optional_path) in enumerate(scenarios):
        arguments = ["measure", scenario]
        label = scenario
        if optional_path is not None:
            arguments.append(str(optional_path))
            label = (
                f"{scenario}_own"
                if optional_path == context.repo
                else f"{scenario}_external"
            )
        result = run_command(
            timed_resource_command(executable, tuple(arguments)),
            cwd=context.repo,
            environment=context.environment,
            log_path=context.run_root / "resources" / f"{index:02d}-{label}.json",
        )
        record = require_object(
            cast(JsonValue, json.loads(result.stdout)),
            "resource probe result",
        )
        peak_rss, rss_source = parse_peak_rss(result.stderr)
        record["label"] = label
        record["peak_rss_bytes"] = peak_rss
        record["peak_rss_source"] = rss_source
        records.append(record)
    write_json(context.run_root / "resources" / "results.json", records)
    return records


def build_symbolized_probe(context: CaptureContext, profile_target: Path) -> Path:
    """Build a debug-symbol bench profile without changing optimization flags."""
    environment = context.environment.copy()
    environment["CARGO_TARGET_DIR"] = str(profile_target)
    environment["CARGO_PROFILE_BENCH_DEBUG"] = "2"
    environment["CARGO_PROFILE_BENCH_STRIP"] = "none"
    result = run_command(
        (
            "cargo",
            "bench",
            "--bench",
            "phase0-resources",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ),
        cwd=context.repo,
        environment=environment,
        log_path=context.run_root / "profiles" / "build.json",
    )
    for line in result.stdout.splitlines():
        try:
            value = require_object(
                cast(JsonValue, json.loads(line)),
                "profile compiler artifact",
            )
        except (json.JSONDecodeError, CaptureError):
            continue
        target = value.get("target")
        executable = value.get("executable")
        if (
            value.get("reason") == "compiler-artifact"
            and isinstance(target, dict)
            and target.get("name") == "phase0-resources"
            and isinstance(executable, str)
        ):
            return Path(executable)
    raise CaptureError("symbolized phase0-resources executable was not reported")


def profile_macos(
    context: CaptureContext,
    executable: Path,
    scenario: str,
    environment: dict[str, str],
) -> dict[str, object]:
    """Capture one macOS sample stack report."""
    profile_root = context.run_root / "profiles" / scenario
    profile_root.mkdir(parents=True, exist_ok=True)
    process_stdout = (profile_root / "probe_stdout.json").open("w", encoding="utf-8")
    process_stderr = (profile_root / "probe_stderr.log").open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(executable), "profile", scenario, str(context.profile_seconds)],
        cwd=context.repo,
        env=environment,
        text=True,
        stdout=process_stdout,
        stderr=process_stderr,
    )
    raw = profile_root / "sample.txt"
    try:
        time.sleep(0.75)
        sample = run_command(
            (
                "sample",
                str(process.pid),
                "5",
                "1",
                "-mayDie",
                "-fullPaths",
                "-file",
                str(raw),
            ),
            cwd=context.repo,
            environment=environment,
            log_path=profile_root / "sample_command.json",
        )
        exit_code = process.wait(timeout=context.profile_seconds + 10)
        if exit_code != 0:
            raise CaptureError(f"profile probe {scenario} exited {exit_code}")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=10)
        process_stdout.close()
        process_stderr.close()

    text = raw.read_text(encoding="utf-8", errors="replace")
    symbol_hits = sorted(
        symbol
        for symbol in (
            "profile_full_read_once",
            "profile_edit_once",
            "profile_grep_once",
            "format_hashline_content",
            "apply_edits",
            "run_grep",
        )
        if symbol in text
    )
    return {
        "scenario": scenario,
        "tool": "sample",
        "command_exit_code": sample.returncode,
        "raw_path": str(raw.relative_to(context.run_root)),
        "symbol_hits": symbol_hits,
        "symbolized": bool(symbol_hits),
    }


def profile_linux(
    context: CaptureContext,
    executable: Path,
    scenario: str,
    environment: dict[str, str],
) -> dict[str, object]:
    """Capture one Linux perf DWARF call-graph report."""
    profile_root = context.run_root / "profiles" / scenario
    profile_root.mkdir(parents=True, exist_ok=True)
    data = profile_root / "perf.data"
    record = run_command(
        (
            "perf",
            "record",
            "-F",
            "999",
            "-g",
            "--call-graph",
            "dwarf",
            "-o",
            str(data),
            "--",
            str(executable),
            "profile",
            scenario,
            str(context.profile_seconds),
        ),
        cwd=context.repo,
        environment=environment,
        log_path=profile_root / "perf_record.json",
    )
    report = run_command(
        (
            "perf",
            "report",
            "--stdio",
            "--no-children",
            "-i",
            str(data),
        ),
        cwd=context.repo,
        environment=environment,
        log_path=profile_root / "perf_report.json",
    )
    raw = profile_root / "perf_report.txt"
    raw.write_text(report.stdout, encoding="utf-8")
    symbol_hits = sorted(
        symbol
        for symbol in (
            "profile_full_read_once",
            "profile_edit_once",
            "profile_grep_once",
            "format_hashline_content",
            "apply_edits",
            "run_grep",
        )
        if symbol in report.stdout
    )
    return {
        "scenario": scenario,
        "tool": "perf",
        "command_exit_code": record.returncode,
        "raw_path": str(raw.relative_to(context.run_root)),
        "perf_data_path": str(data.relative_to(context.run_root)),
        "symbol_hits": symbol_hits,
        "symbolized": bool(symbol_hits),
    }


def capture_profiles(
    context: CaptureContext, profile_target: Path
) -> list[dict[str, object]]:
    """Capture four symbolized hot-path profiles."""
    executable = build_symbolized_probe(context, profile_target)
    environment = context.environment.copy()
    environment["CARGO_TARGET_DIR"] = str(profile_target)
    records = []
    for scenario in PROFILE_SCENARIOS:
        if platform.system() == "Darwin":
            record = profile_macos(context, executable, scenario, environment)
        else:
            record = profile_linux(context, executable, scenario, environment)
        if not record["symbolized"]:
            raise CaptureError(f"profile {scenario} contains no workload symbols")
        records.append(record)
    write_json(context.run_root / "profiles" / "results.json", records)
    return records


def capture_quality(context: CaptureContext) -> list[dict[str, object]]:
    """Run the plan's Phase 0 quality matrix sequentially."""
    records: list[dict[str, object]] = []
    for name, command in QUALITY_COMMANDS:
        result = run_command(
            command,
            cwd=context.repo,
            environment=context.environment,
            log_path=context.run_root / "quality" / f"{name}.json",
            check=False,
        )
        records.append(
            {
                "name": name,
                "command": list(command),
                "exit_code": result.returncode,
            }
        )
        if result.returncode != 0:
            raise CaptureError(f"quality command failed: {name}")
    write_json(context.run_root / "quality" / "results.json", records)
    return records


def write_hash_manifest(run_root: Path) -> dict[str, str]:
    """Hash every raw artifact except the checksum file itself."""
    checksum_path = run_root / "SHA256SUMS.json"
    hashes = {
        str(path.relative_to(run_root)): sha256_file(path)
        for path in sorted(run_root.rglob("*"))
        if path.is_file() and path != checksum_path
    }
    write_json(checksum_path, hashes)
    return hashes


def update_latest(goal_root: Path, key: str, run_root: Path) -> None:
    """Point one platform's canonical latest record at an immutable run."""
    platform_root = goal_root / "artifacts" / key
    relative = run_root.relative_to(platform_root)
    write_json(
        platform_root / "latest.json",
        {
            "schema_version": SCHEMA_VERSION,
            "run": str(relative),
        },
    )


def capture(args: argparse.Namespace) -> int:
    """Capture the complete artifact set for the live platform."""
    repo = Path.cwd().resolve()
    goal_root = (repo / args.goal_root).resolve()
    external_repo = Path(args.external_repo).resolve()
    key = platform_key()
    candidate, parent = assert_clean_repo(repo)
    run_id = f"{datetime.now(UTC).strftime('%Y%m%dT%H%M%SZ')}-{candidate[:12]}"
    run_root = goal_root / "artifacts" / key / "runs" / run_id
    run_root.mkdir(parents=True, exist_ok=False)

    with (
        tempfile.TemporaryDirectory(prefix=f"hashline-{key}-target-") as build_temp,
        tempfile.TemporaryDirectory(prefix=f"hashline-{key}-profile-") as profile_temp,
    ):
        build_root = Path(build_temp)
        environment = clean_environment(build_root)
        context = CaptureContext(
            repo=repo,
            goal_root=goal_root,
            run_root=run_root,
            platform_key=key,
            candidate_commit=candidate,
            phase0_parent=parent,
            external_repo=external_repo,
            rounds=args.rounds,
            filesystem_samples=args.filesystem_samples,
            profile_seconds=args.profile_seconds,
            build_root=build_root,
            environment=environment,
        )

        try:
            environment_record = capture_environment(context)
            write_json(run_root / "environment.json", environment_record)
            corpora = {
                "synthetic": {
                    "generator": "benches/support/phase0_workloads.rs",
                    "seeds": [
                        "0xB2000010",
                        "0xB2000050",
                        "0xB2000100",
                        "0xf1500050",
                    ],
                },
                "repository": git_corpus_manifest(repo, "hashline-rs"),
                "external_repository": git_corpus_manifest(
                    external_repo,
                    "external-rust-repository",
                ),
            }
            write_json(run_root / "corpora" / "manifest.json", corpora)

            executables = build_benchmarks(context)
            probe = resource_executable(context, executables)
            self_test = run_command(
                (str(probe), "self-test"),
                cwd=repo,
                environment=environment,
                log_path=run_root / "commands" / "resource_self_test.json",
            )
            require_object(
                cast(JsonValue, json.loads(self_test.stdout)),
                "resource self-test",
            )

            baseline = capture_baseline(context)
            candidate_full = capture_candidate_full(context)
            paired = capture_pairs(context, executables["hashline"])
            corpus = generate_filesystem_corpus(context, probe)
            filesystem_results = capture_filesystem(context, probe, corpus)
            resources = capture_resources(context, probe)
            profiles = capture_profiles(context, Path(profile_temp))
            quality = capture_quality(context)

            manifest: dict[str, object] = {
                "schema_version": SCHEMA_VERSION,
                "status": "pass",
                "platform_key": key,
                "captured_at": datetime.now(UTC).isoformat(),
                "plan": {
                    "path": PLAN_PATH,
                    "sha256": sha256_file(repo / PLAN_PATH),
                },
                "baseline": baseline,
                "candidate": candidate_full,
                "provenance": {
                    "candidate_commit": candidate,
                    "phase0_parent": parent,
                    "dirty": False,
                    "ambient_rustflags_removed": True,
                },
                "corpora": corpora,
                "paired_result_count": len(paired),
                "filesystem_result_count": len(filesystem_results),
                "resource_result_count": len(resources),
                "profile_result_count": len(profiles),
                "quality": quality,
            }
            write_json(run_root / "manifest.json", manifest)
            hashes = write_hash_manifest(run_root)
            if not hashes:
                raise CaptureError("artifact checksum manifest is empty")
            update_latest(goal_root, key, run_root)
            LOGGER.info("captured %s evidence at %s", key, run_root)
            return 0
        except Exception as error:
            write_json(
                run_root / "FAILED.json",
                {
                    "schema_version": SCHEMA_VERSION,
                    "status": "fail",
                    "error": str(error),
                    "failed_at": datetime.now(UTC).isoformat(),
                },
            )
            write_hash_manifest(run_root)
            LOGGER.exception("Phase 0 capture failed")
            return 1


def latest_run(goal_root: Path, key: str) -> Path:
    """Resolve one platform's latest immutable run directory."""
    platform_root = goal_root / "artifacts" / key
    latest = require_object(load_json(platform_root / "latest.json"), f"{key} latest")
    run = latest.get("run")
    if not isinstance(run, str):
        raise CaptureError(f"{key} latest record has no run path")
    path = (platform_root / run).resolve()
    if not path.is_relative_to(platform_root.resolve()):
        raise CaptureError(f"{key} latest run escapes its platform artifact root")
    if not path.is_dir():
        raise CaptureError(f"{key} latest run is missing: {path}")
    return path


def validate_hashes(run_root: Path) -> int:
    """Verify every checksum in one immutable artifact run."""
    hashes = require_object(load_json(run_root / "SHA256SUMS.json"), "checksums")
    if not hashes:
        raise CaptureError(f"empty checksum manifest: {run_root}")
    for relative, expected in hashes.items():
        if not isinstance(expected, str):
            raise CaptureError(f"non-string checksum for {relative}")
        path = run_root / relative
        if not path.is_file():
            raise CaptureError(f"hashed artifact missing: {path}")
        actual = sha256_file(path)
        if actual != expected:
            raise CaptureError(f"checksum mismatch: {path}")
    return len(hashes)


def validate_pair_results(run_root: Path) -> None:
    """Validate all required interleaved Criterion estimates."""
    results = require_list(
        load_json(run_root / "paired" / "results.json"),
        "paired results",
    )
    by_name = {
        cast(str, require_object(item, "pair result").get("name")): require_object(
            item,
            "pair result",
        )
        for item in results
    }
    for spec in PAIR_SPECS:
        result = by_name.get(spec.name)
        if result is None:
            raise CaptureError(f"missing pair result: {spec.name}")
        runs = require_list(result.get("runs"), f"{spec.name} runs")
        variants = [
            require_object(run, f"{spec.name} run").get("variant") for run in runs
        ]
        if len(runs) < 6 or variants.count("base") != variants.count("candidate"):
            raise CaptureError(f"{spec.name} is not a balanced three-round pair")
        if all(
            variants[index] == variants[index + 1] for index in range(len(variants) - 1)
        ):
            raise CaptureError(f"{spec.name} execution order is not interleaved")
        for run in runs:
            run_object = require_object(run, f"{spec.name} run")
            estimate = require_object(
                run_object.get("estimate"),
                f"{spec.name} estimate",
            )
            point = estimate.get("point_estimate_ns")
            interval = require_object(
                estimate.get("confidence_interval_ns"),
                f"{spec.name} confidence interval",
            )
            lower = interval.get("lower_bound")
            upper = interval.get("upper_bound")
            if not all(
                isinstance(value, (int, float)) for value in (point, lower, upper)
            ):
                raise CaptureError(f"{spec.name} has incomplete Criterion estimates")
            if not cast(float, lower) <= cast(float, point) <= cast(float, upper):
                raise CaptureError(
                    f"{spec.name} point estimate lies outside its interval"
                )


def validate_filesystem(run_root: Path) -> None:
    """Validate balanced cold/warm filesystem samples and page-cache policies."""
    value = require_object(
        load_json(run_root / "filesystem" / "results.json"),
        "filesystem results",
    )
    records = require_list(value.get("records"), "filesystem records")
    for cache_state in ("cold", "warm"):
        policies = {
            cast(str, record_object["cache_policy"])
            for record in records
            for record_object in [require_object(record, "filesystem record")]
            if record_object.get("cache_state") == cache_state
            and isinstance(record_object.get("cache_policy"), str)
        }
        if len(policies) != 1:
            raise CaptureError(f"{cache_state} filesystem policy is not comparable")
        for variant in ("base", "candidate"):
            selected = [
                require_object(record, "filesystem record")
                for record in records
                if require_object(record, "filesystem record").get("cache_state")
                == cache_state
                and require_object(record, "filesystem record").get("variant")
                == variant
            ]
            if len(selected) < 10:
                raise CaptureError(
                    f"{cache_state}/{variant} has fewer than ten samples"
                )


def validate_resources(run_root: Path) -> None:
    """Validate allocation counters and peak RSS for required workloads."""
    records = require_list(
        load_json(run_root / "resources" / "results.json"),
        "resource results",
    )
    labels = {
        cast(str, require_object(record, "resource record").get("label"))
        for record in records
    }
    required = {
        *RESOURCE_SCENARIOS,
        "real_tree_grep_base_own",
        "real_tree_grep_base_external",
    }
    if not required.issubset(labels):
        raise CaptureError(
            "resource scenarios missing: " + ", ".join(sorted(required - labels))
        )
    for record in records:
        item = require_object(record, "resource record")
        allocations = require_object(item.get("allocations"), "allocation counters")
        if not isinstance(allocations.get("allocation_calls"), int):
            raise CaptureError("resource record lacks allocation count")
        if not isinstance(item.get("peak_rss_bytes"), int):
            raise CaptureError("resource record lacks peak RSS")


def validate_profiles(run_root: Path) -> None:
    """Validate the four symbolized profile reports."""
    records = require_list(
        load_json(run_root / "profiles" / "results.json"),
        "profile results",
    )
    by_scenario = {
        cast(
            str, require_object(record, "profile record").get("scenario")
        ): require_object(
            record,
            "profile record",
        )
        for record in records
    }
    for scenario in PROFILE_SCENARIOS:
        record = by_scenario.get(scenario)
        if record is None or record.get("symbolized") is not True:
            raise CaptureError(f"profile is not symbolized: {scenario}")
        raw = record.get("raw_path")
        if not isinstance(raw, str) or not (run_root / raw).is_file():
            raise CaptureError(f"profile raw report is missing: {scenario}")


def validate_quality(run_root: Path) -> None:
    """Validate every declared quality command."""
    records = require_list(
        load_json(run_root / "quality" / "results.json"),
        "quality results",
    )
    statuses = {
        cast(str, require_object(record, "quality record").get("name")): require_object(
            record,
            "quality record",
        ).get("exit_code")
        for record in records
    }
    for name, _ in QUALITY_COMMANDS:
        if statuses.get(name) != 0:
            raise CaptureError(f"quality command did not pass: {name}")


def validate_corpora(run_root: Path) -> None:
    """Validate real-repository corpus identity and size."""
    value = require_object(
        load_json(run_root / "corpora" / "manifest.json"),
        "corpus manifest",
    )
    for key in ("repository", "external_repository"):
        corpus = require_object(value.get(key), f"{key} corpus")
        commit = corpus.get("commit")
        count = corpus.get("rust_file_count")
        digest = corpus.get("aggregate_sha256")
        if not isinstance(commit, str) or len(commit) != 40:
            raise CaptureError(f"{key} has no exact commit")
        if not isinstance(count, int) or count <= 0:
            raise CaptureError(f"{key} has no Rust files")
        if not isinstance(digest, str) or len(digest) != 64:
            raise CaptureError(f"{key} has no aggregate corpus hash")
    external = require_object(value.get("external_repository"), "external corpus")
    if cast(int, external["rust_file_count"]) < 300:
        raise CaptureError(
            "external Rust repository is too small for the large-repo gate"
        )


def validate_run(run_root: Path, key: str) -> dict[str, JsonValue]:
    """Validate one complete platform run."""
    checksum_count = validate_hashes(run_root)
    manifest = require_object(load_json(run_root / "manifest.json"), "manifest")
    if manifest.get("status") != "pass" or manifest.get("platform_key") != key:
        raise CaptureError(f"{key} manifest is not passing")
    baseline = require_object(manifest.get("baseline"), f"{key} baseline")
    if baseline.get("head") != PLAN_BASELINE or baseline.get("dirty") is not False:
        raise CaptureError(f"{key} baseline is not the exact clean plan commit")
    provenance = require_object(manifest.get("provenance"), f"{key} provenance")
    if provenance.get("dirty") is not False:
        raise CaptureError(f"{key} candidate provenance is dirty")
    validate_pair_results(run_root)
    validate_filesystem(run_root)
    validate_resources(run_root)
    validate_profiles(run_root)
    validate_quality(run_root)
    validate_corpora(run_root)
    return {
        "platform": key,
        "run_root": str(run_root),
        "candidate_commit": provenance.get("candidate_commit"),
        "checksum_count": checksum_count,
    }


def evaluate(args: argparse.Namespace) -> int:
    """Evaluate both platform artifact sets without running benchmarks."""
    repo = Path.cwd().resolve()
    goal_root = (repo / args.goal_root).resolve()
    try:
        records = [
            validate_run(latest_run(goal_root, key), key) for key in REQUIRED_PLATFORMS
        ]
        commits = {record.get("candidate_commit") for record in records}
        if len(commits) != 1:
            raise CaptureError(
                "macOS and Linux artifacts use different candidate commits"
            )
        summary = {
            "schema_version": SCHEMA_VERSION,
            "status": "pass",
            "evaluated_at": datetime.now(UTC).isoformat(),
            "candidate_commit": next(iter(commits)),
            "platforms": records,
        }
        write_json(goal_root / "evaluation.json", summary)
        LOGGER.info("Phase 0 evaluator PASS for %s", next(iter(commits)))
        return 0
    except (CaptureError, OSError, ValueError, json.JSONDecodeError) as error:
        failure = {
            "schema_version": SCHEMA_VERSION,
            "status": "fail",
            "evaluated_at": datetime.now(UTC).isoformat(),
            "error": str(error),
        }
        write_json(goal_root / "evaluation.json", failure)
        LOGGER.error("Phase 0 evaluator FAIL: %s", error)
        return 1


def self_test() -> int:
    """Exercise deterministic statistics and pair-contract invariants."""
    interval = bootstrap_median_interval([10, 20, 30, 40, 50])
    if interval["point_estimate_ns"] != 30.0:
        raise CaptureError("bootstrap point estimate is not the median")
    if interval["lower_bound_ns"] > 30 or interval["upper_bound_ns"] < 30:
        raise CaptureError("bootstrap interval does not contain the median")
    names = {pair.name for pair in PAIR_SPECS}
    if len(names) != len(PAIR_SPECS):
        raise CaptureError("paired benchmark names are not unique")
    LOGGER.info("Phase 0 Python self-test PASS (%d pairs)", len(PAIR_SPECS))
    return 0


def parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""
    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)

    capture_parser = subcommands.add_parser("capture")
    capture_parser.add_argument("--goal-root", required=True)
    capture_parser.add_argument("--external-repo", required=True)
    capture_parser.add_argument("--rounds", type=int, default=3)
    capture_parser.add_argument("--filesystem-samples", type=int, default=12)
    capture_parser.add_argument("--profile-seconds", type=int, default=8)

    evaluate_parser = subcommands.add_parser("evaluate")
    evaluate_parser.add_argument("--goal-root", required=True)

    subcommands.add_parser("self-test")
    return root


def main() -> int:
    """Dispatch the requested Phase 0 workflow command."""
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    args = parser().parse_args()
    if args.command == "capture":
        if args.rounds < 3:
            raise CaptureError("at least three interleaved rounds are required")
        if args.filesystem_samples < 10:
            raise CaptureError("at least ten cold/warm samples are required")
        if args.profile_seconds < 6:
            raise CaptureError("profiles must run long enough for five-second sampling")
        return capture(args)
    if args.command == "evaluate":
        return evaluate(args)
    return self_test()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CaptureError as error:
        LOGGER.error("%s", error)
        raise SystemExit(1) from error
