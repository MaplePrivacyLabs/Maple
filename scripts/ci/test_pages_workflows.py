"""Regression checks for the GitHub Actions credential and source boundaries."""

import copy
import functools
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"


@functools.cache
def workflow(name):
    result = subprocess.run(
        ["yq", "-o=json", ".", str(WORKFLOWS / name)],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def normalized(value):
    return " ".join(value.split())


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for child in value.values():
            yield from strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)


class PagesWorkflowTests(unittest.TestCase):
    def assert_no_secrets(self, value):
        for text in strings(value):
            self.assertNotRegex(text, r"\bsecrets\b")

    def test_build_is_unprivileged_and_skips_forks(self):
        build = workflow("pages-preview-build.yml")
        self.assertEqual(build["name"], "Pages preview build")
        self.assertEqual(set(build["on"]), {"push", "pull_request"})
        self.assertEqual(build["permissions"], {"contents": "read"})
        self.assert_no_secrets(build)
        self.assertEqual(set(build["jobs"]), {"build-preview"})
        job = build["jobs"]["build-preview"]
        self.assertNotIn("permissions", job)
        self.assertNotIn("environment", job)
        self.assertEqual(
            normalized(job["if"]),
            "github.event_name == 'push' || "
            "(github.event_name == 'pull_request' && "
            "github.event.pull_request.head.repo.full_name == github.repository)",
        )
        self.assertEqual(
            job["env"]["SOURCE_SHA"],
            "${{ github.event_name == 'pull_request' && "
            "github.event.pull_request.head.sha || github.sha }}",
        )
        checkouts = [
            step for step in job["steps"]
            if step.get("uses", "").startswith("actions/checkout@")
        ]
        self.assertEqual(len(checkouts), 2)
        self.assertEqual(
            checkouts[0]["with"],
            {"ref": "${{ env.SOURCE_SHA }}", "persist-credentials": False},
        )
        self.assertEqual(
            checkouts[1]["with"],
            {"ref": "${{ github.sha }}", "path": ".pages-tools", "persist-credentials": False},
        )

    def test_master_preview_uses_the_same_development_profile_as_prs(self):
        preview = workflow("pages-preview-build.yml")
        old_build = workflow("web-build.yml")
        for event in ("push", "pull_request"):
            self.assertEqual(preview["on"][event]["branches"], ["master"])
            self.assertTrue(
                set(old_build["on"][event]["paths"])
                <= set(preview["on"][event]["paths"])
            )
        legacy_paths = preview["on"]["pull_request"]["paths"]
        self.assertIn("frontend/**", legacy_paths)
        self.assertIn("!frontend/src-tauri/**", legacy_paths)
        self.assertLess(legacy_paths.index("frontend/**"), legacy_paths.index("!frontend/src-tauri/**"))
        build_steps = [
            step for step in preview["jobs"]["build-preview"]["steps"]
            if "./scripts/ci/web.sh" in step.get("run", "")
        ]
        self.assertEqual(len(build_steps), 1)
        self.assertEqual(build_steps[0]["env"], {
            "MAPLE_WEB_ENVIRONMENT": "pr",
            "MAPLE_IOS_DEV_AUTH_ORIGIN": "${{ vars.MAPLE_IOS_DEV_AUTH_ORIGIN }}",
        })
        self.assertNotIn("if", build_steps[0])

    def test_optional_native_auth_origin_is_only_exported_in_development(self):
        origin = "https://dev-auth.example.test"
        for profile in ("pr", "release"):
            for configured in ("", origin):
                with self.subTest(profile=profile, configured=configured):
                    environment = dict(os.environ, MAPLE_IOS_DEV_AUTH_ORIGIN=configured,
                                       VITE_MAPLE_DEV_AUTH_ORIGIN="https://stale.example.test")
                    result = subprocess.run([
                        "bash", "-c", 'source scripts/ci/_common.sh; '
                        f'use_{profile}_environment; '
                        'printf "%s" "${VITE_MAPLE_DEV_AUTH_ORIGIN:-}"',
                    ], cwd=ROOT, env=environment, check=True, capture_output=True, text=True)
                    self.assertEqual(result.stdout, configured if profile == "pr" else "")

    def test_preview_artifact_is_bound_to_one_run_and_attempt(self):
        steps = workflow("pages-preview-build.yml")["jobs"]["build-preview"]["steps"]
        uploads = [
            step for step in steps
            if step.get("uses", "").startswith("actions/upload-artifact@")
        ]
        self.assertEqual(len(uploads), 1)
        upload = uploads[0]["with"]
        self.assertEqual(
            upload["name"],
            "maple-pages-preview-${{ github.run_id }}-${{ github.run_attempt }}",
        )
        self.assertEqual(
            upload["path"].splitlines(),
            [
                "${{ steps.artifact.outputs.directory }}/maple-web-dist.tar.gz",
                "${{ steps.artifact.outputs.directory }}/pages-artifact.json",
            ],
        )
        self.assertEqual(upload["if-no-files-found"], "error")
        descriptions = [
            step["run"] for step in steps
            if "scripts/ci/pages_artifact.py" in step.get("run", "")
        ]
        self.assertEqual(len(descriptions), 1)
        self.assertIn('"$GITHUB_WORKSPACE/.pages-tools#pages"', descriptions[0])
        self.assertIn(
            '-c python3 -I "$GITHUB_WORKSPACE/.pages-tools/scripts/ci/pages_artifact.py" manifest',
            descriptions[0],
        )
        for argument in (
            "--profile pr", '--sha "$SOURCE_SHA"',
            '--run-id "$GITHUB_RUN_ID"', '--run-attempt "$GITHUB_RUN_ATTEMPT"',
        ):
            self.assertIn(argument, descriptions[0])

    def test_preview_artifact_selection_supports_both_layouts_and_rejects_ambiguity(self):
        steps = workflow("pages-preview-build.yml")["jobs"]["build-preview"]["steps"]
        description = next(step for step in steps if step.get("id") == "artifact")
        modern = "apps/maple-research/frontend/src-tauri/target/reproducibility"
        legacy = "frontend/src-tauri/target/reproducibility"
        for directories, expected in (((modern,), modern), ((legacy,), legacy),
                                      ((modern, legacy), None), ((), None)):
            with self.subTest(directories=directories), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                for directory in directories:
                    archive = root / directory / "maple-web-dist.tar.gz"
                    archive.parent.mkdir(parents=True)
                    archive.write_bytes(b"fixture")
                # The selection is shell logic; artifact validation has its own tests.
                binaries = root / "bin"
                binaries.mkdir()
                nix = binaries / "nix"
                nix.write_text("#!/bin/sh\nexit 0\n")
                nix.chmod(0o755)
                output = root / "output"
                environment = {**os.environ, "PATH": str(binaries) + os.pathsep + os.environ["PATH"],
                               "GITHUB_WORKSPACE": tmp, "GITHUB_OUTPUT": str(output),
                               "SOURCE_SHA": "a" * 40, "GITHUB_RUN_ID": "100", "GITHUB_RUN_ATTEMPT": "2"}
                result = subprocess.run(["bash", "-c", description["run"]], cwd=root,
                                        env=environment, capture_output=True, text=True)
                if expected is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(output.exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(output.read_text(), f"directory={expected}\n")

    def test_publisher_uses_only_trusted_checkout_and_dependencies(self):
        publish = workflow("pages-publish.yml")
        self.assertEqual(set(publish["on"]), {"workflow_run", "workflow_dispatch"})
        self.assertEqual(
            publish["on"]["workflow_run"],
            {"workflows": ["Pages preview build", "Release"], "types": ["completed"]},
        )
        self.assertEqual(publish["permissions"], {"contents": "read"})
        self.assertEqual(set(publish["jobs"]), {"preview", "production"})
        for job in publish["jobs"].values():
            with self.subTest(job=job["name"]):
                checkouts = [
                    step for step in job["steps"]
                    if step.get("uses", "").startswith("actions/checkout@")
                ]
                self.assertEqual(len(checkouts), 1)
                self.assertEqual(
                    checkouts[0]["with"],
                    {"ref": "${{ github.sha }}", "persist-credentials": False},
                )
                actions = [step["uses"].split("@")[0] for step in job["steps"] if "uses" in step]
                self.assertEqual(actions, ["actions/checkout", "DeterminateSystems/nix-installer-action"])
                installs = [step for step in job["steps"] if "bun install" in step.get("run", "")]
                self.assertEqual(len(installs), 1)
                self.assertEqual(installs[0]["working-directory"], "services/updates")
                self.assertEqual(
                    installs[0]["run"],
                    "nix develop --no-update-lock-file ../..#pages -c bun install --frozen-lockfile --ignore-scripts",
                )
                for step in job["steps"]:
                    self.assertNotIn("${{", step.get("run", ""))
                    self.assertNotIn(".#ci", step.get("run", ""))

    def test_cf_credentials_exist_only_in_final_deploy_step(self):
        publish = workflow("pages-publish.yml")
        self.assertNotIn("env", publish)
        for target, job in publish["jobs"].items():
            with self.subTest(target=target):
                self.assertNotIn("env", job)
                # GitHub's automatic job deployment uses the publisher checkout
                # SHA and can inactivate the explicit artifact-SHA deployment.
                self.assertEqual(
                    job["environment"],
                    {"name": f"pages-{target}", "deployment": False},
                )
                steps = job["steps"]
                deploy = steps[-1]
                self.assertEqual(
                    deploy["env"],
                    {
                        "GH_TOKEN": "${{ github.token }}",
                        "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
                        "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
                    },
                )
                self.assertEqual(
                    deploy["run"],
                    "nix develop --no-update-lock-file .#pages -c python3 -I scripts/ci/pages_deploy.py "
                    'deploy --state "$RUNNER_TEMP/maple-pages"',
                )
                self.assertEqual(
                    steps[-2]["run"],
                    "nix develop --no-update-lock-file .#pages -c python3 -I scripts/ci/pages_deploy.py "
                    f'prepare --target {target} --state "$RUNNER_TEMP/maple-pages"',
                )
                self.assertEqual(steps[-2]["env"], {"GH_TOKEN": "${{ github.token }}"})
                before_secrets = copy.deepcopy(job)
                before_secrets["steps"] = steps[:-1]
                self.assert_no_secrets(before_secrets)

    def test_publisher_permissions_do_not_expand_build_authority(self):
        jobs = workflow("pages-publish.yml")["jobs"]
        self.assertEqual(
            jobs["preview"]["permissions"],
            {"contents": "read", "actions": "read", "pull-requests": "write", "deployments": "write"},
        )
        self.assertEqual(
            jobs["production"]["permissions"],
            {"contents": "write", "actions": "read", "deployments": "write"},
        )

    def test_rollout_and_event_gates_are_fail_closed(self):
        jobs = workflow("pages-publish.yml")["jobs"]
        preview_if = normalized(jobs["preview"]["if"])
        self.assertTrue(preview_if.startswith("vars.MAPLE_PAGES_PREVIEW_ENABLED == 'true' &&"))
        for control in (
            "github.event_name == 'workflow_run'",
            "workflow_run.conclusion == 'success'",
            "workflow_run.path == '.github/workflows/pages-preview-build.yml'",
            "workflow_run.head_repository.full_name == github.repository",
            "workflow_run.event == 'pull_request'",
            "workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'master'",
        ):
            self.assertIn(control, preview_if)
        production_if = normalized(jobs["production"]["if"])
        self.assertTrue(production_if.startswith("vars.MAPLE_PAGES_PRODUCTION_ENABLED == 'true' &&"))
        for control in (
            "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master'",
            "github.event_name == 'workflow_run'",
            "workflow_run.conclusion == 'success'",
            "workflow_run.event == 'release'",
            "workflow_run.path == '.github/workflows/release.yml'",
            "workflow_run.head_repository.full_name == github.repository",
        ):
            self.assertIn(control, production_if)

    def test_legacy_and_direct_production_have_one_rollout_switch_and_lock(self):
        legacy = workflow("pages-production.yml")
        direct = workflow("pages-publish.yml")["jobs"]["production"]
        self.assertTrue(
            normalized(legacy["jobs"]["promote"]["if"]).startswith(
                "vars.MAPLE_PAGES_PRODUCTION_ENABLED != 'true' &&"
            )
        )
        self.assertEqual(direct["concurrency"], legacy["concurrency"])
        self.assertEqual(direct["concurrency"], {"group": "pages-production", "cancel-in-progress": False})
        self.assertEqual(
            workflow("pages-publish.yml")["jobs"]["preview"]["concurrency"],
            {"group": "pages-preview-${{ github.event.workflow_run.head_branch }}", "cancel-in-progress": False},
        )

    def test_actions_are_immutable_and_nix_gets_no_github_token(self):
        for name in ("pages-preview-build.yml", "pages-publish.yml", "pages-tests.yml"):
            for job in workflow(name)["jobs"].values():
                for step in job["steps"]:
                    if "uses" not in step:
                        continue
                    with self.subTest(workflow=name, action=step["uses"]):
                        self.assertRegex(step["uses"], r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                        self.assertNotIn("actions/cache@", step["uses"])
                        if step["uses"].startswith("DeterminateSystems/nix-installer-action@"):
                            self.assertEqual(step["with"]["github-token"], "")


if __name__ == "__main__":
    unittest.main()
