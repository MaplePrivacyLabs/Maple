"""Regression tests for the two native apps' independent/shared input boundary."""

from pathlib import Path
import subprocess
import sys
import unittest

from agent_change_detection import (
    affects_agent, affects_pi_reference, classify_paths, classify_pi_reference,
)
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
            "apps/maple-agent/scripts/build-release.sh",
            "apps/maple-agent/scripts/package-release.sh",
            "apps/maple-agent/scripts/verify-release.sh",
            "apps/maple-agent/release_profile.rs",
            "apps/maple-agent/release-profiles.json",
            "apps/maple-agent/app/packaging/macos-entitlements.plist",
            "apps/maple-agent/app/macos/Info.plist",
            "apps/maple-agent/new-build-input",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))
                self.assertEqual(research_routes(path), frozenset())

    def test_shared_rust_runtime_inputs_select_both_desktop_apps(self):
        for path in ("proxy/Cargo.toml", "proxy/src/proxy.rs", "proxy/build.rs"):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))
                self.assertEqual(research_routes(path), DESKTOP_PLATFORMS)

    def test_research_typescript_and_independent_services_skip_agent(self):
        for path in (
            "apps/maple-auth/src/main.tsx",
            "apps/maple-auth/package.json",
            "apps/maple-auth/bun.lock",
            "apps/maple-auth/vite.config.ts",
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
            "sdk/rust/examples/api_usage.rs",
            "sdk/src/lib/index.ts", "sdk/package.json",
            "proxy/README.md", "proxy/tests/health.rs", "proxy/Cargo.lock",
            "proxy/Dockerfile", "proxy/flake.nix",
        ):
            with self.subTest(path=path):
                self.assertFalse(affects_agent(path))
                self.assertEqual(research_routes(path), frozenset())

    def test_rust_sdk_build_inputs_select_agent_and_desktop_research(self):
        # The Agent and the Research native shell build sdk/rust from the tree.
        for path in (
            "sdk/rust/Cargo.toml", "sdk/rust/src/client.rs", "sdk/rust/build.rs",
            "sdk/rust/assets/aws_nitro_root.der",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))
                self.assertTrue(research_routes(path) >= {"macos", "linux", "windows"})

    def test_selector_and_shared_tooling_changes_select_agent(self):
        for path in (
            "flake.nix", "flake.lock", ".github/workflows/agent-ci.yml",
            ".github/workflows/agent-desktop-build.yml",
            "scripts/ci/agent_change_detection.py", "scripts/ci/change_detection.py",
            "scripts/ci/verify-agent-rust-deps.py",
            "scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))

    def test_agent_packaging_workflow_stays_independent_of_research(self):
        self.assertTrue(affects_agent(".github/workflows/agent-desktop-build.yml"))
        self.assertEqual(research_routes(".github/workflows/agent-desktop-build.yml"), frozenset())
        for path in ("scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))
                self.assertEqual(research_routes(path), frozenset({"macos", "ios", "ios_onnx"}))

    def test_unknown_roots_and_invalid_paths_fail_safe(self):
        for path in ("new-build-config.toml", "", "/tmp/file", "../file",
                     "apps/maple-agent/../maple-research/frontend/src/main.tsx"):
            with self.subTest(path=path):
                self.assertTrue(affects_agent(path))

    def test_mixed_changes_and_empty_diff(self):
        self.assertFalse(classify_paths([]))
        self.assertFalse(classify_paths(["README.md", "sdk/rust/README.md"]))
        self.assertTrue(classify_paths(["README.md", "proxy/src/proxy.rs"]))

    def test_reference_inputs_always_select_reference_and_rust_checks(self):
        for path in (
            "apps/maple-agent/pi-conformance/pin.json",
            "apps/maple-agent/pi-conformance/flake.lock",
            "apps/maple-agent/pi-conformance/recorder/record.test.ts",
            "apps/maple-agent/pi-conformance/scenarios/text.json",
            "apps/maple-agent/pi-conformance/corpus/basic/events.jsonl",
            "apps/maple-agent/pi-conformance/fixtures/README.md",
            "apps/maple-agent/pi-conformance/README.md",
            "apps/maple-agent/pi-conformance/LICENSE",
            "apps/maple-agent/justfile",
            "flake.nix", "flake.lock", ".gitattributes",
            ".github/workflows/agent-ci.yml",
            ".github/workflows/agent-desktop-build.yml",
            "scripts/ci/agent_change_detection.py", "scripts/ci/change_detection.py",
            "scripts/ci/verify-agent-rust-deps.py",
            "scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py",
            "new-build-config.toml", "", "/tmp/file", "../file",
            "apps/maple-agent/../pi-conformance/pin.json",
        ):
            with self.subTest(path=path):
                self.assertTrue(affects_pi_reference(path))
                self.assertTrue(affects_agent(path))

    def test_reference_does_not_rebuild_for_rust_only_or_unrelated_changes(self):
        for path in (
            "apps/maple-agent/crates/pi-ai/src/lib.rs",
            "apps/maple-agent/crates/pi-conformance/tests/main.rs",
            "apps/maple-agent/app/src/main.rs",
            "apps/maple-agent/Cargo.toml", "apps/maple-agent/Cargo.lock",
            "apps/maple-agent/flake.nix", "apps/maple-agent/flake.lock",
            "apps/maple-agent/README.md", "apps/maple-agent/docs/pi.md",
            "proxy/src/proxy.rs", "sdk/rust/src/client.rs", "sdk/src/lib/index.ts",
            "README.md", "services/opensecret/src/main.rs",
            ".github/workflows/desktop-pr-build.yml",
        ):
            with self.subTest(path=path):
                self.assertFalse(affects_pi_reference(path))
        self.assertFalse(classify_pi_reference([]))
        self.assertFalse(classify_pi_reference(["README.md", "proxy/src/proxy.rs"]))
        self.assertTrue(classify_pi_reference([
            "README.md", "apps/maple-agent/pi-conformance/pin.json",
        ]))

    def test_cli_preserves_null_delimited_names_and_explicit_fallback(self):
        script = Path(__file__).with_name("agent_change_detection.py")
        for arguments, paths, expected in (
            ([], b"README.md\0apps/maple-agent/app/assets/a\nspace name\0", b"agent=true\npi_reference=false\n"),
            ([], b"README.md\0apps/maple-agent/docs/design notes.md\0", b"agent=false\npi_reference=false\n"),
            ([], b"", b"agent=false\npi_reference=false\n"),
            (["--all"], b"", b"agent=true\npi_reference=true\n"),
            ([], b"apps/maple-agent/pi-conformance/fixtures/a\nname\0",
             b"agent=true\npi_reference=true\n"),
            ([], b"../file\0", b"agent=true\npi_reference=true\n"),
        ):
            result = subprocess.run([sys.executable, str(script), *arguments], input=paths,
                                    check=True, capture_output=True)
            self.assertEqual(result.stdout, expected)


    def test_component_hook_scripts_do_not_select_agent(self) -> None:
        self.assertFalse(affects_agent("apps/maple-agent/.githooks/pre-commit"))


if __name__ == "__main__":
    unittest.main()
