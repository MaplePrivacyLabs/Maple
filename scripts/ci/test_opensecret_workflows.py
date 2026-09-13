"""Exercise backend diff selection and enforce scoped CI cache boundaries."""

import functools
import itertools
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

from opensecret_change_detection import CHECK_OUTPUTS, OUTPUTS


ROOT = Path(__file__).resolve().parents[2]


@functools.cache
def workflow(name):
    result = subprocess.run(["yq", "-o=json", ".", str(ROOT / ".github/workflows" / name)],
                            check=True, capture_output=True, text=True)
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


class OpenSecretWorkflowBoundaryTests(unittest.TestCase):
    def test_nix_jobs_fetch_complete_backend_and_submodule_history(self):
        # checkout's fetch-depth applies to submodule update too. Nix's Git
        # fetcher cannot calculate revCount for a shallow recursive input.
        for workflow_name, job_names in (
            ("opensecret-ci.yml", ("rust", "nix")),
            ("opensecret-eif.yml", ("eif", "eif-trusted")),
            ("sdk-integration.yml", ("sdk-integration",)),
        ):
            for job_name in job_names:
                with self.subTest(workflow=workflow_name, job=job_name):
                    steps = workflow(workflow_name)["jobs"][job_name]["steps"]
                    checkouts = [step["with"] for step in steps
                                 if step.get("uses", "").startswith("actions/checkout@")]
                    self.assertEqual(len(checkouts), 1)
                    self.assertEqual(checkouts[0]["submodules"], "recursive")
                    self.assertEqual(checkouts[0].get("fetch-depth"), 0)

    def test_jobs_are_hosted_with_read_only_contents_and_scoped_oidc(self):
        for name in ("opensecret-ci.yml", "opensecret-eif.yml",
                     "opensecret-change-detection.yml", "sdk-integration.yml"):
            config = workflow(name)
            with self.subTest(workflow=name):
                self.assertEqual(config["permissions"], {"contents": "read"})
                self.assertNotIn("pull_request_target", config["on"])
                self.assertNotIn("workflow_run", config["on"])
                for value in strings(config):
                    self.assertNotRegex(value, r"\bsecrets\b|github\.token|\bGH_TOKEN\b")
                for job_name, job in config["jobs"].items():
                    self.assertNotIn("environment", job)
                    if (name, job_name) == ("opensecret-eif.yml", "eif-trusted"):
                        self.assertEqual(job["permissions"], {"contents": "read", "id-token": "write"})
                    else:
                        self.assertIn(job.get("permissions"), (None, {"contents": "read"}))
                        for value in strings(job):
                            self.assertNotRegex(value, r"\bid-token\b|flakehub-cache-action")
                    if "uses" in job:
                        self.assertEqual(job["uses"], "./.github/workflows/opensecret-change-detection.yml")
                        self.assertNotIn("secrets", job)
                        continue
                    expected_runner = "ubuntu-latest"
                    if name == "opensecret-eif.yml":
                        expected_runner = {
                            "eif": "ubuntu-24.04-arm",
                            "eif-trusted": "ubuntu-24.04-arm64-8core",
                        }[job_name]
                    self.assertEqual(job["runs-on"], expected_runner)
                    for step in job["steps"]:
                        self.assertNotIn("${{", step.get("run", ""))
                        action = step.get("uses", "")
                        if action:
                            self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                        if action.startswith("actions/checkout@"):
                            self.assertIs(step["with"]["persist-credentials"], False)
                            self.assertNotIn("repository", step["with"])
                            self.assertNotIn("ref", step["with"])
                        if action.startswith("DeterminateSystems/nix-installer-action@"):
                            self.assertEqual(step["with"]["github-token"], "")

    def test_custom_hosted_runner_is_registered_for_workflow_lint(self):
        result = subprocess.run(
            ["yq", "-o=json", ".", str(ROOT / ".github/actionlint.yaml")],
            check=True, capture_output=True, text=True,
        )
        self.assertEqual(json.loads(result.stdout), {
            "self-hosted-runner": {"labels": ["ubuntu-24.04-arm64-8core"]},
        })

    def test_ordinary_backend_ci_does_not_publish_or_build_eifs(self):
        config = workflow("opensecret-ci.yml")
        self.assertEqual(set(config["on"]), {"push", "pull_request", "schedule", "workflow_dispatch"})
        self.assertEqual(config["on"]["push"]["branches"], ["master"])
        self.assertEqual(config["on"]["pull_request"]["branches"], ["master"])
        self.assertFalse(any((ROOT / "services/opensecret/.github/workflows").glob("*.yml")))
        for value in strings(config["jobs"]):
            self.assertNotRegex(value, r"eif-|deploy-|stage-|scp-|update-pcr|append-pcr|verify-pcr")
            self.assertNotRegex(value, r"upload-artifact|download-artifact|flakehub-cache|gh release")
        cache = next(step["with"] for step in config["jobs"]["rust"]["steps"]
                     if "rust-cache@" in step.get("uses", ""))
        self.assertEqual(cache["workspaces"], "services/opensecret -> target")
        self.assertEqual(cache["save-if"],
                         "${{ github.event_name == 'push' && github.ref == 'refs/heads/master' }}")

    def test_eif_workflow_preserves_approvals_and_keeps_both_environments_independent(self):
        config = workflow("opensecret-eif.yml")
        self.assertEqual(set(config["on"]), {"push", "pull_request", "workflow_dispatch"})
        for event in ("push", "pull_request"):
            self.assertEqual(config["on"][event]["branches"], ["master"])
            self.assertNotIn("paths", config["on"][event])
        self.assertEqual(config["concurrency"]["group"],
                         "opensecret-eif-${{ github.event_name }}-${{ github.ref }}")
        self.assertEqual(set(config["jobs"]), {"changes", "eif", "eif-trusted"})
        for job_name in ("eif", "eif-trusted"):
            job = config["jobs"][job_name]
            self.assertEqual(job["needs"], "changes")
            self.assertEqual(job["strategy"]["matrix"], {"mode": ["dev", "prod"]})
            self.assertIs(job["strategy"]["fail-fast"], False)
            self.assertEqual(job["env"]["EIF_MODE"], "${{ matrix.mode }}")
            self.assertEqual(job["timeout-minutes"], 180 if job_name == "eif-trusted" else 90)
            for key in ("OPENSECRET_DEV_POSTGRES", "OPENSECRET_DEV_ENV", "OPENSECRET_DEV_CONTAINERS"):
                self.assertEqual(job["env"][key], "0")
            commands = [step["run"] for step in job["steps"] if "run" in step]
            self.assertEqual(commands, ['bash scripts/ci/check_opensecret_eif.sh "$EIF_MODE"'])
        for value in strings(config["jobs"]):
            self.assertNotRegex(value, r"deploy-|stage-|scp-|update-pcr|append-pcr|generate-keys")
            self.assertNotRegex(value, r"upload-artifact|download-artifact|gh release")

    def test_eif_cache_setup_precedes_build_and_warms_fork_compatible_cache(self):
        jobs = workflow("opensecret-eif.yml")["jobs"]
        installer_action = "DeterminateSystems/nix-installer-action@ef8a148080ab6020fd15196c2084a2eea5ff2d25"
        for job_name, cache_action, cache_inputs, determinate in (
            ("eif", "DeterminateSystems/magic-nix-cache-action@3c034b51a9deec0a09ef1df8b436ac5db50fae94", {
                "source-revision": "4cc363589df8090801c098cdcde1bdd42562318a",
                "use-flakehub": "disabled",
                "use-gha-cache": "enabled",
            }, False),
            ("eif-trusted", "DeterminateSystems/flakehub-cache-action@1f9a51a2959d3e26c7838c6f3bf9f48acae525ea", {
                "use-gha-cache": "enabled",
                "diff-store": True,
            }, True),
        ):
            with self.subTest(job=job_name):
                steps = jobs[job_name]["steps"]
                self.assertEqual(len(steps), 4)
                self.assertTrue(steps[0]["uses"].startswith("actions/checkout@"))
                self.assertEqual(steps[1]["uses"], installer_action)
                self.assertEqual(steps[1]["with"], {"determinate": determinate, "github-token": ""})
                self.assertEqual(steps[2]["uses"], cache_action)
                self.assertEqual(steps[2]["with"], cache_inputs)
                self.assertNotIn("if", steps[2])
                self.assertNotIn("continue-on-error", steps[2])
                self.assertEqual(steps[3]["run"], 'bash scripts/ci/check_opensecret_eif.sh "$EIF_MODE"')

    def test_eif_event_gate_matches_the_approval_policy(self):
        # Exercise the actual GitHub boolean expression with a restricted,
        # equivalent local representation, not a separately implemented policy.
        expressions = {}
        for job_name in ("eif", "eif-trusted"):
            expression = workflow("opensecret-eif.yml")["jobs"][job_name]["if"]
            expression = expression.strip().removeprefix("${{").removesuffix("}}").strip()
            expression = expression.replace("&&", " and ").replace("||", " or ")
            expression = expression.replace("!cancelled()", "NOT_CANCELLED").replace("always()", "True")
            expressions[job_name] = " ".join(expression.split())
        cases = (
            ("pull_request", "refs/pull/1/merge", "success", "true", "false", False),
            ("pull_request", "refs/pull/1/merge", "success", "true", "true", True),
            ("pull_request", "refs/pull/1/merge", "success", "false", "true", True),
            ("pull_request", "refs/pull/1/merge", "failure", "", "", False),
            ("pull_request", "refs/pull/1/merge", "failure", "true", "true", False),
            ("pull_request", "refs/pull/1/merge", "skipped", "", "", False),
            ("pull_request", "refs/pull/1/merge", "cancelled", "", "", False),
            ("pull_request", "refs/pull/1/merge", "success", "", "", False),
            ("pull_request", "refs/heads/master", "success", "true", "true", True),
            ("push", "refs/heads/master", "success", "true", "false", True),
            ("push", "refs/heads/master", "success", "false", "false", False),
            ("push", "refs/heads/master", "failure", "", "", True),
            ("push", "refs/heads/master", "skipped", "", "", True),
            ("push", "refs/heads/master", "success", "", "", True),
            ("push", "refs/heads/feature", "success", "true", "true", False),
            ("push", "refs/tags/master", "success", "true", "true", False),
            ("workflow_dispatch", "refs/heads/master", "success", "true", "false", True),
            ("workflow_dispatch", "refs/heads/master", "failure", "", "", True),
            ("workflow_dispatch", "refs/heads/feature", "success", "true", "false", True),
            ("workflow_dispatch", "refs/tags/review", "success", "true", "false", True),
            ("workflow_dispatch", "refs/tags/master", "failure", "", "", True),
            ("schedule", "refs/heads/master", "success", "true", "true", False),
            ("pull_request_target", "refs/heads/master", "success", "true", "true", False),
            ("workflow_run", "refs/heads/master", "success", "true", "true", False),
        )
        head_repositories = (
            ("MaplePrivacyLabs/Maple", True),
            ("contributor/Maple", False),
            ("MaplePrivacyLabs/Maple-fork", False),
            ("", False),  # Missing nested properties evaluate to an empty string.
            (None, False),
        )
        for case, (head_repo, same_repo), cancelled in itertools.product(
            cases, head_repositories, (False, True)
        ):
            event, ref, result, eif, approvals, expected = case
            with self.subTest(event=event, ref=ref, result=result, eif=eif,
                              approvals=approvals, head_repo=head_repo, cancelled=cancelled):
                trusted = (
                    (event == "pull_request" and same_repo) or
                    (event in ("push", "workflow_dispatch") and ref == "refs/heads/master")
                )
                selected = []
                for job_name, expression in expressions.items():
                    condition = expression
                    for key, value in {
                        "github.event_name": event, "github.ref": ref,
                        "github.event.pull_request.head.repo.full_name": head_repo,
                        "github.repository": "MaplePrivacyLabs/Maple",
                        "needs.changes.result": result,
                        "needs.changes.outputs.eif": eif,
                        "needs.changes.outputs.pcr_approvals": approvals,
                        "NOT_CANCELLED": not cancelled,
                    }.items():
                        condition = condition.replace(key, repr(value))
                    if eval(condition, {"__builtins__": {}}, {}):
                        selected.append(job_name)
                expected_jobs = ["eif-trusted" if trusted else "eif"] if expected and not cancelled else []
                self.assertEqual(selected, expected_jobs)

    def test_backend_retains_exact_rust_gates_and_disabled_stateful_shell_hooks(self):
        config = workflow("opensecret-ci.yml")
        self.assertEqual(config["defaults"]["run"]["working-directory"], "services/opensecret")
        self.assertEqual(config["env"]["RUSTFLAGS"], "-D warnings")
        for key in ("OPENSECRET_DEV_POSTGRES", "OPENSECRET_DEV_ENV", "OPENSECRET_DEV_CONTAINERS"):
            self.assertEqual(config["env"][key], "0")
        commands = [step["run"] for step in config["jobs"]["rust"]["steps"] if "run" in step]
        self.assertEqual(commands, [
            "nix develop --no-update-lock-file '.?submodules=1' -c cargo fmt --all -- --check",
            "nix develop --no-update-lock-file '.?submodules=1' -c cargo clippy --locked --all-targets --all-features -- -D warnings",
            "nix develop --no-update-lock-file '.?submodules=1' -c cargo test --locked --all-features",
        ])
        nix_commands = [step["run"] for step in config["jobs"]["nix"]["steps"] if "run" in step]
        self.assertEqual(nix_commands, [
            "nix flake check --no-update-lock-file --print-build-logs '.?submodules=1'",
            "nix build --no-link --no-update-lock-file '.?submodules=1#default'",
        ])
        audit = config["jobs"]["audit"]["steps"][-1]["with"]
        toolchain = tomllib.loads((ROOT / "services/opensecret/rust-toolchain.toml").read_text())
        self.assertEqual(audit["rust-version"], toolchain["toolchain"]["channel"])
        self.assertEqual(audit["manifest-path"], "services/opensecret/Cargo.toml")
        self.assertEqual(audit["command"], "check advisories bans")
        self.assertEqual(audit["arguments"], "--config services/opensecret/deny.toml --all-features --locked")

    def test_backend_ci_has_no_standalone_signed_pcr_job(self):
        jobs = workflow("opensecret-ci.yml")["jobs"]
        self.assertEqual(set(jobs), {"changes", "rust", "nix", "audit"})
        for value in strings(jobs):
            self.assertNotIn("pcr_compatibility.py", value)

    def test_selector_exports_only_active_checks_and_the_approval_signal(self):
        expected = {"rust", "nix", "integration", "audit", "eif", "pcr_approvals"}
        config = workflow("opensecret-change-detection.yml")
        self.assertEqual(set(OUTPUTS), expected)
        self.assertEqual(set(config["on"]["workflow_call"]["outputs"]), expected)
        self.assertEqual(set(config["jobs"]["detect"]["outputs"]), expected)

    def test_sdk_integration_uses_checked_out_backend_and_disposable_services(self):
        config = workflow("sdk-integration.yml")
        self.assertEqual(set(config["on"]), {"push", "pull_request", "workflow_dispatch"})
        job = config["jobs"]["sdk-integration"]
        checkouts = [step for step in job["steps"] if "checkout@" in step.get("uses", "")]
        self.assertEqual(len(checkouts), 1)
        self.assertEqual(checkouts[0]["with"]["submodules"], "recursive")
        commands = "\n".join(step.get("run", "") for step in job["steps"])
        self.assertNotIn(".ci/opensecret", commands)
        self.assertNotIn("opensecret-integration-revision", commands)
        self.assertFalse((ROOT / "sdk/opensecret-integration-revision").exists())
        self.assertIn("cd services/opensecret", commands)
        self.assertIn("'./services/opensecret?submodules=1'", commands)
        self.assertIn("services/opensecret/target/debug/opensecret", commands)
        self.assertIn("diesel migration run", commands)
        self.assertIn("sdk/test/integration/bootstrap.sql", commands)
        self.assertIn("bun test src/lib/test/integration/api.test.ts", commands)
        self.assertIn("cargo test --locked --all-features --tests", commands)
        self.assertEqual(config["env"]["APP_MODE"], "local")
        self.assertEqual(config["env"]["OPENAI_API_BASE"], "http://127.0.0.1:9")
        self.assertEqual(job["services"]["postgres"]["env"]["POSTGRES_DB"], "opensecret")

    def test_selector_failures_or_missing_outputs_cannot_skip_validation(self):
        for workflow_name, lanes in (("opensecret-ci.yml", {name: name for name in ("rust", "nix", "audit")}),
                                     ("sdk-integration.yml", {"sdk-integration": "integration"})):
            config = workflow(workflow_name)
            self.assertNotIn("paths", config["on"]["pull_request"])
            self.assertNotIn("paths", config["on"]["push"])
            for job_name, output in lanes.items():
                condition = config["jobs"][job_name]["if"]
                self.assertIn("always() && !cancelled()", condition)
                self.assertIn("needs.changes.result != 'success'", condition)
                self.assertIn(f"needs.changes.outputs.{output} != 'false'", condition)


class SdkWorkflowFailurePropagationTests(unittest.TestCase):
    CHECKS = (
        ("sdk-typescript.yml", "sdk-typescript", (
            "bun install", "bun audit", "bun run format:check", "bun run build", "bun test",
        )),
        ("sdk-rust.yml", "sdk-rust", (
            "cargo fmt", "cargo clippy", "cargo test", "cargo doc",
        )),
    )

    def run_checks(self, name, job, failure=""):
        steps = [step["run"] for step in workflow(name)["jobs"][job]["steps"] if "run" in step]
        # Execute the actual nested shell body, without letting an outer -e or
        # host login profile accidentally supply the workflow's missing guard.
        self.assertEqual(len(steps), 1)
        command = shlex.split(steps[0])
        self.assertEqual(command[:-1], [
            "nix", "develop", "--no-update-lock-file", "./sdk", "-c", "bash", "-lc",
        ])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "sdk/rust").mkdir(parents=True)
            binaries = root / "bin"
            binaries.mkdir()
            trace = root / "commands"
            for tool in ("bun", "cargo"):
                stub = binaries / tool
                stub.write_text(
                    f"#!{sys.executable}\n"
                    "import os, sys\n"
                    "from pathlib import Path\n"
                    "command = ' '.join([Path(sys.argv[0]).name, *sys.argv[1:]])\n"
                    "with open(os.environ['SDK_CHECK_TRACE'], 'a') as trace:\n"
                    "    trace.write(command + '\\n')\n"
                    "failure = os.environ['SDK_CHECK_FAILURE']\n"
                    "sys.exit(42 if failure and command.startswith(failure) else 0)\n"
                )
                stub.chmod(0o755)
            result = subprocess.run(
                [shutil.which("bash"), "--noprofile", "--norc", "-c", command[-1]],
                cwd=root,
                env={"PATH": str(binaries), "HOME": temporary,
                     "SDK_CHECK_TRACE": str(trace), "SDK_CHECK_FAILURE": failure},
                capture_output=True, text=True,
            )
            return result, trace.read_text().splitlines() if trace.exists() else []

    def test_each_failed_sdk_check_stops_the_workflow(self):
        for name, job, checks in self.CHECKS:
            for index, failure in enumerate(checks):
                with self.subTest(workflow=name, failure=failure):
                    result, commands = self.run_checks(name, job, failure)
                    self.assertEqual(result.returncode, 42, result.stderr)
                    self.assertEqual(len(commands), index + 1, commands)
                    for command, expected in zip(commands, checks):
                        self.assertTrue(command.startswith(expected), command)

    def test_successful_sdk_checks_all_execute(self):
        for name, job, checks in self.CHECKS:
            with self.subTest(workflow=name):
                result, commands = self.run_checks(name, job)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(len(commands), len(checks), commands)
                for command, expected in zip(commands, checks):
                    self.assertTrue(command.startswith(expected), command)


class EifComparisonCommandTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.component = self.repo / "services/opensecret"
        self.component.mkdir(parents=True)
        scripts = self.repo / "scripts/ci"
        scripts.mkdir(parents=True)
        self.script = scripts / "check_opensecret_eif.sh"
        shutil.copyfile(ROOT / "scripts/ci/check_opensecret_eif.sh", self.script)
        self.binaries = self.root / "bin"
        self.binaries.mkdir()
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()
        for tool in ("dirname", "mktemp", "rm", "diff"):
            (self.binaries / tool).symlink_to(shutil.which(tool))
        uname = self.binaries / "uname"
        uname.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "print('Linux' if sys.argv[1] == '-s' else os.environ.get('TEST_ARCH', 'aarch64'))\n"
        )
        uname.chmod(0o755)
        nix = self.binaries / "nix"
        nix.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "Path(os.environ['TEST_TRACE']).write_text(json.dumps({'args': args, 'cwd': os.getcwd()}))\n"
            "if os.environ.get('TEST_FAIL_BUILD'):\n"
            "    sys.exit(42)\n"
            "output = Path(args[args.index('--out-link') + 1])\n"
            "output.mkdir()\n"
            "if not os.environ.get('TEST_MISSING_IMAGE'):\n"
            "    (output / 'image.eif').write_bytes(b'fixture EIF, not a real image')\n"
            "if os.environ.get('TEST_PCR_DIRECTORY'):\n"
            "    (output / 'pcr.json').mkdir()\n"
            "elif not os.environ.get('TEST_MISSING_PCR'):\n"
            "    (output / 'pcr.json').write_text(os.environ['TEST_MEASUREMENTS'])\n"
        )
        nix.chmod(0o755)
        self.measurements = {
            mode: json.dumps({"HashAlgorithm": "fixture", "PCR0": f"reviewed-{mode}"}) + "\n"
            for mode in ("dev", "prod")
        }
        for mode, name in (("dev", "pcrDev.json"), ("prod", "pcrProd.json")):
            (self.component / name).write_text(self.measurements[mode])
        for name in ("pcrDevHistory.json", "pcrProdHistory.json"):
            (self.component / name).write_text("untouched history fixture\n")
        self.existing_result = self.component / "result"
        self.existing_result.symlink_to(self.root / "operator-owned-output")
        self.sentinel = self.root / "dotenv-was-loaded"
        (self.component / ".env").write_text(f"touch '{self.sentinel}'\n")
        self.before_files = {
            p.name: p.read_bytes() for p in self.component.iterdir() if p.is_file()
        }

    def run_check(self, mode="dev", **extra_env):
        env = {
            "PATH": str(self.binaries), "HOME": str(self.root),
            "TMPDIR": str(self.scratch), "TEST_TRACE": str(self.root / "trace"),
            "TEST_MEASUREMENTS": self.measurements.get(mode, self.measurements["dev"]), **extra_env,
        }
        result = subprocess.run(
            [shutil.which("bash"), "--noprofile", "--norc", str(self.script), mode],
            cwd=self.root, env=env, capture_output=True, text=True,
        )
        self.assertFalse(self.sentinel.exists())
        self.assertEqual(os.readlink(self.existing_result), str(self.root / "operator-owned-output"))
        self.assertEqual(
            {p.name: p.read_bytes() for p in self.component.iterdir() if p.is_file()},
            self.before_files,
        )
        self.assertEqual(list(self.scratch.iterdir()), [])
        return result

    def test_each_environment_builds_the_pinned_component_without_loading_dotenv(self):
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                result = self.run_check(mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"EIF/PCR approval match ({mode})", result.stdout)
                trace = json.loads((self.root / "trace").read_text())
                self.assertEqual(trace["cwd"], str(self.component))
                self.assertEqual(trace["args"][:4], [
                    "build", "--no-update-lock-file", "--print-build-logs", "--out-link",
                ])
                self.assertEqual(trace["args"][-1], f".?submodules=1#eif-{mode}")

    def test_measurement_mismatch_fails_without_rewriting_approvals(self):
        result = self.run_check(TEST_MEASUREMENTS='{"PCR0":"not approved"}\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("EIF/PCR approval mismatch", result.stderr)

    def test_build_failure_and_missing_generated_measurements_fail(self):
        result = self.run_check(TEST_FAIL_BUILD="1")
        self.assertEqual(result.returncode, 42)
        for failure in ("TEST_MISSING_PCR", "TEST_MISSING_IMAGE", "TEST_PCR_DIRECTORY"):
            with self.subTest(failure=failure):
                result = self.run_check(**{failure: "1"})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("did not produce", result.stderr)

    def test_invalid_mode_and_wrong_architecture_do_not_build(self):
        for mode, extra_env in (("preview", {}), ("dev", {"TEST_ARCH": "x86_64"})):
            with self.subTest(mode=mode, extra_env=extra_env):
                result = self.run_check(mode, **extra_env)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "trace").exists())

    def test_missing_or_symlinked_approved_measurements_do_not_build(self):
        reference = self.component / "pcrDev.json"
        reference.unlink()
        self.before_files.pop(reference.name)
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "trace").exists())
        reference.symlink_to(self.component / "pcrProd.json")
        self.before_files[reference.name] = self.measurements["prod"].encode()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "trace").exists())

    def test_directory_instead_of_approved_measurements_does_not_build(self):
        reference = self.component / "pcrDev.json"
        reference.unlink()
        self.before_files.pop(reference.name)
        reference.mkdir()
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Missing regular approved measurement file", result.stderr)
        self.assertFalse((self.root / "trace").exists())


class OpenSecretDiffSelectionTests(unittest.TestCase):
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
        shutil.copyfile(ROOT / "scripts/ci/opensecret_change_detection.py", scripts / "opensecret_change_detection.py")
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

    def select(self, event, base, head, *, succeeds=True):
        step = next(step for step in workflow("opensecret-change-detection.yml")["jobs"]["detect"]["steps"]
                    if step.get("id") == "classify")
        output = self.root / "output"
        output.unlink(missing_ok=True)
        env = {**os.environ, "GITHUB_EVENT_NAME": event, "BASE_SHA": base, "HEAD_SHA": head,
               "GITHUB_OUTPUT": str(output)}
        result = subprocess.run(["bash", "-c", step["run"]], cwd=self.root, env=env,
                                capture_output=True, text=True)
        if succeeds:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Cannot determine the PR's approved-PCR changes", result.stdout)
        return dict(line.split("=", 1) for line in output.read_text().splitlines()) if output.exists() else {}

    def expected(self, *selected):
        return {name: "true" if name in selected else "false" for name in OUTPUTS}

    def test_docs_only_push_backend_runtime_push_and_signed_pcr_push(self):
        docs = self.commit_file("services/opensecret/docs/design.md", "design\n")
        self.assertEqual(self.select("push", self.base, docs), self.expected())
        runtime = self.commit_file("services/opensecret/src/main.rs", "fn main() {}\n")
        self.assertEqual(self.select("push", docs, runtime), self.expected("rust", "nix", "integration", "eif"))
        pcr = self.commit_file("services/opensecret/pcrDevHistory.json", "[]\n")
        self.assertEqual(self.select("push", runtime, pcr), self.expected("eif", "pcr_approvals"))

    def test_pull_request_uses_merge_base_instead_of_unrelated_base_changes(self):
        master = self.commit_file("services/opensecret/src/main.rs", "fn main() {}\n")
        self.git("checkout", "-qb", "contributor", self.base)
        docs = self.commit_file("services/opensecret/docs/design.md", "design\n")
        self.assertEqual(self.select("pull_request", master, docs), self.expected())

    def test_unrelated_master_approvals_do_not_count_as_pr_approval_edits(self):
        master = self.commit_file("services/opensecret/pcrDev.json", "approved on master\n")
        self.git("checkout", "-qb", "contributor", self.base)
        runtime = self.commit_file("services/opensecret/src/main.rs", "fn main() {}\n")
        self.assertEqual(self.select("pull_request", master, runtime),
                         self.expected("rust", "nix", "integration", "eif"))

    def test_pr_approval_edit_selects_comparison_even_with_backend_changes(self):
        runtime = self.commit_file("services/opensecret/src/main.rs", "fn main() {}\n")
        approvals = self.commit_file("services/opensecret/pcrProdHistory.json", "[]\n")
        self.assertEqual(self.select("pull_request", self.base, approvals),
                         self.expected("rust", "nix", "integration", "eif", "pcr_approvals"))
        self.assertEqual(self.select("pull_request", runtime, approvals),
                         self.expected("eif", "pcr_approvals"))

    def test_deletion_or_rename_out_of_backend_still_selects_contract_checks(self):
        runtime = self.commit_file("services/opensecret/src/main.rs", "fn main() {}\n")
        self.git("mv", "services/opensecret/src/main.rs", "LICENSE")
        self.git("commit", "-qam", "rename fixture")
        self.assertEqual(self.select("push", runtime, self.git("rev-parse", "HEAD")),
                         self.expected("rust", "nix", "integration", "eif"))

    def test_deletion_or_rename_of_an_approval_file_is_an_explicit_edit(self):
        for rename in (False, True):
            with self.subTest(rename=rename):
                before = self.commit_file("services/opensecret/pcrProd.json", "approval\n")
                if rename:
                    (self.root / "services/opensecret/docs").mkdir(exist_ok=True)
                    self.git("mv", "services/opensecret/pcrProd.json", "services/opensecret/docs/old-approval.json")
                else:
                    self.git("rm", "services/opensecret/pcrProd.json")
                self.git("commit", "-qam", "remove approval fixture")
                self.assertEqual(self.select("pull_request", before, self.git("rev-parse", "HEAD")),
                                 self.expected("eif", "pcr_approvals"))

    def test_missing_history_manual_event_classifier_failure_and_partial_output_fail_safe(self):
        for event, base, head in (("push", "0" * 40, self.base), ("push", "a" * 40, self.base),
                                  ("workflow_dispatch", "", "")):
            with self.subTest(event=event, base=base):
                self.assertEqual(self.select(event, base, head), self.expected(*CHECK_OUTPUTS))
        classifier = self.root / "scripts/ci/opensecret_change_detection.py"
        classifier.write_text("print('rust=false')\nraise RuntimeError('fixture')\n")
        self.assertEqual(self.select("push", self.base, self.base), self.expected(*CHECK_OUTPUTS))
        self.assertEqual(self.select("pull_request", self.base, self.base, succeeds=False), {})

    def test_pr_missing_history_fails_routing_without_inventing_approval_changes(self):
        self.assertEqual(self.select("pull_request", "a" * 40, self.base, succeeds=False), {})

    def test_successful_but_incomplete_classifier_output_is_rejected(self):
        classifier = self.root / "scripts/ci/opensecret_change_detection.py"
        classifier.write_text(
            "OUTPUTS = " + repr(OUTPUTS) + "\nprint('rust=false')\n"
        )
        self.assertEqual(self.select("pull_request", self.base, self.base, succeeds=False), {})
        self.assertEqual(self.select("push", self.base, self.base), self.expected(*CHECK_OUTPUTS))

    def test_schedule_selects_only_advisory_audit(self):
        self.assertEqual(self.select("schedule", "", ""), self.expected("audit"))


if __name__ == "__main__":
    unittest.main()
