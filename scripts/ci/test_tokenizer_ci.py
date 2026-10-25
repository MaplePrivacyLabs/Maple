#!/usr/bin/env python3
"""Prove the shared crate's gates execute independently of consumer tests."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
MANIFEST = str(ROOT / "crates/model-tokenizer/Cargo.toml")
EXPECTED_GATES = [
    ["fmt", "--all", "--manifest-path", MANIFEST, "--", "--check"],
    ["clippy", "--locked", "--all-targets", "--all-features", "--manifest-path", MANIFEST, "--", "-D", "warnings"],
    ["test", "--locked", "--all-features", "--manifest-path", MANIFEST],
    ["test", "--locked", "--all-features", "--features", "serde_json/preserve_order", "--manifest-path", MANIFEST],
]


class TokenizerCiTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="maple-tokenizer-gate-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.calls = self.directory / "cargo-calls.jsonl"
        self.staged = self.directory / "staged-paths.txt"
        fake_cargo = self.directory / "cargo"
        fake_cargo.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "with open(os.environ['MAPLE_TOKENIZER_GATE_TEST_CALLS'], 'a') as capture:\n"
            "    capture.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "if os.environ.get('MAPLE_TOKENIZER_GATE_TEST_FAIL') == sys.argv[1]:\n"
            "    sys.exit(17)\n",
            encoding="utf-8",
        )
        fake_cargo.chmod(0o755)
        self.environment = {
            "PATH": str(self.directory) + os.pathsep + os.environ.get("PATH", ""),
            "MAPLE_TOKENIZER_GATE_TEST_CALLS": str(self.calls),
            "MAPLE_HOOK_REPO_ROOT": str(ROOT),
            "MAPLE_HOOK_STAGED": str(self.staged),
        }

    def run_script(self, path: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(ROOT / path)], cwd=ROOT / "services/opensecret",
            env=self.environment, capture_output=True, text=True, check=False,
        )

    def captured(self) -> list[list[str]]:
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def test_standalone_helper_runs_format_lint_and_both_serialization_test_modes(self) -> None:
        result = self.run_script("scripts/ci/tokenizer.sh")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.captured(), EXPECTED_GATES)

    def test_standalone_helper_propagates_failure_and_stops_later_gates(self) -> None:
        self.environment["MAPLE_TOKENIZER_GATE_TEST_FAIL"] = "clippy"
        result = self.run_script("scripts/ci/tokenizer.sh")
        self.assertEqual(result.returncode, 17)
        self.assertEqual(self.captured(), EXPECTED_GATES[:2])

    def test_backend_hook_runs_standalone_suite_for_shared_crate_or_gate_changes(self) -> None:
        for staged in (
            "crates/model-tokenizer/src/render.rs\n",
            "crates/model-tokenizer/tests/fixtures/hf_chat.json\n",
            "crates/model-tokenizer/Cargo.lock\n",
            "scripts/ci/tokenizer.sh\n",
        ):
            with self.subTest(staged=staged):
                self.calls.unlink(missing_ok=True)
                self.staged.write_text(staged)
                result = self.run_script("services/opensecret/.githooks/pre-commit")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                calls = self.captured()
                self.assertEqual(len(calls), 7)
                self.assertEqual(calls[3:], EXPECTED_GATES)

    def test_backend_only_change_does_not_duplicate_shared_crate_tests_in_hook(self) -> None:
        self.staged.write_text("services/opensecret/src/main.rs\n")
        result = self.run_script("services/opensecret/.githooks/pre-commit")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.captured(), [
            ["fmt", "--all", "--", "--check"],
            ["clippy", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings"],
            ["test", "--locked", "--all-features"],
        ])

    def test_backend_workflow_runs_standalone_goldens_once(self) -> None:
        backend = (ROOT / ".github/workflows/opensecret-ci.yml").read_text()
        self.assertEqual(backend.count("-c bash ../../scripts/ci/tokenizer.sh"), 1)


if __name__ == "__main__":
    unittest.main()
