"""Backend, client-compatibility, and application-packaging routing regressions."""

from pathlib import Path
import subprocess
import sys
import unittest

from agent_change_detection import affects_agent
from change_detection import classify_path as research_routes
from opensecret_change_detection import CHECK_OUTPUTS, OUTPUTS, classify_paths


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

    def test_sdk_changes_select_compatibility_without_app_routing(self):
        for path in ("sdk/src/lib/client.ts", "sdk/src/lib/test/integration/api.test.ts",
                     "sdk/rust/src/client.rs", "sdk/rust/Cargo.lock", "sdk/test/integration/bootstrap.sql",
                     "sdk/flake.nix", "sdk/bun.lock", "sdk/package.json"):
            with self.subTest(path=path):
                self.assert_routes([path], "integration")
        self.assertEqual(research_routes("sdk/src/lib/client.ts"), frozenset())
        self.assertFalse(affects_agent("sdk/rust/src/client.rs"))

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
        self.assert_routes([".github/workflows/opensecret-ci.yml"], "rust", "nix", "audit", "eif")
        self.assert_routes([".github/workflows/sdk-integration.yml"], "integration")

    def test_eif_workflow_and_comparison_helper_select_only_master_eif_checks(self):
        for path in (".github/workflows/opensecret-eif.yml", "scripts/ci/check_opensecret_eif.sh"):
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


if __name__ == "__main__":
    unittest.main()
