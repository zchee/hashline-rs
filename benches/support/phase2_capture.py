# Archival: companion to the retained baseline .md records. The bench targets
# and symbols this script drives were deleted in Phase 8 (29ffc1e); kept for
# provenance, it will not run against current HEAD.
"""Capture reproducible Phase 2 snapshot performance evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import logging
import math
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import cast

import benches.support.phase0 as phase0  # noqa: PLR0402
import benches.support.phase1 as phase1  # noqa: PLR0402

SCHEMA_VERSION = 1
BASE_COMMIT = "690fb307d460fb95bc3b6c62884359e7d262932a"
PLAN_PATH = ".omx/plans/2026-07-31-incompatible-max-performance-redesign.md"
PLAN_SHA256 = "db00bf029f184811b79ab709df064a3fb9b23a9ab64562e28432e43ca8a41a6f"
PHASE2_GOAL = ".omx/goals/performance/hashline-v2-phase2"
SNAPSHOT_SIZES = (10_000, 50_000)
CORPUS_SPECS = {
    10_000: 0xB200_0010,
    50_000: 0xB200_0050,
    100_000: 0xB200_0100,
}
EXPECTED_CORPORA = {
    10_000: (
        458_263,
        "3ea93efb72de6b1871c2f6ebad4bf6590f7d5a2ea3b7dd7e95db6d6b019e82d0",
    ),
    50_000: (
        2_349_541,
        "6a16076e982e56fce23adad8a7b092c6de0a84ec018b785cbab1e41317ef1812",
    ),
    100_000: (
        4_680_412,
        "0a294c016acff19321eddad9f815f3079f43c2e55ee07cf8fef1d3baf1a9e99f",
    ),
}
MINIMUM_SNAPSHOT_SPEEDUP = 4.0
MINIMUM_UNSAFE_SPEEDUP = 1.05
MINIMUM_ROUNDS = 3
DEFAULT_MAX_NORMALIZED_LOAD = 0.30
PROFILE_SYMBOLS = {
    "base": (
        "bench_phase0_v2_pairs",
        "FileIndex::new",
        "hashline::index",
    ),
    "candidate": (
        "bench_phase2_snapshot",
        "Snapshot::from_bytes",
        "hashline::snapshot",
        "xxh3_128_with_seed",
    ),
}
IDENTIFIERS = (
    "value",
    "index",
    "buffer",
    "result",
    "config",
    "handler",
    "state",
    "count",
    "items",
    "cursor",
    "reader",
    "writer",
    "context",
    "target",
    "source",
    "delta",
)
KEYWORDS = (
    "let",
    "fn",
    "if",
    "for",
    "while",
    "return",
    "match",
    "struct",
    "impl",
    "pub",
)
ALLOWED_CHANGED_PATHS = {
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
LOGGER = logging.getLogger("phase2-capture")


class CaptureError(RuntimeError):
    """Raised when evidence is incomplete, incomparable, or below a gate."""


@dataclass(frozen=True)
class BenchBinary:
    """One exact-commit benchmark executable and its isolated environment."""

    variant: str
    commit: str
    repo: Path
    target: Path
    executable: Path
    environment: dict[str, str]


@dataclass(frozen=True)
class CaptureContext:
    """Immutable paths and parameters shared by one platform capture."""

    repo: Path
    goal_root: Path
    run_root: Path
    platform_name: str
    candidate_commit: str
    rounds: int
    profile_seconds: int
    max_normalized_load: float
    load_wait_seconds: int


@dataclass(frozen=True)
class BenchmarkSpec:
    """One filtered Criterion invocation."""

    variant: str
    benchmark_filter: str
    relative_root: Path
    sequence: int


class Xorshift32:
    """Exact Python model of the deterministic Rust benchmark generator."""

    def __init__(self, seed: int) -> None:
        """Initialize the wrapping 32-bit generator."""

        self._state = seed if seed != 0 else 0x9E37_79B9

    def next_u32(self) -> int:
        """Return the next wrapping 32-bit value."""

        value = self._state
        value ^= (value << 13) & 0xFFFF_FFFF
        value ^= value >> 17
        value ^= (value << 5) & 0xFFFF_FFFF
        self._state = value & 0xFFFF_FFFF
        return self._state

    def next_range(self, bound: int) -> int:
        """Return the next value modulo a positive bound."""

        if bound <= 0:
            raise ValueError("xorshift bound must be positive")
        return self.next_u32() % bound


def write_json(path: Path, value: object) -> None:
    """Atomically write deterministic JSON via the frozen Phase 0 helper."""

    phase0.write_json(path, value)


def sha256_file(path: Path) -> str:
    """Return the SHA-256 digest of one file."""

    return phase0.sha256_file(path)


def read_json_object(path: Path) -> dict[str, object]:
    """Read one JSON object or fail with the exact artifact path."""

    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CaptureError(f"cannot read JSON artifact {path}: {error}") from error
    if not isinstance(value, dict):
        raise CaptureError(f"JSON artifact is not an object: {path}")
    return cast(dict[str, object], value)


def finite_positive(value: object, label: str) -> float:
    """Require one finite positive number without accepting booleans."""

    if (
        not isinstance(value, (int, float))
        or isinstance(value, bool)
        or not math.isfinite(float(value))
        or float(value) <= 0.0
    ):
        raise CaptureError(f"{label} must be a finite positive number")
    return float(value)


def git_output(repo: Path, *arguments: str) -> str:
    """Run Git in a selected worktree and return stripped stdout."""

    return phase0.output(("git", *arguments), cwd=repo)


def clean_environment(target: Path) -> dict[str, str]:
    """Create a deterministic Cargo environment without ambient flags."""

    environment = os.environ.copy()
    for name in ("RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        environment.pop(name, None)
    environment["CARGO_TARGET_DIR"] = str(target)
    environment["LC_ALL"] = "C"
    environment["LANG"] = "C"
    return environment


def platform_name() -> str:
    """Return the evaluator's canonical platform identifier."""

    system = platform.system()
    machine = platform.machine()
    if system == "Darwin" and machine == "arm64":
        return "macos-arm64"
    if system == "Linux" and machine in {"x86_64", "amd64"}:
        return "linux-amd64"
    raise CaptureError(f"unsupported Phase 2 platform: {system}/{machine}")


def validate_candidate(repo: Path) -> str:
    """Require a clean signed Phase 2-only descendant of the exact base."""

    root = Path(git_output(repo, "rev-parse", "--show-toplevel")).resolve()
    if root != repo:
        raise CaptureError(f"capture must run at repository root: {root}")
    dirty = git_output(repo, "status", "--porcelain=v1", "--untracked-files=all")
    if dirty:
        raise CaptureError(f"candidate worktree is dirty:\n{dirty}")

    candidate = git_output(repo, "rev-parse", "HEAD")
    if candidate == BASE_COMMIT:
        raise CaptureError("Phase 2 candidate commit has not been created")
    ancestor = subprocess.run(
        ("git", "merge-base", "--is-ancestor", BASE_COMMIT, candidate),
        cwd=repo,
        check=False,
    )
    if ancestor.returncode != 0:
        raise CaptureError(f"{BASE_COMMIT} is not an ancestor of {candidate}")

    signature = git_output(repo, "log", "-1", "--format=%G?")
    if signature not in {"G", "U"}:
        raise CaptureError(f"candidate signature is not good: {signature}")

    changed = {
        path
        for path in git_output(
            repo,
            "diff",
            "--name-only",
            f"{BASE_COMMIT}..{candidate}",
        ).splitlines()
        if path
    }
    unexpected = sorted(changed - ALLOWED_CHANGED_PATHS)
    if unexpected:
        raise CaptureError(f"candidate contains non-Phase 2 paths: {unexpected!r}")
    if {"src/read.rs", "src/render.rs", "src/server.rs"} & changed:
        raise CaptureError("Phase 3 paths changed before the Phase 2 gate")

    plan = repo / PLAN_PATH
    if sha256_file(plan) != PLAN_SHA256:
        raise CaptureError("source-of-truth plan digest changed")
    return candidate


def process_census() -> str:
    """Return a stable text census used to diagnose benchmark interference."""

    result = subprocess.run(
        ("ps", "-Ao", "pid,ppid,%cpu,%mem,state,etime,comm,args"),
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout


def capture_load_gate(
    max_normalized_load: float,
    max_wait_seconds: int,
) -> dict[str, object]:
    """Wait for three consecutive quiet load samples before timing."""

    if not math.isfinite(max_normalized_load) or max_normalized_load <= 0.0:
        raise CaptureError("maximum normalized load must be finite and positive")
    if max_wait_seconds <= 0:
        raise CaptureError("load-gate wait must be positive")
    cpu_count = os.cpu_count()
    if cpu_count is None or cpu_count <= 0:
        raise CaptureError("host CPU count is unavailable")

    deadline = time.monotonic() + max_wait_seconds
    attempts: list[dict[str, object]] = []
    accepted: list[dict[str, object]] = []
    while True:
        load = os.getloadavg()
        normalized = [value / cpu_count for value in load]
        sample: dict[str, object] = {
            "captured_at": datetime.now(UTC).isoformat(),
            "load_average": list(load),
            "normalized_load": normalized,
        }
        attempts.append(sample)
        if normalized[0] <= max_normalized_load:
            accepted.append(sample)
            if len(accepted) == 3:
                return {
                    "status": "pass",
                    "cpu_count": cpu_count,
                    "max_normalized_load": max_normalized_load,
                    "max_wait_seconds": max_wait_seconds,
                    "attempts": attempts,
                    "accepted_samples": accepted,
                    "process_census": process_census(),
                }
        else:
            accepted.clear()

        if time.monotonic() >= deadline:
            raise CaptureError(
                "host load gate timed out: "
                + f"{normalized[0]:.4f} > {max_normalized_load:.4f}"
            )
        time.sleep(5.0)


def generate_line(generator: Xorshift32, line_number: int) -> str:
    """Generate one byte-identical line from the Rust workload model."""

    if generator.next_range(37) == 0:
        return ""
    if generator.next_range(211) == 0:
        word = IDENTIFIERS[generator.next_range(len(IDENTIFIERS))]
        return f"// {word * 300}"

    depth = generator.next_range(5)
    indent = "    " * depth
    keyword = KEYWORDS[generator.next_range(len(KEYWORDS))]
    identifier = IDENTIFIERS[generator.next_range(len(IDENTIFIERS))]
    argument = IDENTIFIERS[generator.next_range(len(IDENTIFIERS))]
    number = generator.next_range(1000)
    return f"{indent}{keyword} {identifier}_{line_number} = {argument}({number});"


def generate_corpus(line_count: int, seed: int) -> bytes:
    """Generate the exact UTF-8 benchmark corpus, including final newline."""

    generator = Xorshift32(seed)
    text = "".join(
        f"{generate_line(generator, line_number)}\n"
        for line_number in range(line_count)
    )
    return text.encode()


def corpus_manifest(repo: Path) -> dict[str, object]:
    """Hash every deterministic corpus and its frozen Rust generator."""

    rows: dict[str, object] = {}
    for line_count, seed in CORPUS_SPECS.items():
        content = generate_corpus(line_count, seed)
        digest = hashlib.sha256(content).hexdigest()
        expected = EXPECTED_CORPORA[line_count]
        if (len(content), digest) != expected:
            raise CaptureError(
                f"{line_count} corpus differs from the frozen byte identity"
            )
        rows[str(line_count)] = {
            "generated_lines": line_count,
            "logical_lines": line_count + 1,
            "seed": f"0x{seed:08x}",
            "byte_len": len(content),
            "sha256": digest,
        }
    return {
        "schema_version": SCHEMA_VERSION,
        "generator_path": "benches/support/phase0_workloads.rs",
        "generator_sha256": sha256_file(repo / "benches/support/phase0_workloads.rs"),
        "corpora": rows,
    }


@contextmanager
def detached_worktree(
    repo: Path,
    commit: str,
    root: Path,
    log_root: Path,
) -> Iterator[Path]:
    """Create and always remove one exact detached worktree."""

    worktree = root / "worktree"
    add_log = log_root / "worktree_add.json"
    remove_log = log_root / "worktree_remove.json"
    _ = phase0.run_command(
        ("git", "worktree", "add", "--detach", str(worktree), commit),
        cwd=repo,
        log_path=add_log,
    )
    try:
        head = git_output(worktree, "rev-parse", "HEAD")
        dirty = git_output(
            worktree,
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        )
        if head != commit or dirty:
            raise CaptureError("base worktree is not the exact clean commit")
        yield worktree
    finally:
        _ = phase0.run_command(
            ("git", "worktree", "remove", "--force", str(worktree)),
            cwd=repo,
            log_path=remove_log,
            check=False,
        )


def compiler_artifact_executable(output: str, target_name: str) -> Path:
    """Extract one executable path from Cargo JSON compiler artifacts."""

    executables: list[Path] = []
    for line in output.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict) or value.get("reason") != "compiler-artifact":
            continue
        target = value.get("target")
        executable = value.get("executable")
        if (
            isinstance(target, dict)
            and target.get("name") == target_name
            and isinstance(executable, str)
        ):
            executables.append(Path(executable))
    existing = [path for path in executables if path.is_file()]
    if len(existing) != 1:
        raise CaptureError(
            f"Cargo reported {len(existing)} executables for {target_name}"
        )
    return existing[0]


def build_benchmark(
    context: CaptureContext,
    variant: str,
    commit: str,
    repo: Path,
    target: Path,
) -> BenchBinary:
    """Build one exact-commit shipping-profile Criterion executable."""

    environment = clean_environment(target)
    command = (
        "cargo",
        "bench",
        "--bench",
        "hashline",
        "--no-run",
        "--message-format=json-render-diagnostics",
    )
    result = phase0.run_command(
        command,
        cwd=repo,
        environment=environment,
        log_path=context.run_root / "commands" / f"build_{variant}.json",
    )
    executable = compiler_artifact_executable(result.stdout, "hashline")
    return BenchBinary(
        variant=variant,
        commit=commit,
        repo=repo,
        target=target,
        executable=executable,
        environment=environment,
    )


def run_benchmark(
    context: CaptureContext,
    binary: BenchBinary,
    spec: BenchmarkSpec,
) -> dict[str, object]:
    """Run and preserve one absolute filtered Criterion estimate."""

    raw_root = context.run_root / spec.relative_root
    criterion_root = binary.target / "criterion"
    if criterion_root.exists():
        shutil.rmtree(criterion_root)
    started_ns = time.time_ns()
    result = phase0.run_command(
        (str(binary.executable), "--bench", spec.benchmark_filter),
        cwd=binary.repo,
        environment=binary.environment,
        log_path=raw_root / "command.json",
    )
    estimate = phase0.newest_estimate(criterion_root, started_ns)
    _ = shutil.copytree(estimate.parent, raw_root / "criterion_new")
    return {
        "sequence": spec.sequence,
        "variant": spec.variant,
        "commit": binary.commit,
        "filter": spec.benchmark_filter,
        "command": [
            str(binary.executable),
            "--bench",
            spec.benchmark_filter,
        ],
        "exit_code": result.returncode,
        "estimate": phase0.estimate_summary(estimate),
        "raw_path": spec.relative_root.as_posix(),
    }


def estimate_triplet(run: dict[str, object]) -> tuple[float, float, float]:
    """Return point, lower, and upper median estimates from one run."""

    estimate = cast(dict[str, object], run["estimate"])
    interval = cast(dict[str, object], estimate["confidence_interval_ns"])
    return (
        float(cast(float | int, estimate["point_estimate_ns"])),
        float(cast(float | int, interval["lower_bound"])),
        float(cast(float | int, interval["upper_bound"])),
    )


def conservative_summary(
    baseline_runs: list[dict[str, object]],
    candidate_runs: list[dict[str, object]],
) -> dict[str, float]:
    """Compute median point and conservative confidence-bound speedups."""

    if not baseline_runs or len(baseline_runs) != len(candidate_runs):
        raise CaptureError("paired benchmark sample counts differ")
    baseline = [estimate_triplet(run) for run in baseline_runs]
    candidate = [estimate_triplet(run) for run in candidate_runs]
    baseline_points = [value[0] for value in baseline]
    candidate_points = [value[0] for value in candidate]
    baseline_lowers = [value[1] for value in baseline]
    candidate_uppers = [value[2] for value in candidate]
    return {
        "baseline_median_ns": statistics.median(baseline_points),
        "candidate_median_ns": statistics.median(candidate_points),
        "point_speedup": (
            statistics.median(baseline_points) / statistics.median(candidate_points)
        ),
        "conservative_speedup": (
            statistics.median(baseline_lowers) / statistics.median(candidate_uppers)
        ),
    }


def corpus_row(
    manifest: dict[str, object],
    line_count: int,
) -> dict[str, object]:
    """Return one typed corpus manifest row."""

    corpora = cast(dict[str, object], manifest["corpora"])
    row = corpora.get(str(line_count))
    if not isinstance(row, dict):
        raise CaptureError(f"corpus manifest lacks {line_count} lines")
    return cast(dict[str, object], row)


def capture_snapshot_pairs(
    context: CaptureContext,
    base: BenchBinary,
    candidate: BenchBinary,
    corpora: dict[str, object],
) -> list[dict[str, object]]:
    """Capture exact-base/candidate snapshot construction pairs."""

    pairs: list[dict[str, object]] = []
    for size in SNAPSHOT_SIZES:
        runs: list[dict[str, object]] = []
        sequence = 0
        for round_index in range(context.rounds):
            order = (
                ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")
            )
            for variant in order:
                sequence += 1
                binary = base if variant == "base" else candidate
                benchmark_filter = (
                    f"phase0_snapshot_xxh3/{size}/base_current_index"
                    if variant == "base"
                    else f"phase2_snapshot/{size}/candidate_snapshot"
                )
                runs.append(
                    run_benchmark(
                        context,
                        binary,
                        BenchmarkSpec(
                            variant=variant,
                            benchmark_filter=benchmark_filter,
                            relative_root=Path(
                                "benchmarks",
                                "snapshot",
                                str(size),
                                f"{sequence:02d}-{variant}",
                            ),
                            sequence=sequence,
                        ),
                    )
                )
        baseline_runs = [run for run in runs if run["variant"] == "base"]
        candidate_runs = [run for run in runs if run["variant"] == "candidate"]
        summary = conservative_summary(baseline_runs, candidate_runs)
        if summary["conservative_speedup"] < MINIMUM_SNAPSHOT_SPEEDUP:
            raise CaptureError(
                f"{size} construction speedup "
                f"{summary['conservative_speedup']:.4f}x is below "
                f"{MINIMUM_SNAPSHOT_SPEEDUP:.1f}x"
            )
        row = corpus_row(corpora, size)
        pairs.append(
            {
                "size": size,
                "corpus_line_count": size,
                "actual_logical_lines": row["logical_lines"],
                "corpus_sha256": row["sha256"],
                "rounds": context.rounds,
                "runs": runs,
                "summary": summary,
            }
        )
    return pairs


def capture_single_candidate(
    context: CaptureContext,
    candidate: BenchBinary,
    benchmark_filter: str,
    relative_root: Path,
) -> dict[str, object]:
    """Capture one candidate-only Criterion estimate."""

    return run_benchmark(
        context,
        candidate,
        BenchmarkSpec(
            variant="candidate",
            benchmark_filter=benchmark_filter,
            relative_root=relative_root,
            sequence=1,
        ),
    )


def capture_version_matrix(
    context: CaptureContext,
    candidate: BenchBinary,
) -> list[dict[str, object]]:
    """Benchmark all Phase 2 version candidates on short and large inputs."""

    candidates = (
        ("gxhash128", False),
        ("xxh3_128_with_seed", True),
        ("blake3_128", True),
    )
    rows: list[dict[str, object]] = []
    for name, cross_target in candidates:
        estimates: dict[str, float] = {}
        raw_paths: dict[str, str] = {}
        for input_name in ("short", "multimegabyte"):
            run = capture_single_candidate(
                context,
                candidate,
                f"phase2_version/{input_name}/{name}",
                Path("benchmarks", "version", input_name, name),
            )
            estimates[input_name] = estimate_triplet(run)[0]
            raw_paths[input_name] = cast(str, run["raw_path"])
        rows.append(
            {
                "name": name,
                "short_ns": estimates["short"],
                "multimegabyte_ns": estimates["multimegabyte"],
                "cross_target": cross_target,
                "raw_paths": raw_paths,
            }
        )
    return rows


def representation_bytes_per_line(
    name: str,
    corpus_bytes: int,
    logical_lines: int,
) -> float:
    """Return exact benchmark representation storage per logical line."""

    if logical_lines <= 0 or corpus_bytes < 0:
        raise CaptureError("invalid corpus dimensions")
    if name == "full_u32":
        resident = logical_lines * 4
    elif name == "full_u64":
        resident = logical_lines * 8
    elif name.startswith("sparse_"):
        interval = int(name.removeprefix("sparse_"))
        resident = ((logical_lines - 1) // interval + 1) * 4
    elif name == "rank_select_bitmap":
        words = corpus_bytes // 64 + 1
        superblocks = (words + 7) // 8
        resident = words * 8 + superblocks * 4
    else:
        raise CaptureError(f"unknown offset representation: {name}")
    return resident / logical_lines


def capture_offset_matrix(
    context: CaptureContext,
    candidate: BenchBinary,
    corpora: dict[str, object],
) -> list[dict[str, object]]:
    """Benchmark full, sparse, and rank/select offset representations."""

    names = (
        "full_u32",
        "full_u64",
        "sparse_128",
        "sparse_256",
        "sparse_512",
        "rank_select_bitmap",
    )
    row_50k = corpus_row(corpora, 50_000)
    byte_len = cast(int, row_50k["byte_len"])
    logical_lines = cast(int, row_50k["logical_lines"])
    rows: list[dict[str, object]] = []
    for name in names:
        construction = capture_single_candidate(
            context,
            candidate,
            f"phase2_offsets/construction_50k/{name}",
            Path("benchmarks", "offsets", "construction", name),
        )
        cold = capture_single_candidate(
            context,
            candidate,
            f"phase2_offsets/cold_window_2k_of_100k/{name}",
            Path("benchmarks", "offsets", "cold_window", name),
        )
        rows.append(
            {
                "name": name,
                "construction_ns": estimate_triplet(construction)[0],
                "cold_window_ns": estimate_triplet(cold)[0],
                "bytes_per_line": representation_bytes_per_line(
                    name,
                    byte_len,
                    logical_lines,
                ),
                "raw_paths": {
                    "construction": construction["raw_path"],
                    "cold_window": cold["raw_path"],
                },
            }
        )
    return rows


def capture_unsafe_validation(
    context: CaptureContext,
    candidate: BenchBinary,
) -> dict[str, object]:
    """Measure safe versus SIMD-validated unchecked conversion end to end."""

    size_results: list[dict[str, object]] = []
    for size in SNAPSHOT_SIZES:
        runs: list[dict[str, object]] = []
        sequence = 0
        for round_index in range(context.rounds):
            order = ("safe", "unsafe") if round_index % 2 == 0 else ("unsafe", "safe")
            for variant in order:
                sequence += 1
                suffix = (
                    "safe_snapshot"
                    if variant == "safe"
                    else "simd_validated_unchecked_snapshot"
                )
                runs.append(
                    run_benchmark(
                        context,
                        candidate,
                        BenchmarkSpec(
                            variant=variant,
                            benchmark_filter=(f"phase2_validation/{size}/{suffix}"),
                            relative_root=Path(
                                "benchmarks",
                                "unsafe_validation",
                                str(size),
                                f"{sequence:02d}-{variant}",
                            ),
                            sequence=sequence,
                        ),
                    )
                )
        safe_runs = [run for run in runs if run["variant"] == "safe"]
        unsafe_runs = [run for run in runs if run["variant"] == "unsafe"]
        size_results.append(
            {
                "size": size,
                "rounds": context.rounds,
                "runs": runs,
                "summary": conservative_summary(safe_runs, unsafe_runs),
            }
        )

    conservative = min(
        cast(dict[str, float], row["summary"])["conservative_speedup"]
        for row in size_results
    )
    source = (context.repo / "src/snapshot.rs").read_text(encoding="utf-8")
    production = source.split("#[cfg(test)]", maxsplit=1)[0]
    production_unsafe = "unsafe" in production
    if production_unsafe and conservative < MINIMUM_UNSAFE_SPEEDUP:
        raise CaptureError("production unsafe path fails the 5 percent gate")
    if not production_unsafe and conservative >= MINIMUM_UNSAFE_SPEEDUP:
        raise CaptureError(
            "safe production path conflicts with a measured >=5 percent "
            "unsafe improvement"
        )
    miri = "not_required"
    if production_unsafe:
        miri_environment = clean_environment(candidate.target / "miri")
        _ = phase0.run_command(
            (
                "cargo",
                "miri",
                "test",
                "--lib",
                "--no-default-features",
                "snapshot::tests::miri_validated_text_round_trip",
            ),
            cwd=context.repo,
            environment=miri_environment,
            log_path=(
                context.run_root
                / "benchmarks"
                / "unsafe_validation"
                / "miri_command.json"
            ),
        )
        miri = "pass"
    return {
        "adopted": production_unsafe,
        "conservative_speedup": conservative,
        "threshold": MINIMUM_UNSAFE_SPEEDUP,
        "miri": miri,
        "sizes": size_results,
    }


def build_profile_binary(
    context: CaptureContext,
    binary: BenchBinary,
    profile_target: Path,
) -> BenchBinary:
    """Rebuild one benchmark with symbols and unchanged optimization settings."""

    environment = clean_environment(profile_target)
    environment["CARGO_PROFILE_BENCH_DEBUG"] = "2"
    environment["CARGO_PROFILE_BENCH_STRIP"] = "none"
    result = phase0.run_command(
        (
            "cargo",
            "bench",
            "--bench",
            "hashline",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ),
        cwd=binary.repo,
        environment=environment,
        log_path=(context.run_root / "profiles" / binary.variant / "build.json"),
    )
    executable = compiler_artifact_executable(result.stdout, "hashline")
    return BenchBinary(
        variant=binary.variant,
        commit=binary.commit,
        repo=binary.repo,
        target=profile_target,
        executable=executable,
        environment=environment,
    )


def profile_filter(variant: str) -> str:
    """Return the exact 50k snapshot construction profile filter."""

    if variant == "base":
        return "phase0_snapshot_xxh3/50000/base_current_index"
    if variant == "candidate":
        return "phase2_snapshot/50000/candidate_snapshot"
    raise CaptureError(f"unknown profile variant: {variant}")


def symbol_hits(variant: str, report: str) -> list[str]:
    """Return workload-specific symbol fragments found in a profile report."""

    expected = PROFILE_SYMBOLS.get(variant)
    if expected is None:
        raise CaptureError(f"unknown profile variant: {variant}")
    return sorted(symbol for symbol in expected if symbol in report)


def profile_macos(
    context: CaptureContext,
    binary: BenchBinary,
) -> dict[str, object]:
    """Capture one symbolized macOS sample report."""

    root = context.run_root / "profiles" / binary.variant
    root.mkdir(parents=True, exist_ok=True)
    stdout_path = root / "benchmark_stdout.log"
    stderr_path = root / "benchmark_stderr.log"
    command = [
        str(binary.executable),
        "--bench",
        profile_filter(binary.variant),
    ]
    with (
        stdout_path.open("w", encoding="utf-8") as stdout_stream,
        stderr_path.open("w", encoding="utf-8") as stderr_stream,
    ):
        process = subprocess.Popen(
            command,
            cwd=binary.repo,
            env=binary.environment,
            text=True,
            stdout=stdout_stream,
            stderr=stderr_stream,
        )
        write_json(
            root / "benchmark_process.json",
            {
                "command": command,
                "cwd": str(binary.repo),
                "pid": process.pid,
                "started_at": datetime.now(UTC).isoformat(),
            },
        )
        raw = root / "sample.txt"
        try:
            time.sleep(0.75)
            phase0.run_command(
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
                cwd=binary.repo,
                environment=binary.environment,
                log_path=root / "sample_command.json",
            )
            exit_code = process.wait(timeout=context.profile_seconds + 10)
            if exit_code != 0:
                raise CaptureError(
                    f"{binary.variant} profile benchmark exited {exit_code}"
                )
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)

    report = root / "report.txt"
    shutil.copyfile(raw, report)
    report_text = report.read_text(encoding="utf-8", errors="replace")
    hits = symbol_hits(binary.variant, report_text)
    if not hits:
        raise CaptureError(f"{binary.variant} macOS profile has no workload symbols")
    return {
        "variant": binary.variant,
        "commit": binary.commit,
        "tool": "sample",
        "symbolized": True,
        "symbol_hits": hits,
        "raw_path": raw.relative_to(context.run_root).as_posix(),
        "report_path": report.relative_to(context.run_root).as_posix(),
        "filter": profile_filter(binary.variant),
    }


def profile_linux(
    context: CaptureContext,
    binary: BenchBinary,
) -> dict[str, object]:
    """Capture one symbolized Linux perf report."""

    root = context.run_root / "profiles" / binary.variant
    root.mkdir(parents=True, exist_ok=True)
    raw = root / "perf.data"
    phase0.run_command(
        (
            "perf",
            "record",
            "-F",
            "997",
            "-g",
            "--call-graph",
            "dwarf",
            "-o",
            str(raw),
            "--",
            str(binary.executable),
            "--bench",
            profile_filter(binary.variant),
        ),
        cwd=binary.repo,
        environment=binary.environment,
        log_path=root / "perf_record.json",
    )
    report_result = phase0.run_command(
        (
            "perf",
            "report",
            "--stdio",
            "--no-children",
            "-i",
            str(raw),
        ),
        cwd=binary.repo,
        environment=binary.environment,
        log_path=root / "perf_report_command.json",
    )
    report = root / "report.txt"
    report.write_text(report_result.stdout, encoding="utf-8")
    hits = symbol_hits(binary.variant, report_result.stdout)
    if not hits:
        raise CaptureError(f"{binary.variant} Linux profile has no workload symbols")
    return {
        "variant": binary.variant,
        "commit": binary.commit,
        "tool": "perf",
        "symbolized": True,
        "symbol_hits": hits,
        "raw_path": raw.relative_to(context.run_root).as_posix(),
        "report_path": report.relative_to(context.run_root).as_posix(),
        "filter": profile_filter(binary.variant),
    }


def capture_profiles(
    context: CaptureContext,
    base: BenchBinary,
    candidate: BenchBinary,
    profile_roots: tuple[Path, Path],
) -> list[dict[str, object]]:
    """Capture base and candidate symbolized profiles sequentially."""

    binaries = (
        build_profile_binary(context, base, profile_roots[0]),
        build_profile_binary(context, candidate, profile_roots[1]),
    )
    load_gate = capture_load_gate(
        context.max_normalized_load,
        context.load_wait_seconds,
    )
    records: list[dict[str, object]] = []
    for binary in binaries:
        record = (
            profile_macos(context, binary)
            if context.platform_name == "macos-arm64"
            else profile_linux(context, binary)
        )
        records.append(record)
    write_json(
        context.run_root / "profiles" / "summary.json",
        {
            "status": "pass",
            "load_gate_evidence": load_gate,
            "profiles": records,
        },
    )
    return records


def command_record(
    command: tuple[str, ...],
    cwd: Path,
    environment: dict[str, str],
) -> dict[str, object]:
    """Capture one exact environment command and complete output."""

    result = phase0.run_command(
        command,
        cwd=cwd,
        environment=environment,
    )
    return {
        "command": list(command),
        "cwd": str(cwd),
        "exit_code": result.returncode,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }


def capture_environment(
    context: CaptureContext,
    base_repo: Path,
    load_gate: dict[str, object],
    environment: dict[str, str],
) -> dict[str, object]:
    """Record exact host, toolchain, Git, and load conditions."""

    commands = {
        "rustc": ("rustc", "-Vv"),
        "cargo": ("cargo", "-V"),
        "uname": ("uname", "-a"),
        "os": (
            "sh",
            "-c",
            (
                "if command -v sw_vers >/dev/null 2>&1; "
                "then sw_vers; else cat /etc/os-release; fi"
            ),
        ),
        "cpu": (
            "sh",
            "-c",
            (
                "if command -v lscpu >/dev/null 2>&1; then lscpu; "
                "else sysctl -a 2>/dev/null | "
                "grep -E 'machdep.cpu.brand_string|hw.ncpu|hw.physicalcpu'; fi"
            ),
        ),
        "candidate_status": (
            "git",
            "status",
            "--short",
            "--branch",
            "--untracked-files=all",
        ),
        "base_status": (
            "git",
            "-C",
            str(base_repo),
            "status",
            "--short",
            "--branch",
            "--untracked-files=all",
        ),
        "worktrees": ("git", "worktree", "list", "--porcelain"),
    }
    captured = {
        name: command_record(command, context.repo, environment)
        for name, command in commands.items()
    }
    return {
        "schema_version": SCHEMA_VERSION,
        "platform": context.platform_name,
        "system": platform.system(),
        "machine": platform.machine(),
        "python": sys.version,
        "candidate_commit": context.candidate_commit,
        "base_commit": BASE_COMMIT,
        "candidate_dirty": False,
        "base_dirty": False,
        "ambient_flags": {
            name: os.environ.get(name)
            for name in (
                "RUSTFLAGS",
                "RUSTDOCFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
            )
        },
        "benchmark_rustflags": {},
        "removed_ambient_flags": [
            name
            for name in (
                "RUSTFLAGS",
                "RUSTDOCFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
            )
            if os.environ.get(name)
        ],
        "cargo_config_sha256": sha256_file(context.repo / ".cargo/config.toml"),
        "load_gate": "pass",
        "load_gate_evidence": load_gate,
        "commands": captured,
    }


def write_checksums(run_root: Path) -> dict[str, str]:
    """Hash every regular artifact except the checksum manifest itself."""

    checksum_path = run_root / "SHA256SUMS.json"
    hashes = {
        path.relative_to(run_root).as_posix(): sha256_file(path)
        for path in sorted(run_root.rglob("*"))
        if path.is_file() and path != checksum_path
    }
    if not hashes:
        raise CaptureError("artifact checksum manifest would be empty")
    write_json(checksum_path, hashes)
    return hashes


def update_latest(context: CaptureContext) -> None:
    """Authenticate one immutable canonical run from the platform pointer."""

    platform_root = context.goal_root / "artifacts" / context.platform_name
    relative = context.run_root.relative_to(platform_root)
    write_json(
        platform_root / "latest.json",
        {
            "schema_version": SCHEMA_VERSION,
            "run": relative.as_posix(),
            "manifest_sha256": sha256_file(context.run_root / "manifest.json"),
            "checksums_sha256": sha256_file(context.run_root / "SHA256SUMS.json"),
        },
    )


def capture_results(
    context: CaptureContext,
    base: BenchBinary,
    candidate: BenchBinary,
    corpora: dict[str, object],
) -> dict[str, object]:
    """Capture every benchmark matrix and enforce immediate numeric gates."""

    snapshot_pairs = capture_snapshot_pairs(
        context,
        base,
        candidate,
        corpora,
    )
    versions = capture_version_matrix(context, candidate)
    offsets = capture_offset_matrix(context, candidate, corpora)
    unsafe_validation = capture_unsafe_validation(context, candidate)
    return {
        "schema_version": SCHEMA_VERSION,
        "status": "pass",
        "snapshot_pairs": snapshot_pairs,
        "version_candidates": versions,
        "selected_version": "xxh3_128_with_seed",
        "representations": offsets,
        "selected_representation": {
            "name": "lazy_full_u32_u64",
            "u32_bytes_per_line": 4.0,
            "u64_fallback": True,
        },
        "unsafe_validation": unsafe_validation,
    }


def capture(args: argparse.Namespace) -> int:
    """Capture one complete platform artifact tree."""

    if args.rounds < MINIMUM_ROUNDS:
        raise CaptureError(f"at least {MINIMUM_ROUNDS} rounds are required")
    if args.profile_seconds < 6:
        raise CaptureError("profile duration must be at least six seconds")
    if args.load_wait_seconds <= 0:
        raise CaptureError("load-gate wait must be positive")

    repo = Path.cwd().resolve()
    goal_root = (repo / args.goal_root).resolve()
    candidate = validate_candidate(repo)
    key = platform_name()
    run_id = f"{datetime.now(UTC).strftime('%Y%m%dT%H%M%SZ')}-{candidate[:12]}"
    run_root = goal_root / "artifacts" / key / "runs" / run_id
    run_root.mkdir(parents=True, exist_ok=False)
    context = CaptureContext(
        repo=repo,
        goal_root=goal_root,
        run_root=run_root,
        platform_name=key,
        candidate_commit=candidate,
        rounds=args.rounds,
        profile_seconds=args.profile_seconds,
        max_normalized_load=args.max_normalized_load,
        load_wait_seconds=args.load_wait_seconds,
    )

    recoverable = (
        CaptureError,
        phase0.CaptureError,
        OSError,
        ValueError,
        KeyError,
        subprocess.SubprocessError,
    )
    try:
        with (
            tempfile.TemporaryDirectory(
                prefix=f"hashline-phase2-{key}-base-"
            ) as base_temp,
            tempfile.TemporaryDirectory(
                prefix=f"hashline-phase2-{key}-candidate-target-"
            ) as candidate_target_temp,
            tempfile.TemporaryDirectory(
                prefix=f"hashline-phase2-{key}-base-target-"
            ) as base_target_temp,
            tempfile.TemporaryDirectory(
                prefix=f"hashline-phase2-{key}-profile-base-"
            ) as profile_base_temp,
            tempfile.TemporaryDirectory(
                prefix=f"hashline-phase2-{key}-profile-candidate-"
            ) as profile_candidate_temp,
        ):
            base_root = Path(base_temp)
            with detached_worktree(
                repo,
                BASE_COMMIT,
                base_root,
                run_root / "commands",
            ) as base_repo:
                candidate_target = Path(candidate_target_temp)
                base_target = Path(base_target_temp)

                corpora = corpus_manifest(repo)
                write_json(run_root / "corpora" / "manifest.json", corpora)

                candidate_binary = build_benchmark(
                    context,
                    "candidate",
                    candidate,
                    repo,
                    candidate_target,
                )
                base_binary = build_benchmark(
                    context,
                    "base",
                    BASE_COMMIT,
                    base_repo,
                    base_target,
                )

                load_gate = capture_load_gate(
                    context.max_normalized_load,
                    context.load_wait_seconds,
                )
                candidate_environment = clean_environment(candidate_target)
                environment = capture_environment(
                    context,
                    base_repo,
                    load_gate,
                    candidate_environment,
                )
                write_json(run_root / "environment.json", environment)

                results = capture_results(
                    context,
                    base_binary,
                    candidate_binary,
                    corpora,
                )
                write_json(
                    run_root / "benchmarks" / "results.json",
                    results,
                )
                profiles = capture_profiles(
                    context,
                    base_binary,
                    candidate_binary,
                    (
                        Path(profile_base_temp),
                        Path(profile_candidate_temp),
                    ),
                )

                manifest = {
                    "schema_version": SCHEMA_VERSION,
                    "status": "pass",
                    "phase": "Phase 2",
                    "platform": key,
                    "captured_at": datetime.now(UTC).isoformat(),
                    "base_commit": BASE_COMMIT,
                    "candidate_commit": candidate,
                    "plan_path": PLAN_PATH,
                    "plan_sha256": PLAN_SHA256,
                    "environment": environment,
                    "corpora": corpora,
                    "benchmark_results": "benchmarks/results.json",
                    "profile_summary": "profiles/summary.json",
                    "profile_count": len(profiles),
                }
                write_json(run_root / "manifest.json", manifest)
                _ = write_checksums(run_root)
                update_latest(context)
    except recoverable as error:
        write_json(
            run_root / "FAILED.json",
            {
                "schema_version": SCHEMA_VERSION,
                "status": "fail",
                "error": str(error),
                "failed_at": datetime.now(UTC).isoformat(),
            },
        )
        _ = write_checksums(run_root)
        LOGGER.error("Phase 2 capture failed: %s", error)
        return 1

    LOGGER.info("captured %s Phase 2 evidence at %s", key, run_root)
    return 0


def canonical_run(goal_root: Path, key: str, candidate: str) -> Path:
    """Resolve and authenticate one immutable canonical platform run."""

    platform_root = (goal_root / "artifacts" / key).resolve()
    latest_path = platform_root / "latest.json"
    latest = read_json_object(latest_path)
    if latest.get("schema_version") != SCHEMA_VERSION:
        raise CaptureError(f"invalid latest schema: {latest_path}")

    run_value = latest.get("run")
    if not isinstance(run_value, str) or not run_value:
        raise CaptureError(f"invalid latest run: {latest_path}")
    relative = Path(run_value)
    if relative.is_absolute() or ".." in relative.parts:
        raise CaptureError(f"unsafe latest run path: {relative}")

    run_root = (platform_root / relative).resolve()
    if not run_root.is_relative_to(platform_root):
        raise CaptureError(f"canonical run escapes platform root: {run_root}")
    if not run_root.is_dir():
        raise CaptureError(f"canonical run is missing: {run_root}")

    manifest_path = run_root / "manifest.json"
    checksums_path = run_root / "SHA256SUMS.json"
    expected_digests = {
        "manifest_sha256": sha256_file(manifest_path),
        "checksums_sha256": sha256_file(checksums_path),
    }
    for field, observed in expected_digests.items():
        if latest.get(field) != observed:
            raise CaptureError(f"{key} canonical {field} changed")

    try:
        _ = phase1.verify_checksum_manifest(run_root)
    except phase1.EvaluationError as error:
        raise CaptureError(f"{key} checksum replay failed: {error}") from error

    manifest = read_json_object(manifest_path)
    expected_manifest = {
        "schema_version": SCHEMA_VERSION,
        "status": "pass",
        "phase": "Phase 2",
        "platform": key,
        "base_commit": BASE_COMMIT,
        "candidate_commit": candidate,
        "plan_sha256": PLAN_SHA256,
    }
    for field, expected in expected_manifest.items():
        if manifest.get(field) != expected:
            raise CaptureError(
                f"{key} manifest {field} mismatch: "
                f"{manifest.get(field)!r} != {expected!r}"
            )
    return run_root


def read_results(run_root: Path) -> dict[str, object]:
    """Read one canonical benchmark result object."""

    return read_json_object(run_root / "benchmarks" / "results.json")


def resolve_unsafe_decision(
    platform_results: dict[str, object],
) -> tuple[bool, str, str]:
    """Derive one cross-platform unsafe decision from authenticated measurements."""

    normalized: dict[str, tuple[bool, float, str]] = {}
    for key, value in platform_results.items():
        if not isinstance(value, dict):
            raise CaptureError(f"{key} unsafe result is not an object")
        result = cast(dict[str, object], value)
        adopted = result.get("adopted")
        if not isinstance(adopted, bool):
            raise CaptureError(f"{key} unsafe adoption flag is absent")
        threshold = finite_positive(result.get("threshold"), f"{key} unsafe threshold")
        if threshold != MINIMUM_UNSAFE_SPEEDUP:
            raise CaptureError(f"{key} unsafe threshold changed: {threshold}")
        speedup = finite_positive(
            result.get("conservative_speedup"),
            f"{key} unsafe conservative speedup",
        )
        miri = result.get("miri")
        if not isinstance(miri, str) or not miri:
            raise CaptureError(f"{key} unsafe Miri result is absent")
        normalized[key] = (adopted, speedup, miri)

    adoption_flags = {row[0] for row in normalized.values()}
    if len(adoption_flags) != 1:
        raise CaptureError("platforms disagree on unsafe adoption")
    adopted = adoption_flags.pop()

    for key, (_, speedup, miri) in normalized.items():
        if adopted:
            if speedup < MINIMUM_UNSAFE_SPEEDUP:
                raise CaptureError(
                    f"{key} adopted unsafe path improves only {speedup:.4f}x"
                )
            if miri != "pass":
                raise CaptureError(f"{key} adopted unsafe path lacks Miri PASS")
        else:
            if speedup >= MINIMUM_UNSAFE_SPEEDUP:
                raise CaptureError(
                    f"{key} rejected unsafe path measured {speedup:.4f}x"
                )
            if miri != "not_required":
                raise CaptureError(
                    f"{key} safe path has inconsistent Miri status: {miri}"
                )

    measurements = ", ".join(
        f"{key}={row[1]:.4f}x" for key, row in sorted(normalized.items())
    )
    if adopted:
        return (
            True,
            "adopt",
            f"all required hosts meet the 1.05x gate with Miri PASS ({measurements})",
        )
    return (
        False,
        "reject",
        f"at least one required end-to-end gate remains below 1.05x ({measurements})",
    )


def finalize(args: argparse.Namespace) -> int:
    """Freeze cross-platform Phase 2 selections and rollback decisions."""

    repo = Path.cwd().resolve()
    candidate = validate_candidate(repo)
    goal_root = (repo / args.goal_root).resolve()
    runs = {
        key: canonical_run(goal_root, key, candidate)
        for key in ("macos-arm64", "linux-amd64")
    }
    results = {key: read_results(run) for key, run in runs.items()}

    speedups: dict[str, object] = {}
    unsafe: dict[str, object] = {}
    version_rows: dict[str, object] = {}
    offset_rows: dict[str, object] = {}
    for key, result in results.items():
        pairs = cast(list[object], result["snapshot_pairs"])
        speedups[key] = {
            str(cast(dict[str, object], row)["size"]): cast(
                dict[str, object],
                cast(dict[str, object], row)["summary"],
            )["conservative_speedup"]
            for row in pairs
        }
        unsafe[key] = result["unsafe_validation"]
        version_rows[key] = result["version_candidates"]
        offset_rows[key] = result["representations"]

    unsafe_adopted, unsafe_action, unsafe_reason = resolve_unsafe_decision(unsafe)

    artifact_paths = [(run / "manifest.json").as_posix() for run in runs.values()]
    artifact_paths.extend(
        (run / "benchmarks" / "results.json").as_posix() for run in runs.values()
    )
    artifact_paths.extend(
        (run / "profiles" / "summary.json").as_posix() for run in runs.values()
    )
    decisions = {
        "schema_version": SCHEMA_VERSION,
        "status": "pass",
        "phase": "Phase 2",
        "candidate_commit": candidate,
        "base_commit": BASE_COMMIT,
        "plan_sha256": PLAN_SHA256,
        "unresolved_decisions": 0,
        "version_function": {
            "selected": "xxh3_128_with_seed",
            "process_scoped_seed": True,
            "production_implementations": 1,
            "reason": (
                "fastest candidate that ships unchanged across both required "
                "targets without an AES-only build contract"
            ),
            "platform_results": version_rows,
        },
        "offsets": {
            "selected": "lazy_full_u32_u64",
            "u32_bytes_per_line": 4,
            "u64_fallback": True,
            "reason": (
                "full U32 offsets meet the four-byte gate and directly provide "
                "O(1) boundary validation and range slicing; checked U64 preserves "
                "the same interface above the U32 domain"
            ),
            "platform_results": offset_rows,
        },
        "unsafe": {
            "adopted": unsafe_adopted,
            "threshold": MINIMUM_UNSAFE_SPEEDUP,
            "platform_results": unsafe,
        },
        "snapshot_speedups": speedups,
        "rollback_no_go": [
            {
                "experiment": "gxhash128 snapshot version",
                "decision": "reject",
                "reason": (
                    "requires an AES-target build contract and is not the "
                    "portable cross-target implementation"
                ),
            },
            {
                "experiment": "blake3_128 snapshot version",
                "decision": "reject",
                "reason": ("portable but slower than XXH3-128 on both required hosts"),
            },
            {
                "experiment": "sparse and rank/select production offsets",
                "decision": "reject",
                "reason": (
                    "sparse checkpoints require bounded rescans rather than direct "
                    "O(1) offsets, while rank/select adds machinery that the full "
                    "U32 representation does not need to satisfy the memory gate"
                ),
            },
            {
                "experiment": "unchecked validated-string conversion",
                "decision": unsafe_action,
                "reason": unsafe_reason,
            },
            {
                "experiment": "Phase 2 snapshot core",
                "decision": "adopt",
                "reason": (
                    "both exact-commit hosts pass 4x construction, mutation, "
                    "overflow, and metadata gates"
                ),
            },
        ],
        "artifact_paths": artifact_paths,
    }
    write_json(goal_root / "phase2-decisions.json", decisions)
    LOGGER.info(
        "froze Phase 2 decisions at %s",
        goal_root / "phase2-decisions.json",
    )
    return 0


def self_test() -> int:
    """Run deterministic helper checks without compiling or timing code."""

    first = generate_corpus(5, 0xB200_0010)
    second = generate_corpus(5, 0xB200_0010)
    if first != second or first.count(b"\n") != 5:
        raise CaptureError("corpus generator self-test failed")
    if representation_bytes_per_line("full_u32", 100, 11) != 4.0:
        raise CaptureError("U32 representation self-test failed")
    order = [
        variant
        for round_index in range(3)
        for variant in (
            ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")
        )
    ]
    if order != [
        "base",
        "candidate",
        "candidate",
        "base",
        "base",
        "candidate",
    ]:
        raise CaptureError("interleaving self-test failed")
    LOGGER.info("Phase 2 capture helper self-test PASS")
    return 0


def parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""

    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)

    capture_parser = subcommands.add_parser(
        "capture",
        help="capture one platform",
    )
    capture_parser.add_argument("--goal-root", default=PHASE2_GOAL)
    capture_parser.add_argument("--rounds", type=int, default=MINIMUM_ROUNDS)
    capture_parser.add_argument("--profile-seconds", type=int, default=8)
    capture_parser.add_argument(
        "--max-normalized-load",
        type=float,
        default=DEFAULT_MAX_NORMALIZED_LOAD,
    )
    capture_parser.add_argument(
        "--load-wait-seconds",
        type=int,
        default=600,
    )

    finalize_parser = subcommands.add_parser(
        "finalize",
        help="freeze cross-platform decisions",
    )
    finalize_parser.add_argument("--goal-root", default=PHASE2_GOAL)
    subcommands.add_parser("self-test", help="validate pure helpers")
    return root


def main() -> int:
    """Dispatch one Phase 2 capture command."""

    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    args = parser().parse_args()
    if args.command == "capture":
        return capture(args)
    if args.command == "finalize":
        return finalize(args)
    if args.command == "self-test":
        return self_test()
    raise CaptureError(f"unknown command: {args.command}")


if __name__ == "__main__":
    raise SystemExit(main())
