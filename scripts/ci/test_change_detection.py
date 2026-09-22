#!/usr/bin/env python3
"""Unit tests for the monorepo CI path classifier."""

import unittest

from change_detection import OUTPUTS, classify_paths


def enabled(*names: str) -> dict[str, bool]:
    selected = set(names)
    return {name: name in selected for name in OUTPUTS}


class ChangeDetectionTests(unittest.TestCase):
    def assert_routes(self, paths: list[str], *names: str) -> None:
        self.assertEqual(classify_paths(paths), enabled(*names))

    def test_documentation_and_independent_components_skip_maple_app_builds(self) -> None:
        self.assert_routes(["README.md", "docs/monorepo-plan.md"])
        self.assert_routes(["apps/maple-research/README.md", "apps/maple-research/AGENTS.md"])
        self.assert_routes(["apps/maple-research/docs/development.md", "apps/maple-research/deny.toml"])
        self.assert_routes(["sdk/README.md", "sdk/test/client.test.ts"])
        self.assert_routes(["sdk/src/lib/test/models.test.ts"])
        self.assert_routes(["sdk/src/lib/test/integration/web.test.ts"])
        self.assert_routes(["services/updates/src/index.ts"])
        self.assert_routes([".githooks/pre-commit", "justfile", "apps/maple-research/zapstore.yaml"])
        self.assert_routes(["apps/maple-research/.githooks/pre-commit", "proxy/.githooks/pre-commit", "sdk/.githooks/pre-commit"])
        self.assert_routes([".github/workflows/codeql.yml"])

    def test_agent_component_does_not_trigger_research_packaging(self) -> None:
        for path in (
            "apps/maple-agent/app/src/main.rs",
            "apps/maple-agent/crates/maple-agent/src/agent.rs",
            "apps/maple-agent/Cargo.toml",
            "apps/maple-agent/Cargo.lock",
            "apps/maple-agent/flake.nix",
            "apps/maple-agent/flake.lock",
            "apps/maple-agent/README.md",
            ".github/workflows/agent-ci.yml",
            "scripts/ci/agent_change_detection.py",
        ):
            with self.subTest(path=path):
                self.assert_routes([path])
        # A mixed PR still selects the lanes needed by a shared crate change.
        self.assert_routes(
            ["apps/maple-agent/app/src/main.rs", "sdk/rust/src/client.rs"],
            "macos", "linux", "windows",
        )

    def test_auth_component_does_not_trigger_research_packaging(self) -> None:
        for path in (
            "apps/maple-auth/src/main.tsx", "apps/maple-auth/package.json",
            "apps/maple-auth/bun.lock", "apps/maple-auth/vite.config.ts",
            "apps/maple-auth/.githooks/pre-commit", ".github/workflows/auth-pages-ci.yml",
            "scripts/ci/auth-common.sh", "scripts/ci/auth-ci.sh", "scripts/ci/auth-web.sh",
        ):
            with self.subTest(path=path):
                self.assert_routes([path])
        self.assert_routes(
            ["apps/maple-auth/src/main.tsx", "apps/maple-research/frontend/src/app.tsx"],
            "frontend",
        )

    def test_in_tree_rust_runtime_inputs_mark_desktop_lanes(self) -> None:
        for path in (
            "proxy/Cargo.toml",
            "proxy/src/proxy.rs",
            "sdk/rust/Cargo.toml",
            "sdk/rust/src/client.rs",
            "sdk/rust/assets/aws_nitro_root.der",
            "proxy/build.rs",
            "sdk/rust/build.rs",
        ):
            with self.subTest(path=path):
                self.assert_routes([path], "macos", "linux", "windows")

    def test_standalone_rust_inputs_skip_maple_app_builds(self) -> None:
        for path in (
            "proxy/Cargo.lock",
            "proxy/Dockerfile",
            "proxy/tests/health_test.rs",
            "sdk/rust/Cargo.lock",
            "sdk/rust/tests/client.rs",
            "sdk/rust/examples/api_usage.rs",
        ):
            with self.subTest(path=path):
                self.assert_routes([path])

    def test_typescript_sdk_inputs_mark_only_the_frontend_lane(self) -> None:
        self.assert_routes(["sdk/src/lib/index.ts"], "frontend")
        self.assert_routes(["sdk/package.json"], "frontend")
        self.assert_routes(["sdk/bun.lock"], "frontend")

    def test_renderer_changes_only_mark_the_frontend_lane(self) -> None:
        self.assert_routes(["apps/maple-research/frontend/src/routes/index.tsx"], "frontend")
        self.assert_routes(["apps/maple-research/frontend/public/favicon.svg"], "frontend")

    def test_frontend_dependency_changes_affect_every_packaged_app(self) -> None:
        self.assert_routes(
            ["apps/maple-research/frontend/package.json"],
            "frontend",
            "macos",
            "linux",
            "windows",
            "ios",
            "android",
        )

    def test_shared_native_changes_affect_every_native_target(self) -> None:
        self.assert_routes(
            ["apps/maple-research/frontend/src-tauri/src/lib.rs"],
            "macos",
            "linux",
            "windows",
            "ios",
            "android",
        )

    def test_platform_owned_paths_only_mark_their_platform(self) -> None:
        self.assert_routes(["apps/maple-research/frontend/src-tauri/gen/android/build.gradle.kts"], "android")
        self.assert_routes(["apps/maple-research/frontend/src-tauri/icons/android/mipmap-hdpi/ic_launcher.png"], "android")
        self.assert_routes(["apps/maple-research/frontend/src-tauri/gen/apple/project.yml"], "ios")
        self.assert_routes(["apps/maple-research/frontend/src-tauri/resources/windows/install-dlls.nsh"], "windows")
        self.assert_routes(["apps/maple-research/frontend/src-tauri/tauri.macos.conf.json"], "macos")

    def test_ios_onnx_inputs_mark_the_cache_warmer(self) -> None:
        self.assert_routes(
            ["apps/maple-research/frontend/src-tauri/scripts/onnxruntime-pins.sh"],
            "macos",
            "linux",
            "windows",
            "ios",
            "android",
            "ios_onnx",
        )

    def test_ios_variants_only_select_ios_and_dev_workflow_keeps_its_own_trigger(self) -> None:
        for path in (
            "scripts/ci/ios-build-profile.py", "scripts/ci/ios-variant.sh",
            "apps/maple-research/frontend/src-tauri/tauri.ios-dev.conf.json",
        ):
            with self.subTest(path=path):
                self.assert_routes([path], "ios")
        # The new workflow always builds master; editing it alone does not
        # select an unrelated production app package or ONNX cache warmer.
        self.assert_routes([".github/workflows/ios-dev-testflight.yml"])

    def test_mixed_changes_union_their_routes(self) -> None:
        self.assert_routes(
            ["sdk/src/lib/index.ts", "apps/maple-research/frontend/src/routes/index.tsx", "apps/maple-research/frontend/src-tauri/gen/android/build.gradle.kts"],
            "frontend",
            "android",
        )

    def test_unclassified_root_paths_fail_safe(self) -> None:
        self.assert_routes(["new-build-system/input.toml"], *OUTPUTS)
        self.assert_routes(["apps/maple-research/new-build-input.toml"], *OUTPUTS)
        # Removed legacy paths in the relocation diff must still trigger full validation.
        self.assert_routes(["frontend/src/app.tsx"], *OUTPUTS)
        self.assert_routes(["../outside"], *OUTPUTS)


if __name__ == "__main__":
    unittest.main()
