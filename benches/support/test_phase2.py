# Archival: companion to the retained baseline .md records. The bench targets
# and symbols this script drives were deleted in Phase 8 (29ffc1e); kept for
# provenance, it will not run against current HEAD.
"""Regression tests for the Phase 2 capture and evaluator contracts."""

from __future__ import annotations

import copy
import hashlib
import os
import tempfile
import unittest
from pathlib import Path
from typing import cast
from unittest import mock

import benches.support.phase1 as phase1  # noqa: PLR0402
import benches.support.phase2 as phase2  # noqa: PLR0402
import benches.support.phase2_capture as phase2_capture  # noqa: PLR0402


def estimate(
    point: float,
    lower: float,
    upper: float,
) -> dict[str, object]:
    """Build the absolute estimate subset stored with one benchmark run."""

    return {
        "estimate_kind": "absolute",
        "point_estimate_ns": point,
        "confidence_interval_ns": {
            "lower_bound": lower,
            "upper_bound": upper,
            "confidence_level": 0.95,
        },
    }


def write_criterion_estimate(
    path: Path,
    point: float,
    lower: float,
    upper: float,
) -> None:
    """Write the real Criterion median JSON shape consumed by the evaluator."""

    phase1.write_json(
        path,
        {
            "median": {
                "confidence_interval": {
                    "confidence_level": 0.95,
                    "lower_bound": lower,
                    "upper_bound": upper,
                },
                "point_estimate": point,
                "standard_error": 0.1,
            }
        },
    )


def paired_fixture(
    run_root: Path,
    candidate_commit: str,
) -> dict[str, object]:
    """Create six interleaved exact-commit runs with raw Criterion evidence."""

    order = ("base", "candidate", "candidate", "base", "base", "candidate")
    base_values = iter(
        (
            (400.0, 390.0, 410.0),
            (410.0, 400.0, 420.0),
            (390.0, 380.0, 400.0),
        )
    )
    candidate_values = iter(
        (
            (80.0, 76.0, 84.0),
            (82.0, 78.0, 86.0),
            (78.0, 74.0, 82.0),
        )
    )
    runs: list[dict[str, object]] = []
    for sequence, variant in enumerate(order, start=1):
        point, lower, upper = next(
            base_values if variant == "base" else candidate_values
        )
        relative = Path("benchmarks", "snapshot", f"{sequence:02d}-{variant}")
        write_criterion_estimate(
            run_root / relative / "criterion_new" / "estimates.json",
            point,
            lower,
            upper,
        )
        runs.append(
            {
                "sequence": sequence,
                "variant": variant,
                "commit": (
                    phase2.BASE_COMMIT if variant == "base" else candidate_commit
                ),
                "raw_path": relative.as_posix(),
                "estimate": estimate(point, lower, upper),
            }
        )

    baseline = [run for run in runs if run["variant"] == "base"]
    candidate = [run for run in runs if run["variant"] == "candidate"]
    return {
        "size": 10_000,
        "corpus_line_count": 10_000,
        "corpus_sha256": "3ea93efb",
        "rounds": 3,
        "runs": runs,
        "summary": phase2_capture.conservative_summary(
            baseline,
            candidate,
        ),
    }


class Phase2CaptureContractTests(unittest.TestCase):
    """Pin reproducibility and arithmetic in the capture harness."""

    def test_corpus_port_matches_independent_rust_byte_identities(self) -> None:
        """Every generated corpus must retain its independently derived hash."""

        for line_count, seed in phase2_capture.CORPUS_SPECS.items():
            with self.subTest(line_count=line_count):
                content = phase2_capture.generate_corpus(line_count, seed)
                expected_length, expected_digest = phase2_capture.EXPECTED_CORPORA[
                    line_count
                ]
                self.assertEqual(len(content), expected_length)
                self.assertEqual(
                    hashlib.sha256(content).hexdigest(),
                    expected_digest,
                )
                self.assertEqual(content.count(b"\n"), line_count)

    def test_clean_environment_removes_every_ambient_compiler_flag(self) -> None:
        """Neither correctness nor benchmark builds may inherit ambient flags."""

        ambient = {
            "RUSTFLAGS": "-C target-cpu=native",
            "RUSTDOCFLAGS": "-C opt-level=3",
            "CARGO_ENCODED_RUSTFLAGS": "-Clto",
        }
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.dict(os.environ, ambient, clear=False),
        ):
            environment = phase2_capture.clean_environment(Path(directory))

        for name in ambient:
            self.assertNotIn(name, environment)
        self.assertEqual(environment["LC_ALL"], "C")
        self.assertEqual(environment["LANG"], "C")
        self.assertEqual(
            environment["CARGO_TARGET_DIR"],
            directory,
        )

    def test_load_gate_requires_three_consecutive_quiet_samples(self) -> None:
        """A noisy sample must reset rather than merely dilute the gate."""

        loads = (
            (0.8, 0.8, 0.8),
            (1.6, 1.6, 1.6),
            (0.8, 0.8, 0.8),
            (0.8, 0.8, 0.8),
            (0.8, 0.8, 0.8),
        )
        with (
            mock.patch.object(phase2_capture.os, "cpu_count", return_value=4),
            mock.patch.object(
                phase2_capture.os,
                "getloadavg",
                side_effect=loads,
            ),
            mock.patch.object(
                phase2_capture.time,
                "monotonic",
                side_effect=(0.0, 0.1, 0.2, 0.3, 0.4),
            ),
            mock.patch.object(phase2_capture.time, "sleep"),
            mock.patch.object(
                phase2_capture,
                "process_census",
                return_value="quiet",
            ),
        ):
            evidence = phase2_capture.capture_load_gate(0.30, 10)

        self.assertEqual(evidence["status"], "pass")
        attempts = cast(list[object], evidence["attempts"])
        accepted_samples = cast(list[object], evidence["accepted_samples"])
        self.assertEqual(len(attempts), 5)
        self.assertEqual(len(accepted_samples), 3)
        self.assertEqual(evidence["process_census"], "quiet")

    def test_load_gate_times_out_fail_closed(self) -> None:
        """Persistent host load cannot produce a PASS manifest."""

        with (
            mock.patch.object(phase2_capture.os, "cpu_count", return_value=4),
            mock.patch.object(
                phase2_capture.os,
                "getloadavg",
                return_value=(2.0, 2.0, 2.0),
            ),
            mock.patch.object(
                phase2_capture.time,
                "monotonic",
                side_effect=(0.0, 2.0),
            ),
            mock.patch.object(phase2_capture.time, "sleep"),
            self.assertRaisesRegex(
                phase2_capture.CaptureError,
                "timed out",
            ),
        ):
            _ = phase2_capture.capture_load_gate(0.30, 1)

    def test_representation_memory_math_covers_every_prototype(self) -> None:
        """Resident-byte accounting must include full, sparse, and bitmap rows."""

        logical_lines = 50_001
        corpus_bytes = phase2_capture.EXPECTED_CORPORA[50_000][0]
        self.assertEqual(
            phase2_capture.representation_bytes_per_line(
                "full_u32",
                corpus_bytes,
                logical_lines,
            ),
            4.0,
        )
        self.assertEqual(
            phase2_capture.representation_bytes_per_line(
                "full_u64",
                corpus_bytes,
                logical_lines,
            ),
            8.0,
        )
        self.assertLess(
            phase2_capture.representation_bytes_per_line(
                "sparse_128",
                corpus_bytes,
                logical_lines,
            ),
            0.04,
        )
        self.assertGreater(
            phase2_capture.representation_bytes_per_line(
                "rank_select_bitmap",
                corpus_bytes,
                logical_lines,
            ),
            5.0,
        )
        with self.assertRaisesRegex(
            phase2_capture.CaptureError,
            "unknown offset representation",
        ):
            _ = phase2_capture.representation_bytes_per_line(
                "invented",
                corpus_bytes,
                logical_lines,
            )

    def test_conservative_speedup_uses_opposing_confidence_bounds(self) -> None:
        """The gate must not divide optimistic point estimates."""

        baseline: list[dict[str, object]] = [
            {"estimate": estimate(400.0, 390.0, 410.0)},
            {"estimate": estimate(410.0, 400.0, 420.0)},
            {"estimate": estimate(390.0, 380.0, 400.0)},
        ]
        candidate: list[dict[str, object]] = [
            {"estimate": estimate(80.0, 76.0, 84.0)},
            {"estimate": estimate(82.0, 78.0, 86.0)},
            {"estimate": estimate(78.0, 74.0, 82.0)},
        ]

        summary = phase2_capture.conservative_summary(
            baseline,
            candidate,
        )

        self.assertAlmostEqual(summary["point_speedup"], 5.0)
        self.assertAlmostEqual(
            summary["conservative_speedup"],
            390.0 / 84.0,
        )
        with self.assertRaisesRegex(
            phase2_capture.CaptureError,
            "sample counts differ",
        ):
            _ = phase2_capture.conservative_summary(
                baseline,
                candidate[:-1],
            )

    def test_checksum_manifest_detects_new_or_mutated_raw_bytes(self) -> None:
        """No artifact can change or appear after SHA-256 coverage is frozen."""

        with tempfile.TemporaryDirectory() as directory:
            run_root = Path(directory)
            artifact = run_root / "raw" / "result.txt"
            artifact.parent.mkdir()
            _ = artifact.write_text("original\n", encoding="utf-8")
            hashes = phase2_capture.write_checksums(run_root)

            self.assertEqual(set(hashes), {"raw/result.txt"})
            replay = phase1.verify_checksum_manifest(run_root)
            self.assertEqual(replay["entry_count"], 1)

            _ = artifact.write_text("mutated\n", encoding="utf-8")
            with self.assertRaisesRegex(
                phase1.EvaluationError,
                "checksum mismatch",
            ):
                _ = phase1.verify_checksum_manifest(run_root)

    def test_canonical_run_authenticates_pointer_manifest_and_raw_bytes(self) -> None:
        """Finalization must reject a forged pointer or post-capture mutation."""

        candidate_commit = "c" * 40
        with tempfile.TemporaryDirectory() as directory:
            goal_root = Path(directory)
            platform_root = goal_root / "artifacts" / "macos-arm64"
            run_root = platform_root / "runs" / "run-1"
            phase2_capture.write_json(
                run_root / "manifest.json",
                {
                    "schema_version": phase2_capture.SCHEMA_VERSION,
                    "status": "pass",
                    "phase": "Phase 2",
                    "platform": "macos-arm64",
                    "base_commit": phase2_capture.BASE_COMMIT,
                    "candidate_commit": candidate_commit,
                    "plan_sha256": phase2_capture.PLAN_SHA256,
                },
            )
            result_path = run_root / "benchmarks" / "results.json"
            phase2_capture.write_json(result_path, {"status": "pass"})
            _ = phase2_capture.write_checksums(run_root)
            latest: dict[str, object] = {
                "schema_version": phase2_capture.SCHEMA_VERSION,
                "run": "runs/run-1",
                "manifest_sha256": phase2_capture.sha256_file(
                    run_root / "manifest.json"
                ),
                "checksums_sha256": phase2_capture.sha256_file(
                    run_root / "SHA256SUMS.json"
                ),
            }
            phase2_capture.write_json(platform_root / "latest.json", latest)

            self.assertEqual(
                phase2_capture.canonical_run(
                    goal_root,
                    "macos-arm64",
                    candidate_commit,
                ),
                run_root.resolve(),
            )

            forged = copy.deepcopy(latest)
            forged["manifest_sha256"] = "0" * 64
            phase2_capture.write_json(platform_root / "latest.json", forged)
            with self.assertRaisesRegex(
                phase2_capture.CaptureError,
                "canonical manifest_sha256 changed",
            ):
                _ = phase2_capture.canonical_run(
                    goal_root,
                    "macos-arm64",
                    candidate_commit,
                )

            phase2_capture.write_json(platform_root / "latest.json", latest)
            phase2_capture.write_json(result_path, {"status": "mutated"})
            with self.assertRaisesRegex(
                phase2_capture.CaptureError,
                "checksum replay failed",
            ):
                _ = phase2_capture.canonical_run(
                    goal_root,
                    "macos-arm64",
                    candidate_commit,
                )

    def test_cross_platform_unsafe_decision_is_evidence_derived(self) -> None:
        """Both hosts must agree and independently satisfy the configured gate."""

        rejected: dict[str, object] = {
            "macos-arm64": {
                "adopted": False,
                "conservative_speedup": 1.02,
                "threshold": phase2_capture.MINIMUM_UNSAFE_SPEEDUP,
                "miri": "not_required",
            },
            "linux-amd64": {
                "adopted": False,
                "conservative_speedup": 1.04,
                "threshold": phase2_capture.MINIMUM_UNSAFE_SPEEDUP,
                "miri": "not_required",
            },
        }
        adopted, action, reason = phase2_capture.resolve_unsafe_decision(rejected)
        self.assertFalse(adopted)
        self.assertEqual(action, "reject")
        self.assertIn("macos-arm64=1.0200x", reason)

        accepted: dict[str, object] = {
            key: {
                "adopted": True,
                "conservative_speedup": speedup,
                "threshold": phase2_capture.MINIMUM_UNSAFE_SPEEDUP,
                "miri": "pass",
            }
            for key, speedup in (
                ("macos-arm64", 1.06),
                ("linux-amd64", 1.08),
            )
        }
        adopted, action, reason = phase2_capture.resolve_unsafe_decision(accepted)
        self.assertTrue(adopted)
        self.assertEqual(action, "adopt")
        self.assertIn("Miri PASS", reason)

        inconsistent = copy.deepcopy(rejected)
        linux = cast(dict[str, object], inconsistent["linux-amd64"])
        linux["adopted"] = True
        with self.assertRaisesRegex(
            phase2_capture.CaptureError,
            "platforms disagree",
        ):
            _ = phase2_capture.resolve_unsafe_decision(inconsistent)


class Phase2EvaluatorContractTests(unittest.TestCase):
    """Pin evaluator replay and rejection behavior."""

    def test_pair_replays_raw_estimates_and_exact_interleaving(self) -> None:
        """Summary-only or reordered performance claims must fail."""

        candidate_commit = "c" * 40
        with tempfile.TemporaryDirectory() as directory:
            run_root = Path(directory)
            pair = paired_fixture(run_root, candidate_commit)

            result = phase2.verify_pair(
                pair,
                10_000,
                run_root,
                candidate_commit,
            )

            conservative_speedup = cast(
                float,
                result["conservative_speedup"],
            )
            self.assertGreaterEqual(
                conservative_speedup,
                phase2.MINIMUM_SNAPSHOT_SPEEDUP,
            )
            reordered = copy.deepcopy(pair)
            runs = cast(list[object], reordered["runs"])
            runs[0], runs[1] = runs[1], runs[0]
            with self.assertRaisesRegex(
                phase2.EvaluationError,
                "not interleaved",
            ):
                _ = phase2.verify_pair(
                    reordered,
                    10_000,
                    run_root,
                    candidate_commit,
                )

    def test_unsafe_gate_rejects_safe_source_above_five_percent(self) -> None:
        """A measured qualifying unsafe gain cannot be discarded silently."""

        accepted_safe: dict[str, object] = {
            "unsafe_validation": {
                "adopted": False,
                "conservative_speedup": 1.049,
                "miri": "not_required",
            }
        }
        result = phase2.verify_unsafe_result(
            accepted_safe,
            "macos-arm64",
            0,
        )
        self.assertFalse(result["adopted"])

        rejected_safe = copy.deepcopy(accepted_safe)
        rejected_result = cast(
            dict[str, object],
            rejected_safe["unsafe_validation"],
        )
        rejected_result["conservative_speedup"] = 1.05
        with self.assertRaisesRegex(
            phase2.EvaluationError,
            "rejected unsafe path",
        ):
            _ = phase2.verify_unsafe_result(
                rejected_safe,
                "macos-arm64",
                0,
            )

    def test_live_source_contract_has_one_version_engine_and_audited_unsafe(
        self,
    ) -> None:
        """Production uses one version engine and exposes its audited unsafe path."""

        result = phase2.verify_source_contract()

        self.assertGreater(cast(int, result["production_unsafe_tokens"]), 0)
        engines = result["version_engines"]
        self.assertEqual(
            engines,
            {
                "gxhash128": False,
                "xxh3_128_with_seed": True,
                "blake3": False,
            },
        )


if __name__ == "__main__":
    _ = unittest.main()
