#!/usr/bin/env python3
"""Unit tests for the pre-commit hook component selector."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
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
            "services/agent-updates/.githooks/pre-commit",
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

    def test_agent_publication_inputs_select_both_consumers_without_research(self) -> None:
        for path in ("apps/maple-agent/scripts/release-info.py", "apps/maple-agent/release-profiles.json"):
            with self.subTest(path=path):
                self.assert_selects([path], "agent", "agent_updates")

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
        self.assert_selects(["services/opensecret/entrypoint.sh"], "opensecret")
        self.assert_selects(["services/opensecret/tests/entrypoint_entropy_preflight.sh"], "opensecret")
        self.assert_selects(["services/opensecret/nix/eif.nix"])
        self.assert_selects(["services/updates/src/index.ts"], "updates")

    def test_download_tracking_selects_both_workers(self) -> None:
        self.assert_selects(["services/download-tracking/tracking.ts"], "updates", "agent_updates")

    def test_agent_updates_component_is_isolated_from_research(self) -> None:
        for path in (
            "services/agent-updates/src/worker.ts",
            "services/agent-updates/publisher/publisher.py",
            "services/agent-updates/publisher/test_publisher.py",
            "services/agent-updates/bun.lock",
            "services/agent-updates/wrangler.jsonc",
        ):
            with self.subTest(path=path):
                self.assert_selects([path], "agent_updates")

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


class AgentUpdatesHookTests(unittest.TestCase):
    def test_service_paths_run_the_bash_ci_entrypoint_in_the_root_shell(self) -> None:
        source = Path(__file__).resolve().parents[2]
        for use_nix in (True, False):
            with self.subTest(nix=use_nix), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / "repository with spaces"
                root.mkdir()
                for relative in (
                    ".githooks/pre-commit", ".githooks/lib/common.sh",
                    "scripts/ci/hook_change_detection.py",
                ):
                    destination = root / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source / relative, destination)
                script = root / "scripts/ci/agent-updates.sh"
                script.write_text(
                    '#!/usr/bin/env bash\nset -euo pipefail\n'
                    'selected=("$MAPLE_HOOK_SELECTED")\n'
                    'printf "%s\\n%s\\n%s\\n" "${selected[0]}" "$PWD" '
                    '"$MAPLE_HOOK_IN_NIX" > hook-result\n'
                )
                tools = root / "tools"
                tools.mkdir()
                nix = tools / "nix"
                nix.write_text(
                    '#!/bin/sh\n'
                    '[ "$1" = develop ] && [ "$2" = --no-update-lock-file ] '
                    '&& [ "$3" = ".#ci" ] && [ "$4" = -c ] || exit 70\n'
                    'shift 4\nexec "$@"\n'
                )
                nix.chmod(0o755)
                changed = root / "services/agent-updates/src/worker.ts"
                changed.parent.mkdir(parents=True)
                changed.write_text("export default {};\n")
                environment = {key: value for key, value in os.environ.items()
                               if not key.startswith(("GIT_", "MAPLE_HOOK_"))}
                environment["PATH"] = f"{tools}{os.pathsep}{environment['PATH']}"
                environment["MAPLE_HOOK_NO_NIX"] = "0" if use_nix else "1"
                subprocess.run(["git", "init", "-q"], cwd=root, env=environment, check=True)
                subprocess.run(["git", "add", str(changed)], cwd=root, env=environment, check=True)
                subprocess.run(["sh", ".githooks/pre-commit"], cwd=root, env=environment,
                               check=True, capture_output=True, text=True)
                selected, directory, in_nix = (root / "hook-result").read_text().splitlines()
                self.assertEqual(selected.strip(), "agent_updates")
                self.assertEqual(Path(directory).resolve(), root.resolve())
                self.assertEqual(in_nix, "1" if use_nix else "0")


if __name__ == "__main__":
    unittest.main()
