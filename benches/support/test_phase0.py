"""Tests for the Phase 0 capture and evaluator contract."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import phase0


def test_pair_contract_has_unique_balanced_filters() -> None:
    """Every required target must identify distinct base and candidate functions."""
    names = {pair.name for pair in phase0.PAIR_SPECS}
    filters = {
        benchmark_filter
        for pair in phase0.PAIR_SPECS
        for benchmark_filter in (pair.base_filter, pair.candidate_filter)
    }

    assert len(names) == len(phase0.PAIR_SPECS)
    assert len(filters) == len(phase0.PAIR_SPECS) * 2
    assert all(pair.base_filter != pair.candidate_filter for pair in phase0.PAIR_SPECS)


def test_bootstrap_interval_is_deterministic_and_contains_median() -> None:
    """The reported filesystem interval must be reproducible."""
    samples = [11, 17, 23, 31, 47]
    first = phase0.bootstrap_median_interval(samples)
    second = phase0.bootstrap_median_interval(samples)

    assert first == second
    assert first["point_estimate_ns"] == 23.0
    assert first["lower_bound_ns"] <= 23.0 <= first["upper_bound_ns"]


def test_hash_manifest_covers_existing_artifacts(tmp_path: Path) -> None:
    """The immutable-run checksum manifest must cover every preexisting file."""
    nested = tmp_path / "raw" / "result.json"
    nested.parent.mkdir()
    nested.write_text('{"status":"pass"}\n', encoding="utf-8")

    hashes = phase0.write_hash_manifest(tmp_path)

    assert set(hashes) == {"raw/result.json"}
    phase0.validate_hashes(tmp_path)
