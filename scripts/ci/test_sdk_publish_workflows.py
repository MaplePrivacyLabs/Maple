"""Regression tests for the SDK publication privilege and artifact boundaries."""

import functools
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


@functools.cache
def workflow(kind):
    result = subprocess.run(
        ["yq", "-o=json", ".", str(ROOT / f".github/workflows/sdk-publish-{kind}.yml")],
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


class SDKPublishingWorkflowTests(unittest.TestCase):
    def test_only_manual_master_publication_and_independent_serialization(self):
        groups = set()
        for kind in ("npm", "rust"):
            with self.subTest(kind=kind):
                config = workflow(kind)
                self.assertEqual(set(config["on"]), {"workflow_dispatch"})
                inputs = config["on"]["workflow_dispatch"]["inputs"]
                self.assertEqual(set(inputs), {"version", "mode", "dry_run"})
                self.assertTrue(inputs["version"]["required"])
                self.assertEqual(inputs["mode"]["options"], ["trusted", "bootstrap"])
                self.assertEqual(inputs["mode"]["default"], "trusted")
                self.assertIs(inputs["dry_run"]["default"], True)
                self.assertIs(config["concurrency"]["cancel-in-progress"], False)
                groups.add(config["concurrency"]["group"])
                for job in config["jobs"].values():
                    condition = job["if"]
                    for guard in (
                        "github.event_name == 'workflow_dispatch'",
                        "github.ref == 'refs/heads/master'",
                        "github.repository == 'MaplePrivacyLabs/Maple'",
                        "github.repository_id == '923138240'",
                        "github.repository_owner_id == '322649754'",
                    ):
                        self.assertIn(guard, condition)
                self.assertIn("!inputs.dry_run", config["jobs"]["publish"]["if"])
                self.assertEqual(config["jobs"]["publish"]["needs"], "build")
        self.assertEqual(len(groups), 2)

    def test_builders_cannot_receive_registry_or_oidc_credentials(self):
        for kind in ("npm", "rust"):
            with self.subTest(kind=kind):
                config = workflow(kind)
                self.assertEqual(config["permissions"], {"contents": "read"})
                job = config["jobs"]["build"]
                self.assertNotIn("environment", job)
                self.assertNotIn("permissions", job)
                for value in strings(job):
                    self.assertNotRegex(value, r"\bsecrets\b|github\.token|id-token|NODE_AUTH_TOKEN|CARGO_REGISTRY_TOKEN")
                self.assertIn("sdk_publish.py source", job["steps"][1]["run"])
                uploads = [step for step in job["steps"] if "upload-artifact@" in step.get("uses", "")]
                self.assertEqual(len(uploads), 1)
                self.assertEqual(uploads[0]["with"]["path"], "${{ runner.temp }}/sdk-bundle/")
                self.assertEqual(uploads[0]["with"]["if-no-files-found"], "error")
                self.assertEqual(job["outputs"]["artifact_id"], "${{ steps.bundle.outputs.artifact-id }}")

    def test_all_actions_are_pinned_and_checkout_cannot_change_source(self):
        for kind in ("npm", "rust"):
            for job in workflow(kind)["jobs"].values():
                for step in job["steps"]:
                    self.assertNotIn("${{", step.get("run", ""))
                    if "python3" in step.get("run", ""):
                        self.assertNotRegex(step["run"], r"python3 (?!-I\b)")
                    action = step.get("uses", "")
                    if action:
                        self.assertRegex(action, r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
                    if action.startswith("actions/checkout@"):
                        self.assertIs(step["with"]["persist-credentials"], False)
                        self.assertNotIn("submodules", step["with"])
                        self.assertEqual(step["with"]["ref"], "${{ github.sha }}")
                    if action.startswith("DeterminateSystems/determinate-nix-action@"):
                        self.assertEqual(step["with"]["github-token"], "")

    def test_publishers_only_upload_validated_same_run_artifacts(self):
        for kind, environment, token in (
            ("npm", "sdk-npm", "NPM_BOOTSTRAP_TOKEN"),
            ("rust", "sdk-crates", "CARGO_REGISTRY_TOKEN"),
        ):
            with self.subTest(kind=kind):
                job = workflow(kind)["jobs"]["publish"]
                self.assertEqual(job["environment"], environment)
                self.assertEqual(job["permissions"], {"contents": "read", "id-token": "write"})
                downloads = [step for step in job["steps"] if "download-artifact@" in step.get("uses", "")]
                self.assertEqual(len(downloads), 1)
                self.assertEqual(downloads[0]["with"], {
                    "artifact-ids": "${{ needs.build.outputs.artifact_id }}",
                    "merge-multiple": True,
                    "path": "${{ runner.temp }}/sdk-bundle",
                })
                publication = next(step for step in job["steps"] if step.get("id") == "publish")
                self.assertTrue(publication["continue-on-error"])
                self.assertEqual(set(publication["env"]), {token})
                before = job["steps"][:job["steps"].index(publication)]
                self.assertTrue(any("sdk_publish.py verify" in step.get("run", "") for step in before))
                final = job["steps"][-1]
                self.assertIn("always()", final["if"])
                self.assertIn("steps.publish.outcome != 'skipped'", final["if"])
                self.assertIn("sdk_publish.py confirm", final["run"])
                self.assertNotIn("env", final)
                for step in job["steps"]:
                    for text in strings(step):
                        self.assertNotRegex(text, r"actions/cache@|rust-cache@|nix-installer-action@|\bnpm (?:ci|install)\b|\bbun\b|\bcargo (?:build|test|package|publish)\b")
                    if step is not publication:
                        for text in strings(step):
                            self.assertNotIn("secrets.", text)

    def test_no_workflow_creates_releases_tags_or_write_contents(self):
        for kind in ("npm", "rust"):
            config = workflow(kind)
            for text in strings(config):
                self.assertNotRegex(text, r"contents: write|gh release|git (?:push|tag)|releases/latest|softprops/action-gh-release")
            self.assertEqual(config["permissions"], {"contents": "read"})


class NPMPublishCommandTests(unittest.TestCase):
    def run_publish(self, mode, token=""):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        tools = root / "bin"
        tools.mkdir()
        (root / "sdk-bundle").mkdir()
        (root / "sdk-bundle/package.tgz").write_bytes(b"fixture package")
        # A checkout .npmrc must never be used by the privileged npm invocation.
        checkout = root / "checkout"
        checkout.mkdir()
        (checkout / ".npmrc").write_text("registry=https://attacker.invalid/\n")
        (tools / "node").write_text("#!/bin/sh\necho v24.14.0\n")
        (tools / "npm").write_text(
            # Linux Nix sandboxes do not provide /usr/bin/env.
            f"#!{sys.executable}\n"
            "import json, os, pathlib, sys\n"
            "if sys.argv[1:] == ['--version']:\n"
            "    print('11.9.0')\n"
            "    sys.exit(0)\n"
            "config = pathlib.Path(os.environ['NPM_CONFIG_USERCONFIG'])\n"
            "pathlib.Path(os.environ['RESULT']).write_text(json.dumps({\n"
            "  'args': sys.argv[1:], 'cwd': os.getcwd(),\n"
            "  'config': config.read_text(),\n"
            "  'global': os.environ.get('NPM_CONFIG_GLOBALCONFIG'),\n"
            "  'has_auth': bool(os.environ.get('NODE_AUTH_TOKEN')),\n"
            "  'has_bootstrap': 'NPM_BOOTSTRAP_TOKEN' in os.environ,\n"
            "  'has_legacy': 'NPM_TOKEN' in os.environ,\n"
            "}))\n"
        )
        for path in tools.iterdir():
            path.chmod(0o755)
        command = next(step["run"] for step in workflow("npm")["jobs"]["publish"]["steps"]
                       if step.get("id") == "publish")
        result = subprocess.run(["bash", "-c", command], cwd=checkout,
                                env={**os.environ, "PATH": f"{tools}:{os.environ['PATH']}",
                                     "RUNNER_TEMP": str(root), "SDK_MODE": mode,
                                     "NPM_BOOTSTRAP_TOKEN": token, "NODE_AUTH_TOKEN": "ambient-fixture",
                                     "NPM_TOKEN": "ambient-fixture", "RESULT": str(root / "result")},
                                text=True, capture_output=True)
        return result, root

    def test_trusted_mode_uses_no_fallback_token_or_checkout_config(self):
        result, root = self.run_publish("trusted")
        self.assertEqual(result.returncode, 0, result.stderr)
        recorded = json.loads((root / "result").read_text())
        self.assertFalse(recorded["has_auth"])
        self.assertFalse(recorded["has_bootstrap"])
        self.assertFalse(recorded["has_legacy"])
        self.assertEqual(recorded["config"], "")
        self.assertEqual(recorded["global"], "/dev/null")
        self.assertNotEqual(recorded["cwd"], str(root / "checkout"))
        self.assertEqual(recorded["args"], [
            "publish", str(root / "sdk-bundle/package.tgz"), "--ignore-scripts", "--provenance",
            "--access", "public", "--tag", "latest", "--fetch-retries=0",
            "--registry", "https://registry.npmjs.org/",
        ])

    def test_bootstrap_token_is_step_local_and_not_written_to_disk_or_logs(self):
        result, root = self.run_publish("bootstrap", "bootstrap-fixture-value")
        self.assertEqual(result.returncode, 0, result.stderr)
        recorded = json.loads((root / "result").read_text())
        self.assertTrue(recorded["has_auth"])
        self.assertFalse(recorded["has_bootstrap"])
        self.assertEqual(recorded["config"], "//registry.npmjs.org/:_authToken=${NODE_AUTH_TOKEN}\n")
        self.assertNotIn("bootstrap-fixture-value", result.stdout + result.stderr)
        self.assertFalse(list(root.glob("sdk-npm-publish.*")))

    def test_missing_bootstrap_secret_or_invalid_mode_never_invokes_publish(self):
        for mode in ("bootstrap", "anything-else"):
            with self.subTest(mode=mode):
                result, root = self.run_publish(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((root / "result").exists())


if __name__ == "__main__":
    unittest.main()
