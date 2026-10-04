"""Security, component selection, and Research release isolation for Agent CI."""

import functools
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


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


class AgentWorkflowBoundaryTests(unittest.TestCase):
    def test_contributor_build_has_no_credentials_or_privileged_events(self):
        config = workflow("agent-ci.yml")
        self.assertEqual(set(config["on"]), {"push", "pull_request", "workflow_dispatch"})
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        self.assertEqual(config["on"]["pull_request"]["branches"], ["master"])
        self.assertEqual(config["permissions"], {"contents": "read"})
        for value in strings(config):
            self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
        for job in config["jobs"].values():
            self.assertNotIn("environment", job)
            self.assertNotIn("uses", job)
            self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
            for step in job["steps"]:
                self.assertNotIn("${{", step.get("run", ""))
                action = step.get("uses", "")
                if action:
                    self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                if action.startswith("actions/checkout@"):
                    self.assertIs(step["with"]["persist-credentials"], False)
                if action.startswith("DeterminateSystems/nix-installer-action@"):
                    self.assertEqual(step["with"]["github-token"], "")

    def test_agent_cache_and_artifacts_do_not_share_research_publication(self):
        steps = workflow("agent-ci.yml")["jobs"]["desktop"]["steps"]
        caches = [step["with"] for step in steps if "rust-cache@" in step.get("uses", "")]
        self.assertTrue(caches)
        for cache in caches:
            self.assertEqual(cache["workspaces"], "apps/maple-agent -> target")
            self.assertTrue(cache["key"].startswith("maple-agent-"))
            self.assertEqual(cache["save-if"],
                             "${{ github.event_name == 'push' && github.ref == 'refs/heads/master' }}")
        for step in steps:
            action = step.get("uses", "")
            self.assertNotIn("release", action.lower())
            self.assertNotIn("download-artifact", action)
            if "upload-artifact@" in action:
                self.assertTrue(step["with"]["name"].startswith("maple-agent-"))
                self.assertTrue(step["with"]["path"].startswith("apps/maple-agent/target/"))
        self.assertFalse(any((ROOT / "apps/maple-agent/.github/workflows").glob("*.yml")))

    def test_failed_or_missing_selection_cannot_skip_the_desktop_matrix(self):
        condition = workflow("agent-ci.yml")["jobs"]["desktop"]["if"]
        self.assertIn("always() && !cancelled()", condition)
        self.assertIn("needs.changes.result != 'success'", condition)
        self.assertIn("needs.changes.outputs.agent != 'false'", condition)
        self.assertNotIn("head.repo", condition)

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
            ("pages-production.yml", "promote"),
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
            "pull_request": {"branches": ["master"]},
            "workflow_dispatch": None,
        })
        self.assertEqual(config["permissions"], {"contents": "read"})
        for value in strings({key: value for key, value in config.items() if key != "jobs"}):
            self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
        jobs = config["jobs"]
        signed = jobs["macos"]
        self.assertEqual(signed["environment"], "desktop-signing")
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
                self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
                if name != "macos":
                    self.assertNotIn("environment", job)
                    for value in strings(job):
                        self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
                self.assertFalse(any(step.get("continue-on-error") for step in job["steps"]))

    def test_actions_and_checkouts_do_not_grant_publication_or_persist_credentials(self):
        for job in workflow(self.WORKFLOW)["jobs"].values():
            for step in job["steps"]:
                self.assertNotIn("${{", step.get("run", ""))
                action = step.get("uses", "")
                if action:
                    self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                    self.assertNotIn("release", action.lower())
                    self.assertNotIn("attest", action.lower())
                if action.startswith("actions/checkout@"):
                    self.assertEqual(step["with"]["ref"], "${{ github.sha }}")
                    self.assertIs(step["with"]["persist-credentials"], False)
                if action.startswith("DeterminateSystems/nix-installer-action@"):
                    self.assertEqual(step["with"]["github-token"], "")
                if action.startswith("Swatinem/rust-cache@"):
                    cache = step["with"]
                    self.assertEqual(cache["workspaces"], "apps/maple-agent -> target")
                    self.assertTrue(cache["key"].startswith("maple-agent-release-"))
                    self.assertIn("${{ matrix.variant }}", cache["key"])
                    self.assertEqual(cache["save-if"],
                                     "${{ github.event_name == 'push' && github.ref == 'refs/heads/master' }}")

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

    def test_master_and_manual_packaging_do_not_depend_on_available_diff_history(self):
        classify = next(step for step in workflow(self.WORKFLOW)["jobs"]["changes"]["steps"]
                        if step.get("id") == "classify")
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "outputs"
            for event in ("push", "workflow_dispatch"):
                with self.subTest(event=event):
                    output.unlink(missing_ok=True)
                    subprocess.run(["bash", "-c", classify["run"]], check=True,
                                   capture_output=True, cwd=directory,
                                   env={**os.environ, "GITHUB_EVENT_NAME": event,
                                        "GITHUB_OUTPUT": str(output), "BASE_SHA": "", "HEAD_SHA": ""})
                    self.assertEqual(output.read_text(), "agent=true\n")

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
                self.assertEqual(job["strategy"]["matrix"], {"variant": ["dev", "prod"]})
                self.assertIs(job["strategy"]["fail-fast"], False)
                self.assertEqual(job["env"]["MAPLE_RELEASE_PROFILE"], "${{ matrix.variant }}")
                script = "build-release.sh" if name in ("macos-build", "linux") else "verify-prebuilt-release.sh"
                profile_step = next(step for step in job["steps"]
                                    if "./scripts/" + script in step.get("run", ""))
                self.assertIn('"$MAPLE_RELEASE_PROFILE"', profile_step["run"])
                uploads = [step for step in job["steps"]
                           if step.get("uses", "").startswith("actions/upload-artifact@")]
                self.assertEqual(len(uploads), 1)
                upload = uploads[0]
                self.assertNotIn("if", upload)
                self.assertEqual(upload["with"]["name"],
                                 "maple-agent-${{ matrix.variant }}-" + platform +
                                 "-${{ github.run_id }}")
                self.assertIs(upload["with"]["overwrite"], True)
                expected_path = ("apps/maple-agent/target/release/maple-agent" if name == "macos-build"
                                 else "apps/maple-agent/dist/${{ matrix.variant }}/")
                self.assertEqual(upload["with"]["path"], expected_path)
                self.assertEqual(upload["with"]["if-no-files-found"], "error")

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
                for step in steps:
                    self.assertNotRegex(step.get("run", ""), r"\bcargo\b")
                download = next(i for i, step in enumerate(steps)
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                self.assertEqual(steps[download]["with"], {
                    "name": upload["name"], "path": "apps/maple-agent/target/release",
                })
                verify = next(i for i, step in enumerate(steps)
                              if "./scripts/verify-prebuilt-release.sh" in step.get("run", ""))
                package = next(i for i, step in enumerate(steps)
                               if "./scripts/package-release.sh" in step.get("run", ""))
                self.assertGreater(verify, download)
                self.assertGreater(package, verify)
                self.assertNotIn("env", steps[verify])
                self.assertIn('"$MAPLE_RELEASE_PROFILE"', steps[verify]["run"])

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
                self.assertEqual(job["needs"], build)
                self.assertNotIn("if", job)  # Failed/skipped builds cannot enter verification.
                self.assertNotIn("environment", job)
                self.assertEqual(job["strategy"]["matrix"], {"variant": ["dev", "prod"]})
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
                if name == "verify-macos":
                    self.assertNotIn("--unsigned", job["steps"][verify]["run"])
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

    def select(self, event, base, head):
        step = next(step for step in workflow("agent-ci.yml")["jobs"]["changes"]["steps"]
                    if step.get("id") == "classify")
        output = self.root / "output"
        output.unlink(missing_ok=True)
        env = {**os.environ, "GITHUB_EVENT_NAME": event, "BASE_SHA": base, "HEAD_SHA": head,
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
