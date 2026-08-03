"""Tests for the Phase 1 evaluator contract."""

from __future__ import annotations

import hashlib
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from benches.support import phase1


class Phase1EvaluatorTests(unittest.TestCase):
    """Pin the fail-closed Phase 1 evidence contract."""

    def test_checksum_manifest_replays_exact_coverage_and_bytes(self) -> None:
        """A canonical run must contain exactly the bytes named by its manifest."""

        with tempfile.TemporaryDirectory() as directory:
            run_root = Path(directory)
            artifact = run_root / "raw" / "result.json"
            artifact.parent.mkdir()
            _ = artifact.write_text('{"status":"pass"}\n', encoding="utf-8")
            digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
            phase1.write_json(run_root / "SHA256SUMS.json", {"raw/result.json": digest})

            result = phase1.verify_checksum_manifest(run_root)

            self.assertEqual(result["entry_count"], 1)
            self.assertEqual(result["file_count"], 2)
            self.assertEqual(
                result["total_bytes"],
                artifact.stat().st_size + (run_root / "SHA256SUMS.json").stat().st_size,
            )

    def test_checksum_manifest_rejects_mutation_and_unhashed_files(self) -> None:
        """Changed or newly added raw evidence must fail closed."""

        with tempfile.TemporaryDirectory() as directory:
            run_root = Path(directory)
            artifact = run_root / "result.txt"
            _ = artifact.write_text("original\n", encoding="utf-8")
            digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
            phase1.write_json(run_root / "SHA256SUMS.json", {"result.txt": digest})

            _ = artifact.write_text("mutated\n", encoding="utf-8")
            with self.assertRaisesRegex(phase1.EvaluationError, "checksum mismatch"):
                _ = phase1.verify_checksum_manifest(run_root)

            _ = artifact.write_text("original\n", encoding="utf-8")
            _ = (run_root / "unhashed.txt").write_text("unexpected\n", encoding="utf-8")
            with self.assertRaisesRegex(phase1.EvaluationError, "coverage mismatch"):
                _ = phase1.verify_checksum_manifest(run_root)

    def test_checksum_manifest_rejects_path_escape(self) -> None:
        """A manifest entry can never address bytes outside its run root."""

        for path in ("/absolute", "../parent", "nested/../../parent"):
            with self.subTest(path=path), self.assertRaises(phase1.EvaluationError):
                phase1.validate_relative_artifact_path(path)

    def test_cli_contract_rejects_v1_controls(self) -> None:
        """Only root and confinement controls remain in the shipped CLI."""

        with tempfile.TemporaryDirectory() as directory:
            help_path = Path(directory) / "help.txt"
            _ = help_path.write_text("Usage: hashline-mcp [--root PATH] [--restrict]\n")
            result = phase1.verify_cli_help(help_path)
            self.assertEqual(result["present"], [])

            _ = help_path.write_text(
                "Usage: hashline-mcp [--root PATH] [--restrict] [--scheme NAME]\n"
            )
            with self.assertRaisesRegex(phase1.EvaluationError, "compatibility"):
                _ = phase1.verify_cli_help(help_path)

    def test_each_rule_requires_its_own_executable_example(self) -> None:
        """A global fence count cannot hide a rule with no runnable example."""

        blocks = [
            f"### {rule}: example\n\n```rust\nassert True\n```"
            for rule in phase1.RULE_IDS
        ]
        self.assertEqual(
            phase1.rules_without_executable_examples("\n\n".join(blocks)),
            [],
        )

        missing_rule = "R007"
        missing_index = phase1.RULE_IDS.index(missing_rule)
        blocks[missing_index] = (
            f"### {missing_rule}: example\n\n```text\nnot executable\n```"
        )
        self.assertEqual(
            phase1.rules_without_executable_examples("\n\n".join(blocks)),
            [missing_rule],
        )

    def test_command_environment_removes_ambient_compiler_flags(self) -> None:
        """Quality gates cannot inherit flags that change generated code."""

        ambient = {
            "RUSTFLAGS": "-C target-cpu=native",
            "RUSTDOCFLAGS": "-C target-feature=+aes",
            "CARGO_ENCODED_RUSTFLAGS": "-Clto",
        }
        with mock.patch.dict(os.environ, ambient, clear=False):
            environment, removed = phase1.command_environment()

        self.assertEqual(sorted(removed), sorted(ambient))
        for name in ambient:
            self.assertNotIn(name, environment)
        self.assertEqual(environment["CARGO_TERM_COLOR"], "never")
        self.assertEqual(environment["PYTHONDONTWRITEBYTECODE"], "1")


if __name__ == "__main__":
    _ = unittest.main()
