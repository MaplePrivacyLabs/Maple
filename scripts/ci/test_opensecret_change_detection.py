"""Backend, client-compatibility, and application-packaging routing regressions."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from agent_change_detection import affects_agent
from change_detection import classify_path as research_routes
from opensecret_change_detection import CHECK_OUTPUTS, OUTPUTS, classify_paths
from test_workflow_policy import ROOT, load


class OpenSecretChangeDetectionTests(unittest.TestCase):
    def assert_routes(self, paths, *selected):
        self.assertEqual(classify_paths(paths), {name: name in selected for name in OUTPUTS})

    def test_backend_runtime_and_migrations_select_compatibility_without_packaging_apps(self):
        for relative in ("src/main.rs", "src/web/session.rs", "tests/contracts.rs",
                         "migrations/2026/up.sql", ".cargo/config.toml", "build.rs"):
            path = "services/opensecret/" + relative
            with self.subTest(path=path):
                expected = ("rust", "integration") if relative.startswith(("tests/", "migrations/")) else ("rust", "nix", "integration", "eif")
                self.assert_routes([path], *expected)
                self.assertEqual(research_routes(path), frozenset())
                self.assertFalse(affects_agent(path))

    def test_backend_dependency_and_toolchain_inputs_select_all_backend_checks(self):
        for relative in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "flake.nix", "flake.lock"):
            path = "services/opensecret/" + relative
            with self.subTest(path=path):
                self.assert_routes([path], *CHECK_OUTPUTS)
                self.assertEqual(research_routes(path), frozenset())
                self.assertFalse(affects_agent(path))

    def test_backend_flake_shell_test_selects_nix_without_rust_or_app_builds(self):
        path = "services/opensecret/tests/entrypoint_entropy_preflight.sh"
        self.assert_routes([path], "nix")
        self.assertEqual(research_routes(path), frozenset())
        self.assertFalse(affects_agent(path))

    def test_backend_nix_only_inputs_and_dependency_policy_keep_independent_checks(self):
        for relative in ("nix/kernel-upstream.nix", "entrypoint.sh", "continuum-proxy",
                         "nitro-toolkit", "nitro-toolkit/init/main.c", "privatemode-public"):
            self.assert_routes(["services/opensecret/" + relative], "nix", "eif")
        self.assert_routes(["services/opensecret/deny.toml"], "audit")
        self.assert_routes(["services/opensecret/.env.sample"], "integration")

    def test_parent_helper_images_do_not_select_measured_eif_checks(self):
        self.assert_routes(["services/opensecret/nitro-toolkit/README.md"])
        for relative in (
            "nitro-toolkit/credential_requester/Dockerfile",
            "nitro-toolkit/credential_requester/requirements.in",
            "nitro-toolkit/credential_requester/requirements.txt",
            "nitro-toolkit/credential_requester/credential_requester.py",
            "nitro-toolkit/credential_requester/test_credential_requester.py",
            "nitro-toolkit/logging/Dockerfile",
            "nitro-toolkit/logging/requirements.in",
            "nitro-toolkit/logging/requirements.txt",
            "nitro-toolkit/logging/cloudwatch_logger.py",
            "nitro-toolkit/logging/test_cloudwatch_logger.py",
        ):
            with self.subTest(path=relative):
                self.assert_routes(["services/opensecret/" + relative], "helpers")
        # These files are copied into the enclave rootfs and must retain EIF routing.
        for relative in ("nitro-toolkit/traffic_forwarder.py", "nitro-toolkit/vsock_helper.py"):
            with self.subTest(path=relative):
                self.assert_routes(["services/opensecret/" + relative], "nix", "eif")

    def test_only_the_four_approved_json_files_select_pr_eif_comparison(self):
        for relative in ("pcrDev.json", "pcrDevHistory.json", "pcrProd.json", "pcrProdHistory.json"):
            path = "services/opensecret/" + relative
            with self.subTest(path=path):
                self.assert_routes([path], "eif", "pcr_approvals")
                self.assertEqual(research_routes(path), frozenset())
                self.assertFalse(affects_agent(path))

    def test_pcr_tooling_and_other_pcr_names_do_not_select_backend_jobs(self):
        for relative in ("pcrPreview.json", "pcrPreviewHistory.json", "pcr_sign.js", "pcr_verify.js",
                         "scripts/pcr_compatibility.py", "scripts/test_pcr_compatibility.py"):
            path = "services/opensecret/" + relative
            with self.subTest(path=path):
                self.assert_routes([path])
                self.assertEqual(research_routes(path), frozenset())
                self.assertFalse(affects_agent(path))

    def test_sdk_changes_select_compatibility_and_only_rust_inputs_route_to_apps(self):
        for path in ("sdk/src/lib/client.ts", "sdk/src/lib/test/integration/api.test.ts",
                     "sdk/rust/src/client.rs", "sdk/rust/Cargo.lock", "sdk/test/integration/bootstrap.sql",
                     "sdk/flake.nix", "sdk/bun.lock", "sdk/package.json"):
            with self.subTest(path=path):
                self.assert_routes([path], "integration")
        # Frontends pin the TypeScript SDK; the proxy and both desktops build sdk/rust from the tree.
        self.assertEqual(research_routes("sdk/src/lib/client.ts"), frozenset())
        self.assertFalse(affects_agent("sdk/src/lib/client.ts"))
        self.assertTrue(affects_agent("sdk/rust/src/client.rs"))
        self.assertFalse(affects_agent("sdk/rust/Cargo.lock"))

    def test_independent_components_and_docs_skip_backend_checks(self):
        for path in ("apps/maple-research/frontend/src/main.tsx", "apps/maple-agent/app/src/main.rs",
                     "proxy/src/proxy.rs", "services/updates/src/index.ts", "sdk/README.md",
                     "docs/monorepo-plan.md", "README.md", "flake.nix", "flake.lock",
                     "services/opensecret/README.md", "services/opensecret/AGENTS.md",
                     "services/opensecret/docs/nitro-deploy.md", "services/opensecret/.agents/skills/example.md",
                     ".github/workflows/agent-ci.yml", "scripts/ci/change_detection.py"):
            with self.subTest(path=path):
                self.assert_routes([path])

    def test_disposable_database_harness_selects_integration_without_app_packaging(self):
        path = ".agents/skills/validate-opensecret/scripts/disposable_db_tests.sh"
        self.assert_routes([path], "integration")
        self.assertEqual(research_routes(path), frozenset())
        self.assertFalse(affects_agent(path))
        self.assert_routes([".agents/skills/validate-opensecret/SKILL.md"])

    def test_submodules_and_selector_changes_select_all_backend_checks(self):
        for path in (".gitmodules", ".github/workflows/opensecret-change-detection.yml",
                     "scripts/ci/opensecret_change_detection.py"):
            self.assert_routes([path], *CHECK_OUTPUTS)
        self.assertFalse(affects_agent(".gitmodules"))
        self.assertEqual(research_routes(".gitmodules"), frozenset())
        self.assert_routes([".github/workflows/opensecret-ci.yml"], "rust", "nix", "audit", "eif", "helpers")
        self.assert_routes([".github/workflows/sdk-integration.yml"], "integration")

    def test_eif_workflow_and_build_helpers_select_only_eif_checks(self):
        for path in (".github/workflows/opensecret-eif.yml", "scripts/ci/build_opensecret_eif.sh",
                     "scripts/ci/check_opensecret_eif.sh", "scripts/ci/verify_opensecret_eif_rootfs.py",
                     "scripts/ci/fixtures/eif/comodo-aaa-services-root.pem"):
            self.assert_routes([path], "eif")
            self.assertEqual(research_routes(path), frozenset())
            self.assertFalse(affects_agent(path))

    def test_mixed_backend_and_approval_changes_retain_both_signals(self):
        self.assert_routes(
            ["services/opensecret/src/main.rs", "services/opensecret/pcrProd.json"],
            "rust", "nix", "integration", "eif", "pcr_approvals",
        )
        self.assert_routes(
            ["sdk/src/lib/client.ts", "services/opensecret/pcrDevHistory.json"],
            "integration", "eif", "pcr_approvals",
        )

    def test_unknown_inputs_invalid_paths_and_empty_diff(self):
        for path in ("unknown-root.toml", "services/opensecret/new-build-input", "", "/tmp/path",
                     "services/opensecret/../updates/src/main.ts"):
                self.assert_routes([path], *CHECK_OUTPUTS)
        self.assert_routes([])
        self.assert_routes(["README.md", "services/opensecret/src/main.rs"], "rust", "nix", "integration", "eif")

    def test_null_delimited_cli_and_explicit_fallback(self):
        script = Path(__file__).with_name("opensecret_change_detection.py")
        for arguments, paths, selected in (
            ([], b"README.md\0services/opensecret/src/name with\nnewline.rs\0", {"rust", "nix", "integration", "eif"}),
            ([], b"README.md\0services/opensecret/docs/note with spaces.md\0", set()),
            (["--all"], b"", set(CHECK_OUTPUTS)),
        ):
            result = subprocess.run([sys.executable, str(script), *arguments], input=paths,
                                    check=True, capture_output=True)
            expected = "".join(f"{name}={'true' if name in selected else 'false'}\n" for name in OUTPUTS)
            self.assertEqual(result.stdout.decode(), expected)

    def test_provider_manifests_only_select_the_flake_check(self):
        for path in ("services/opensecret/secretspec.toml",
                     "services/opensecret/secretspec/pcr-signing.toml",
                     "services/opensecret/secretspec/README.md"):
            with self.subTest(path=path):
                self.assert_routes([path], "nix")
                self.assertEqual(research_routes(path), frozenset())
                self.assertFalse(affects_agent(path))


    def test_component_hook_scripts_select_nothing(self):
        self.assert_routes(["services/opensecret/.githooks/pre-commit"])


class OpenSecretDiffSelectionTests(unittest.TestCase):
    """The opensecret-change-detection.yml classify step, run against fixture histories."""
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
        step = next(step for step in load(ROOT / ".github/workflows/opensecret-change-detection.yml")
                    ["jobs"]["detect"]["steps"] if step.get("id") == "classify")
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

    def test_parent_helper_only_diff_skips_eif_while_enclave_input_retains_it(self):
        helper = self.commit_file("services/opensecret/nitro-toolkit/logging/Dockerfile", "FROM scratch\n")
        for event in ("push", "pull_request"):
            with self.subTest(event=event):
                self.assertEqual(self.select(event, self.base, helper), self.expected("helpers"))
        docs = self.commit_file("services/opensecret/nitro-toolkit/README.md", "helper docs\n")
        self.assertEqual(self.select("push", helper, docs), self.expected())
        enclave = self.commit_file("services/opensecret/nitro-toolkit/vsock_helper.py", "# enclave input\n")
        self.assertEqual(self.select("push", docs, enclave), self.expected("nix", "eif"))

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
