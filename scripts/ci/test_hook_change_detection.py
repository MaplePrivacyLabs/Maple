#!/usr/bin/env python3
"""Unit tests for the pre-commit hook component selector."""

import unittest

from hook_change_detection import OUTPUTS, classify_paths


def enabled(*names: str) -> dict[str, bool]:
    selected = set(names)
    return {name: name in selected for name in OUTPUTS}


class HookChangeDetectionTests(unittest.TestCase):
    def assert_selects(self, paths: list[str], *names: str) -> None:
        self.assertEqual(classify_paths(paths), enabled(*names))

    def test_documentation_and_hook_metadata_select_nothing(self) -> None:
        for path in (
            "README.md",
            "AGENTS.md",
            "docs/sdk-publishing.md",
            ".agents/skills/develop-maple/SKILL.md",
            ".githooks/pre-commit",
            ".githooks/lib/common.sh",
            "setup-hooks.sh",
            "justfile",
            "apps/maple-research/AGENTS.md",
            "apps/maple-research/.githooks/pre-commit",
            "apps/maple-auth/AGENTS.md",
            "apps/maple-auth/.githooks/pre-commit",
            "apps/maple-agent/AGENTS.md",
            "apps/maple-agent/.githooks/pre-commit",
            "sdk/README.md",
            "sdk/.githooks/pre-commit",
            "proxy/README.md",
            "proxy/.githooks/pre-commit",
            "services/opensecret/AGENTS.md",
            "services/opensecret/.githooks/pre-commit",
            "services/updates/.githooks/pre-commit",
            "services/opensecret/docs/nitro-deploy.md",
            "services/opensecret/pcrProd.json",
            "scripts/prepare-frontend-deps.sh",
        ):
            with self.subTest(path=path):
                self.assert_selects([path])

    def test_research_components(self) -> None:
        self.assert_selects(["apps/maple-research/frontend/src/App.tsx"], "research_frontend")
        self.assert_selects(["apps/maple-research/frontend/package.json"], "research_frontend")
        self.assert_selects(["apps/maple-research/frontend/src-tauri/src/lib.rs"], "research_rust")
        self.assert_selects(["apps/maple-research/frontend/src-tauri/Cargo.lock"], "research_rust")
        self.assert_selects(
            ["apps/maple-research/frontend/src/App.tsx", "apps/maple-research/frontend/src-tauri/src/lib.rs"],
            "research_frontend",
            "research_rust",
        )

    def test_auth_component_does_not_select_research(self) -> None:
        for path in ("apps/maple-auth/src/main.tsx", "apps/maple-auth/package.json",
                     "apps/maple-auth/bun.lock", "apps/maple-auth/vite.config.ts"):
            with self.subTest(path=path):
                self.assert_selects([path], "auth")
        self.assert_selects(
            ["apps/maple-auth/src/main.tsx", "apps/maple-research/frontend/src/App.tsx"],
            "auth", "research_frontend",
        )

    def test_agent_component(self) -> None:
        for path in ("apps/maple-agent/crates/maple-agent/src/agent.rs", "apps/maple-agent/Cargo.lock", "apps/maple-agent/flake.nix"):
            with self.subTest(path=path):
                self.assert_selects([path], "agent")

    def test_sdk_components(self) -> None:
        self.assert_selects(["sdk/rust/src/client.rs"], "sdk_rust")
        self.assert_selects(["sdk/rust/tests/crypto.rs"], "sdk_rust")
        self.assert_selects(["sdk/src/lib/main.tsx"], "sdk_ts")
        self.assert_selects(["sdk/src/lib/test/models.test.ts"], "sdk_ts")
        self.assert_selects(["sdk/package.json"], "sdk_ts")
        self.assert_selects(["sdk/flake.lock"], "sdk_rust", "sdk_ts")
        self.assert_selects(["sdk/test/integration/bootstrap.sql"])

    def test_proxy_opensecret_and_updates(self) -> None:
        self.assert_selects(["proxy/src/main.rs"], "proxy")
        self.assert_selects(["proxy/Cargo.toml"], "proxy")
        self.assert_selects(["proxy/deny.toml"])
        self.assert_selects(["services/opensecret/src/main.rs"], "opensecret")
        self.assert_selects(["services/opensecret/migrations/0001/up.sql"], "opensecret")
        self.assert_selects(["services/opensecret/nix/eif.nix"])
        self.assert_selects(["services/updates/src/index.ts"], "updates")

    def test_shared_crates_do_not_fan_out_to_consumers(self) -> None:
        self.assert_selects(["proxy/src/proxy.rs", "sdk/rust/src/client.rs"], "proxy", "sdk_rust")
        self.assert_selects(
            ["proxy/src/proxy.rs", "apps/maple-agent/Cargo.toml"],
            "proxy",
            "agent",
        )

    def test_repository_inputs(self) -> None:
        for path in (".github/workflows/proxy-rust.yml", "scripts/ci/change_detection.py", "flake.nix", "flake.lock"):
            with self.subTest(path=path):
                self.assert_selects([path], "repo")

    def test_unknown_root_selects_everything(self) -> None:
        self.assert_selects(["newservice/main.rs"], *OUTPUTS)


if __name__ == "__main__":
    unittest.main()
