"""Regression checks for the independent auth build and publisher authority."""

import copy
import unittest

from test_pages_workflows import WORKFLOWS, normalized, strings, workflow


CI = "auth-pages-ci.yml"
BUILD = "auth-pages-build.yml"
PUBLISH = "auth-pages-publish.yml"
DEV_PUBLISH = "auth-pages-dev-publish.yml"
CHECK_PAGES = (
    "nix build --no-update-lock-file --no-link --print-build-logs "
    ".#checks.x86_64-linux.pages"
)
BUILD_AUTH = "nix develop --no-update-lock-file .#ci -c bash scripts/ci/auth-web.sh"
CHECK_AUTH = "nix develop --no-update-lock-file .#ci -c bash scripts/ci/auth-ci.sh"
ARTIFACT_DIRECTORY = "apps/maple-auth/target/reproducibility"
INTERNAL_ARTIFACT = (
    "github.event_name != 'pull_request' || "
    "github.event.pull_request.head.repo.full_name == github.repository"
)
PUBLISHERS = (
    (PUBLISH, "production", "MAPLE_AUTH_PAGES_PRODUCTION_ENABLED", "", "maple-auth-pages"),
    (DEV_PUBLISH, "development", "MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED",
     " --environment development --target production", "maple-auth-dev-pages"),
    (DEV_PUBLISH, "preview", "MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED",
     " --environment development --target preview", "maple-auth-preview-pages"),
)


class AuthPagesWorkflowTests(unittest.TestCase):
    def assert_no_secrets(self, value):
        for text in strings(value):
            self.assertNotRegex(text, r"\bsecrets\b")

    def test_ci_includes_stacked_prs_and_forks_but_manual_builds_require_master(self):
        ci = workflow(CI)
        self.assertEqual(ci["name"], "Auth Pages CI")
        self.assertEqual(set(ci["on"]), {"pull_request", "push", "workflow_dispatch"})
        self.assertIsNone(ci["on"]["workflow_dispatch"])
        self.assertNotIn("branches", ci["on"]["pull_request"])
        self.assertNotIn("branches-ignore", ci["on"]["pull_request"])
        self.assertEqual(ci["on"]["push"]["branches"], ["master"])
        self.assertEqual(set(ci["jobs"]), {"auth"})
        self.assertEqual(
            normalized(ci["jobs"]["auth"]["if"]),
            "github.event_name == 'pull_request' || "
            "((github.event_name == 'push' || github.event_name == 'workflow_dispatch') && "
            "github.ref == 'refs/heads/master')",
        )

    def test_ci_checks_out_exact_pr_head_and_labels_that_same_source(self):
        job = workflow(CI)["jobs"]["auth"]
        self.assertEqual(job["env"], {"SOURCE_SHA":
                         "${{ github.event_name == 'pull_request' && "
                         "github.event.pull_request.head.sha || github.sha }}"})
        checkouts = [step for step in job["steps"]
                     if step.get("uses", "").startswith("actions/checkout@")]
        self.assertEqual(len(checkouts), 1)
        self.assertEqual(checkouts[0]["with"],
                         {"ref": "${{ env.SOURCE_SHA }}", "persist-credentials": False})
        describe = next(step for step in job["steps"]
                        if "scripts/ci/pages_artifact.py" in step.get("run", ""))
        self.assertIn('--sha "$SOURCE_SHA"', describe["run"])
        self.assertNotIn('--sha "$GITHUB_SHA"', describe["run"])

    def test_ci_covers_only_auth_and_shared_build_tooling(self):
        events = workflow(CI)["on"]
        self.assertEqual(events["pull_request"]["paths"], events["push"]["paths"])
        self.assertEqual(set(events["pull_request"]["paths"]), {
            ".github/workflows/auth-pages-*.yml", ".github/workflows/pages-tests.yml",
            "apps/maple-auth/**", "scripts/ci/auth-*.sh",
            "scripts/ci/pages_*.py", "scripts/ci/test_pages_*.py", "flake.nix", "flake.lock",
        })
        for event in ("pull_request", "push"):
            paths = workflow("pages-tests.yml")["on"][event]["paths"]
            self.assertIn(".github/workflows/auth-pages-*.yml", paths)
            self.assertIn("scripts/ci/auth-*.sh", paths)

    def test_builds_are_unprivileged_and_test_before_building_fixed_profiles(self):
        for name, job_name, profile in ((CI, "auth", "dev"), (BUILD, "build", "release")):
            with self.subTest(workflow=name):
                config = workflow(name)
                self.assertEqual(config["permissions"], {"contents": "read"})
                self.assert_no_secrets(config)
                self.assertNotIn("env", config)
                job = config["jobs"][job_name]
                self.assertNotIn("permissions", job)
                self.assertNotIn("environment", job)
                if name == BUILD:
                    self.assertNotIn("env", job)
                steps = job["steps"]
                builds = [step for step in steps if step.get("run") == BUILD_AUTH]
                self.assertEqual(len(builds), 1)
                self.assertEqual(builds[0]["env"], {"MAPLE_AUTH_ENVIRONMENT": profile})
                self.assertNotIn("if", builds[0])
                for command in (CHECK_PAGES, CHECK_AUTH):
                    checks = [step for step in steps if step.get("run") == command]
                    self.assertEqual(len(checks), 1)
                    self.assertNotIn("if", checks[0])
                    self.assertLess(steps.index(checks[0]), steps.index(builds[0]))
                for step in steps:
                    self.assertNotIn("GH_TOKEN", step.get("env", {}))
                    self.assertNotIn("github.token", " ".join(strings(step.get("env", {}))))
                commands = " ".join(step.get("run", "") for step in steps)
                for research_input in ("maple-research", "scripts/ci/frontend.sh", "scripts/ci/web.sh",
                                       "prepare-frontend-deps", "prepare-typescript-sdk"):
                    self.assertNotIn(research_input, commands)

    def test_production_build_remains_manual_and_master_only(self):
        build = workflow(BUILD)
        self.assertEqual(build["name"], "Auth Pages build")
        self.assertEqual(build["on"], {"workflow_dispatch": None})
        self.assertEqual(set(build["jobs"]), {"build"})
        job = build["jobs"]["build"]
        self.assertEqual(normalized(job["if"]),
                         "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master'")
        checkouts = [step for step in job["steps"]
                     if step.get("uses", "").startswith("actions/checkout@")]
        self.assertEqual(len(checkouts), 1)
        self.assertEqual(checkouts[0]["with"],
                         {"ref": "${{ github.sha }}", "persist-credentials": False})

    def test_artifacts_bind_profile_source_run_and_attempt_and_forks_cannot_upload(self):
        for name, job, profile, environment, source in (
            (CI, "auth", "auth-dev", "development", "SOURCE_SHA"),
            (BUILD, "build", "auth-release", "production", "GITHUB_SHA"),
        ):
            with self.subTest(workflow=name):
                steps = workflow(name)["jobs"][job]["steps"]
                descriptions = [step for step in steps
                                if "scripts/ci/pages_artifact.py" in step.get("run", "")]
                uploads = [step for step in steps
                           if step.get("uses", "").startswith("actions/upload-artifact@")]
                self.assertEqual(len(descriptions), 1)
                self.assertEqual(len(uploads), 1)
                self.assertLess(steps.index(descriptions[0]), steps.index(uploads[0]))
                for argument in (
                    f'artifact_dir="{ARTIFACT_DIRECTORY}"',
                    "nix develop --no-update-lock-file .#pages -c python3 -I scripts/ci/pages_artifact.py manifest",
                    '--archive "$artifact_dir/maple-auth-dist.tar.gz"', f"--profile {profile}",
                    f'--sha "${source}"', '--run-id "$GITHUB_RUN_ID"',
                    '--run-attempt "$GITHUB_RUN_ATTEMPT"', '--output "$artifact_dir/pages-artifact.json"',
                ):
                    self.assertIn(argument, descriptions[0]["run"])
                for step in (descriptions[0], uploads[0]):
                    if name == CI:
                        self.assertEqual(normalized(step["if"]), INTERNAL_ARTIFACT)
                    else:
                        self.assertNotIn("if", step)
                self.assertEqual(uploads[0]["with"], {
                    "name": f"maple-auth-{environment}-${{{{ github.run_id }}}}-${{{{ github.run_attempt }}}}",
                    "path": f"{ARTIFACT_DIRECTORY}/maple-auth-dist.tar.gz\n"
                            f"{ARTIFACT_DIRECTORY}/pages-artifact.json\n",
                    "if-no-files-found": "error", "retention-days": 5,
                })

    def test_ci_is_the_single_development_producer(self):
        self.assertFalse((WORKFLOWS / "auth-pages-dev-build.yml").exists())
        producers = []
        for path in WORKFLOWS.glob("auth-pages-*.yml"):
            for name, job in workflow(path.name)["jobs"].items():
                if any(step.get("run") == BUILD_AUTH for step in job["steps"]):
                    producers.append((path.name, name))
        self.assertCountEqual(producers, [(CI, "auth"), (BUILD, "build")])

    def test_production_publisher_remains_manual_master_only_and_disabled_by_default(self):
        publish = workflow(PUBLISH)
        self.assertEqual(publish["name"], "Publish Auth Pages")
        self.assertEqual(set(publish["on"]), {"workflow_dispatch"})
        self.assertEqual(set(publish["jobs"]), {"production"})
        self.assertEqual(normalized(publish["jobs"]["production"]["if"]),
                         "vars.MAPLE_AUTH_PAGES_PRODUCTION_ENABLED == 'true' && "
                         "github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master'")

    def test_manual_recovery_inputs_select_only_run_and_attempt(self):
        for name in (PUBLISH, DEV_PUBLISH):
            with self.subTest(workflow=name):
                inputs = workflow(name)["on"]["workflow_dispatch"]["inputs"]
                self.assertEqual(set(inputs), {"build_run_id", "build_run_attempt"})
                for value in inputs.values():
                    self.assertEqual(value["type"], "string")
                    self.assertIs(value["required"], True)
                    self.assertNotIn("default", value)

    def test_development_and_preview_event_gates_are_disjoint_and_internal(self):
        publish = workflow(DEV_PUBLISH)
        self.assertEqual(publish["name"], "Publish Auth Dev Pages")
        self.assertEqual(set(publish["on"]), {"workflow_run", "workflow_dispatch"})
        self.assertEqual(publish["on"]["workflow_run"],
                         {"workflows": ["Auth Pages CI"], "types": ["completed"]})
        self.assertEqual(set(publish["jobs"]), {"development", "preview"})
        self.assertEqual(normalized(publish["jobs"]["development"]["if"]),
                         "vars.MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED == 'true' && "
                         "((github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/master') || "
                         "(github.event_name == 'workflow_run' && "
                         "github.event.workflow_run.conclusion == 'success' && "
                         "github.event.workflow_run.path == '.github/workflows/auth-pages-ci.yml' && "
                         "github.event.workflow_run.head_repository.full_name == github.repository && "
                         "github.event.workflow_run.head_branch == 'master' && "
                         "(github.event.workflow_run.event == 'push' || github.event.workflow_run.event == 'workflow_dispatch')))")
        self.assertEqual(normalized(publish["jobs"]["preview"]["if"]),
                         "vars.MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED == 'true' && "
                         "github.event_name == 'workflow_run' && "
                         "github.event.workflow_run.conclusion == 'success' && "
                         "github.event.workflow_run.path == '.github/workflows/auth-pages-ci.yml' && "
                         "github.event.workflow_run.head_repository.full_name == github.repository && "
                         "github.event.workflow_run.event == 'pull_request'")

    def test_publisher_permissions_environments_and_queues_match_their_target(self):
        queues = []
        for name, target, _, _, _ in PUBLISHERS:
            with self.subTest(workflow=name, target=target):
                publish = workflow(name)
                self.assertEqual(publish["permissions"], {"contents": "read"})
                job = publish["jobs"][target]
                permissions = {"contents": "write", "actions": "read", "deployments": "write"}
                if target == "preview":
                    permissions.update(contents="read", **{"pull-requests": "write"})
                self.assertEqual(job["permissions"], permissions)
                environment = "production" if name == PUBLISH else "development"
                self.assertEqual(job["environment"],
                                 {"name": f"auth-pages-{environment}", "deployment": False})
                queue = f"pages-auth-{target}"
                if target == "preview":
                    queue += "-${{ github.event.workflow_run.head_branch }}"
                self.assertEqual(job["concurrency"], {"group": queue, "cancel-in-progress": False})
                queues.append(queue)
                for app in workflow("pages-publish.yml")["jobs"].values():
                    self.assertNotEqual(job["concurrency"], app["concurrency"])
                    self.assertNotEqual(job["environment"]["name"], app["environment"]["name"])
        self.assertEqual(len(queues), len(set(queues)))
        self.assertNotIn("MAPLE_AUTH_PAGES_PRODUCTION_ENABLED", " ".join(strings(workflow(DEV_PUBLISH))))
        self.assertNotIn("MAPLE_AUTH_PAGES_DEVELOPMENT_ENABLED", " ".join(strings(workflow(PUBLISH))))

    def test_publishers_execute_only_trusted_code_and_dependencies(self):
        for name, target, _, _, _ in PUBLISHERS:
            with self.subTest(workflow=name, target=target):
                steps = workflow(name)["jobs"][target]["steps"]
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
                self.assertNotIn("inputs.", " ".join(strings(steps)))
                self.assertNotIn("scripts/ci/auth-web.sh", " ".join(strings(steps)))

    def test_credentials_and_cli_targets_are_scoped_to_prepare_and_final_deploy(self):
        for name, target, flag, arguments, state in PUBLISHERS:
            with self.subTest(workflow=name, target=target):
                publish = workflow(name)
                self.assertNotIn("env", publish)
                job = publish["jobs"][target]
                self.assertNotIn("env", job)
                steps = job["steps"]
                common_env = {"GH_TOKEN": "${{ github.token }}", flag: "${{ vars." + flag + " }}"}
                self.assertEqual(steps[-2]["env"], common_env)
                self.assertEqual(steps[-1]["env"], {
                    **common_env, "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
                    "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
                })
                for step, command in ((steps[-2], "prepare"), (steps[-1], "deploy")):
                    self.assertEqual(step["run"],
                                     "nix develop --no-update-lock-file .#pages -c python3 -I "
                                     f'scripts/ci/pages_auth_deploy.py {command}{arguments} --state "$RUNNER_TEMP/{state}"')
                before_deploy = copy.deepcopy(job)
                before_deploy["steps"] = steps[:-1]
                self.assert_no_secrets(before_deploy)
                for step in steps[:-2]:
                    self.assertNotIn("env", step)

    def test_actions_are_immutable_without_caches_or_persisted_credentials(self):
        for name in (CI, BUILD, PUBLISH, DEV_PUBLISH):
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
