"""Regression tests for the two native apps' independent/shared input boundary."""

from pathlib import Path
import subprocess
import sys
import unittest

from agent_change_detection import affects_agent, classify_paths
from change_detection import DESKTOP_PLATFORMS, classify_path as research_routes


class AgentChangeDetectionTests(unittest.TestCase):
    def test_agent_runtime_build_and_asset_inputs_select_only_agent(self):
        for path in (
            "apps/maple-agent/app/src/main.rs",
            "apps/maple-agent/app/assets/fonts/Maple.ttf",
            "apps/maple-agent/crates/maple-agent/src/agent.rs",
            "apps/maple-agent/Cargo.toml",
            "apps/maple-agent/Cargo.lock",
            "apps/maple-agent/flake.nix",
            "apps/maple-agent/flake.lock",
            "apps/maple-agent/rust-toolchain.toml",
            "apps/maple-agent/justfile",
            "apps/maple-agent/scripts/macos-debug-app.sh",
            "apps/maple-agent/crates/maple-code-mode/python/worker.py",
            "apps/maple-agent/scripts/python-runtime.json",
            "apps/maple-agent/scripts/python-licenses/LICENSE",
            "apps/maple-agent/scripts/prepare-python.py",
            "apps/maple-agent/scripts/package-archive.py",
            "apps/maple-agent/.gitattributes",
            "apps/maple-agent/new-build-input",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))
                self.assertEqual(research_routes(path), frozenset())

    def test_shared_rust_runtime_inputs_select_both_desktop_apps(self):
        for path in (
            "sdk/rust/Cargo.toml", "sdk/rust/src/client.rs",
            "sdk/rust/assets/aws_nitro_root.der", "sdk/rust/build.rs",
            "proxy/Cargo.toml", "proxy/src/proxy.rs", "proxy/build.rs",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))
                self.assertEqual(research_routes(path), DESKTOP_PLATFORMS)

    def test_research_typescript_and_independent_services_skip_agent(self):
        for path in (
            "apps/maple-research/frontend/src/main.tsx",
            "apps/maple-research/frontend/src-tauri/src/lib.rs",
            "sdk/src/lib/index.ts", "sdk/package.json", "sdk/flake.nix",
            "services/updates/src/index.ts", "services/opensecret/src/main.rs",
            ".github/workflows/desktop-pr-build.yml", "scripts/ci/desktop-pr.sh",
        ):
            with self.subTest(path=path):
                self.assertFalse(affects_agent(path))

    def test_docs_and_standalone_dependency_inputs_skip_both_apps(self):
        for path in (
            "apps/maple-agent/README.md", "apps/maple-agent/AGENTS.md",
            "apps/maple-agent/CLAUDE.md", "apps/maple-agent/LICENSE",
            "apps/maple-agent/docs/development.md", "README.md",
            "sdk/rust/README.md", "sdk/rust/tests/client.rs", "sdk/rust/Cargo.lock",
            "proxy/README.md", "proxy/tests/health.rs", "proxy/Cargo.lock",
            "proxy/Dockerfile", "proxy/flake.nix",
        ):
            with self.subTest(path=path):
                self.assertFalse(affects_agent(path))
                self.assertEqual(research_routes(path), frozenset())

    def test_selector_and_shared_tooling_changes_select_agent(self):
        for path in (
            "flake.nix", "flake.lock", ".github/workflows/agent-ci.yml",
            "scripts/ci/agent_change_detection.py", "scripts/ci/change_detection.py",
            "scripts/ci/verify-agent-rust-deps.py",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))

    def test_unknown_roots_and_invalid_paths_fail_safe(self):
        for path in ("new-build-config.toml", "", "/tmp/file", "../file",
                     "apps/maple-agent/../maple-research/frontend/src/main.tsx"):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))

    def test_mixed_changes_and_empty_diff(self):
        self.assertFalse(classify_paths([]))
        self.assertFalse(classify_paths(["README.md", "sdk/rust/README.md"]))
        self.assertTrue(classify_paths(["README.md", "proxy/src/proxy.rs"]))

    def test_cli_preserves_null_delimited_names_and_explicit_fallback(self):
        script = Path(__file__).with_name("agent_change_detection.py")
        for arguments, paths, expected in (
            ([], b"README.md\0apps/maple-agent/app/assets/a\nspace name\0", b"agent=true\n"),
            ([], b"README.md\0apps/maple-agent/docs/design notes.md\0", b"agent=false\n"),
            ([], b"", b"agent=false\n"),
            (["--all"], b"", b"agent=true\n"),
        ):
            result = subprocess.run([sys.executable, str(script), *arguments], input=paths,
                                    check=True, capture_output=True)
            self.assertEqual(result.stdout, expected)


if __name__ == "__main__":
    unittest.main()
