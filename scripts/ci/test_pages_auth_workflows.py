"""Regression checks for the independent auth build and publisher authority."""

import copy
import unittest

from test_pages_workflows import normalized, strings, workflow


CI = "auth-pages-ci.yml"
BUILD = "auth-pages-build.yml"
PUBLISH = "auth-pages-publish.yml"
CHECK_PAGES = (
    "nix build --no-update-lock-file --no-link --print-build-logs "
    ".#checks.x86_64-linux.pages"
)
BUILD_AUTH = "nix develop --no-update-lock-file .#ci -c bash scripts/ci/auth-web.sh"
CHECK_AUTH = "nix develop --no-update-lock-file .#ci -c bash scripts/ci/auth-ci.sh"
ARTIFACT_DIRECTORY = "apps/maple-auth/target/reproducibility"


class AuthPagesWorkflowTests(unittest.TestCase):
    def assert_no_secrets(self, value):
        for text in strings(value):
            self.assertNotRegex(text, r"\bsecrets\b")

    def test_ci_includes_stacked_pr_bases_and_forks(self):
        ci = workflow(CI)
        self.assertEqual(ci["name"], "Auth Pages CI")
        self.assertEqual(set(ci["on"]), {"pull_request", "push"})
        # An allowlist of master would silently skip the SDK-based stacked PR.
        self.assertNotIn("branches", ci["on"]["pull_request"])
        self.assertNotIn("branches-ignore", ci["on"]["pull_request"])
        self.assertEqual(ci["on"]["push"]["branches"], ["master"])
        self.assertEqual(set(ci["jobs"]), {"auth"})
        self.assertEqual(
            normalized(ci["jobs"]["auth"]["if"]),
            "github.event_name == 'pull_request' || "
            "(github.event_name == 'push' && github.ref == 'refs/heads/master')",
        )

    def test_ci_covers_only_auth_and_shared_build_tooling(self):
        events = workflow(CI)["on"]
        self.assertEqual(events["pull_request"]["paths"], events["push"]["paths"])
        paths = events["pull_request"]["paths"]
        self.assertEqual(
            set(paths),
            {
                ".github/workflows/auth-pages-*.yml", ".github/workflows/pages-tests.yml",
                "apps/maple-auth/**", "scripts/ci/auth-*.sh",
                "scripts/ci/pages_*.py", "scripts/ci/test_pages_*.py",
                "flake.nix", "flake.lock",
            },
        )
        for event in ("pull_request", "push"):
            test_paths = workflow("pages-tests.yml")["on"][event]["paths"]
            self.assertIn(".github/workflows/auth-pages-*.yml", test_paths)
            self.assertIn("scripts/ci/auth-*.sh", test_paths)

    def test_builds_are_unprivileged_and_have_distinct_profiles(self):
        for name, job_name, profile in ((CI, "auth", "pr"), (BUILD, "build", "release")):
            with self.subTest(workflow=name):
                config = workflow(name)
                self.assertEqual(config["permissions"], {"contents": "read"})
                self.assert_no_secrets(config)
                self.assertNotIn("env", config)
                job = config["jobs"][job_name]
                self.assertNotIn("permissions", job)
                self.assertNotIn("environment", job)
                self.assertNotIn("env", job)
                steps = job["steps"]
                checks = [step for step in steps if step.get("run") == CHECK_PAGES]
                builds = [step for step in steps if step.get("run") == BUILD_AUTH]
                self.assertEqual(len(checks), 1)
                self.assertEqual(len(builds), 1)
                self.assertLess(steps.index(checks[0]), steps.index(builds[0]))
                self.assertEqual(builds[0]["env"], {"MAPLE_AUTH_ENVIRONMENT": profile})
                self.assertNotIn("if", builds[0])
                for step in steps:
                    self.assertNotIn("GH_TOKEN", step.get("env", {}))
                    self.assertNotIn("github.token", " ".join(strings(step.get("env", {}))))
        ci_actions = [step.get("uses", "") for step in workflow(CI)["jobs"]["auth"]["steps"]]
        self.assertFalse(any(action.startswith("actions/upload-artifact@") for action in ci_actions))

    def test_builds_test_only_the_standalone_auth_application(self):
        for name, job in ((CI, "auth"), (BUILD, "build")):
            with self.subTest(workflow=name):
                steps = workflow(name)["jobs"][job]["steps"]
                checks = [step for step in steps if step.get("run") == CHECK_AUTH]
                builds = [step for step in steps if step.get("run") == BUILD_AUTH]
                self.assertEqual(len(checks), 1)
                self.assertNotIn("if", checks[0])
                self.assertLess(steps.index(checks[0]), steps.index(builds[0]))
                commands = " ".join(step.get("run", "") for step in steps)
                for research_input in ("maple-research", "scripts/ci/frontend.sh", "scripts/ci/web.sh",
                                       "prepare-frontend-deps", "prepare-typescript-sdk"):
                    self.assertNotIn(research_input, commands)

    def test_production_build_is_manual_and_master_only(self):
        build = workflow(BUILD)
        self.assertEqual(build["name"], "Auth Pages build")
        self.assertEqual(build["on"], {"workflow_dispatch": None})
        self.assertEqual(set(build["jobs"]), {"build"})
        job = build["jobs"]["build"]
        self.assertEqual(
            normalized(job["if"]),
            "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master'",
        )
        checkout = [step for step in job["steps"]
                    if step.get("uses", "").startswith("actions/checkout@")]
        self.assertEqual(len(checkout), 1)
        self.assertEqual(checkout[0]["with"],
                         {"ref": "${{ github.sha }}", "persist-credentials": False})

    def test_production_artifact_is_bound_to_auth_source_run_and_attempt(self):
        steps = workflow(BUILD)["jobs"]["build"]["steps"]
        descriptions = [step["run"] for step in steps
                        if "scripts/ci/pages_artifact.py" in step.get("run", "")]
        self.assertEqual(len(descriptions), 1)
        describe = descriptions[0]
        for argument in (
            f'artifact_dir="{ARTIFACT_DIRECTORY}"',
            "nix develop --no-update-lock-file .#pages -c python3 -I scripts/ci/pages_artifact.py manifest",
            '--archive "$artifact_dir/maple-auth-dist.tar.gz"',
            "--profile auth-release", '--sha "$GITHUB_SHA"',
            '--run-id "$GITHUB_RUN_ID"', '--run-attempt "$GITHUB_RUN_ATTEMPT"',
            '--output "$artifact_dir/pages-artifact.json"',
        ):
            self.assertIn(argument, describe)
        uploads = [step for step in steps
                   if step.get("uses", "").startswith("actions/upload-artifact@")]
        self.assertEqual(len(uploads), 1)
        upload = uploads[0]["with"]
        self.assertEqual(upload["name"],
                         "maple-auth-production-${{ github.run_id }}-${{ github.run_attempt }}")
        self.assertEqual(upload["path"].splitlines(), [
            f"{ARTIFACT_DIRECTORY}/maple-auth-dist.tar.gz",
            f"{ARTIFACT_DIRECTORY}/pages-artifact.json",
        ])
        self.assertEqual(upload["if-no-files-found"], "error")

    def test_publisher_is_manual_master_only_and_disabled_by_default(self):
        publish = workflow(PUBLISH)
        self.assertEqual(publish["name"], "Publish Auth Pages")
        self.assertEqual(set(publish["on"]), {"workflow_dispatch"})
        inputs = publish["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual(set(inputs), {"build_run_id", "build_run_attempt"})
        for value in inputs.values():
            self.assertEqual(value["type"], "string")
            self.assertIs(value["required"], True)
            self.assertNotIn("default", value)
        self.assertEqual(publish["permissions"], {"contents": "read"})
        self.assertEqual(set(publish["jobs"]), {"production"})
        job = publish["jobs"]["production"]
        self.assertEqual(
            normalized(job["if"]),
            "vars.MAPLE_AUTH_PAGES_PRODUCTION_ENABLED == 'true' && "
            "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master'",
        )
        self.assertEqual(job["permissions"],
                         {"contents": "write", "actions": "read", "deployments": "write"})
        self.assertEqual(job["environment"],
                         {"name": "auth-pages-production", "deployment": False})
        self.assertEqual(job["concurrency"],
                         {"group": "pages-auth-production", "cancel-in-progress": False})
        app_job = workflow("pages-publish.yml")["jobs"]["production"]
        self.assertNotEqual(job["concurrency"]["group"], app_job["concurrency"]["group"])
        self.assertNotEqual(job["environment"]["name"], app_job["environment"]["name"])

    def test_publisher_executes_only_trusted_code_and_dependencies(self):
        job = workflow(PUBLISH)["jobs"]["production"]
        steps = job["steps"]
        actions = [step["uses"].split("@")[0] for step in steps if "uses" in step]
        self.assertEqual(actions, ["actions/checkout", "DeterminateSystems/nix-installer-action"])
        self.assertEqual(steps[0]["with"],
                         {"ref": "${{ github.sha }}", "persist-credentials": False})
        installs = [step for step in steps if "bun install" in step.get("run", "")]
        self.assertEqual(len(installs), 1)
        self.assertEqual(installs[0]["working-directory"], "services/updates")
        self.assertEqual(installs[0]["run"],
                         "nix develop --no-update-lock-file ../..#pages -c bun install --frozen-lockfile --ignore-scripts")
        self.assertNotIn("env", installs[0])
        for step in steps:
            self.assertNotIn("${{", step.get("run", ""))
            self.assertNotIn(".#ci", step.get("run", ""))
        # Input IDs are parsed from the event by trusted Python, never shell code.
        self.assertNotIn("inputs.", " ".join(strings(steps)))
        self.assertNotIn("scripts/ci/auth-web.sh", " ".join(strings(steps)))

    def test_credentials_are_scoped_to_preparation_and_final_deployment(self):
        publish = workflow(PUBLISH)
        self.assertNotIn("env", publish)
        job = publish["jobs"]["production"]
        self.assertNotIn("env", job)
        steps = job["steps"]
        common_env = {
            "GH_TOKEN": "${{ github.token }}",
            "MAPLE_AUTH_PAGES_PRODUCTION_ENABLED": "${{ vars.MAPLE_AUTH_PAGES_PRODUCTION_ENABLED }}",
        }
        self.assertEqual(steps[-2]["env"], common_env)
        self.assertEqual(steps[-1]["env"], {
            **common_env,
            "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
            "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
        })
        for step, command in ((steps[-2], "prepare"), (steps[-1], "deploy")):
            self.assertEqual(step["run"],
                             "nix develop --no-update-lock-file .#pages -c python3 -I "
                             f'scripts/ci/pages_auth_deploy.py {command} --state "$RUNNER_TEMP/maple-auth-pages"')
        before_deploy = copy.deepcopy(job)
        before_deploy["steps"] = steps[:-1]
        self.assert_no_secrets(before_deploy)
        for step in steps[:-2]:
            self.assertNotIn("env", step)

    def test_actions_are_immutable_without_caches_or_persisted_credentials(self):
        for name in (CI, BUILD, PUBLISH):
            for job in workflow(name)["jobs"].values():
                for step in job["steps"]:
                    if "uses" not in step:
                        continue
                    with self.subTest(workflow=name, action=step["uses"]):
                        self.assertRegex(step["uses"], r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                        self.assertNotIn("cache", step["uses"].lower())
                        if step["uses"].startswith("actions/checkout@"):
                            self.assertIs(step["with"]["persist-credentials"], False)
                        if step["uses"].startswith("DeterminateSystems/nix-installer-action@"):
                            self.assertEqual(step["with"]["github-token"], "")


if __name__ == "__main__":
    unittest.main()
