"""Lane selection: the table, the planner's events, and the Agent selector it replaces."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import lanes
import plan
from lanes import DISPATCH, NIGHTLY, PR, PUSH, QUEUE


ACTIVE = {lane.id for lane in lanes.LANES if lane.legacy is None}
AGENT_PR = {"agent-lint", "agent-test", "agent-macos"}
SHA_A, SHA_B = "a" * 40, "b" * 40


def selected(paths, event=PR, labels=()):
    return set(plan.select(event, paths, labels))


def owners(path):
    return {lane.id for lane in lanes.LANES if plan.matches(path, plan.lane_globs(lane))}


def agent(paths, event=PR):
    """The decision agent_change_detection.py made: does any Agent check run?"""
    return any(lane.startswith("agent-") for lane in selected(paths, event))


class PortedAgentSelectorTests(unittest.TestCase):
    """Every case of test_agent_change_detection.py, as an Agent lane decision."""

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
                self.assertEqual(selected([path]), AGENT_PR)

    def test_rust_sdk_and_proxy_runtime_inputs_select_agent(self):
        for path in ("proxy/Cargo.toml", "proxy/src/proxy.rs", "proxy/build.rs", "sdk/rust/Cargo.toml",
                     "sdk/rust/src/client.rs", "sdk/rust/build.rs", "sdk/rust/assets/aws_nitro_root.der"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]) - {"repo"}, AGENT_PR)  # proxy/Cargo.toml feeds the release gates
                self.assertEqual({lane for lane in selected([path], PUSH) if lane.startswith("agent-")},
                                 {lane for lane in ACTIVE if lane.startswith("agent-")})

    def test_research_typescript_and_independent_services_skip_agent(self):
        for path in (
            "apps/maple-auth/src/main.tsx", "apps/maple-auth/package.json", "apps/maple-auth/bun.lock",
            "apps/maple-auth/vite.config.ts", "apps/maple-research/frontend/src/main.tsx",
            "apps/maple-research/frontend/src-tauri/src/lib.rs", "sdk/src/lib/index.ts", "sdk/package.json",
            "sdk/flake.nix", "services/updates/src/index.ts", "services/opensecret/src/main.rs",
            ".github/workflows/desktop-pr-build.yml", "scripts/ci/desktop-pr.sh",
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
        self.assertEqual(selected(["scripts/ci/verify-agent-rust-deps.py"]), {"agent-lint", "repo"})
        for path in ("scripts/ci/apple-toolchain.json", "scripts/ci/select-xcode.py"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), {"agent-macos", "repo"})
        self.assertEqual(selected(["scripts/ci/agent_windows.py"]), {"agent-windows", "repo"})

    def test_intentional_differences_select_only_the_repo_lane(self):
        for path in (
            # The Agent flake has no root inputs.
            "flake.nix", "flake.lock",
            # These drive agent-ci.yml and agent-desktop-build.yml, which still own them.
            ".github/workflows/agent-ci.yml", ".github/workflows/agent-desktop-build.yml",
            "scripts/ci/agent_change_detection.py", "scripts/ci/agent_master_selection.py",
            "scripts/ci/agent_cache.py", "scripts/ci/agent_cache_scope.py", "scripts/ci/agent_cache_command.py",
            "scripts/ci/agent_dependency_archive.py", "scripts/ci/agent_measure.py", "scripts/ci/change_detection.py",
            # Signing scripts: no Agent PR lane signs anything.
            "scripts/ci/windows-artifact-sign.ps1", "scripts/ci/install-windows-artifact-signing.ps1",
        ):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), {"repo"})
                self.assertEqual(selected([path], PUSH), {"repo"})

    def test_unknown_roots_and_malformed_paths_select_every_lane(self):
        for path in ("new-build-config.toml", "newroot/x", "", "/tmp/file", "../file", "docs/../flake.nix",
                     "apps/maple-agent/../maple-research/frontend/src/main.tsx"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), ACTIVE)
                self.assertEqual(selected([path], PUSH), ACTIVE)

    def test_mixed_changes_and_empty_diff(self):
        self.assertEqual(selected([]), set())
        self.assertEqual(selected(["README.md", "sdk/rust/README.md"]), set())
        self.assertEqual(selected(["README.md", "proxy/src/proxy.rs"]), AGENT_PR)

    def test_explain_reads_null_delimited_names(self):
        for data, expected in ((b"README.md\0apps/maple-agent/app/assets/a\nspace name\0", "agent-lint"),
                               (b"README.md\0apps/maple-agent/docs/design notes.md\0", "no ci.yml lane"),
                               (b"", "no ci.yml lane")):
            result = subprocess.run([sys.executable, str(Path(plan.__file__)), "explain"], input=data,
                                    check=True, capture_output=True)
            self.assertIn(expected, result.stdout.decode())


class LaneTableTests(unittest.TestCase):
    def test_typescript_sdk_never_selects_a_client_lane(self):
        self.assertEqual(selected(["sdk/src/index.ts"], PUSH), set())
        self.assertFalse({o for o in owners("sdk/src/index.ts") if o.startswith(("agent-", "research-"))})

    def test_release_metadata_and_publisher_inputs_select_the_repo_lane(self):
        for path in ("apps/maple-research/frontend/package.json", "apps/maple-research/frontend/src-tauri/Cargo.toml",
                     "apps/maple-research/frontend/src-tauri/tauri.conf.json", "proxy/justfile", ".github/CODEOWNERS",
                     ".github/actionlint.yaml", "services/agent-updates/publisher/publisher.py"):
            with self.subTest(path=path):
                self.assertIn("repo", selected([path]))

    def test_rust_sdk_tests_and_proxy_container_select_no_agent_lane(self):
        self.assertEqual(selected(["sdk/rust/tests/x.rs"], PUSH), set())
        self.assertEqual(selected(["proxy/src/main.rs"]), AGENT_PR)
        self.assertEqual(selected(["proxy/Dockerfile"], PUSH), set())

    def test_windows_clippy_runs_on_prs_only_for_windows_paths_or_labels(self):
        windows = "apps/maple-agent/app/src/host/windows_spawn.rs"
        other = "apps/maple-agent/app/src/ui/x.rs"
        self.assertIn("agent-windows", selected([windows]))
        self.assertIn("agent-windows", selected([windows], QUEUE))
        self.assertNotIn("agent-windows", selected([other]))
        self.assertIn("agent-windows", selected([other], PUSH))
        self.assertIn("agent-windows", selected(["docs/x.md"], labels=["ci:windows"]))
        self.assertEqual(selected(["docs/x.md"], labels=["ci:full"]), ACTIVE)
        self.assertEqual(selected(["docs/x.md"], labels=["unrelated"]), set())

    def test_inert_paths_select_nothing(self):
        for path in ("apps/maple-agent/docs/x.md", "docs/x.md", ".agents/skills/x/SKILL.md"):
            with self.subTest(path=path):
                self.assertTrue(plan.inert(path))
                self.assertEqual(selected([path], PUSH), set())
        evidence = ".agents/skills/release-maple/scripts/ci_evidence.py"
        self.assertFalse(plan.inert(evidence))
        self.assertEqual(selected([evidence]), {"repo"})

    def test_ci_infrastructure_selects_every_active_gate_lane(self):
        for path in (".github/workflows/ci.yml", ".github/actions/setup-nix/action.yml",
                     "scripts/ci/plan.py", "scripts/ci/lanes.py"):
            with self.subTest(path=path):
                self.assertEqual(selected([path]), {lane.id for lane in lanes.LANES
                                                    if lane.legacy is None and lane.gate})

    def test_legacy_components_are_classified_but_select_nothing(self):
        for path in ("services/opensecret/src/main.rs", "apps/maple-research/frontend/src/App.tsx"):
            with self.subTest(path=path):
                self.assertTrue(plan.classified(path))
                self.assertEqual(selected([path], PUSH), set())

    def test_every_tracked_file_is_classified_and_ci_yml_matches(self):
        self.assertEqual(plan.lint_problems(plan.tracked_files()), [])
        self.assertIn("unclassified: newroot/x", plan.lint_problems(["newroot/x"]))


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
                self.assertEqual({lane for lane, on in outputs["lanes"].items() if on}, ACTIVE)

    def test_schedule_and_dispatch(self):
        _, outputs = self.github(NIGHTLY, {})
        self.assertEqual({lane for lane, on in outputs["lanes"].items() if on},
                         {lane.id for lane in lanes.LANES if lane.legacy is None and NIGHTLY in lane.events})
        _, outputs = self.github(DISPATCH, {}, LANES_INPUT="agent-test")
        self.assertEqual({lane for lane, on in outputs["lanes"].items() if on}, {"agent-test"})
        self.assertEqual([entry["id"] for entry in outputs["nix"]], ["agent-test"])

    def test_malformed_event_fails_a_pr_and_selects_everything_on_push(self):
        self.assertEqual(self.github(PR, {"pull_request": {}})[0], 1)
        self.assertEqual(self.github(QUEUE, {})[0], 1)
        code, outputs = self.github(PUSH, {})
        self.assertEqual(code, 0)
        self.assertTrue(all(outputs["lanes"].values()))
        code, outputs = self.github(DISPATCH, {}, LANES_INPUT="no-such-lane")
        self.assertEqual((code, all(outputs["lanes"].values())), (0, True))

    def test_outputs_empty_matrix_and_runner_overrides(self):
        lanes_json, nix_json = plan.outputs({})
        self.assertEqual(nix_json, "[]")
        self.assertEqual(json.loads(lanes_json), dict.fromkeys(sorted(ACTIVE), False))
        with mock.patch.dict(os.environ, {"CI_LINUX_HEAVY_RUNNER": "blacksmith-16vcpu-ubuntu-2404"}):
            entries = json.loads(plan.outputs(dict.fromkeys(ACTIVE, ["x"]))[1])
        runners = {entry["id"]: entry["runner"] for entry in entries}
        self.assertEqual(runners, {"agent-lint": "blacksmith-8vcpu-ubuntu-2404",
                                   "agent-test": "blacksmith-16vcpu-ubuntu-2404",
                                   "agent-macos": "blacksmith-6vcpu-macos-26", "repo": "ubuntu-latest"})
        self.assertEqual({len(entry["cache_key"]) for entry in entries if entry["cache"]}, {16})

    def test_run_executes_the_lane_command_in_its_flake(self):
        with mock.patch.object(plan.subprocess, "run") as run, contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(io.StringIO()):
            run.return_value.returncode = 0
            self.assertEqual(plan.run("agent-test"), 0)
            self.assertEqual(plan.run("agent-windows"), 2)  # a special ci.yml job
        command, = run.call_args.args
        self.assertEqual(command, ["nix", "develop", "--no-update-lock-file", "./apps/maple-agent", "-c",
                                   "bash", "-euo", "pipefail", "-c", "cd apps/maple-agent && just ci-test"])
        self.assertEqual(run.call_args.kwargs["env"]["RUSTFLAGS"], "-D warnings")

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


if __name__ == "__main__":
    unittest.main()
