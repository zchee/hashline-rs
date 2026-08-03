"""Evaluate the incompatible-redesign Phase 1 protocol freeze."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shlex
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath
from typing import cast

LOGGER_NAME = "hashline.phase1"
SCHEMA_VERSION = 1
BASE_COMMIT = "6afe83059de218d71d4161fb36848d849c9da0a6"
PLAN_PATH = ".omx/plans/2026-07-31-incompatible-max-performance-redesign.md"
PLAN_SHA256 = "db00bf029f184811b79ab709df064a3fb9b23a9ab64562e28432e43ca8a41a6f"
PHASE0_ROOT = Path(".omx/goals/performance/hashline-v2-phase0")
RULE_IDS = tuple(f"R{number:03d}" for number in range(1, 23))

IMMUTABLE_FILES = {
    PLAN_PATH: PLAN_SHA256,
    ".omx/goals/performance/hashline-v2-phase0/evaluation.json": "21af7734da8399ec4cf8544e202b09e8e6e339aea35dc2acd179c227476ef1b4",
    ".omx/goals/performance/hashline-v2-phase0/phase0-exit-gate-audit.json": "81e04dcc6a433070c0ecd58f4191113b00a11bcc49a220c7dc845d043d185a7f",
    ".omx/goals/performance/hashline-v2-phase0/state.json": "c3b40749533c24c85765a8c7224c521bc18535d697ad3515124cc7716af8633a",
    ".omx/goals/performance/hashline-v2-phase0/ledger.jsonl": "b110059d0c13930c0441171062c8cd1d07d4e1880ca5f2981b7194cd6f820107",
    ".omx/goals/performance/hashline-v2-phase0/codex-goal-complete.json": "f1b85c24f9ead933429b6704c57b4dc1de2d025f286585aafdf25fc625873a24",
    ".omx/goals/performance/hashline-v2-phase0/transfers/hashline-6afe83059de2.bundle": "f2e4f78776431d7722455946960121943bb7bbe19ba1e5950d6e399832e773c0",
}
CANONICAL_RUNS = {
    "macos-arm64": {
        "path": PHASE0_ROOT
        / "artifacts/macos-arm64/runs/20260731T124545Z-6afe83059de2",
        "manifest_sha256": "7bd988efe269cafe0db39efb141c1e5c3e32a3a930d8c2d70eb9ec7f0b8f754d",
        "checksums_sha256": "647e2109d8330fc0db858196abfdd51ad9a66a5c53f2b68333bc5af1e201dd0e",
        "entry_count": 3418,
        "total_bytes": 65_141_104,
    },
    "linux-amd64": {
        "path": PHASE0_ROOT
        / "artifacts/linux-amd64/runs/20260731T133009Z-6afe83059de2",
        "manifest_sha256": "fe47f3e57bdf367a5f97231069c65add07631a8ad2b8ccbabc3ab35e5e8de83a",
        "checksums_sha256": "d026dafa0897aff732f7c41c389281bd334f44ccd990b1f5cdc629eba2a877ba",
        "entry_count": 3418,
        "total_bytes": 605_377_617,
    },
}
ALLOWED_CHANGED_PATHS = frozenset(
    {
        "benches/support/phase1.py",
        "benches/support/test_phase1.py",
        "docs/protocol.md",
        "src/edit/types.rs",
        "src/grep.rs",
        "src/lib.rs",
        "src/main.rs",
        "src/protocol.rs",
        "src/read.rs",
        "src/server.rs",
    }
)
REQUIRED_CHANGED_PATHS = ALLOWED_CHANGED_PATHS
FORBIDDEN_PHASE2_PATHS = (
    "src/cache.rs",
    "src/persist.rs",
    "src/snapshot.rs",
)
PHASE0_TRACKED_PATHS = (
    "benches/BASELINE.md",
    "benches/V2_BASELINE.md",
    "benches/hashline.rs",
    "benches/support/phase0.py",
    "benches/support/phase0_resources.rs",
    "benches/support/phase0_workloads.rs",
    "benches/support/test_phase0.py",
)
QUALITY_COMMANDS = (
    (
        "phase1_contract_tests",
        (
            "python3",
            "-B",
            "-m",
            "unittest",
            "discover",
            "-s",
            "benches/support",
            "-p",
            "test_phase1.py",
        ),
    ),
    ("fmt", ("cargo", "fmt", "--all", "--", "--check")),
    (
        "build_all",
        ("cargo", "build", "--all-targets", "--all-features"),
    ),
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
    (
        "test_all",
        ("cargo", "test", "--all-targets", "--all-features"),
    ),
    (
        "test_no_default",
        ("cargo", "test", "--all-targets", "--no-default-features"),
    ),
    (
        "doc_test",
        ("cargo", "test", "--doc", "--all-features"),
    ),
    (
        "doc",
        ("cargo", "doc", "--no-deps", "--all-features"),
    ),
    (
        "cli_help",
        (
            "cargo",
            "run",
            "--quiet",
            "--bin",
            "hashline-mcp",
            "--",
            "--help",
        ),
    ),
)


class EvaluationError(RuntimeError):
    """Raised when a Phase 1 exit-gate invariant fails."""


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


def sha256_file(path: Path) -> str:
    """Return the SHA-256 digest of a file without loading it into memory."""

    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path) -> object:
    """Read a UTF-8 JSON artifact."""

    return cast(object, json.loads(path.read_text(encoding="utf-8")))


def read_json_object(path: Path) -> dict[str, object]:
    """Read a JSON object whose keys are strings."""

    value = read_json(path)
    if not isinstance(value, dict):
        raise EvaluationError(f"JSON artifact is not an object: {path}")
    mapping = cast(dict[object, object], value)
    if not all(isinstance(key, str) for key in mapping):
        raise EvaluationError(f"JSON object has a non-string key: {path}")
    return cast(dict[str, object], mapping)


def write_json(path: Path, value: object) -> None:
    """Atomically write stable, human-readable JSON."""

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(f"{path.suffix}.tmp")
    _ = temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    _ = temporary.replace(path)


def git_text(*arguments: str) -> str:
    """Run Git and return stripped UTF-8 stdout."""

    result = subprocess.run(
        ("git", *arguments),
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def require(condition: bool, message: str) -> None:
    """Fail the evaluator when an invariant is false."""

    if not condition:
        raise EvaluationError(message)


def verify_immutable_file(path_text: str, expected_sha256: str) -> dict[str, object]:
    """Verify one immutable Phase 0 or plan artifact."""

    path = Path(path_text)
    require(path.is_file(), f"immutable artifact is missing: {path}")
    actual_sha256 = sha256_file(path)
    require(
        actual_sha256 == expected_sha256,
        f"immutable artifact changed: {path}: {actual_sha256} != {expected_sha256}",
    )
    return {
        "path": path.as_posix(),
        "sha256": actual_sha256,
        "bytes": path.stat().st_size,
    }


def validate_relative_artifact_path(path_text: str) -> None:
    """Reject absolute or parent-traversing checksum-manifest entries."""

    path = PurePosixPath(path_text)
    require(not path.is_absolute(), f"absolute checksum path: {path_text}")
    require(".." not in path.parts, f"parent traversal in checksum path: {path_text}")


def verify_checksum_manifest(run_root: Path) -> dict[str, object]:
    """Replay a Phase 0 run's exact checksum coverage and contents."""

    manifest_path = run_root / "SHA256SUMS.json"
    manifest = read_json_object(manifest_path)

    expected: dict[str, str] = {}
    for relative, digest_value in manifest.items():
        if not isinstance(digest_value, str):
            raise EvaluationError(f"non-string checksum digest: {relative}")
        digest = digest_value
        validate_relative_artifact_path(relative)
        require(
            re.fullmatch(r"[0-9a-f]{64}", digest) is not None,
            f"invalid checksum digest: {relative}: {digest}",
        )
        expected[relative] = digest

    actual_paths = {
        path.relative_to(run_root).as_posix()
        for path in run_root.rglob("*")
        if path.is_file() and path != manifest_path
    }
    missing = sorted(set(expected) - actual_paths)
    unexpected = sorted(actual_paths - set(expected))
    require(
        actual_paths == set(expected),
        f"checksum coverage mismatch: missing={missing!r}, unexpected={unexpected!r}",
    )

    total_bytes = manifest_path.stat().st_size
    for relative, expected_digest in sorted(expected.items()):
        path = run_root / relative
        actual_digest = sha256_file(path)
        require(
            actual_digest == expected_digest,
            f"checksum mismatch: {path}: {actual_digest} != {expected_digest}",
        )
        total_bytes += path.stat().st_size

    return {
        "path": run_root.as_posix(),
        "entry_count": len(expected),
        "file_count": len(expected) + 1,
        "total_bytes": total_bytes,
        "checksums_sha256": sha256_file(manifest_path),
    }


def verify_phase0_evidence() -> dict[str, object]:
    """Prove the canonical Phase 0 evidence is byte-for-byte unchanged."""

    immutable = [
        verify_immutable_file(path, digest) for path, digest in IMMUTABLE_FILES.items()
    ]

    evaluation = read_json_object(PHASE0_ROOT / "evaluation.json")
    state = read_json_object(PHASE0_ROOT / "state.json")
    audit = read_json_object(PHASE0_ROOT / "phase0-exit-gate-audit.json")
    require(evaluation.get("status") == "pass", "Phase 0 evaluation is not PASS")
    require(
        evaluation.get("candidate_commit") == BASE_COMMIT,
        "Phase 0 evaluation candidate changed",
    )
    require(state.get("status") == "complete", "Phase 0 OMX state is not complete")
    require(audit.get("status") == "pass", "Phase 0 independent audit is not PASS")

    runs: dict[str, object] = {}
    for platform_name, specification in CANONICAL_RUNS.items():
        run_root = cast(Path, specification["path"])
        require(run_root.is_dir(), f"canonical run is missing: {run_root}")
        require(
            sha256_file(run_root / "manifest.json") == specification["manifest_sha256"],
            f"canonical manifest changed: {platform_name}",
        )
        result = verify_checksum_manifest(run_root)
        require(
            result["checksums_sha256"] == specification["checksums_sha256"],
            f"canonical checksum manifest changed: {platform_name}",
        )
        require(
            result["entry_count"] == specification["entry_count"],
            f"canonical checksum count changed: {platform_name}",
        )
        require(
            result["total_bytes"] == specification["total_bytes"],
            f"canonical byte count changed: {platform_name}",
        )
        runs[platform_name] = result

    return {"immutable_files": immutable, "canonical_runs": runs}


def verify_repository_scope() -> dict[str, object]:
    """Verify commit identity, cleanliness, signature, and Phase 1-only scope."""

    root = git_text("rev-parse", "--show-toplevel")
    require(
        Path(root).resolve() == Path.cwd().resolve(), f"wrong repository root: {root}"
    )

    head = git_text("rev-parse", "HEAD")
    require(head != BASE_COMMIT, "Phase 1 has no candidate commit")
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
        f"Phase 1 changed unrelated paths: {sorted(changed - ALLOWED_CHANGED_PATHS)!r}",
    )
    require(
        REQUIRED_CHANGED_PATHS <= changed,
        f"Phase 1 required paths are absent: {sorted(REQUIRED_CHANGED_PATHS - changed)!r}",
    )

    phase0_diff = git_text(
        "diff",
        "--name-only",
        f"{BASE_COMMIT}..{head}",
        "--",
        *PHASE0_TRACKED_PATHS,
    )
    require(not phase0_diff, f"tracked Phase 0 evidence changed:\n{phase0_diff}")

    present_phase2 = [path for path in FORBIDDEN_PHASE2_PATHS if Path(path).exists()]
    require(not present_phase2, f"Phase 2 implementation started: {present_phase2!r}")

    signature = git_text("log", "-1", "--format=%G?")
    require(
        signature in {"G", "U"}, f"candidate commit signature is not good: {signature}"
    )

    return {
        "base_commit": BASE_COMMIT,
        "candidate_commit": head,
        "signature_status": signature,
        "changed_paths": sorted(changed),
        "dirty": False,
    }


def rules_without_executable_examples(documentation: str) -> list[str]:
    """Return frozen rules that lack a Rust doc-test block in their section."""

    missing: list[str] = []
    for rule in RULE_IDS:
        section_with_rust = re.compile(
            rf"^### {re.escape(rule)}\b(?:(?!^### ).)*?^\x60{{3}}rust\s*$",
            re.MULTILINE | re.DOTALL,
        )
        if section_with_rust.search(documentation) is None:
            missing.append(rule)
    return missing


def verify_semantic_coverage() -> dict[str, object]:
    """Require one named test and documentation entry for every frozen rule."""

    documentation_path = Path("docs/protocol.md")
    source_path = Path("src/protocol.rs")
    documentation = documentation_path.read_text(encoding="utf-8")
    source = source_path.read_text(encoding="utf-8")

    unresolved = re.compile(
        r"\b(?:TODO|TBD|FIXME|XXX|unresolved|to be decided|placeholder)\b",
        re.IGNORECASE,
    )
    require(
        unresolved.search(documentation) is None,
        "protocol document contains an unresolved marker",
    )
    require(
        unresolved.search(source) is None,
        "protocol source contains an unresolved marker",
    )

    missing_documentation = [rule for rule in RULE_IDS if rule not in documentation]
    missing_tests = [
        rule
        for rule in RULE_IDS
        if f"fn {rule.lower().replace('-', '_')}_" not in source
    ]
    require(
        not missing_documentation,
        f"semantic rules missing from documentation: {missing_documentation!r}",
    )
    require(
        not missing_tests,
        f"semantic rules missing named tests: {missing_tests!r}",
    )

    executable_fences = len(re.findall(r"^`{3}rust\s*$", documentation, re.MULTILINE))
    missing_examples = rules_without_executable_examples(documentation)
    require(
        not missing_examples,
        f"semantic rules missing executable Rust examples: {missing_examples!r}",
    )
    require(
        executable_fences >= len(RULE_IDS),
        f"protocol document has only {executable_fences} executable Rust examples",
    )

    return {
        "rule_ids": list(RULE_IDS),
        "rule_count": len(RULE_IDS),
        "executable_rust_examples": executable_fences,
        "rules_with_executable_examples": len(RULE_IDS) - len(missing_examples),
    }


def command_environment() -> tuple[dict[str, str], list[str]]:
    """Return a deterministic cargo environment and removed ambient flags."""

    environment = os.environ.copy()
    removed: list[str] = []
    for name in ("RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        if name in environment:
            removed.append(name)
            del environment[name]
    environment["CARGO_TERM_COLOR"] = "never"
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    return environment, removed


def run_quality_command(
    artifact_root: Path,
    name: str,
    command: tuple[str, ...],
    environment: dict[str, str],
) -> CommandRecord:
    """Run one quality gate and persist complete stdout/stderr."""

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
    _ = stdout_path.write_bytes(result.stdout)
    _ = stderr_path.write_bytes(result.stderr)

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


def verify_cli_help(path: Path) -> dict[str, object]:
    """Verify the shipped CLI has no v1 scheme/hash compatibility controls."""

    text = path.read_text(encoding="utf-8")
    forbidden = (
        "--scheme",
        "--hash-len",
        "--chunk-size",
        "--checkpoint-interval",
        "HASHLINE_SCHEME",
        "HASHLINE_HASH_LEN",
        "HASHLINE_CHUNK_SIZE",
        "HASHLINE_CHECKPOINT_INTERVAL",
    )
    present = [token for token in forbidden if token in text]
    require(not present, f"v1 CLI compatibility controls remain: {present!r}")
    require(
        "--root" in text and "--restrict" in text, "required CLI controls are missing"
    )
    return {"forbidden_tokens": list(forbidden), "present": present}


def environment_record(
    removed_ambient_flags: list[str],
) -> dict[str, object]:
    """Capture the exact evaluator host and toolchain."""

    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": sys.version,
        "rustc": gitless_command_text(("rustc", "--version", "--verbose")),
        "cargo": gitless_command_text(("cargo", "--version", "--verbose")),
        "removed_ambient_flags": removed_ambient_flags,
    }


def gitless_command_text(command: tuple[str, ...]) -> str:
    """Return stdout from a small read-only command."""

    result = subprocess.run(
        command,
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def evaluate(goal_root: Path) -> Path:
    """Run every Phase 1 exit gate and return the evaluation artifact path."""

    head_hint = git_text("rev-parse", "--short=12", "HEAD")
    timestamp = datetime.now(UTC).strftime("%Y%m%dT%H%M%SZ")
    artifact_root = goal_root / "evaluations" / f"{timestamp}-{head_hint}"
    artifact_root.mkdir(parents=True, exist_ok=False)
    evaluation_path = artifact_root / "evaluation.json"
    top_level_path = goal_root / "evaluation.json"

    commands: list[dict[str, object]] = []
    result: dict[str, object] = {
        "schema_version": SCHEMA_VERSION,
        "status": "fail",
        "started_at": datetime.now(UTC).isoformat(),
        "artifact_root": artifact_root.as_posix(),
        "evaluator_command": " ".join(shlex.quote(argument) for argument in sys.argv),
        "commands": commands,
    }

    try:
        result["phase0_evidence"] = verify_phase0_evidence()
        result["repository"] = verify_repository_scope()
        result["semantic_coverage"] = verify_semantic_coverage()

        environment, removed = command_environment()
        result["environment"] = environment_record(removed)
        for name, command in QUALITY_COMMANDS:
            record = run_quality_command(
                artifact_root,
                name,
                command,
                environment,
            )
            commands.append(cast(dict[str, object], asdict(record)))

        cli_record = next(record for record in commands if record["name"] == "cli_help")
        result["cli_contract"] = verify_cli_help(
            Path(cast(str, cli_record["stdout_path"]))
        )
        result["status"] = "pass"
    except (EvaluationError, OSError, subprocess.SubprocessError, ValueError) as error:
        result["error"] = str(error)
    finally:
        result["completed_at"] = datetime.now(UTC).isoformat()
        write_json(evaluation_path, result)
        write_json(top_level_path, result)

    failure = result.get("error", "unknown failure")
    require(
        result["status"] == "pass",
        f"Phase 1 evaluator FAIL: {failure}; see {evaluation_path}",
    )
    return evaluation_path


def parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""

    command_parser = argparse.ArgumentParser(description=__doc__)
    subparsers = command_parser.add_subparsers(dest="command", required=True)
    evaluate_parser = subparsers.add_parser("evaluate")
    _ = evaluate_parser.add_argument("--goal-root", type=Path, required=True)
    return command_parser


def main() -> int:
    """CLI entrypoint."""

    arguments = parser().parse_args()
    command = cast(str, arguments.command)
    if command == "evaluate":
        path = evaluate(cast(Path, arguments.goal_root))
        print(f"INFO Phase 1 evaluator PASS: {path}")
        return 0
    raise AssertionError(f"unhandled command: {command}")


if __name__ == "__main__":
    raise SystemExit(main())
