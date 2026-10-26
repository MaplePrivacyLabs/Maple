"""Regression checks for the GitHub Actions credential and source boundaries."""

import copy
import functools
import json
import os
from pathlib import Path
import subprocess
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

    def test_production_web_build_is_manual_master_only_and_unprivileged(self):
        build = workflow("pages-production-build.yml")
        self.assertEqual(build["name"], "Pages production build")
        self.assertEqual(set(build["on"]), {"workflow_dispatch"})
        self.assertEqual(build["permissions"], {"contents": "read"})
        self.assert_no_secrets(build)
        self.assertEqual(set(build["jobs"]), {"build"})
        job = build["jobs"]["build"]
        self.assertNotIn("permissions", job)
        self.assertNotIn("environment", job)
        self.assertEqual(normalized(job["if"]),
                         "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master'")
        self.assertEqual(job["steps"][0]["with"],
                         {"ref": "${{ github.sha }}", "persist-credentials": False})
        compile_steps = [step for step in job["steps"] if "bash scripts/ci/web.sh" in step.get("run", "")]
        self.assertEqual(len(compile_steps), 1)
        self.assertEqual(compile_steps[0]["env"], {"MAPLE_WEB_ENVIRONMENT": "release"})
        manifest = next(step["run"] for step in job["steps"] if "pages_artifact.py manifest" in step.get("run", ""))
        for argument in ('--profile release', '--sha "$GITHUB_SHA"', '--run-id "$GITHUB_RUN_ID"',
                         '--run-attempt "$GITHUB_RUN_ATTEMPT"'):
            self.assertIn(argument, manifest)
        self.assertNotIn("${{", manifest)
        upload = job["steps"][-1]
        self.assertEqual(upload["with"], {
            "name": "maple-pages-production-${{ github.run_id }}-${{ github.run_attempt }}",
            "path": "apps/maple-research/frontend/src-tauri/target/reproducibility/maple-web-dist.tar.gz\n"
                    "apps/maple-research/frontend/src-tauri/target/reproducibility/pages-artifact.json\n",
            "if-no-files-found": "error", "retention-days": 5,
        })

    def test_manual_publisher_inputs_select_only_exact_build_run_and_attempt(self):
        inputs = workflow("pages-publish.yml")["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual(set(inputs), {"build_run_id", "build_run_attempt"})
        for field in inputs.values():
            self.assertEqual(field["type"], "string")
            self.assertEqual(field["default"], "")
            self.assertFalse(field["required"])

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
        # Fork PRs never produce a preview artifact.
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository", normalized(job["if"]))
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
                self.assertEqual(actions, ["actions/checkout", "DeterminateSystems/determinate-nix-action"])
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

    def test_production_has_one_explicitly_enabled_publisher_and_distinct_lock(self):
        production_jobs = [
            (path.name, job_id)
            for path in sorted(WORKFLOWS.glob("*.yml"))
            for job_id, job in workflow(path.name).get("jobs", {}).items()
            if "vars.MAPLE_PAGES_PRODUCTION_ENABLED" in str(job.get("if", ""))
        ]
        # Disabling publication must not activate an alternate production writer.
        self.assertEqual(production_jobs, [("pages-publish.yml", "production")])
        direct = workflow("pages-publish.yml")["jobs"]["production"]
        self.assertEqual(direct["concurrency"], {"group": "pages-production", "cancel-in-progress": False})
        self.assertEqual(
            workflow("pages-publish.yml")["jobs"]["preview"]["concurrency"],
            {"group": "pages-preview-${{ github.event.workflow_run.head_branch }}", "cancel-in-progress": False},
        )

    def test_producers_and_publisher_use_no_caches(self):
        for name in ("pages-preview-build.yml", "pages-production-build.yml", "pages-publish.yml"):
            for job in workflow(name)["jobs"].values():
                for step in job["steps"]:
                    self.assertNotIn("cache", step.get("uses", "").lower(), name)


if __name__ == "__main__":
    unittest.main()
