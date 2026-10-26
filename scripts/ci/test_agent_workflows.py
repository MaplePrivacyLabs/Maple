"""Security, component selection, and Research release isolation for Agent CI."""

import functools
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

import agent_master_selection


ROOT = Path(__file__).resolve().parents[2]
RUST_CACHE_COMMAND = 'python3 "${{ github.workspace }}/scripts/ci/agent_cache_command.py" {0}'
TRUSTED_DEPENDENCY_WRITE = "${{ success() && github.repository == 'MaplePrivacyLabs/Maple' && github.event_name == 'push' && github.ref == 'refs/heads/master' }}"
TRUSTED_DEPENDENCY_UPLOAD = "${{ success() && github.repository == 'MaplePrivacyLabs/Maple' && github.event_name == 'push' && github.ref == 'refs/heads/master' && steps.dependency-seed.outputs.needs-upload == 'true' }}"
DEPENDENCY_BUILDERS = (
    ("agent-ci.yml", "desktop"), ("agent-ci.yml", "provider"), ("agent-ci.yml", "linux-release"),
    ("agent-desktop-build.yml", "macos-build"), ("agent-desktop-build.yml", "linux"),
    ("agent-desktop-build.yml", "windows-build"),
)


@functools.cache
def workflow(name):
    result = subprocess.run(
        ["yq", "-o=json", ".", str(ROOT / ".github" / "workflows" / name)],
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, child in value.items():
            yield str(key)
            yield from strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)


def variants_for(job, event):
    value = job["strategy"]["matrix"]["variant"]
    if isinstance(value, list):
        return value
    # Interpret only the event-based JSON choice used by this workflow. Reject
    # other contexts so fork/base/selector state cannot change the PR profiles.
    choice = re.fullmatch(
        r"\$\{\{ fromJSON\(github\.event_name == '([^']+)' && '([^']+)' \|\| '([^']+)'\) \}\}",
        value,
    )
    if choice is None:
        raise AssertionError(f"unsupported variant matrix: {value}")
    return json.loads(choice[2] if event == choice[1] else choice[3])


class AgentWorkflowBoundaryTests(unittest.TestCase):
    def test_native_workspace_commands_keep_the_declared_cache_profile(self):
        steps = workflow("agent-ci.yml")["jobs"]["desktop"]["steps"]
        expected = {
            "Build macOS workspace and test targets": (
                "runner.os == 'macOS'",
                "nix develop --no-update-lock-file . -c python3 ../../scripts/ci/agent_measure.py --phase macos-build --target target -- cargo build --workspace --all-targets --locked --timings",
            ),
            "Test macOS workspace": (
                "runner.os == 'macOS'",
                "nix develop --no-update-lock-file . -c python3 ../../scripts/ci/agent_measure.py --phase macos-test --target target -- cargo test --workspace --locked --timings",
            ),
            "Build Windows workspace and test targets": (
                "runner.os == 'Windows'",
                "python3 ../../scripts/ci/agent_measure.py --phase windows-build --target target -- cargo build --workspace --all-targets --locked --timings",
            ),
            "Test Windows workspace": (
                "runner.os == 'Windows'",
                "python3 ../../scripts/ci/agent_measure.py --phase windows-test --target target -- cargo test --workspace --locked --timings",
            ),
        }
        for name, (condition, command) in expected.items():
            with self.subTest(step=name):
                selected = [step for step in steps if step.get("name") == name]
                self.assertEqual(len(selected), 1)
                self.assertEqual(selected[0]["if"], condition)
                self.assertEqual(selected[0]["run"].strip(), command)
                self.assertNotIn("env", selected[0])
        self.assertEqual(workflow("agent-ci.yml")["defaults"]["run"]["working-directory"], "apps/maple-agent")

    def test_git_metadata_freshness_is_checked_with_each_platform_toolchain(self):
        steps = workflow("agent-ci.yml")["jobs"]["desktop"]["steps"]
        freshness = next(step for step in steps if step.get("name") == "Verify Git metadata freshness on native workspace builds")
        self.assertEqual(freshness["if"], "runner.os != 'Linux'")
        self.assertEqual(freshness["run"].strip(),
                         'if [ "$RUNNER_OS" = Windows ]; then\n'
                         '  python3 scripts/test_git_metadata.py\n'
                         'else\n'
                         '  nix develop --no-update-lock-file . -c python3 scripts/test_git_metadata.py\n'
                         'fi')
        self.assertNotIn("continue-on-error", freshness)
        recipes = (ROOT / "apps/maple-agent/justfile").read_text()
        self.assertIn("ci: fmt-check git-metadata-test clippy test", recipes)
        self.assertIn("git-metadata-test:\n    python3 scripts/test_git_metadata.py", recipes)
        hook = (ROOT / "apps/maple-agent/.githooks/pre-commit").read_text()
        self.assertIn("hook_run python3 scripts/test_git_metadata.py", hook)

    def test_windows_linker_is_pinned_and_smoked_before_fingerprint_or_cache(self):
        jobs = workflow("agent-ci.yml")["jobs"]
        for name in ("desktop", "provider"):
            with self.subTest(job=name):
                steps = jobs[name]["steps"]
                native = next(i for i, step in enumerate(steps)
                              if step.get("uses", "").startswith("ilammy/msvc-dev-cmd@"))
                smoke = next(i for i, step in enumerate(steps)
                             if step.get("run") == "python3 ../../scripts/ci/agent_windows.py")
                fingerprint = next(i for i, step in enumerate(steps) if step.get("id") == "environment")
                cache = next(i for i, step in enumerate(steps)
                             if "rust-cache@" in step.get("uses", "")
                             or "actions/cache/restore@" in step.get("uses", ""))
                self.assertLess(native, smoke)
                self.assertLess(smoke, fingerprint)
                self.assertLess(fingerprint, cache)
                self.assertEqual(steps[smoke]["if"], "runner.os == 'Windows'")
                self.assertNotIn("continue-on-error", steps[smoke])

    def test_windows_crypto_pins_native_perl_only_after_required_module_probe(self):
        step = next(step for step in workflow("agent-ci.yml")["jobs"]["desktop"]["steps"]
                    if step.get("name") == "Select native Windows Perl for the static crypto build")
        self.assertEqual(step["if"], "runner.os == 'Windows'")
        with tempfile.TemporaryDirectory(prefix="native perl ") as temporary:
            folder = Path(temporary)
            native_perl = folder / "native perl.exe"
            native_perl.write_text("#!/bin/sh\n"
                                   'test "$1" = "-MLocale::Maketext::Simple" || exit 41\n'
                                   'test "$2" = "-MIPC::Cmd" || exit 42\n'
                                   'test "$3" = "-e" || exit 43\n'
                                   'case "$4" in *MSWin32*) ;; *) exit 44 ;; esac\n'
                                   'exit "$PERL_PROBE_EXIT"\n')
            native_perl.chmod(0o755)
            cygpath = folder / "cygpath"
            cygpath.write_text('#!/bin/sh\ntest "$1" = -m || exit 45\n'
                               'test "$2" = "$PERL_PROBE_PATH" || exit 46\n'
                               'printf "%s\\n" "C:/Native Perl/perl.exe"\n')
            cygpath.chmod(0o755)
            script = step["run"].replace("/c/Strawberry/perl/bin/perl.exe", '"' + str(native_perl) + '"')
            destination = folder / "github-env"
            for probe_exit in (0, 2):
                with self.subTest(probe_exit=probe_exit):
                    destination.unlink(missing_ok=True)
                    environment = {**os.environ, "PATH": str(folder) + os.pathsep + os.environ["PATH"],
                                   "GITHUB_ENV": str(destination), "PERL_PROBE_PATH": str(native_perl),
                                   "PERL_PROBE_EXIT": str(probe_exit)}
                    result = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", script],
                                            env=environment, capture_output=True, text=True)
                    self.assertEqual(result.returncode, probe_exit, result.stderr)
                    if probe_exit == 0:
                        self.assertEqual(destination.read_text(),
                                         "OPENSSL_SRC_PERL=C:/Native Perl/perl.exe\n")
                    else:
                        self.assertFalse(destination.exists(), "Failed probes cannot select a Perl executable")

    def test_contributor_build_has_no_credentials_or_privileged_events(self):
        config = workflow("agent-ci.yml")
        self.assertEqual(set(config["on"]), {"push", "pull_request", "workflow_dispatch"})
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        self.assertIsNone(config["on"]["pull_request"])
        self.assertEqual(config["permissions"], {"contents": "read"})
        for value in strings({key: value for key, value in config.items() if key != "jobs"}):
            self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
        for name, job in config["jobs"].items():
            self.assertNotIn("environment", job)
            self.assertNotIn("uses", job)
            if name in {"changes", "desktop", "provider", "linux-release"}:
                self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
            else:
                self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
            for step in job["steps"]:
                self.assertNotIn("${{", step.get("run", ""))
                if name == "changes" and step.get("id") == "master":
                    self.assertEqual(step["env"], {"GH_TOKEN": "${{ github.token }}"})
                    self.assertIn("agent_master_selection.py --lane ci", step["run"])
                    self.assertEqual(step["if"], "${{ github.repository == 'MaplePrivacyLabs/Maple' && github.ref == 'refs/heads/master' && github.event_name == 'push' }}")
                elif name in {"desktop", "provider", "linux-release"} and step.get("id") == "dependency-archive":
                    expected = {"GH_TOKEN": "${{ github.token }}"}
                    if name == "provider":
                        expected["PROVIDER_DEPENDENCY_FINGERPRINT"] = "${{ steps.provider.outputs.dependency-fingerprint }}"
                    self.assertEqual(step["env"], expected)
                    self.assertIn("agent_dependency_archive.py restore", step["run"])
                else:
                    for value in strings(step):
                        self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
                action = step.get("uses", "")
                if action:
                    self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)?@[0-9a-f]{40}$")
                if action.startswith("actions/checkout@"):
                    self.assertIs(step["with"]["persist-credentials"], False)
                if action.startswith("DeterminateSystems/determinate-nix-action@"):
                    self.assertEqual(step["with"]["github-token"], "")

    def test_agent_cache_and_artifacts_do_not_share_research_publication(self):
        steps = workflow("agent-ci.yml")["jobs"]["desktop"]["steps"]
        caches = [step["with"] for step in steps if "rust-cache@" in step.get("uses", "")]
        self.assertTrue(caches)
        for cache in caches:
            self.assertEqual(cache["workspaces"], "apps/maple-agent -> target")
            self.assertTrue(cache["key"].startswith("maple-agent-"))
            self.assertEqual(cache["save-if"], "false")
        for step in steps:
            action = step.get("uses", "")
            self.assertNotIn("release", action.lower())
            self.assertNotIn("download-artifact", action)
            if "upload-artifact@" in action:
                if step.get("name") == "Upload durable dependency archive":
                    self.assertEqual(step["with"]["name"], "${{ steps.dependency-seed.outputs.artifact-name }}")
                else:
                    self.assertTrue(step["with"]["name"].startswith("maple-agent-"))
                    self.assertTrue(step["with"]["path"].startswith("apps/maple-agent/target/"))
        self.assertFalse(any((ROOT / "apps/maple-agent/.github/workflows").glob("*.yml")))

    def test_failed_or_missing_selection_cannot_skip_the_desktop_matrix(self):
        condition = workflow("agent-ci.yml")["jobs"]["desktop"]["if"]
        self.assertIn("always() && !cancelled()", condition)
        self.assertIn("needs.changes.result != 'success'", condition)
        self.assertIn("needs.changes.outputs.agent != 'false'", condition)
        self.assertNotIn("head.repo", condition)

    def test_parallel_jobs_retain_selection_platforms_and_required_gate(self):
        jobs = workflow("agent-ci.yml")["jobs"]
        for name in ("desktop", "provider", "linux-release"):
            with self.subTest(job=name):
                job = jobs[name]
                self.assertEqual(job["needs"], "changes")
                self.assertEqual(job["if"], jobs["desktop"]["if"])
                self.assertFalse(any(step.get("continue-on-error") for step in job["steps"]))
        self.assertEqual(jobs["provider"]["strategy"], jobs["desktop"]["strategy"])
        self.assertEqual(jobs["provider"]["runs-on"], jobs["desktop"]["runs-on"])
        self.assertEqual(jobs["linux-release"]["runs-on"], "blacksmith-8vcpu-ubuntu-2404")
        self.assertEqual(jobs["required"]["needs"], ["changes", "desktop", "provider", "linux-release"])
        self.assertEqual(jobs["required"]["if"], "${{ always() && !cancelled() }}")
        gate = jobs["required"]["steps"][0]["run"]
        success = {"SELECTOR_RESULT": "success", "AGENT_SELECTED": "true",
                   "DESKTOP_RESULT": "success", "PROVIDER_RESULT": "success", "RELEASE_RESULT": "success"}
        for selected in ("true", "", "false"):
            for selector in ("success", "failure", "skipped"):
                for key in ("DESKTOP_RESULT", "PROVIDER_RESULT", "RELEASE_RESULT"):
                    for result in ("success", "failure", "skipped", "cancelled"):
                        env = {**success, "SELECTOR_RESULT": selector, "AGENT_SELECTED": selected, key: result}
                        omitted = selector == "success" and selected == "false"
                        if omitted:
                            env.update({name: "skipped" for name in success if name.endswith("_RESULT") and name != "SELECTOR_RESULT"})
                            env[key] = result
                        command = subprocess.run(["bash", "-c", gate], env={**os.environ, **env}, capture_output=True)
                        expected = result == ("skipped" if omitted else "success")
                        self.assertEqual(command.returncode == 0, expected, env)

    def test_master_selection_ci_verifiers_match_actual_platform_job_names(self):
        jobs = workflow("agent-ci.yml")["jobs"]
        actual = {jobs["linux-release"]["name"]}
        for name in ("desktop", "provider"):
            job = jobs[name]
            self.assertEqual(set(job["strategy"]["matrix"]), {"os"})
            actual.update(job["name"].replace("${{ matrix.os }}", runner)
                          for runner in job["strategy"]["matrix"]["os"])
        self.assertEqual(agent_master_selection.CI_VERIFIERS, actual)

    def test_dependency_cache_restore_prefix_uses_actual_environment_without_lock_hash(self):
        for filename, name in DEPENDENCY_BUILDERS:
            if name == "provider":
                continue
            with self.subTest(workflow=filename, job=name):
                steps = workflow(filename)["jobs"][name]["steps"]
                fingerprint = next(i for i, step in enumerate(steps) if step.get("id") == "environment")
                artifact = next(i for i, step in enumerate(steps) if step.get("id") == "dependency-archive")
                cache_index = next(i for i, step in enumerate(steps) if "rust-cache@" in step.get("uses", ""))
                self.assertLess(fingerprint, artifact)
                self.assertLess(artifact, cache_index)
                cache = steps[cache_index]["with"]
                self.assertEqual(steps[cache_index]["if"], "${{ steps.dependency-archive.outputs.restored != 'true' }}")
                self.assertEqual(cache["add-rust-environment-hash-key"], "true")
                self.assertIn("steps.environment.outputs.compatibility", cache["key"])
                self.assertNotIn("hashFiles", cache["key"])
                self.assertEqual(cache["cmd-format"], RUST_CACHE_COMMAND)
                self.assertEqual(cache["save-if"], "false")
                self.assertIn("python3 ../../scripts/ci/agent_cache_scope.py", steps[fingerprint]["run"])
                if name != "windows-build":
                    self.assertIn("nix develop --no-update-lock-file . -c", steps[fingerprint]["run"])

    def test_legacy_cache_restore_preserves_abi_scope_and_never_writes(self):
        for filename, name in DEPENDENCY_BUILDERS:
            if name == "provider":
                continue
            with self.subTest(workflow=filename, job=name):
                steps = workflow(filename)["jobs"][name]["steps"]
                primary_index = next(i for i, step in enumerate(steps) if step.get("id") == "rust-cache")
                marker_index = next(i for i, step in enumerate(steps) if step.get("id") == "restored-target")
                caches = [(i, step) for i, step in enumerate(steps) if "rust-cache@" in step.get("uses", "")]
                self.assertEqual(len(caches), 2)
                fallback_index, fallback = caches[1]
                primary = steps[primary_index]
                self.assertLess(primary_index, marker_index)
                self.assertLess(marker_index, fallback_index)
                self.assertEqual(fallback["uses"], primary["uses"])
                self.assertEqual(fallback["if"], "${{ steps.dependency-archive.outputs.restored != 'true' && steps.restored-target.outputs.present != 'true' }}")
                self.assertEqual(fallback["with"], {**primary["with"],
                    "add-rust-environment-hash-key": "false",
                    "key": primary["with"]["key"].replace("outputs.compatibility", "outputs.legacy_compatibility")})
                self.assertEqual(primary["with"]["add-rust-environment-hash-key"], "true")
                self.assertEqual(primary["with"]["save-if"], "false")
                self.assertNotIn("continue-on-error", fallback)

    def test_primary_prefix_restore_does_not_download_a_second_archive(self):
        # Metadata and a restored CACHEDIR.TAG do not prove useful dependencies.
        # Require real target fingerprints and a Rust library/metadata output.
        for filename, name in DEPENDENCY_BUILDERS:
            if name == "provider":
                continue
            marker = next(step for step in workflow(filename)["jobs"][name]["steps"]
                          if step.get("id") == "restored-target")
            for case, expected in (("empty", False), ("tag_only", False),
                                   ("fingerprint_only", False), ("library_only", False), ("empty_library", False),
                                   ("rlib", True), ("rmeta", True), ("nested", True)):
                with self.subTest(workflow=filename, job=name, case=case), tempfile.TemporaryDirectory(prefix="cache restore ") as directory:
                    root = Path(directory)
                    target = root / "apps/maple-agent/target"
                    profile = target / ("aarch64-apple-darwin/release" if case == "nested" else "debug")
                    profile.mkdir(parents=True)
                    if case != "empty":
                        (target / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
                    if case in {"fingerprint_only", "empty_library", "rlib", "rmeta", "nested"}:
                        unit = profile / ".fingerprint/public-123"
                        unit.mkdir(parents=True)
                        (unit / "lib-public").write_text("fingerprint")
                    if case in {"library_only", "empty_library", "rlib", "rmeta", "nested"}:
                        deps = profile / "deps"
                        deps.mkdir()
                        extension = "rmeta" if case == "rmeta" else "rlib"
                        (deps / f"libpublic-123.{extension}").write_bytes(b"" if case == "empty_library" else b"actual dependency")
                    output = root / "outputs"
                    subprocess.run(["bash", "-eu", "-c", marker["run"]], check=True,
                                   env={**os.environ, "GITHUB_WORKSPACE": str(root), "GITHUB_OUTPUT": str(output)},
                                   capture_output=True)
                    self.assertEqual(output.read_text(), f"present={str(expected).lower()}\n")

    def test_provider_restores_only_after_verified_graph_with_no_compatible_fallback(self):
        steps = workflow("agent-ci.yml")["jobs"]["provider"]["steps"]
        indices = {step.get("id"): i for i, step in enumerate(steps) if step.get("id")}
        test = next(i for i, step in enumerate(steps) if step.get("name") == "Test released rusqlite against the static provider")
        self.assertLess(indices["provider"], indices["environment"])
        self.assertLess(indices["environment"], indices["dependency-archive"])
        self.assertLess(indices["dependency-archive"], indices["provider-cache"])
        self.assertLess(indices["provider-cache"], test)
        self.assertLess(test, indices["dependency-seed"])
        self.assertIn('--cargo-config-dir "$PROVIDER_WORK"', steps[indices["environment"]]["run"])
        native = steps[indices["provider-cache"]]
        self.assertEqual(native["if"], "${{ steps.dependency-archive.outputs.restored != 'true' }}")
        self.assertNotIn("restore-keys", native["with"])
        self.assertEqual(native["with"]["path"], "${{ env.PROVIDER_TARGET }}")
        self.assertIn("steps.environment.outputs.legacy_compatibility", native["with"]["key"])
        self.assertIn("steps.provider.outputs.dependency-fingerprint", native["with"]["key"])
        self.assertIn("--phase prepare", steps[indices["provider"]]["run"])
        self.assertIn("--phase run", steps[test]["run"])
        for ident in ("dependency-archive", "dependency-seed"):
            self.assertIn('--dependency-fingerprint "$PROVIDER_DEPENDENCY_FINGERPRINT"', steps[indices[ident]]["run"])
            self.assertEqual(steps[indices[ident]]["env"]["PROVIDER_DEPENDENCY_FINGERPRINT"], "${{ steps.provider.outputs.dependency-fingerprint }}")
        self.assertIn('--metadata "$PROVIDER_WORK/maple-provider-metadata.json"', steps[indices["dependency-seed"]]["run"])
        self.assertFalse(any("actions/cache/save@" in step.get("uses", "") for step in steps))

    def test_durable_archive_restore_identity_and_trusted_fail_closed_upload(self):
        for filename, name in DEPENDENCY_BUILDERS:
            with self.subTest(workflow=filename, job=name):
                job = workflow(filename)["jobs"][name]
                self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
                self.assertEqual(job["env"]["DEPENDENCY_CACHE_JOB"], job["name"])
                steps = job["steps"]
                restore = next(step for step in steps if step.get("id") == "dependency-archive")
                pack = next(step for step in steps if step.get("id") == "dependency-seed")
                upload = next(step for step in steps if step.get("name") == "Upload durable dependency archive")
                self.assertIn("agent_dependency_archive.py restore", restore["run"])
                self.assertIn("agent_dependency_archive.py pack", pack["run"])
                for step in (restore, pack):
                    self.assertIn(f"--workflow {filename}", step["run"])
                    self.assertIn('--job-name "$DEPENDENCY_CACHE_JOB"', step["run"])
                    self.assertIn('--receipt "$RUNNER_TEMP/agent-dependency-receipt.json"', step["run"])
                    self.assertNotIn("continue-on-error", step)
                self.assertEqual(restore["env"]["GH_TOKEN"], "${{ github.token }}")
                self.assertNotIn("GH_TOKEN", pack.get("env", {}))
                self.assertEqual(pack["if"], TRUSTED_DEPENDENCY_WRITE)
                # A verified <24h receipt can suppress compression/upload. Only
                # the helper's positive needs-upload result permits a write.
                self.assertEqual(upload["if"], TRUSTED_DEPENDENCY_UPLOAD)
                self.assertEqual(upload["uses"], "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02")
                self.assertEqual(upload["with"], {
                    "name": "${{ steps.dependency-seed.outputs.artifact-name }}",
                    "path": "${{ runner.temp }}/agent-dependency-seed/manifest.json\n${{ runner.temp }}/agent-dependency-seed/dependencies.tar.gz\n",
                    "if-no-files-found": "error", "compression-level": 0,
                    "overwrite": True, "retention-days": 3,
                })
                self.assertNotIn("continue-on-error", upload)
                self.assertLess(steps.index(restore), steps.index(pack))
                self.assertLess(steps.index(pack), steps.index(upload))
                if name != "provider":
                    graph = next(step for step in steps if step.get("name") == "Verify the canonical SDK and proxy dependency graph")
                    self.assertIn('metadata="$RUNNER_TEMP/agent-workspace-metadata.json"', graph["run"])
                    self.assertNotIn("rm -f", graph["run"])
                    self.assertIn('--metadata "$RUNNER_TEMP/agent-workspace-metadata.json"', pack["run"])
                # Every earlier validation must succeed before pack/upload;
                # diagnostics remain the only explicitly always-run steps.
                for step in steps[:steps.index(pack)]:
                    self.assertNotIn("continue-on-error", step)
                for step in steps[steps.index(pack) + 1:]:
                    if "run" in step:
                        self.assertEqual(step.get("if"), "always()")
                        self.assertIn("agent_measure.py --summarize", step["run"])
                self.assertNotIn("always()", pack["if"])
                self.assertNotIn("always()", upload["if"])

    def test_dependency_archives_are_absent_from_protected_and_consumer_jobs(self):
        for name, job in workflow("agent-desktop-build.yml")["jobs"].items():
            if name in {"macos-build", "linux", "windows-build"}:
                continue
            for step in job["steps"]:
                self.assertNotIn("agent_dependency_archive.py", step.get("run", ""))
                self.assertNotIn(step.get("id"), {"dependency-archive", "dependency-seed"})

    def test_workspace_checks_and_plain_release_keep_original_coverage(self):
        jobs = workflow("agent-ci.yml")["jobs"]
        workspace = "\n".join(step.get("run", "") for step in jobs["desktop"]["steps"])
        for command in ("-- just ci", "cargo build --workspace --all-targets --locked --timings",
                        "cargo test --workspace --locked --timings", "./scripts/macos-debug-app.sh",
                        "scripts/audit-sqlcipher-dependencies.py", "dependencies/sqlcipher/verify_sqlcipher.py",
                        "scripts/verify-sqlcipher-build.py", "verify-agent-rust-deps.py"):
            self.assertIn(command, workspace)
        self.assertNotIn("test-rusqlite-provider.py", workspace)
        self.assertNotIn("cargo build --release", workspace)
        release = "\n".join(step.get("run", "") for step in jobs["linux-release"]["steps"])
        for command in ("cargo build --release -p maple-agent-app --locked --timings",
                        "generate-third-party-notices.py --check", "scripts/verify-sqlcipher-build.py",
                        "verify-agent-rust-deps.py"):
            self.assertIn(command, release)

    def test_namespaced_agent_releases_do_not_enter_research_jobs(self):
        release = workflow("release.yml")
        classifier = release["jobs"]["classify-app-release"]
        self.assertEqual(classifier["if"], "startsWith(github.event.release.tag_name, 'v')")
        # All release work remains downstream of the classifier, with GitHub's
        # default success gate. Skipping it skips builds and publishers together.
        for name, job in release["jobs"].items():
            if name == "classify-app-release":
                continue
            needs = job["needs"]
            if isinstance(needs, str):
                needs = [needs]
            self.assertIn("classify-app-release", needs)
            self.assertNotIn("if", job)
        for name, job_name in (
            ("pages-publish.yml", "production"),
            ("updates-publish.yml", "publish"),
            ("proxy-publish.yml", "prepare"),
            ("zapstore-publish.yml", "publish"),
        ):
            with self.subTest(workflow=name):
                condition = workflow(name)["jobs"][job_name]["if"]
                self.assertIn("startsWith(github.event.workflow_run.head_branch, 'v') &&", condition)
                self.assertIn("github.event.workflow_run.conclusion == 'success'", condition)
                self.assertIn("github.event.workflow_run.event == 'release'", condition)


class AgentDesktopPackagingBoundaryTests(unittest.TestCase):
    WORKFLOW = "agent-desktop-build.yml"
    MASTER_GUARD = (
        "github.repository == 'MaplePrivacyLabs/Maple' && "
        "github.ref == 'refs/heads/master' && "
        "(github.event_name == 'push' || github.event_name == 'workflow_dispatch')"
    )
    APPLE_SECRETS = {
        "APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_ID",
        "APPLE_ID_PASSWORD", "APPLE_TEAM_ID",
    }
    ATTEST_ACTION = "actions/attest@281a49d4cbb0a72c9575a50d18f6deb515a11deb"

    def test_windows_reuses_existing_environment_only_for_trusted_master_signing(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        signer = jobs["windows"]
        self.assertEqual(signer["environment"], "windows-signing")
        self.assertEqual(signer["runs-on"], "windows-2025")
        self.assertEqual(signer["needs"], "windows-build")
        self.assertIn(self.MASTER_GUARD, signer["if"])
        self.assertIn("needs.windows-build.result == 'success'", signer["if"])
        steps = signer["steps"]
        preflight = next(i for i, step in enumerate(steps) if "windows-release.py prepare" in step.get("run", ""))
        login = next(i for i, step in enumerate(steps) if step.get("uses", "").startswith("azure/login@"))
        package = next(i for i, step in enumerate(steps) if "windows-release.py package" in step.get("run", ""))
        self.assertLess(preflight, login)
        self.assertLess(login, package)
        self.assertEqual(steps[login]["with"], {
            "client-id": "${{ secrets.AZURE_CLIENT_ID }}",
            "tenant-id": "${{ secrets.AZURE_TENANT_ID }}",
            "subscription-id": "${{ secrets.AZURE_SUBSCRIPTION_ID }}",
        })
        self.assertEqual(steps[package]["env"], {
            "MAPLE_WINDOWS_ARTIFACT_SIGNING_" + name: "${{ secrets.AZURE_ARTIFACT_SIGNING_" + name + " }}"
            for name in ("ENDPOINT", "ACCOUNT_NAME", "CERTIFICATE_PROFILE_NAME")
        })
        self.assertNotIn("--unsigned", steps[package]["run"])
        for step in steps:
            self.assertNotRegex(step.get("run", ""), r"cargo\b|--smoke|maple-agent\.exe\s+--")
            self.assertNotIn("rust-cache@", step.get("uses", ""))
            self.assertNotIn("rust-toolchain@", step.get("uses", ""))
        self.assertFalse(any("TAURI_SIGNING" in value for value in strings(signer)))

    def test_windows_preview_and_native_verifier_have_no_signing_credentials(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        build = jobs["windows-build"]
        self.assertIn("target-feature=+crt-static", build["env"]["RUSTFLAGS"])
        steps = build["steps"]
        linker = next(i for i, step in enumerate(steps) if "agent_windows.py" in step.get("run", ""))
        fingerprint = next(i for i, step in enumerate(steps) if step.get("id") == "environment")
        self.assertLess(linker, fingerprint)
        self.assertTrue(any("test_windows_build_info.py" in step.get("run", "") for step in steps))
        release = next(step["run"] for step in steps
                       if step.get("name") == "Build Windows release binary and notices without signing credentials")
        # Native Windows CreateProcess cannot launch a Unix shebang script.
        self.assertIn('git_bash="$(cygpath -w "$BASH")"', release)
        self.assertIn('--target target -- "$git_bash" ./scripts/build-release.sh "$MAPLE_RELEASE_PROFILE"', release)
        self.assertNotIn('-- bash ', release)
        shell_probe = next(i for i, step in enumerate(steps)
                           if step.get("name") == "Smoke native Git Bash process launch")
        graph = next(i for i, step in enumerate(steps)
                     if step.get("name") == "Verify the canonical SDK and proxy dependency graph")
        self.assertLess(shell_probe, graph)
        upload = next(step["with"] for step in steps
                      if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertEqual(upload["path"].splitlines(), [
            "apps/maple-agent/target/release/maple-agent.exe",
            "apps/maple-agent/target/third-party-notices",
        ])
        for name in ("windows-unsigned", "windows"):
            download = next(step["with"] for step in jobs[name]["steps"]
                            if step.get("uses", "").startswith("actions/download-artifact@"))
            self.assertEqual(download, {"name": upload["name"], "path": "apps/maple-agent/target"})
        preview = jobs["windows-unsigned"]
        self.assertEqual(preview["strategy"]["matrix"], {"variant": ["dev"]})
        self.assertIn("github.event_name == 'pull_request'", preview["if"])
        self.assertNotIn("github.event_name == 'push'", preview["if"])
        self.assertTrue(any('package "$MAPLE_RELEASE_PROFILE" --unsigned' in step.get("run", "")
                            for step in preview["steps"]))
        verify = jobs["verify-windows"]
        self.assertEqual(verify["needs"], ["windows", "windows-unsigned"])
        self.assertIn("always() && !cancelled()", verify["if"])
        self.assertIn("needs.windows-unsigned.result == 'success'", verify["if"])
        self.assertIn("needs.windows.result == 'success'", verify["if"])
        smoke = next(step["run"] for step in verify["steps"] if "windows-release.py verify" in step.get("run", ""))
        self.assertIn("--smoke", smoke)
        self.assertIn("--unsigned", smoke)
        self.assertIn('"$GITHUB_EVENT_NAME" = pull_request', smoke)
        for name in ("windows-build", "windows-unsigned", "verify-windows"):
            self.assertNotIn("environment", jobs[name])
            self.assertFalse(any("secrets." in value for value in strings(jobs[name])))

    def test_windows_verifier_clears_runner_paths_without_hiding_signing_configuration(self):
        steps = workflow(self.WORKFLOW)["jobs"]["verify-windows"]["steps"]
        smoke = next(step["run"] for step in steps if "windows-release.py verify" in step.get("run", ""))
        paths = {"AZURE_CONFIG_DIR", "AZURE_DEVOPS_CACHE_DIR", "AZURE_EXTENSION_DIR"}
        credentials = {"AZURE_CLIENT_ID", "AZURE_FUTURE_TOKEN", "MAPLE_WINDOWS_ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME"}
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "environment-names.json"
            # Execute the real workflow script with a harmless Python probe in
            # place of the installer verifier. Record names, never values.
            probe = '''python3() {
              "$PROBE_PYTHON" -c 'import json, os; print(json.dumps(sorted(os.environ)))' > "$PROBE_OUTPUT"
            }
            '''
            subprocess.run(["bash", "-euo", "pipefail", "-c", probe + smoke], check=True,
                           env={**os.environ, **dict.fromkeys(paths | credentials, "fixture"),
                                "GITHUB_EVENT_NAME": "pull_request", "MAPLE_RELEASE_PROFILE": "dev",
                                "PROBE_PYTHON": sys.executable, "PROBE_OUTPUT": str(output)},
                           capture_output=True)
            remaining = set(json.loads(output.read_text()))
            self.assertFalse(paths & remaining)
            self.assertTrue(credentials <= remaining)

    def test_only_pr_updates_can_replace_an_in_progress_or_pending_build(self):
        concurrency = workflow(self.WORKFLOW)["concurrency"]
        self.assertEqual(concurrency, {
            "group": "maple-agent-desktop-builds-${{ github.event_name == 'pull_request' && github.ref || github.run_id }}",
            "cancel-in-progress": "${{ github.event_name == 'pull_request' }}",
        })

    def test_signing_recipes_and_embedded_profile_inputs_require_release_owner_review(self):
        owners = set((ROOT / ".github/CODEOWNERS").read_text().splitlines())
        for path in (
            "/apps/maple-agent/scripts/",
            "/apps/maple-agent/app/packaging/",
            "/apps/maple-agent/release_profile.rs",
            "/apps/maple-agent/release-profiles.json",
        ):
            with self.subTest(path=path):
                self.assertIn(path + " @AnthonyRonning", owners)

    def test_only_trusted_master_can_receive_apple_signing_credentials(self):
        config = workflow(self.WORKFLOW)
        self.assertEqual(config["name"], "Maple Agent Desktop Builds")
        self.assertEqual(config["on"], {
            "push": {"branches": ["master"]},
            "pull_request": None,
            "workflow_dispatch": None,
        })
        self.assertEqual(config["permissions"], {"contents": "read"})
        for value in strings({key: value for key, value in config.items() if key != "jobs"}):
            self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
        jobs = config["jobs"]
        signed = jobs["macos"]
        self.assertEqual(signed["environment"], "desktop-signing")
        self.assertEqual(signed["runs-on"], "macos-26")
        condition = " ".join(signed["if"].split())
        self.assertIn(self.MASTER_GUARD, condition)
        self.assertIn("always() && !cancelled()", condition)
        self.assertNotIn("needs.changes.outputs.agent", condition)
        secret_steps = [step for step in signed["steps"]
                        if any("secrets." in value for value in strings(step))]
        self.assertEqual(len(secret_steps), 1)
        package = secret_steps[0]
        self.assertIn("./scripts/package-release.sh", package["run"])
        self.assertNotIn("--unsigned", package["run"])
        self.assertEqual(package["env"], {
            name: "${{ secrets." + name + " }}" for name in self.APPLE_SECRETS
        })
        for name, job in jobs.items():
            with self.subTest(job=name):
                self.assertNotIn("uses", job)
                if name in ("macos", "attest-linux", "windows"):
                    self.assertEqual(job["permissions"], {
                        "contents": "read", "id-token": "write", "attestations": "write",
                    })
                    self.assertIn(self.MASTER_GUARD, " ".join(job["if"].split()))
                elif name in ("verify-macos", "verify-linux", "verify-windows"):
                    self.assertEqual(job["permissions"], {
                        "contents": "read", "attestations": "read",
                    })
                elif name in ("changes", "macos-build", "linux", "windows-build"):
                    self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
                else:
                    self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
                if name not in ("macos", "windows"):
                    self.assertNotIn("environment", job)
                    for step in job["steps"]:
                        if "gh attestation verify" in step.get("run", ""):
                            self.assertIn(name, ("verify-macos", "verify-linux", "verify-windows"))
                            self.assertEqual(step["env"], {"GH_TOKEN": "${{ github.token }}"})
                            if name in ("verify-linux", "verify-windows"):
                                self.assertIn(self.MASTER_GUARD, " ".join(step["if"].split()))
                        elif name in ("macos-build", "linux", "windows-build") and step.get("id") == "dependency-archive":
                            self.assertEqual(step["env"], {"GH_TOKEN": "${{ github.token }}"})
                            self.assertIn("agent_dependency_archive.py restore", step["run"])
                        elif name == "changes" and step.get("id") == "master":
                            self.assertEqual(step["if"],
                                             "${{ github.repository == 'MaplePrivacyLabs/Maple' && "
                                             "github.ref == 'refs/heads/master' && github.event_name == 'push' }}")
                            self.assertEqual(step["env"], {"GH_TOKEN": "${{ github.token }}"})
                            self.assertIn("agent_master_selection.py", step["run"])
                        else:
                            for value in strings(step):
                                self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
                self.assertFalse(any(step.get("continue-on-error") for step in job["steps"]))

    def test_actions_and_checkouts_do_not_grant_publication_or_persist_credentials(self):
        for job in workflow(self.WORKFLOW)["jobs"].values():
            for step in job["steps"]:
                self.assertNotIn("${{", step.get("run", ""))
                action = step.get("uses", "")
                if action:
                    self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)?@[0-9a-f]{40}$")
                    self.assertNotIn("release", action.lower())
                    if "attest" in action.lower():
                        self.assertEqual(action, self.ATTEST_ACTION)
                if action.startswith("actions/checkout@"):
                    self.assertEqual(step["with"]["ref"], "${{ github.sha }}")
                    self.assertIs(step["with"]["persist-credentials"], False)
                if action.startswith("DeterminateSystems/determinate-nix-action@"):
                    self.assertEqual(step["with"]["github-token"], "")
                if action.startswith("Swatinem/rust-cache@"):
                    cache = step["with"]
                    self.assertEqual(cache["workspaces"], "apps/maple-agent -> target")
                    self.assertTrue(cache["key"].startswith("maple-agent-release-"))
                    self.assertIn("${{ matrix.variant }}", cache["key"])
                    self.assertEqual(cache["save-if"], "false")

    def test_unsigned_pr_jobs_are_fail_safe_and_never_use_signed_macos_artifact_names(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        macos = " ".join(jobs["macos-unsigned"]["if"].split())
        self.assertIn("github.event_name == 'pull_request'", macos)
        self.assertNotIn("github.event_name == 'push'", macos)
        for name in ("macos-unsigned", "linux"):
            condition = " ".join(jobs[name]["if"].split())
            self.assertIn("always() && !cancelled()", condition)
            self.assertIn("needs.changes.result != 'success'", condition)
            self.assertIn("needs.changes.outputs.agent != 'false'", condition)
        self.assertIn(self.MASTER_GUARD, " ".join(jobs["linux"]["if"].split()))
        package = next(step for step in jobs["macos-unsigned"]["steps"]
                       if "./scripts/package-release.sh" in step.get("run", ""))
        self.assertIn("--unsigned", package["run"])
        verify = next(step for step in jobs["macos-unsigned"]["steps"]
                      if "./scripts/verify-release.sh" in step.get("run", ""))
        self.assertIn("--unsigned", verify["run"])

    def test_master_selection_and_manual_force_fall_back_to_building(self):
        classify = next(step for step in workflow(self.WORKFLOW)["jobs"]["changes"]["steps"]
                        if step.get("id") == "classify")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "outputs"
            for event, selected, expected in (
                ("push", "", "true"), ("push", "true", "true"),
                ("push", "unknown", "true"), ("push", "false", "false"),
                ("workflow_dispatch", "", "true"), ("workflow_dispatch", "false", "true"),
            ):
                with self.subTest(event=event, selected=selected):
                    output.unlink(missing_ok=True)
                    subprocess.run(["bash", "-c", classify["run"]], check=True,
                                   capture_output=True, cwd=directory,
                                   env={**os.environ, "GITHUB_EVENT_NAME": event,
                                        "GITHUB_OUTPUT": str(output), "BASE_SHA": "", "HEAD_SHA": "",
                                        "MASTER_AGENT": selected})
                    self.assertEqual(output.read_text(), f"agent={expected}\n")

    def test_pr_profiles_match_producers_and_consumers_while_master_keeps_both(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        for name in ("macos-build", "linux", "verify-linux", "windows-build", "verify-windows"):
            self.assertEqual(set(jobs[name]["strategy"]["matrix"]), {"variant"})
        for event, expected in (("pull_request", ["dev"]),
                                ("push", ["dev", "prod"]),
                                ("workflow_dispatch", ["dev", "prod"])):
            with self.subTest(event=event):
                for name in ("macos-build", "linux", "verify-linux", "windows-build", "verify-windows"):
                    self.assertEqual(variants_for(jobs[name], event), expected, name)
                macos_consumer = "macos-unsigned" if event == "pull_request" else "macos"
                self.assertEqual(variants_for(jobs[macos_consumer], event), expected)
                for producer, consumer in (("macos-build", macos_consumer),
                                           ("linux", "verify-linux"), ("windows-build", "verify-windows")):
                    self.assertEqual(variants_for(jobs[producer], event),
                                     variants_for(jobs[consumer], event))
        self.assertEqual(jobs["macos-unsigned"]["strategy"]["matrix"], {"variant": ["dev"]})
        for name in ("macos", "attest-linux", "verify-macos", "windows"):
            self.assertEqual(jobs[name]["strategy"]["matrix"], {"variant": ["dev", "prod"]})

    def test_profiles_and_artifacts_keep_platform_variant_and_run_identity(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        for name, platform in (
            ("macos-build", "macos-aarch64-prebuilt"),
            ("macos", "macos-aarch64"),
            ("macos-unsigned", "macos-aarch64-unsigned"),
            ("linux", "linux-x86_64"),
        ):
            with self.subTest(job=name):
                job = jobs[name]
                self.assertIs(job["strategy"]["fail-fast"], False)
                self.assertEqual(job["env"]["MAPLE_RELEASE_PROFILE"], "${{ matrix.variant }}")
                script = "build-release.sh" if name in ("macos-build", "linux") else "verify-prebuilt-release.sh"
                profile_step = next(step for step in job["steps"]
                                    if "./scripts/" + script in step.get("run", ""))
                self.assertIn('"$MAPLE_RELEASE_PROFILE"', profile_step["run"])
                uploads = [step for step in job["steps"]
                           if step.get("uses", "").startswith("actions/upload-artifact@")
                           and step.get("with", {}).get("path") != "apps/maple-agent/target/ci-metrics/"
                           and step.get("name") != "Upload durable dependency archive"]
                self.assertEqual(len(uploads), 1)
                upload = uploads[0]
                self.assertNotIn("if", upload)
                self.assertEqual(upload["with"]["name"],
                                 "maple-agent-${{ matrix.variant }}-" + platform +
                                 "-${{ github.run_id }}")
                self.assertIs(upload["with"]["overwrite"], True)
                expected_paths = (["apps/maple-agent/target/release/maple-agent",
                                   "apps/maple-agent/target/third-party-notices"] if name == "macos-build"
                                  else ["apps/maple-agent/dist/${{ matrix.variant }}/"])
                self.assertEqual(upload["with"]["path"].splitlines(), expected_paths)
                self.assertEqual(upload["with"]["if-no-files-found"], "error")
                self.assertEqual(upload["with"]["retention-days"], 7 if name == "macos-build" else 30)

    def test_provenance_is_mandatory_and_only_minted_by_trusted_master_jobs(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        for name, job in jobs.items():
            attests = [i for i, step in enumerate(job["steps"])
                       if step.get("uses") == self.ATTEST_ACTION]
            with self.subTest(job=name):
                if name not in ("macos", "attest-linux", "windows"):
                    self.assertEqual(attests, [])
                    self.assertNotIn("id-token", job.get("permissions", {}))
                    self.assertIn(job.get("permissions", {}).get("attestations"), (None, "read"))
                    continue
                self.assertEqual(len(attests), 1)
                self.assertIn(self.MASTER_GUARD, " ".join(job["if"].split()))
                attest = job["steps"][attests[0]]
                self.assertNotIn("if", attest)
                self.assertNotIn("continue-on-error", attest)
                if name in ("macos", "windows"):
                    package = next(i for i, step in enumerate(job["steps"])
                                   if ("./scripts/package-release.sh" if name == "macos" else "windows-release.py package") in step.get("run", ""))
                    upload = next(i for i, step in enumerate(job["steps"])
                                  if step.get("uses", "").startswith("actions/upload-artifact@"))
                    self.assertGreater(attests[0], package)
                    self.assertLess(attests[0], upload)
                    self.assertEqual(attest["with"], {
                        "subject-path": "apps/maple-agent/dist/${{ matrix.variant }}/*",
                    })
                else:
                    # PR compilation and packaging have no OIDC permission.
                    # A separate job attests downloaded trusted-master outputs.
                    self.assertEqual(job["needs"], "linux")
                    self.assertIn("needs.linux.result == 'success'", job["if"])
                    self.assertEqual(attest["with"], {"subject-path": "artifacts/*"})
                    linux_upload = next(step["with"] for step in jobs["linux"]["steps"]
                                        if step.get("uses", "").startswith("actions/upload-artifact@"))
                    download = next(i for i, step in enumerate(job["steps"])
                                    if step.get("uses", "").startswith("actions/download-artifact@"))
                    self.assertLess(download, attests[0])
                    self.assertEqual(job["steps"][download]["with"], {
                        "name": linux_upload["name"], "path": "artifacts",
                    })
                    scanner_build = next(i for i, step in enumerate(job["steps"])
                                         if "cargo build --locked -p maple-agent-updater --example validate-release-archive"
                                         in step.get("run", ""))
                    metadata = next(i for i, step in enumerate(job["steps"])
                                    if "release-info.py verify" in step.get("run", ""))
                    scanner = next(i for i, step in enumerate(job["steps"])
                                   if "./scripts/verify-updater-archive.sh" in step.get("run", ""))
                    self.assertLess(scanner_build, scanner)
                    self.assertLess(download, metadata)
                    self.assertLess(metadata, scanner)
                    self.assertLess(scanner, attests[0])
                    self.assertNotIn("if", job["steps"][scanner_build])
                    self.assertNotIn("if", job["steps"][scanner])
                    self.assertIn('--profile "$MAPLE_RELEASE_PROFILE"', job["steps"][scanner]["run"])
                    self.assertIn("--platform linux-x86_64", job["steps"][scanner]["run"])
                    self.assertFalse(any("verify-release.sh" in step.get("run", "")
                                         or "AppRun" in step.get("run", "")
                                         for step in job["steps"]),
                                     "attestation inspects data without executing artifact code")

    def test_linux_official_bundle_reuses_one_profile_build_and_has_no_appimage_runtime(self):
        job = workflow(self.WORKFLOW)["jobs"]["linux"]
        self.assertEqual(job["name"], "Linux bundle (${{ matrix.variant }})")
        builds = [step for step in job["steps"]
                  if "./scripts/build-release.sh" in step.get("run", "")]
        self.assertEqual(len(builds), 1)
        self.assertFalse(any("cargo build" in step.get("run", "") for step in job["steps"]))
        package = (ROOT / "apps/maple-agent/scripts/package-release.sh").read_text()
        self.assertIn('scripts/linux-release-bundle.sh" "$binary" "$staging/${name}.tar.gz"', package)
        self.assertIn('export MAPLE_PACKAGE_SOURCE_SHA="$source_sha"', package)
        self.assertNotIn("linux-release-appimage", package)

    def test_macos_packaging_uses_verified_prebuilt_binaries_on_fresh_runners(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        build = jobs["macos-build"]
        self.assertNotIn("environment", build)
        build_condition = " ".join(build["if"].split())
        self.assertIn(self.MASTER_GUARD, build_condition)
        self.assertIn("needs.changes.result != 'success'", build_condition)
        self.assertIn("needs.changes.outputs.agent != 'false'", build_condition)
        self.assertTrue(any(step.get("uses", "").startswith("Swatinem/rust-cache@")
                            for step in build["steps"]))
        upload = next(step["with"] for step in build["steps"]
                      if step.get("uses", "").startswith("actions/upload-artifact@"))
        for name in ("macos", "macos-unsigned"):
            with self.subTest(job=name):
                job = jobs[name]
                self.assertIn("macos-build", job["needs"])
                self.assertIn("needs.macos-build.result == 'success'", job["if"])
                steps = job["steps"]
                self.assertFalse(any("rust-cache@" in step.get("uses", "") for step in steps))
                self.assertFalse(any("build-release.sh" in step.get("run", "") for step in steps))
                scanner_builds = [i for i, step in enumerate(steps) if "cargo" in step.get("run", "")]
                self.assertEqual(len(scanner_builds), 1)
                scanner_build = scanner_builds[0]
                self.assertEqual(steps[scanner_build]["run"], "nix develop --no-update-lock-file . -c cargo build --locked -p maple-agent-updater --example validate-release-archive")
                self.assertEqual(steps[scanner_build]["if"], "${{ matrix.variant == 'prod' }}")
                self.assertNotIn("env", steps[scanner_build], "scanner compilation must receive no signing credentials")
                download = next(i for i, step in enumerate(steps)
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                self.assertEqual(steps[download]["with"], {
                    "name": upload["name"], "path": "apps/maple-agent/target",
                })
                verify = next(i for i, step in enumerate(steps)
                              if "./scripts/verify-prebuilt-release.sh" in step.get("run", ""))
                package = next(i for i, step in enumerate(steps)
                               if "./scripts/package-release.sh" in step.get("run", ""))
                scanner = next(i for i, step in enumerate(steps)
                               if "./scripts/verify-updater-archive.sh" in step.get("run", ""))
                self.assertLess(scanner_build, package)
                self.assertGreater(scanner, package)
                self.assertEqual(steps[scanner]["if"], "${{ matrix.variant == 'prod' }}")
                self.assertNotIn("env", steps[scanner], "archive data inspection must receive no signing credentials")
                self.assertIn('"dist/$MAPLE_RELEASE_PROFILE"', steps[scanner]["run"])
                self.assertEqual("--unsigned" in steps[scanner]["run"], name == "macos-unsigned")
                for index, step in enumerate(steps):
                    if step.get("uses", "").startswith(("actions/upload-artifact@", "actions/attest@")):
                        self.assertLess(scanner, index, "actual archive must pass the installer parser before publication")
                self.assertGreater(verify, download)
                self.assertGreater(package, verify)
                self.assertNotIn("env", steps[verify])
                self.assertIn('"$MAPLE_RELEASE_PROFILE"', steps[verify]["run"])

    def test_prebuilt_notice_payload_preserves_binary_and_target_graph_layout(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        build_steps = jobs["macos-build"]["steps"]
        build = next(i for i, step in enumerate(build_steps)
                     if "./scripts/build-release.sh" in step.get("run", ""))
        upload = next(i for i, step in enumerate(build_steps)
                      if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertLess(build, upload)
        uploaded_paths = [Path(path) for path in build_steps[upload]["with"]["path"].splitlines()]
        # upload-artifact preserves paths relative to their least common ancestor.
        # Adding a notices directory must not move the downloaded executable out
        # of target/release or disconnect target-specific inventories from it.
        common_root = Path(os.path.commonpath(uploaded_paths))
        self.assertEqual(common_root, Path("apps/maple-agent/target"))
        binary = Path("apps/maple-agent/target/release/maple-agent")
        notices = Path("apps/maple-agent/target/third-party-notices/aarch64-apple-darwin")
        payload = [binary, *(notices / name for name in
                            ("manifest.json", "THIRD_PARTY_NOTICES.html", "THIRD_PARTY_NOTICES.txt"))]
        self.assertIn(binary, uploaded_paths)
        self.assertIn(notices.parent, uploaded_paths)
        for name in ("macos", "macos-unsigned"):
            with self.subTest(job=name):
                download = next(step["with"] for step in jobs[name]["steps"]
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                destination = Path(download["path"])
                reconstructed = [destination / path.relative_to(common_root) for path in payload]
                self.assertEqual(reconstructed, payload)

    def test_partial_producer_and_verifier_only_retries_reuse_successful_artifacts(self):
        jobs = workflow(self.WORKFLOW)["jobs"]

        def artifact_name(template, variant, attempt):
            for expression, value in (
                ("matrix.variant", variant),
                ("github.run_id", "12345"),
                ("github.run_attempt", str(attempt)),
            ):
                template = template.replace("${{ " + expression + " }}", value)
            self.assertNotIn("${{", template)
            return template

        for producer, verifier in (
            ("macos-build", "macos"), ("macos-build", "macos-unsigned"),
            ("macos", "verify-macos"), ("linux", "verify-linux"),
        ):
            with self.subTest(producer=producer):
                upload = next(step["with"] for step in jobs[producer]["steps"]
                              if step.get("uses", "").startswith("actions/upload-artifact@"))
                download = next(step["with"] for step in jobs[verifier]["steps"]
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                artifacts = {}

                def publish(variant, attempt):
                    name = artifact_name(upload["name"], variant, attempt)
                    if name in artifacts:
                        self.assertIs(upload.get("overwrite"), True)
                    artifacts[name] = (variant, attempt)

                # Attempt 1 retained Dev; Prod failed before publishing. Retry
                # runs only Prod and both dependent verification variants.
                publish("dev", 1)
                publish("prod", 2)
                for attempt in (2, 3):
                    # Attempt 3 retries verification alone, without rebuilding.
                    for variant, producer_attempt in (("dev", 1), ("prod", 2)):
                        name = artifact_name(download["name"], variant, attempt)
                        self.assertIn(name, artifacts)
                        self.assertEqual(artifacts[name], (variant, producer_attempt))

                # A subsequent producer retry replaces only its own artifact.
                publish("prod", 4)
                self.assertEqual(len(artifacts), 2)
                self.assertEqual(artifacts[artifact_name(download["name"], "prod", 4)], ("prod", 4))
                self.assertEqual(artifacts[artifact_name(download["name"], "dev", 4)], ("dev", 1))

    def test_downloaded_packages_are_verified_without_signing_credentials(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        for name, build in (("verify-macos", "macos"), ("verify-linux", "linux")):
            with self.subTest(job=name):
                job = jobs[name]
                if name == "verify-macos":
                    self.assertEqual(job["needs"], build)
                    self.assertNotIn("if", job)  # Failed/skipped builds cannot enter verification.
                else:
                    self.assertEqual(job["needs"], ["linux", "attest-linux"])
                    condition = " ".join(job["if"].split())
                    self.assertIn("always() && !cancelled()", condition)
                    self.assertIn("needs.linux.result == 'success'", condition)
                    self.assertIn("github.event_name == 'pull_request' ||", condition)
                    self.assertIn("needs.attest-linux.result == 'success'", condition)
                    self.assertIn(self.MASTER_GUARD, condition)
                self.assertNotIn("environment", job)
                upload = next(step for step in jobs[build]["steps"]
                              if step.get("uses", "").startswith("actions/upload-artifact@"))
                download = next(i for i, step in enumerate(job["steps"])
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                self.assertEqual(job["steps"][download]["with"], {
                    "name": upload["with"]["name"], "path": "artifacts",
                })
                verify = next(i for i, step in enumerate(job["steps"])
                              if "./scripts/verify-release.sh" in step.get("run", ""))
                self.assertGreater(verify, download)
                provenance = next(i for i, step in enumerate(job["steps"])
                                  if "gh attestation verify" in step.get("run", ""))
                self.assertGreater(provenance, download)
                self.assertLess(provenance, verify)
                if name == "verify-macos":
                    self.assertNotIn("--unsigned", job["steps"][verify]["run"])
                else:
                    scanner_build = next(i for i, step in enumerate(job["steps"])
                                         if "cargo build --locked -p maple-agent-updater --example validate-release-archive"
                                         in step.get("run", ""))
                    scanner = next(i for i, step in enumerate(job["steps"])
                                   if "./scripts/verify-updater-archive.sh" in step.get("run", ""))
                    self.assertLess(provenance, scanner_build)
                    self.assertLess(scanner_build, scanner)
                    self.assertLess(download, scanner)
                    self.assertLess(scanner, verify)
                    self.assertNotIn("if", job["steps"][scanner_build])
                    self.assertNotIn("if", job["steps"][scanner])
        smoke = next(step for step in jobs["verify-linux"]["steps"]
                     if "docker run" in step.get("run", ""))["run"]
        self.assertRegex(smoke, r"ubuntu@sha256:[0-9a-f]{64}")
        self.assertIn("--platform linux/amd64", smoke)
        self.assertIn("--user 65534:65534", smoke)
        self.assertIn("--network none", smoke)
        self.assertIn("--read-only", smoke)
        self.assertRegex(smoke, r"--tmpfs /tmp:[^\s]*mode=1777")
        self.assertIn("--build-info", smoke)
        self.assertIn("actual != expected", smoke)
        self.assertIn("tar -xzpf /artifacts/*-linux-x86_64.tar.gz", smoke)
        self.assertIn("./maple-agent*.app/AppRun", smoke)
        self.assertIn("/bin/sh /checks/smoke-linux-bundle-host.sh ./maple-agent*.app/AppRun >&2", smoke)
        self.assertIn("--volume \"$PWD/apps/maple-agent/scripts/smoke-linux-bundle-host.sh:/checks/smoke-linux-bundle-host.sh:ro\"", smoke)
        self.assertIn("test -d /etc/ssl/certs", smoke)
        self.assertIn("--volume /etc/ssl/certs:/etc/ssl/certs:ro", smoke)
        self.assertNotIn("--appimage-extract", smoke)

    def test_downloaded_provenance_enforces_workflow_ref_commit_and_every_file(self):
        for name in ("verify-macos", "verify-linux", "verify-windows"):
            step = next(step for step in workflow(self.WORKFLOW)["jobs"][name]["steps"]
                        if "gh attestation verify" in step.get("run", ""))
            with self.subTest(job=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                artifacts = root / "artifacts"
                artifacts.mkdir()
                files = ("package with spaces.AppImage", "release-manifest.json", "SHA256SUMS")
                for file in files:
                    (artifacts / file).write_text("fixture\n")
                calls = root / "calls"
                command = r'''gh() { printf '%s\0' "$@" >> "$CALLS"; return "${GH_RESULT:-0}"; }
''' + step["run"]
                env = {**os.environ, "CALLS": str(calls), "GITHUB_REPOSITORY": "MaplePrivacyLabs/Maple",
                       "GITHUB_SHA": "a" * 40}
                subprocess.run(["bash", "-c", command], check=True, capture_output=True,
                               cwd=root, env=env)
                arguments = [value.decode() for value in calls.read_bytes().split(b"\0")[:-1]]
                expected = []
                for file in sorted(files):
                    expected.extend([
                        "attestation", "verify", "artifacts/" + file,
                        "--repo", "MaplePrivacyLabs/Maple",
                        "--signer-workflow", "MaplePrivacyLabs/Maple/.github/workflows/agent-desktop-build.yml",
                        "--source-ref", "refs/heads/master", "--source-digest", "a" * 40,
                        "--deny-self-hosted-runners",
                    ])
                self.assertEqual(arguments, expected)
                result = subprocess.run(["bash", "-c", command], capture_output=True,
                                        cwd=root, env={**env, "GH_RESULT": "42"})
                self.assertEqual(result.returncode, 42)
                for file in artifacts.iterdir():
                    file.unlink()
                result = subprocess.run(["bash", "-c", command], capture_output=True, cwd=root, env=env)
                self.assertNotEqual(result.returncode, 0)  # Empty downloads cannot pass.

    def test_linux_pr_packages_and_both_verifiers_use_unsigned_mode_only_for_pr_events(self):
        jobs = workflow(self.WORKFLOW)["jobs"]
        for job_name, script, directory in (
            ("linux", "package-release.sh", None),
            ("linux", "verify-release.sh", "dist/{profile}"),
            ("verify-linux", "verify-release.sh", "../../artifacts"),
        ):
            step = next(step for step in jobs[job_name]["steps"]
                        if "./scripts/" + script in step.get("run", ""))
            for profile in ("dev", "prod"):
                for event in ("pull_request", "push", "workflow_dispatch"):
                    with self.subTest(job=job_name, script=script, profile=profile, event=event):
                        # Capture Nix's arguments without executing package or signing commands.
                        command = r'''nix() { printf '%s\0' "$@"; }
''' + step["run"]
                        result = subprocess.run(["bash", "-c", command], check=True,
                                                capture_output=True,
                                                env={**os.environ, "GITHUB_EVENT_NAME": event,
                                                     "MAPLE_RELEASE_PROFILE": profile})
                        arguments = [value.decode() for value in result.stdout.split(b"\0")[:-1]]
                        script_index = arguments.index("./scripts/" + script)
                        expected = [profile]
                        if directory is not None:
                            expected.append(directory.format(profile=profile))
                        if event == "pull_request":
                            expected.append("--unsigned")
                        if script == "verify-release.sh":
                            expected.extend(["--platform", "linux-x86_64"])
                        self.assertEqual(arguments[script_index + 1:], expected)

        scanner = next(step for step in jobs["verify-linux"]["steps"]
                       if "./scripts/verify-updater-archive.sh" in step.get("run", ""))
        for profile in ("dev", "prod"):
            for event in ("pull_request", "push", "workflow_dispatch"):
                with self.subTest(scanner_profile=profile, event=event):
                    command = r'''nix() { printf '%s\0' "$@"; }
''' + scanner["run"]
                    result = subprocess.run(["bash", "-c", command], check=True, capture_output=True,
                                            env={**os.environ, "GITHUB_EVENT_NAME": event,
                                                 "MAPLE_RELEASE_PROFILE": profile})
                    arguments = [value.decode() for value in result.stdout.split(b"\0")[:-1]]
                    script_index = arguments.index("./scripts/verify-updater-archive.sh")
                    expected = ["../../artifacts", "--profile", profile, "--platform", "linux-x86_64"]
                    if event == "pull_request":
                        expected.append("--unsigned")
                    self.assertEqual(arguments[script_index + 1:], expected)


class AgentDiffSelectionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "-q", "-b", "master")
        self.git("config", "user.email", "ci@example.invalid")
        self.git("config", "user.name", "CI fixture")
        self.git("config", "core.hooksPath", "/dev/null")
        scripts = self.root / "scripts/ci"
        scripts.mkdir(parents=True)
        for name in ("agent_change_detection.py", "change_detection.py"):
            shutil.copyfile(ROOT / "scripts/ci" / name, scripts / name)
        self.base = self.commit_file("README.md", "initial\n")

    def git(self, *arguments):
        result = subprocess.run(["git", *arguments], cwd=self.root, check=True,
                                capture_output=True, text=True)
        return result.stdout.strip()

    def commit_file(self, path, text):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git("add", "--", path)
        self.git("commit", "-qm", "fixture change")
        return self.git("rev-parse", "HEAD")

    def select(self, event, base, head, *, repository="fork/Maple", ref="refs/heads/topic", master=""):
        step = next(step for step in workflow("agent-ci.yml")["jobs"]["changes"]["steps"]
                    if step.get("id") == "classify")
        output = self.root / "output"
        output.unlink(missing_ok=True)
        env = {**os.environ, "GITHUB_EVENT_NAME": event, "BASE_SHA": base, "HEAD_SHA": head,
               "GITHUB_REPOSITORY": repository, "GITHUB_REF": ref, "MASTER_AGENT": master,
               "GITHUB_OUTPUT": str(output)}
        result = subprocess.run(["bash", "-c", step["run"]], cwd=self.root, env=env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return output.read_text()

    def test_docs_only_push_and_agent_runtime_push(self):
        docs = self.commit_file("apps/maple-agent/docs/design.md", "design\n")
        self.assertEqual(self.select("push", self.base, docs), "agent=false\n")
        runtime = self.commit_file("apps/maple-agent/app/src/main.rs", "fn main() {}\n")
        self.assertEqual(self.select("push", docs, runtime), "agent=true\n")

    def test_canonical_master_preserves_unvalidated_work_and_omits_only_proven_history(self):
        docs = self.commit_file("apps/maple-agent/docs/design.md", "design\n")
        for selected, expected in (("", "true"), ("true", "true"),
                                   ("unknown", "true"), ("false", "false")):
            with self.subTest(selected=selected):
                self.assertEqual(self.select("push", self.base, docs,
                                             repository="MaplePrivacyLabs/Maple",
                                             ref="refs/heads/master", master=selected),
                                 f"agent={expected}\n")

    def test_pull_request_uses_merge_base_instead_of_unrelated_base_changes(self):
        master = self.commit_file("apps/maple-agent/app/src/main.rs", "fn main() {}\n")
        self.git("checkout", "-qb", "contributor", self.base)
        docs = self.commit_file("apps/maple-agent/docs/design.md", "design\n")
        self.assertEqual(self.select("pull_request", master, docs), "agent=false\n")

    def test_deletion_or_rename_out_of_component_still_builds(self):
        runtime = self.commit_file("apps/maple-agent/app/src/main.rs", "fn main() {}\n")
        self.git("mv", "apps/maple-agent/app/src/main.rs", "README.md.moved")
        self.git("commit", "-qm", "rename fixture")
        self.assertEqual(self.select("push", runtime, self.git("rev-parse", "HEAD")), "agent=true\n")

    def test_missing_history_manual_event_and_classifier_failure_select_build(self):
        for event, base, head in (
            ("push", "0" * 40, self.base),
            ("push", "a" * 40, self.base),
            ("workflow_dispatch", "", ""),
        ):
            with self.subTest(event=event, base=base):
                self.assertEqual(self.select(event, base, head), "agent=true\n")
        (self.root / "scripts/ci/agent_change_detection.py").write_text("raise RuntimeError('fixture')\n")
        self.assertEqual(self.select("push", self.base, self.base), "agent=true\n")


if __name__ == "__main__":
    unittest.main()
