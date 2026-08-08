# Archival: companion to the retained baseline .md records. The bench targets
# and symbols this script drives were deleted in Phase 8 (29ffc1e); kept for
# provenance, it will not run against current HEAD.
"""Tests for the Phase 0 capture and evaluator contract."""

from __future__ import annotations

import os
import sys
import tempfile
import time
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import phase0


def write_estimate(path: Path, point: float, lower: float, upper: float) -> None:
    """Write the subset of a Criterion estimate consumed by the harness."""
    phase0.write_json(
        path,
        {
            "median": {
                "confidence_interval": {
                    "confidence_level": 0.95,
                    "lower_bound": lower,
                    "upper_bound": upper,
                },
                "point_estimate": point,
            }
        },
    )


class Phase0ContractTests(unittest.TestCase):
    """Pin the fail-closed Phase 0 evidence contract."""

    def test_pair_contract_has_unique_balanced_filters(self) -> None:
        """Every target must identify distinct base and candidate functions."""
        names = {pair.name for pair in phase0.PAIR_SPECS}
        filters = {
            benchmark_filter
            for pair in phase0.PAIR_SPECS
            for benchmark_filter in (pair.base_filter, pair.candidate_filter)
        }

        self.assertEqual(len(names), len(phase0.PAIR_SPECS))
        self.assertEqual(len(filters), len(phase0.PAIR_SPECS) * 2)
        self.assertTrue(
            all(pair.base_filter != pair.candidate_filter for pair in phase0.PAIR_SPECS)
        )

    def test_bootstrap_interval_is_deterministic_and_contains_median(self) -> None:
        """The reported filesystem interval must be reproducible."""
        samples = [11, 17, 23, 31, 47]
        first = phase0.bootstrap_median_interval(samples)
        second = phase0.bootstrap_median_interval(samples)

        self.assertEqual(first, second)
        self.assertEqual(first["point_estimate_ns"], 23.0)
        self.assertLessEqual(first["lower_bound_ns"], 23.0)
        self.assertGreaterEqual(first["upper_bound_ns"], 23.0)

    def test_newest_estimate_selects_fresh_absolute_output(self) -> None:
        """A newer Criterion change estimate must never masquerade as nanoseconds."""
        with tempfile.TemporaryDirectory() as directory:
            criterion_root = Path(directory)
            started_ns = time.time_ns()
            absolute = criterion_root / "group" / "function" / "new" / "estimates.json"
            relative = (
                criterion_root / "group" / "function" / "change" / "estimates.json"
            )
            write_estimate(absolute, 100.0, 90.0, 110.0)
            write_estimate(relative, 0.0, -0.02, 0.02)
            os.utime(absolute, ns=(started_ns + 1, started_ns + 1))
            os.utime(relative, ns=(started_ns + 2, started_ns + 2))

            selected = phase0.newest_estimate(criterion_root, started_ns)

            self.assertEqual(selected, absolute)
            self.assertEqual(
                phase0.estimate_summary(selected)["estimate_kind"],
                "absolute",
            )

    def test_profile_symbol_validation_replays_raw_report(self) -> None:
        """Profile summaries must be scenario-specific views of raw reports."""
        reports = {
            "full_read": "phase0_resources::profile_full_read_once",
            "edit": "hashline::edit::apply::apply_edits",
            "rare_grep": "_RNvNtCs50PqqpBYgnV_8hashline4grep11search_file",
            "common_grep": "hashline::grep::search_file",
        }
        with tempfile.TemporaryDirectory() as directory:
            run_root = Path(directory)
            records: list[dict[str, object]] = []
            for scenario, report in reports.items():
                raw = run_root / "profiles" / scenario / "report.txt"
                raw.parent.mkdir(parents=True)
                _ = raw.write_text(report, encoding="utf-8")
                symbol_hits = phase0.profile_symbol_hits(scenario, report)
                records.append(
                    {
                        "scenario": scenario,
                        "raw_path": str(raw.relative_to(run_root)),
                        "symbol_hits": symbol_hits,
                        "symbolized": bool(symbol_hits),
                    }
                )

            phase0.write_json(run_root / "profiles" / "results.json", records)
            phase0.validate_profiles(run_root)
            self.assertEqual(
                phase0.profile_symbol_hits("rare_grep", "third_party::search_file"),
                [],
            )

            records[2]["symbol_hits"] = ["profile_grep_once"]
            phase0.write_json(run_root / "profiles" / "results.json", records)
            with self.assertRaisesRegex(
                phase0.CaptureError,
                "profile summary differs from raw report: rare_grep",
            ):
                phase0.validate_profiles(run_root)

    def test_hash_manifest_has_exact_artifact_coverage(self) -> None:
        """The immutable checksum manifest must cover exactly the captured files."""
        with tempfile.TemporaryDirectory() as directory:
            run_root = Path(directory)
            nested = run_root / "raw" / "result.json"
            nested.parent.mkdir()
            nested.write_text('{"status":"pass"}\n', encoding="utf-8")

            hashes = phase0.write_hash_manifest(run_root)

            self.assertEqual(set(hashes), {"raw/result.json"})
            phase0.validate_hashes(run_root)

            (run_root / "unhashed.txt").write_text("unexpected\n", encoding="utf-8")
            with self.assertRaisesRegex(
                phase0.CaptureError,
                "checksum coverage mismatch",
            ):
                phase0.validate_hashes(run_root)


if __name__ == "__main__":
    unittest.main()
