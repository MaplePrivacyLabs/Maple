"""Lane selection: the table, the planner's events and the pre-commit hook."""

import contextlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import lanes
import plan
from lanes import DISPATCH, NIGHTLY, PR, PUSH, QUEUE
from opensecret_change_detection import OUTPUTS


ACTIVE = {lane.id for lane in lanes.LANES}
AGENT_PR = {"agent-lint", "agent-test", "agent-macos", "agent-windows"}
AGENT_PUSH = AGENT_PR | {"agent-full-linux", "agent-full-macos"}
AGENT_PACKAGE = {"agent-package-linux", "agent-package-macos"}
# What opensecret_change_detection.py's "all checks" selects in ci.yml (deps-opensecret follows its manifests).
OPENSECRET_ALL = {"os-rust", "os-sdk-e2e", "os-flake-check", "os-helpers", "os-eif"}
DEPS = {"deps-research", "deps-sdk", "deps-proxy", "deps-opensecret"}
SHA_A, SHA_B = "a" * 40, "b" * 40
HOOK = list(dict.fromkeys(lane.hook for lane in lanes.LANES if lane.hook))


def active(event):
    return {lane.id for lane in plan.active(event)}


def selected(paths, event=PR, labels=()):
    return set(plan.select(event, paths, labels))


def agent_lanes(lanes_):
    return {lane for lane in lanes_ if lane.startswith("agent-") and lane != "agent-updates"}


def agent(paths, event=PR):
    """Does any Agent check run?"""
    return bool(agent_lanes(selected(paths, event)))


class AgentSelectionTests(unittest.TestCase):
    """Which changes run Agent checks."""

    def test_agent_runtime_build_and_asset_inputs_select_agent(self):
        for path in (
            "apps/maple-agent/app/src/main.rs", "apps/maple-agent/app/assets/fonts/Maple.ttf",
            "apps/maple-agent/crates/maple-agent/src/agent.rs", "apps/maple-agent/Cargo.toml",
            "apps/maple-agent/Cargo.lock", "apps/maple-agent/flake.nix", "apps/maple-agent/flake.lock",
            "apps/maple-agent/rust-toolchain.toml", "apps/maple-agent/justfile",
            "apps/maple-agent/scripts/macos-debug-app.sh", "apps/maple-agent/scripts/build-release.sh",
            "apps/maple-agent/scripts/package-release.sh", "apps/maple-agent/scripts/verify-release.sh",
            "apps/maple-agent/release_profile.rs", "apps/maple-agent/release-profiles.json",
            "apps/maple-agent/app/packaging/macos-entitlements.plist", "apps/maple-agent/app/macos/Info.plist",
            "apps/maple-agent/new-build-input",
            # Compiled into the binary with include_str!, so nested notes are not inert.
            "apps/maple-agent/crates/maple-agent/resources/skills/advisor/SKILL.md",
        ):
            with self.subTest(path=path):
                # Dependency and packaging inputs add their own Agent lanes (AgentLaneTests).
                self.assertLessEqual(AGENT_PR, selected([path]))
                self.assertTrue(all(lane.startswith("agent-") for lane in selected([path]) - {"agent-updates"}))

    def test_rust_sdk_and_proxy_runtime_inputs_select_agent(self):
        for path in ("proxy/Cargo.toml", "proxy/src/proxy.rs", "proxy/build.rs", "sdk/rust/Cargo.toml",
                     "sdk/rust/src/client.rs", "sdk/rust/build.rs", "sdk/rust/assets/aws_nitro_root.der"):
            with self.subTest(path=path):
                self.assertEqual(agent_lanes(selected([path])), AGENT_PR)
                self.assertEqual(agent_lanes(selected([path], PUSH)), AGENT_PUSH)

    def test_research_typescript_and_independent_services_skip_agent(self):
        for path in (
            "apps/maple-auth/src/main.tsx", "apps/maple-auth/package.json", "apps/maple-auth/bun.lock",
            "apps/maple-auth/vite.config.ts", "apps/maple-research/frontend/src/main.tsx",
            "apps/maple-research/frontend/src-tauri/src/lib.rs", "sdk/src/lib/index.ts", "sdk/package.json",
            "sdk/flake.nix", "services/updates/src/index.ts", "services/opensecret/src/main.rs",
            ".github/workflows/desktop-build.yml", "scripts/ci/desktop-pr.sh",
        ):
            with self.subTest(path=path):
                self.assertTrue(plan.classified(path))
                self.assertFalse(agent([path]))

    def test_docs_and_standalone_dependency_inputs_skip_agent(self):
        for path in (
            "apps/maple-agent/README.md", "apps/maple-agent/AGENTS.md", "apps/maple-agent/CLAUDE.md",
            "apps/maple-agent/LICENSE", "apps/maple-agent/docs/development.md", "README.md",
            "sdk/rust/README.md", "sdk/rust/tests/client.rs", "sdk/rust/Cargo.lock",
            "sdk/rust/examples/api_usage.rs", "sdk/src/lib/index.ts", "sdk/package.json",
            "proxy/README.md", "proxy/tests/health.rs", "proxy/Cargo.lock", "proxy/Dockerfile", "proxy/flake.nix",
            "apps/maple-agent/.githooks/pre-commit",
        ):
            with self.subTest(path=path):
                self.assertFalse(agent([path]))
                self.assertFalse(agent([path], PUSH))

    def test_shared_ci_tooling_selects_the_agent_lanes_that_use_it(self):
        self.assertEqual(selected(["scripts/ci/verify-agent-rust-deps.py"]), {"agent-lint", "research-rust", "repo"})
        for path in ("scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), {"agent-macos", "research-ios", "repo"})
        self.assertEqual(selected(["scripts/ci/agent_windows.py"]),
                         {"agent-windows", "agent-windows-updater", "repo"})

    def test_intentional_differences_select_only_the_repo_lane(self):
        for path in (
            # The Research selector, which only the signed Research builds use.
            "scripts/ci/change_detection.py",
            # Signing scripts: no ci.yml Agent lane signs anything (agent-desktop-build.yml does).
            "scripts/ci/windows-artifact-sign.ps1", "scripts/ci/install-windows-artifact-signing.ps1",
        ):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), {"repo"})
                self.assertEqual(selected([path], PUSH), {"repo"})
        for path in ("flake.nix", "flake.lock"):  # the Agent flake has no root inputs
            with self.subTest(path=path):
                self.assertFalse(agent([path]) or agent([path], PUSH))

    def test_unknown_roots_and_malformed_paths_select_every_lane(self):
        for path in ("new-build-config.toml", "newroot/x", "", "/tmp/file", "../file", "docs/../flake.nix",
                     "apps/maple-agent/../maple-research/frontend/src/main.tsx"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), active(PR))
                self.assertEqual(selected([path], PUSH), active(PUSH))

    def test_mixed_changes_and_empty_diff(self):
        self.assertEqual(selected([]), set())
        self.assertEqual(selected(["README.md", "proxy/README.md"]), set())
        self.assertEqual(selected(["README.md", "proxy/src/proxy.rs"]), AGENT_PR | {"proxy", "research-rust"})

    def test_explain_reads_null_delimited_names(self):
        for data, expected in ((b"README.md\0apps/maple-agent/app/assets/a\nspace name\0", "agent-lint"),
                               (b"README.md\0apps/maple-agent/docs/design notes.md\0", "no ci.yml lane"),
                               (b"", "no ci.yml lane")):
            result = subprocess.run([sys.executable, str(Path(plan.__file__)), "explain"], input=data,
                                    check=True, capture_output=True)
            self.assertIn(expected, result.stdout.decode())


class LaneTableTests(unittest.TestCase):
    def test_typescript_sdk_never_selects_a_client_lane(self):
        for path in ("sdk/src/index.ts", "sdk/package.json", "sdk/bun.lock"):
            for event in (PR, PUSH):
                with self.subTest(path=path, event=event):
                    # The SDK-to-backend suite is the only other consumer.
                    self.assertEqual(selected([path], event), {"sdk-ts", "os-sdk-e2e"})

    def test_release_metadata_and_publisher_inputs_select_the_repo_lane(self):
        for path in ("apps/maple-research/frontend/package.json", "apps/maple-research/frontend/src-tauri/Cargo.toml",
                     "apps/maple-research/frontend/src-tauri/tauri.conf.json", "proxy/justfile", ".github/CODEOWNERS",
                     ".github/actionlint.yaml", "services/agent-updates/publisher/publisher.py"):
            with self.subTest(path=path):
                self.assertIn("repo", selected([path]))

    def test_rust_sdk_tests_and_proxy_container_select_no_agent_lane(self):
        self.assertEqual(selected(["sdk/rust/tests/x.rs"], PUSH), {"sdk-rust", "os-sdk-e2e"})
        self.assertEqual(agent_lanes(selected(["proxy/src/main.rs"])), AGENT_PR)
        self.assertFalse(agent([".dockerignore", "proxy/Dockerfile"], PUSH))

    def test_windows_clippy_runs_on_every_agent_change(self):
        for path in ("apps/maple-agent/app/src/ui/x.rs", "apps/maple-agent/app/src/host/windows_spawn.rs",
                     "proxy/src/proxy.rs"):
            for event in (PR, QUEUE, PUSH):
                with self.subTest(path=path, event=event):
                    self.assertIn("agent-windows", selected([path], event))
        self.assertIn("agent-windows", selected(["docs/x.md"], labels=["ci:windows"]))
        # Every PR lane but the EIF candidate, which only its own inputs select.
        self.assertEqual(selected(["docs/x.md"], labels=["ci:full"]), active(PR) - {"os-eif"})
        self.assertEqual(selected(["docs/x.md"], labels=["unrelated"]), set())

    def test_windows_updater_tests_run_for_updater_paths_by_label_and_nightly(self):
        for path in ("apps/maple-agent/crates/maple-agent-updater/src/installer.rs",
                     "apps/maple-agent/app/src/windows_bootstrap.rs",
                     ".github/actions/setup-agent-windows/action.yml", "scripts/ci/agent_windows.py"):
            for event in (PR, QUEUE, PUSH):  # a master push saves its cache
                with self.subTest(path=path, event=event):
                    self.assertIn("agent-windows-updater", selected([path], event))
        # Its dependency versions, features and profiles, and CI edits, run it too.
        for path in ("apps/maple-agent/Cargo.lock", "apps/maple-agent/Cargo.toml", ".github/workflows/ci.yml"):
            with self.subTest(path=path):
                self.assertIn("agent-windows-updater", selected([path]))
        for path in ("apps/maple-agent/app/src/main.rs", "apps/maple-agent/crates/maple-agent/Cargo.toml",
                     "apps/maple-agent/app/src/update.rs", "apps/maple-agent/scripts/windows-release.py"):
            with self.subTest(path=path):
                self.assertNotIn("agent-windows-updater", selected([path]))
        for label in ("ci:windows", "ci:full"):
            self.assertIn("agent-windows-updater", selected(["docs/x.md"], labels=[label]))
        self.assertIn("agent-windows-updater", active(NIGHTLY))

    def test_inert_paths_select_nothing(self):
        for path in ("apps/maple-agent/docs/x.md", "docs/x.md", ".agents/skills/x/SKILL.md"):
            with self.subTest(path=path):
                self.assertTrue(plan.inert(path))
                self.assertEqual(selected([path], PUSH), set())
        evidence = ".agents/skills/release-maple/scripts/ci_evidence.py"
        self.assertFalse(plan.inert(evidence))
        self.assertEqual(selected([evidence]), {"repo"})

    def test_the_opensecret_selector_also_routes_inert_paths(self):
        # Its routing is verbatim: these notes still select what it selects for them.
        self.assertEqual(selected(["sdk/rust/README.md"]), {"os-sdk-e2e"})
        self.assertEqual(selected(["services/agent-updates/README.md"]), set())
        # A new service root is unknown to it: every OpenSecret check, and every lane if not inert.
        self.assertEqual(selected(["services/new-service/README.md"]), OPENSECRET_ALL)
        self.assertEqual(selected(["services/new-service/src/main.ts"]), active(PR))
        self.assertEqual(selected(["services/opensecret/README.md", "services/opensecret/docs/x.md"]), set())

    def test_ci_infrastructure_selects_every_active_gate_lane(self):
        for path in (".github/workflows/ci.yml", ".github/actions/setup-nix/action.yml",
                     "scripts/ci/plan.py", "scripts/ci/lanes.py"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), {lane.id for lane in lanes.LANES if lane.gate})

    def test_every_tracked_file_is_classified_and_ci_yml_matches(self):
        self.assertEqual(plan.lint_problems(plan.tracked_files()), [])
        self.assertIn("unclassified: newroot/x", plan.lint_problems(["newroot/x"]))
        # Without the plan step's variable, the matrix would ignore the override.
        with tempfile.TemporaryDirectory() as tmp:
            workflow = Path(tmp) / "ci.yml"
            workflow.write_text(plan.CI_WORKFLOW.read_text().replace(
                "CI_WINDOWS_RUNNER: ${{ vars.CI_WINDOWS_RUNNER }}\n", ""))
            with mock.patch.object(plan, "CI_WORKFLOW", workflow):
                self.assertIn("ci.yml plan: CI_WINDOWS_RUNNER is not passed to plan.py",
                              plan.lint_problems(plan.tracked_files()))


class HookTests(unittest.TestCase):
    """The pre-commit hook's components: a path selects those of the lanes that own it."""

    def assert_selects(self, paths, *names):
        self.assertEqual(plan.hook_components(paths), [n for n in HOOK if n in names])

    def test_every_component_has_a_branch_in_the_hook(self):
        hook = (Path(plan.ROOT) / ".githooks/pre-commit").read_text()
        self.assertEqual(set(re.findall(r"hook_selected (\w+)", hook)), set(HOOK))

    def test_documentation_and_component_hooks_select_nothing(self):
        for path in (
            "README.md", "AGENTS.md", "docs/sdk-publishing.md", ".agents/skills/develop-maple/SKILL.md",
            "apps/maple-research/AGENTS.md", "apps/maple-research/.githooks/pre-commit",
            "apps/maple-auth/AGENTS.md", "apps/maple-auth/.githooks/pre-commit", "apps/maple-agent/AGENTS.md",
            "apps/maple-agent/.githooks/pre-commit", "sdk/README.md", "sdk/.githooks/pre-commit", "proxy/README.md",
            "proxy/.githooks/pre-commit", "services/opensecret/AGENTS.md", "services/opensecret/.githooks/pre-commit",
            "services/updates/.githooks/pre-commit", "services/agent-updates/.githooks/pre-commit",
            "services/opensecret/docs/nitro-deploy.md", "services/opensecret/pcrProd.json",
            "sdk/test/integration/bootstrap.sql", "apps/maple-research/deny.toml",
        ):
            with self.subTest(path=path):
                self.assert_selects([path])

    def test_component_inputs(self):
        for paths, names in (
            (["apps/maple-research/frontend/src/App.tsx"], ("research_frontend",)),
            (["apps/maple-research/frontend/src-tauri/src/lib.rs"], ("research_rust",)),
            (["apps/maple-research/frontend/src-tauri/Cargo.lock"], ("research_rust",)),
            (["apps/maple-research/frontend/src/App.tsx", "apps/maple-research/frontend/src-tauri/src/lib.rs"],
             ("research_frontend", "research_rust")),
            (["apps/maple-auth/src/main.tsx"], ("auth",)), (["apps/maple-auth/bun.lock"], ("auth",)),
            (["apps/maple-auth/src/main.tsx", "apps/maple-research/frontend/src/App.tsx"],
             ("auth", "research_frontend")),
            (["apps/maple-agent/crates/maple-agent/src/agent.rs"], ("agent",)),
            (["apps/maple-agent/Cargo.lock"], ("agent",)), (["apps/maple-agent/flake.nix"], ("agent",)),
            (["apps/maple-agent/scripts/release-info.py"], ("agent", "agent_updates")),
            (["apps/maple-agent/release-profiles.json"], ("agent", "agent_updates")),
            (["sdk/rust/src/client.rs"], ("sdk_rust",)), (["sdk/rust/tests/crypto.rs"], ("sdk_rust",)),
            (["sdk/src/lib/main.tsx"], ("sdk_ts",)), (["sdk/src/lib/test/models.test.ts"], ("sdk_ts",)),
            (["sdk/package.json"], ("sdk_ts",)), (["sdk/flake.lock"], ("sdk_ts", "sdk_rust")),
            (["proxy/src/main.rs"], ("proxy",)),
            (["services/opensecret/src/main.rs"], ("opensecret",)),
            (["services/opensecret/migrations/0001/up.sql"], ("opensecret",)),
            (["services/opensecret/entrypoint.sh"], ("opensecret",)),
            (["services/opensecret/tests/entrypoint_entropy_preflight.sh"], ("opensecret",)),
            (["services/updates/src/index.ts"], ("updates",)),
            (["services/agent-updates/src/worker.ts"], ("agent_updates",)),
            (["services/agent-updates/bun.lock"], ("agent_updates",)),
            (["services/agent-updates/wrangler.jsonc"], ("agent_updates",)),
            (["services/agent-updates/publisher/publisher.py"], ("agent_updates", "repo")),
            (["services/download-tracking/tracking.ts"], ("updates", "agent_updates")),
            ([".github/workflows/desktop-build.yml"], ("repo",)), (["scripts/ci/change_detection.py"], ("repo",)),
            ([".github/workflows/ci.yml"], ("repo",)), (["scripts/ci/lanes.py"], ("repo",)),
        ):
            with self.subTest(paths=paths):
                self.assert_selects(paths, *names)

    def test_shared_crates_do_not_fan_out_to_their_consumers(self):
        self.assert_selects(["proxy/src/proxy.rs", "sdk/rust/src/client.rs"], "proxy", "sdk_rust")
        self.assert_selects(["proxy/src/proxy.rs", "apps/maple-agent/Cargo.toml"], "proxy", "agent")
        # The OpenSecret selector's routes outside its own directory are CI fan-out too.
        self.assert_selects(["scripts/ci/opensecret_change_detection.py"], "repo")

    def test_shared_inputs_select_every_lane_that_owns_them(self):
        # A path that several lanes own selects each of their components.
        for paths, names in (
            ([".githooks/pre-commit"], ("repo",)), (["setup-hooks.sh"], ("repo",)), (["justfile"], ("repo",)),
            (["scripts/prepare-frontend-deps.sh"], ("research_frontend",)),
            (["proxy/Cargo.toml"], ("proxy", "repo")), (["proxy/deny.toml"], ("proxy",)),
            (["apps/maple-research/frontend/package.json"], ("research_frontend", "repo")),
            (["flake.nix"], ("repo", "research_frontend", "research_rust", "auth", "updates", "agent_updates")),
            (["services/opensecret/nix/eif.nix"], ("opensecret",)),
        ):
            with self.subTest(paths=paths):
                self.assert_selects(paths, *names)

    def test_unknown_roots_select_every_component(self):
        for path in ("newservice/main.rs", "../x"):
            with self.subTest(path=path):
                self.assert_selects([path], *HOOK)

    def test_the_hook_runs_the_selected_component_in_its_flake(self):
        source = Path(plan.ROOT)
        for use_nix in (True, False):
            with self.subTest(nix=use_nix), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / "repository with spaces"
                for relative in (".githooks/pre-commit", ".githooks/lib/common.sh", "scripts/ci/plan.py",
                                 "scripts/ci/lanes.py", "scripts/ci/opensecret_change_detection.py"):
                    (root / relative).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(source / relative, root / relative)
                (root / "scripts/ci/agent-updates.sh").write_text(
                    '#!/usr/bin/env bash\nset -euo pipefail\nselected=("$MAPLE_HOOK_SELECTED")\n'
                    'printf "%s\\n%s\\n%s\\n" "${selected[0]}" "$PWD" "$MAPLE_HOOK_IN_NIX" > hook-result\n')
                nix = root / "tools/nix"
                nix.parent.mkdir()
                nix.write_text('#!/bin/sh\n[ "$1" = develop ] && [ "$2" = --no-update-lock-file ] '
                               '&& [ "$3" = ".#ci" ] && [ "$4" = -c ] || exit 70\nshift 4\nexec "$@"\n')
                nix.chmod(0o755)
                changed = root / "services/agent-updates/src/worker.ts"
                changed.parent.mkdir(parents=True)
                changed.write_text("export default {};\n")
                env = {key: value for key, value in os.environ.items() if not key.startswith(("GIT_", "MAPLE_HOOK_"))}
                env.update(PATH=f"{nix.parent}{os.pathsep}{env['PATH']}", MAPLE_HOOK_NO_NIX="0" if use_nix else "1")
                for command in (["git", "init", "-q"], ["git", "add", str(changed)], ["sh", ".githooks/pre-commit"]):
                    subprocess.run(command, cwd=root, env=env, check=True, capture_output=True)
                component, directory, in_nix = (root / "hook-result").read_text().splitlines()
                self.assertEqual(component.strip(), "agent_updates")
                self.assertEqual(Path(directory).resolve(), root.resolve())
                self.assertEqual(in_nix, "1" if use_nix else "0")


class AgentLaneTests(unittest.TestCase):
    """Agent supply-chain, full-suite, benchmark and packaging lanes."""

    def test_dependency_inputs_select_supply_chain_and_the_provider_suite(self):
        both = {"agent-supply-chain", "agent-provider"}
        for path, lanes_ in (("apps/maple-agent/Cargo.lock", both),
                             ("apps/maple-agent/dependencies/sqlcipher/provenance.json", both),
                             ("apps/maple-agent/scripts/test_rusqlite_provider.py", {"agent-provider"}),
                             ("apps/maple-agent/crates/maple-agent/Cargo.toml", {"agent-supply-chain"}),
                             ("apps/maple-agent/app/assets/dict/LICENSE", set()),
                             ("apps/maple-agent/crates/maple-agent/src/store/keys/SOURCES.md", set()),
                             ("apps/maple-agent/app/src/main.rs", set())):
            with self.subTest(path=path):
                self.assertEqual(selected([path]) & both, lanes_)
                self.assertEqual(selected([path], PUSH) & both, lanes_)
        self.assertIn("agent-supply-chain", active(NIGHTLY))
        self.assertNotIn("agent-provider", active(NIGHTLY))  # nightly.yml runs it on every OS

    def test_packaging_rehearsals_run_only_by_label_and_nightly(self):
        # A PR never waits on a packaging build: master's agent-desktop-build.yml
        # builds the real packages, and ci:package or the nightly run rehearses them.
        packages = AGENT_PACKAGE | {"agent-package-windows"}
        for path in ("apps/maple-agent/scripts/build-release.sh", "apps/maple-agent/flake.lock",
                     "apps/maple-agent/scripts/windows-installer.nsi", "apps/maple-agent/scripts/windows-release.py",
                     "apps/maple-agent/app/packaging/macos-entitlements.plist", "apps/maple-agent/release-profiles.json",
                     ".github/workflows/agent-desktop-build.yml", "scripts/ci/apple-toolchain.json",
                     "scripts/ci/agent_windows.py"):
            for event in (PR, QUEUE, PUSH):
                with self.subTest(path=path, event=event):
                    self.assertFalse(packages & selected([path], event))
        for label in ("ci:package", "ci:full"):
            self.assertEqual(agent_lanes(selected(["docs/x.md"], labels=[label])) & packages, packages)
        self.assertLessEqual(packages, active(NIGHTLY))

    def test_full_suites_run_on_master_nightly_and_by_label(self):
        full = {"agent-full-linux", "agent-full-macos", "agent-full-windows"}
        self.assertFalse(full & selected(["apps/maple-agent/app/src/main.rs"]))
        # Windows runs nightly on its free runner rather than on every master push.
        self.assertEqual(full & selected(["apps/maple-agent/app/src/main.rs"], PUSH), full - {"agent-full-windows"})
        self.assertLessEqual(full, selected(["docs/x.md"], labels=["ci:full"]))
        self.assertIn("agent-full-windows", selected(["docs/x.md"], labels=["ci:windows"]))
        self.assertLessEqual(full | {"agent-windows"}, active(NIGHTLY))
        self.assertFalse({"agent-provider"} & active(NIGHTLY))  # nightly.yml runs it on three OSes
        # Nightly refreshes the rehearsals' own caches.
        self.assertLessEqual(AGENT_PACKAGE | {"agent-package-windows"}, active(NIGHTLY))
        self.assertFalse(full & selected([".github/workflows/ci.yml"]))  # CI edits prove the gate lanes only

    def test_agent_desktop_selector_covers_the_signed_builds_inputs(self):
        for path in ("apps/maple-agent/app/src/main.rs", "sdk/rust/src/client.rs", "proxy/src/proxy.rs",
                     ".github/workflows/agent-desktop-build.yml", ".github/actions/setup-agent-windows/action.yml",
                     "scripts/ci/select-xcode.py", "scripts/ci/agent_windows.py",
                     "scripts/ci/windows-artifact-sign.ps1"):
            with self.subTest(path=path):
                self.assertTrue(plan.matches(path, lanes.AGENT_DESKTOP))
        for path in ("flake.lock", "sdk/src/index.ts", "sdk/rust/tests/x.rs", "proxy/Dockerfile",
                     ".github/workflows/ci.yml", "scripts/ci/plan.py"):
            with self.subTest(path=path):
                self.assertFalse(plan.matches(path, lanes.AGENT_DESKTOP))


class ClientLaneTests(unittest.TestCase):
    """The SDK, proxy, Research and service lanes."""

    def test_component_inputs_select_their_lanes(self):
        for path, pr, push in (
            ("sdk/rust/tests/x.rs", {"sdk-rust", "os-sdk-e2e"}, {"sdk-rust", "os-sdk-e2e"}),
            ("proxy/tests/health.rs", {"proxy"}, {"proxy"}),
            ("proxy/Dockerfile", {"proxy-container"}, {"proxy-container", "proxy-container-arm64"}),
            ("proxy/Cargo.lock", {"proxy", "deps-proxy"}, {"proxy", "deps-proxy", "proxy-container",
                                                           "proxy-container-arm64"}),
            ("sdk/deny.toml", {"deps-sdk"}, {"deps-sdk"}),
            ("apps/maple-research/deny.toml", {"deps-research"}, {"deps-research"}),
            ("apps/maple-auth/src/main.tsx", {"auth"}, set()),
            ("services/download-tracking/tracking.ts", {"updates", "agent-updates"}, {"updates", "agent-updates"}),
            ("services/updates/src/index.ts", {"updates"}, {"updates"}),
            ("services/agent-updates/src/index.ts", {"agent-updates"}, {"agent-updates"}),
            # Master runs these in frontend-tests.yml, web-build.yml and rust-tests.yml.
            ("apps/maple-research/frontend/src/App.tsx", {"research-web"}, set()),
            ("apps/maple-research/frontend/src-tauri/src/lib.rs", {"research-rust"}, set()),
        ):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), pr)
                self.assertEqual(selected([path], PUSH), push)
        self.assertLessEqual({"deps-research", "deps-sdk", "deps-proxy"}, active(NIGHTLY))  # new advisories

    def test_dependency_audits_follow_only_their_manifests_labels_and_nightly(self):
        # A new advisory is drift, not a CI change: no CI edit selects a deps lane.
        for path in (".github/workflows/ci.yml", ".github/actions/setup-nix/action.yml", "scripts/ci/plan.py",
                     "scripts/ci/lanes.py", "scripts/ci/opensecret_change_detection.py", ".gitmodules",
                     ".github/workflows/opensecret-eif.yml", "scripts/ci/check_opensecret_eif.sh",
                     "services/opensecret/flake.lock", "services/opensecret/rust-toolchain.toml",
                     "services/opensecret/src/main.rs", "apps/maple-agent/flake.lock"):
            for event in (PR, QUEUE, PUSH):
                with self.subTest(path=path, event=event):
                    self.assertFalse(selected([path], event) & DEPS)
        for lane, paths in (
                ("deps-opensecret", ("services/opensecret/Cargo.toml", "services/opensecret/Cargo.lock",
                                     "services/opensecret/deny.toml")),
                ("deps-research", ("apps/maple-research/frontend/src-tauri/Cargo.lock",
                                   "apps/maple-research/deny.toml", "sdk/rust/Cargo.toml")),
                ("deps-sdk", ("sdk/rust/Cargo.lock", "sdk/deny.toml")),
                ("deps-proxy", ("proxy/Cargo.lock", "proxy/deny.toml"))):
            for path in paths:
                with self.subTest(lane=lane, path=path):
                    self.assertIn(lane, selected([path]))
                    self.assertIn(lane, selected([path], PUSH))
        self.assertLessEqual(DEPS, selected(["docs/x.md"], labels=["ci:full"]))
        self.assertLessEqual(DEPS, active(NIGHTLY))
        self.assertFalse(selected(["docs/x.md"], labels=["ci:package", "ci:eif", "ci:windows"]) & DEPS)

    def test_research_platform_builds_run_on_native_inputs_or_labels(self):
        desktop = {"research-desktop-macos", "research-desktop-linux", "research-desktop-windows",
                   "research-dev-macos"}
        tauri = "apps/maple-research/frontend/src-tauri/"
        # Mobile builds still follow their native inputs; desktop bundles never block
        # a PR (desktop-build.yml builds master; ci:package rehearses them).
        for path, lanes_ in ((tauri + "gen/apple/project.yml", {"research-ios"}),
                             (tauri + "gen/android/app/build.gradle.kts", {"research-android"}),
                             (tauri + "dmg/background.tiff", {"research-rust"}),
                             (tauri + "tauri.conf.json", {"research-rust", "repo"}),
                             (tauri + "resources/windows/README.md", {"research-rust"}),
                             ("scripts/ci/desktop-windows-pr.sh", {"repo"}),
                             (tauri + "scripts/onnxruntime-pins.sh", {"research-ios", "research-rust"}),
                             ("scripts/ci/verify-release-artifacts.sh", {"repo"}),
                             ("scripts/ci/research-dev-desktop.sh", {"repo"})):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), lanes_)
                self.assertFalse({lane for lane in selected([path], PUSH) if lane.startswith("research-")})
        self.assertEqual(selected(["docs/x.md"], labels=["ci:ios"]), {"research-ios"})
        self.assertEqual(selected(["docs/x.md"], labels=["ci:research-dev-macos"]), {"research-dev-macos"})
        # ci:windows is the Agent's; Research Windows packaging comes with ci:package.
        self.assertEqual(selected(["docs/x.md"], labels=["ci:windows"]),
                         {"agent-windows", "agent-windows-updater", "agent-full-windows"})
        self.assertLessEqual(desktop, selected(["docs/x.md"], labels=["ci:package"]))
        self.assertFalse({lane for lane in selected([".github/workflows/ci.yml"]) if lane.startswith(
            ("research-ios", "research-android", "research-desktop", "research-dev"))})


class OpenSecretLaneTests(unittest.TestCase):
    """The OpenSecret lanes and the PR EIF candidate."""

    def test_the_opensecret_selector_routes_each_lane(self):
        backend = "services/opensecret/"
        for path, pr, push in (
            (backend + "src/main.rs", {"os-rust", "os-sdk-e2e", "os-flake-check", "os-eif"},
             {"os-rust", "os-sdk-e2e", "os-flake-check"}),
            (backend + "migrations/2026/up.sql", {"os-rust", "os-sdk-e2e"}, {"os-rust", "os-sdk-e2e"}),
            (backend + "deny.toml", {"deps-opensecret"}, {"deps-opensecret"}),
            (backend + "nitro-toolkit/logging/Dockerfile", {"os-helpers"}, {"os-helpers"}),
            (backend + "nix/kernel-upstream.nix", {"os-flake-check", "os-eif"}, {"os-flake-check"}),
            (backend + "scripts/pcr_compatibility.py", {"os-pcr"}, {"os-pcr"}),
            # An approval edit is compared by opensecret-eif.yml, never built as a candidate.
            (backend + "pcrDev.json", {"os-pcr"}, {"os-pcr"}),
            (backend + "docs/nitro-deploy.md", set(), set()),
            ("sdk/test/integration/bootstrap.sql", {"os-sdk-e2e"}, {"os-sdk-e2e"}),
            ("scripts/ci/build_opensecret_eif.sh", {"os-eif", "repo"}, {"repo"}),
            (".gitmodules", OPENSECRET_ALL, OPENSECRET_ALL - {"os-eif"}),
            (backend + "Cargo.lock", OPENSECRET_ALL | {"deps-opensecret"}, OPENSECRET_ALL - {"os-eif"} | {
                "deps-opensecret"}),
            ("scripts/ci/opensecret_change_detection.py", OPENSECRET_ALL | {"repo"},
             OPENSECRET_ALL - {"os-eif"} | {"repo"}),
            # Each lane's own entry script.
            ("scripts/ci/opensecret-sdk-e2e.sh", {"os-sdk-e2e", "repo"}, {"os-sdk-e2e", "repo"}),
            ("scripts/ci/opensecret-helpers.sh", {"os-helpers", "repo"}, {"os-helpers", "repo"}),
        ):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), pr)
                self.assertEqual(selected([path], PUSH), push)

    def test_an_approval_edit_anywhere_in_the_diff_drops_the_candidate(self):
        runtime, approval = "services/opensecret/src/main.rs", "services/opensecret/pcrProdHistory.json"
        self.assertIn("os-eif", selected([runtime]))
        self.assertNotIn("os-eif", selected([runtime, approval]))
        self.assertNotIn("os-eif", selected(["docs/x.md"], labels=["ci:full"]))
        self.assertNotIn("os-eif", selected([".github/workflows/ci.yml"]))
        # Its own label builds it, for example to prove an edit of its ci.yml job.
        self.assertEqual(selected([".github/workflows/ci.yml"], labels=["ci:eif"]) & {"os-eif"}, {"os-eif"})
        self.assertNotIn("os-eif", selected([approval], labels=["ci:eif"]))
        self.assertEqual({lane.id for lane in lanes.LANES if lane.events == {PR}}, {"os-eif"})
        self.assertIn("deps-opensecret", active(NIGHTLY))  # new advisories

    def test_eif_builder_and_checkout(self):
        entries = {e["id"]: e for e in json.loads(plan.outputs(dict.fromkeys(ACTIVE, ["x"]))["nix"])}
        for lane in ("os-rust", "os-flake-check", "deps-opensecret"):
            self.assertEqual((entries[lane]["submodules"], entries[lane]["fetch_depth"]), ("recursive", 0))
        self.assertEqual((entries["os-pcr"]["submodules"], entries["os-pcr"]["fetch_depth"]), ("false", 1))
        self.assertEqual(plan.OPENSECRET_OUTPUTS, OUTPUTS)


class PlannerTests(unittest.TestCase):
    def github(self, event, payload, **env):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "event.json").write_text(json.dumps(payload))
            env.update(GITHUB_EVENT_NAME=event, GITHUB_EVENT_PATH=str(root / "event.json"),
                       GITHUB_OUTPUT=str(root / "output"), GITHUB_REPOSITORY="MaplePrivacyLabs/Maple",
                       PATH=os.environ.get("PATH", ""))  # no GH_TOKEN: labels read as none
            with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(io.StringIO()):
                code = plan.github()
            output = (root / "output").read_text() if code == 0 else ""
        return code, {key: json.loads(value) for key, value in
                      (line.split("=", 1) for line in output.splitlines())}

    def test_push_without_a_usable_base_selects_everything(self):
        for before in ("0" * 40, SHA_A, ""):
            with self.subTest(before=before):
                code, outputs = self.github(PUSH, {"before": before, "after": SHA_B})
                self.assertEqual(code, 0)
                self.assertEqual({lane for lane, on in outputs["lanes"].items() if on}, active(PUSH))

    def test_schedule_and_dispatch(self):
        _, outputs = self.github(NIGHTLY, {})
        self.assertEqual({lane for lane, on in outputs["lanes"].items() if on},
                         {lane.id for lane in lanes.LANES if NIGHTLY in lane.events})
        _, outputs = self.github(DISPATCH, {}, LANES_INPUT="agent-test")
        self.assertEqual({lane for lane, on in outputs["lanes"].items() if on}, {"agent-test"})
        self.assertEqual([entry["id"] for entry in outputs["nix"]], ["agent-test"])

    def test_quarantined_lanes_still_run_and_an_unknown_id_fails_the_plan(self):
        code, outputs = self.github(NIGHTLY, {}, CI_QUARANTINE=" research-ios,agent-test, ")
        self.assertEqual((code, outputs["quarantine"]), (0, ["agent-test", "research-ios"]))
        self.assertTrue(outputs["lanes"]["agent-test"])
        self.assertEqual(self.github(NIGHTLY, {})[1]["quarantine"], [])
        for event, payload in ((NIGHTLY, {}), (PUSH, {"before": SHA_A, "after": SHA_B})):
            with self.subTest(event=event):
                self.assertEqual(self.github(event, payload, CI_QUARANTINE="agent-test,agent-tset")[0], 1)
                self.assertEqual(self.github(event, payload, CI_QUARANTINE="agent-test,repo")[0], 1)

    def test_eif_candidate_builds_only_same_repository_prs_to_master_with_a_usable_diff(self):
        pull = {"number": 1, "base": {"sha": SHA_A, "ref": "master"},
                "head": {"sha": SHA_B, "repo": {"full_name": "MaplePrivacyLabs/Maple"}}}
        code, outputs = self.github(PR, {"pull_request": pull})
        self.assertEqual((code, {lane for lane, on in outputs["lanes"].items() if on}), (0, active(PR) - {"os-eif"}))
        with mock.patch.object(plan, "changed_paths", return_value=["services/opensecret/src/main.rs"]):
            for head, base, built in (("MaplePrivacyLabs/Maple", "master", True),
                                      ("MaplePrivacyLabs/Maple", "stacked-branch", False),
                                      ("contributor/Maple", "master", False), (None, "master", False)):
                with self.subTest(head=head, base=base):
                    pull["head"]["repo"] = head and {"full_name": head}
                    pull["base"]["ref"] = base
                    self.assertIs(self.github(PR, {"pull_request": pull})[1]["lanes"]["os-eif"], built)

    def test_malformed_event_fails_a_pr_and_selects_everything_on_push(self):
        self.assertEqual(self.github(PR, {"pull_request": {}})[0], 1)
        self.assertEqual(self.github(QUEUE, {})[0], 1)
        code, outputs = self.github(PUSH, {})
        self.assertEqual(code, 0)
        self.assertEqual({lane for lane, on in outputs["lanes"].items() if on}, active(PUSH))
        code, outputs = self.github(DISPATCH, {}, LANES_INPUT="no-such-lane")
        self.assertEqual((code, {lane for lane, on in outputs["lanes"].items() if on}), (0, active(DISPATCH)))

    def test_outputs_empty_matrix_and_runner_overrides(self):
        empty = plan.outputs({})
        self.assertEqual((empty["nix"], empty["windows"]), ("[]", "[]"))
        self.assertEqual(json.loads(empty["lanes"]), dict.fromkeys(sorted(ACTIVE), False))
        overrides = {"CI_LINUX_HEAVY_RUNNER": "blacksmith-16vcpu-ubuntu-2404", "CI_WINDOWS_RUNNER": "windows-2025"}
        with mock.patch.dict(os.environ, overrides):
            result = plan.outputs(dict.fromkeys(ACTIVE, ["x"]))
        runners = {entry["id"]: entry["runner"] for job in lanes.MATRIX_JOBS for entry in json.loads(result[job])}
        self.assertEqual(runners, {lane.id: overrides.get(lane.runner.variable or "", lane.runner.label)
                                   for lane in lanes.LANES if lane.job in lanes.MATRIX_JOBS})
        self.assertEqual((runners["agent-test"], runners["agent-lint"], runners["agent-windows"],
                          runners["agent-supply-chain"]),
                         ("blacksmith-16vcpu-ubuntu-2404", "blacksmith-16vcpu-ubuntu-2404", "windows-2025",
                          lanes.LINUX.label))
        entries = [entry for job in lanes.MATRIX_JOBS for entry in json.loads(result[job])]
        self.assertEqual({len(entry["cache_key"]) for entry in entries if entry["cache"]}, {16})

    def test_a_cache_key_pins_the_toolchain_and_leaves_lockfiles_to_rust_cache(self):
        # rust-cache hashes the lockfiles after the restore key, so a lockfile change falls
        # back by prefix; the planner's part must match exactly and holds only the pins.
        for lane in lanes.LANES:
            if lane.cache:
                workspace = lane.workspaces.split("->")[0].strip()
                pins = {f"{workspace}/{name}" for name in ("flake.lock", "rust-toolchain.toml")
                        if (plan.ROOT / workspace / name).is_file()}
                with self.subTest(lane=lane.id):
                    self.assertIn(f"{workspace}/flake.lock", pins)
                    self.assertEqual(set(lane.cache_files), pins)

    def test_a_cache_is_saved_only_into_the_home_runners_store(self):
        def saves(**overrides):
            with mock.patch.dict(os.environ, overrides):
                result = plan.outputs(dict.fromkeys(ACTIVE, ["x"]))
            return {entry["id"]: entry["cache_save"] for job in lanes.MATRIX_JOBS for entry in json.loads(result[job])}

        cached = {"agent-test", "agent-windows"}
        self.assertTrue(cached <= {lane.id for lane in lanes.LANES if lane.cache})
        with mock.patch.dict(os.environ):
            for runner in lanes.LINUX, lanes.LINUX_HEAVY, lanes.MACOS, lanes.WINDOWS, lanes.GITHUB_WINDOWS:
                os.environ.pop(runner.variable, None)
            self.assertTrue(all(saves().values()))
            # Another Blacksmith size keeps its vendor's store.
            on = saves(CI_LINUX_HEAVY_RUNNER="blacksmith-16vcpu-ubuntu-2404",
                       CI_WINDOWS_RUNNER="blacksmith-16vcpu-windows-2025")
            self.assertEqual({lane: on[lane] for lane in cached}, dict.fromkeys(cached, True))
            # A failover to the other vendor restores what it can and saves nothing.
            off = saves(CI_LINUX_HEAVY_RUNNER="ubuntu-latest", CI_WINDOWS_RUNNER="windows-2025")
            self.assertEqual({lane: off[lane] for lane in cached}, dict.fromkeys(cached, False))
            # The free GitHub runner has no override, so its lanes always save into GitHub's store.
            self.assertTrue(on["agent-windows-updater"] and off["agent-windows-updater"])
        self.assertEqual(lanes.GITHUB_WINDOWS.variable, "CI_WINDOWS_GITHUB_RUNNER")
        self.assertEqual({lane.id for lane in lanes.LANES if lane.runner == lanes.GITHUB_WINDOWS},
                         {"agent-package-windows", "research-desktop-windows"})
        # The native test suites gain nothing from 16 cores: the free standard runner, without an override.
        self.assertEqual({lane.id for lane in lanes.LANES if lane.runner == lanes.GITHUB_WINDOWS_FREE},
                         {"agent-full-windows", "agent-windows-updater"})

    def test_run_executes_the_lane_command_in_its_flake(self):
        with mock.patch.object(plan.subprocess, "run") as run, contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(io.StringIO()):
            run.return_value.returncode = 0
            self.assertEqual(plan.run("agent-test"), 0)
            command, = run.call_args.args
            self.assertEqual(plan.run("no-such-lane"), 2)
            # Windows has no Nix; Git Bash comes from PATH, never System32's WSL launcher.
            with mock.patch.object(plan.platform, "system", return_value="Windows"), \
                    mock.patch.object(plan.shutil, "which", return_value="C:/Git/usr/bin/bash.exe"):
                self.assertEqual(plan.run("agent-provider"), 0)
        self.assertEqual(command, ["nix", "develop", "--no-update-lock-file", "./apps/maple-agent", "-c",
                                   "bash", "-euo", "pipefail", "-c", "cd apps/maple-agent && just ci-test"])
        self.assertEqual(run.call_args.args[0][:2], ["C:/Git/usr/bin/bash.exe", "-euo"])
        self.assertEqual(run.call_args.kwargs["env"]["RUSTFLAGS"], "-D warnings")

    def verdict(self, lanes_on, quarantine=(), **results):
        needs = {"plan": {"result": "success", "outputs": {"lanes": json.dumps(dict.fromkeys(lanes_on, True)),
                                                           "quarantine": json.dumps(list(quarantine))}}}
        needs.update({job: {"result": result} for job, result in results.items()})
        out = io.StringIO()
        with mock.patch.dict(os.environ, {"NEEDS": json.dumps(needs)}), contextlib.redirect_stdout(out):
            code = plan.verdict()
        self.output = out.getvalue()
        return code

    def test_verdict_needs_every_selected_job_to_succeed_and_the_rest_skipped(self):
        # nix carries agent-lint; agent-package-linux and os-eif are special jobs.
        self.assertEqual(self.verdict(["agent-lint"], nix="success", windows="skipped", **{
            "agent-package-linux": "skipped", "os-eif": "skipped"}), 0)
        self.assertEqual(self.verdict(["agent-lint"], nix="failure", windows="skipped"), 1)
        self.assertEqual(self.verdict(["agent-lint"], nix="skipped"), 1)
        self.assertEqual(self.verdict(["os-eif"], nix="skipped", **{"os-eif": "cancelled"}), 1)
        self.assertEqual(self.verdict([], nix="skipped", **{"os-eif": "failure"}), 1)
        self.assertEqual(self.verdict([], nix="skipped", **{"os-eif": "success"}), 0)
        needs = {"plan": {"result": "failure", "outputs": {}}}
        with mock.patch.dict(os.environ, {"NEEDS": json.dumps(needs)}), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(plan.verdict(), 1)

    def test_verdict_reports_a_quarantined_lanes_failure_without_failing(self):
        # A special job runs one lane.
        package = {"nix": "skipped", "agent-package-linux": "failure"}
        self.assertEqual(self.verdict(["agent-package-linux"], **package), 1)
        self.assertEqual(self.verdict(["agent-package-linux"], ["agent-package-linux"], **package), 0)
        self.assertIn("::warning::agent-package-linux did not succeed", self.output)
        self.assertEqual(self.verdict(["agent-package-linux"], ["agent-package-linux"], nix="skipped",
                                      **{"agent-package-linux": "success"}), 0)
        self.assertNotIn("::warning::", self.output)
        # A matrix job runs many: the run's job results name the entries that failed.
        listing = mock.Mock(stdout="plan\tsuccess\nagent-lint\tsuccess\nagent-test\tfailure\n")
        with mock.patch.object(plan.subprocess, "run", return_value=listing) as run:
            self.assertEqual(self.verdict(["agent-lint", "agent-test"], ["agent-test"], nix="failure"), 0)
            self.assertIn("::warning::agent-test did not succeed", self.output)
            self.assertIn("/actions/runs/", run.call_args.args[0][3])
            # Another failing entry, or a failure no entry explains, still fails ci-ok.
            listing.stdout = "agent-lint\tfailure\nagent-test\tfailure\n"
            self.assertEqual(self.verdict(["agent-lint", "agent-test"], ["agent-test"], nix="failure"), 1)
            listing.stdout = "agent-lint\tsuccess\nagent-test\tsuccess\n"
            self.assertEqual(self.verdict(["agent-lint", "agent-test"], ["agent-test"], nix="failure"), 1)
        with mock.patch.object(plan.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "gh")):
            self.assertEqual(self.verdict(["agent-lint", "agent-test"], ["agent-test"], nix="failure"), 1)

    def test_check_rust_compares_the_flake_compiler_with_the_windows_pin(self):
        for reported, expected in ((f"rustc {lanes.AGENT_WINDOWS_RUST} (abc 2026-01-01)\n", 0),
                                   ("rustc 1.99.0 (abc 2026-01-01)\n", 1)):
            with mock.patch.object(plan.subprocess, "run") as run, contextlib.redirect_stdout(io.StringIO()):
                run.return_value.stdout = reported
                self.assertEqual(plan.check_rust(), expected)


class DiffTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        patcher = mock.patch.object(plan, "ROOT", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.git("init", "-q", "-b", "master")
        self.base = self.commit("README.md")

    def git(self, *args):
        env = {**os.environ, "GIT_AUTHOR_NAME": "CI", "GIT_AUTHOR_EMAIL": "ci@example.invalid",
               "GIT_COMMITTER_NAME": "CI", "GIT_COMMITTER_EMAIL": "ci@example.invalid"}
        return subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args], cwd=self.root, env=env,
                              check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, path):
        (self.root / path).parent.mkdir(parents=True, exist_ok=True)
        (self.root / path).write_text(path)
        self.git("add", "--", path)
        self.git("commit", "-qm", path)
        return self.git("rev-parse", "HEAD")

    def test_pull_requests_diff_from_the_merge_base(self):
        master = self.commit("apps/maple-agent/app/src/main.rs")
        self.git("checkout", "-qb", "topic", self.base)
        docs = self.commit("apps/maple-agent/docs/design.md")
        self.assertEqual(plan.changed_paths(master, docs, merge_base=True), ["apps/maple-agent/docs/design.md"])
        self.assertIn("apps/maple-agent/app/src/main.rs", plan.changed_paths(master, docs, merge_base=False))

    def test_renames_out_of_a_component_keep_the_old_path(self):
        runtime = self.commit("apps/maple-agent/app/src/main.rs")
        self.git("mv", "apps/maple-agent/app/src/main.rs", "README.md.moved")
        self.git("commit", "-qm", "rename")
        paths = plan.changed_paths(runtime, self.git("rev-parse", "HEAD"), merge_base=False)
        self.assertIn("apps/maple-agent/app/src/main.rs", paths)

    def test_missing_or_zero_base_has_no_usable_diff(self):
        for base in ("0" * 40, SHA_A, "HEAD", ""):
            with self.subTest(base=base):
                self.assertIsNone(plan.changed_paths(base, self.base, merge_base=False))

    def only(self, event, payload):
        event_path = self.root / "event.json"
        event_path.write_text(json.dumps(payload))
        output = io.StringIO()
        with mock.patch.dict(os.environ, {"GITHUB_EVENT_NAME": event, "GITHUB_EVENT_PATH": str(event_path)}), \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(plan.only("agent-desktop"), 0)
        return output.getvalue()

    def test_only_selects_the_desktop_build_for_its_inputs_and_whenever_unsure(self):
        docs = self.commit("apps/maple-agent/docs/design.md")
        self.assertEqual(self.only(PUSH, {"before": self.base, "after": docs}), "selected=false\n")
        typescript = self.commit("sdk/src/index.ts")
        self.assertEqual(self.only(PUSH, {"before": docs, "after": typescript}), "selected=false\n")
        runtime = self.commit("proxy/src/main.rs")
        self.assertEqual(self.only(PUSH, {"before": typescript, "after": runtime}), "selected=true\n")
        unknown = self.commit("newroot/x")
        self.assertEqual(self.only(PUSH, {"before": runtime, "after": unknown}), "selected=true\n")
        for event, payload in ((PUSH, {"before": "0" * 40, "after": docs}), (PUSH, {"before": SHA_A, "after": docs}),
                               (PUSH, {}), (DISPATCH, {}), (NIGHTLY, {"schedule": "37 10 * * *"})):
            with self.subTest(event=event, payload=payload):
                self.assertEqual(self.only(event, payload), "selected=true\n")


if __name__ == "__main__":
    unittest.main()
