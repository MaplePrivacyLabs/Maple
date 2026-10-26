"""Keep Agent data publication separate from Research and signing authority."""

import functools
import json
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
MASTER = "github.repository == 'MaplePrivacyLabs/Maple' && github.ref == 'refs/heads/master' && github.event_name == 'workflow_dispatch'"


@functools.cache
def workflow(name):
    return json.loads(subprocess.check_output(
        ["yq", "-o=json", ".", str(ROOT / ".github/workflows" / name)], text=True,
    ))


class AgentUpdatesWorkflows(unittest.TestCase):
    def test_manual_master_only_and_separate_authority(self):
        config = workflow("agent-updates-publish.yml")
        self.assertEqual(set(config["on"]), {"workflow_dispatch"})
        self.assertEqual(config["permissions"], {
            "contents": "read", "actions": "read", "attestations": "read",
        })
        self.assertEqual(config["concurrency"], {
            "group": "agent-updates-production", "cancel-in-progress": False,
        })
        # Normal releases keep both platforms on one version. Historical
        # macOS-only runs remain available through an explicit override.
        self.assertIs(config["on"]["workflow_dispatch"]["inputs"]["include_linux"]["default"], True)
        self.assertIs(config["on"]["workflow_dispatch"]["inputs"]["include_windows"]["default"], False)
        jobs = config["jobs"]
        self.assertEqual(set(jobs), {"verify", "reserve", "sign", "publish"})
        self.assertNotIn("environment", jobs["verify"])
        self.assertEqual(jobs["reserve"]["needs"], "verify")
        self.assertEqual(jobs["sign"]["needs"], "reserve")
        self.assertEqual(jobs["publish"]["needs"], "sign")
        for name, job in jobs.items():
            self.assertIn(MASTER, " ".join(job["if"].split()))
            environment = job.get("environment")
            if isinstance(environment, dict):
                environment = environment["name"]
            if name in {"reserve", "sign", "publish"}:
                self.assertEqual(environment, "updates-production")
            self.assertNotIn("permissions", job)
            for step in job["steps"]:
                self.assertNotIn("continue-on-error", step)
                for value in step.get("env", {}).values():
                    if "secrets." in str(value):
                        secret = re.search(r"secrets\.([A-Z0-9_]+)", value)[1]
                        self.fail(f"credential must be passed from a step output: {secret}")
                if step.get("uses", "").startswith("actions/checkout@"):
                    self.assertEqual(step["with"]["ref"], "${{ github.sha }}")
                    self.assertIs(step["with"]["persist-credentials"], False)

    def test_bws_reuses_existing_access_without_exporting_credentials_to_later_steps(self):
        jobs = workflow("agent-updates-publish.yml")["jobs"]
        bws_action = workflow("opensecret-eif-release.yml")["jobs"]["sign"]["steps"]
        bws_action = next(step["uses"] for step in bws_action
                          if step.get("uses", "").startswith("bitwarden/sm-action@"))
        self.assertEqual(bws_action, "bitwarden/sm-action@1238aae8fc64b212641190a9227c8a734ab1a793")
        for name in ("reserve", "sign", "publish"):
            with self.subTest(job=name):
                job = jobs[name]
                self.assertNotIn("env", job)
                steps = job["steps"]
                fetches = [i for i, step in enumerate(steps)
                           if step.get("uses", "").startswith("bitwarden/sm-action@")]
                self.assertEqual(len(fetches), 1)
                index = fetches[0]
                fetch = steps[index]
                self.assertEqual(fetch["uses"], bws_action)
                self.assertEqual(fetch["with"]["access_token"], "${{ secrets.AGENT_UPDATES_BWS_ACCESS_TOKEN }}")
                self.assertIs(fetch["with"]["set_env"], False)
                realization = next(i for i, step in enumerate(steps)
                                   if step.get("run") == "nix develop --no-update-lock-file .#ci -c true")
                download = next(i for i, step in enumerate(steps)
                                if step.get("uses", "").startswith("actions/download-artifact@"))
                self.assertLess(download, realization)
                self.assertLess(realization, index)
                for step in steps:
                    references = re.findall(r"secrets\.([A-Z0-9_]+)", json.dumps(step))
                    self.assertEqual(references, ["AGENT_UPDATES_BWS_ACCESS_TOKEN"] if step is fetch else [])
                if name == "sign":
                    self.assertEqual(fetch["with"]["secrets"].splitlines(), [
                        "${{ vars.AGENT_UPDATE_SIGNING_KEY_ID }} > AGENT_UPDATE_SIGNING_KEY",
                    ])
                    expected = {"AGENT_UPDATE_SIGNING_KEY": "${{ steps.signing-key.outputs.AGENT_UPDATE_SIGNING_KEY }}"}
                else:
                    self.assertEqual(fetch["with"]["secrets"].splitlines(), [
                        "${{ vars.AGENT_R2_ACCESS_KEY_ID_SECRET_ID }} > AWS_ACCESS_KEY_ID",
                        "${{ vars.AGENT_R2_SECRET_ACCESS_KEY_SECRET_ID }} > AWS_SECRET_ACCESS_KEY",
                    ])
                    expected = {
                        "AWS_ACCESS_KEY_ID": "${{ steps.r2-credentials.outputs.AWS_ACCESS_KEY_ID }}",
                        "AWS_SECRET_ACCESS_KEY": "${{ steps.r2-credentials.outputs.AWS_SECRET_ACCESS_KEY }}",
                    }
                consuming = steps[index + 1]
                for key, value in expected.items():
                    self.assertEqual(consuming["env"][key], value)
                for other in steps[:index] + steps[index + 2:]:
                    self.assertNotRegex(json.dumps(other), r"steps\.(?:signing-key|r2-credentials)\.outputs")
                    self.assertNotRegex(other.get("run", ""), r"GITHUB_ENV|BWS_ACCESS_TOKEN")

    def test_preparation_verifies_provenance_before_any_secrets(self):
        verify = workflow("agent-updates-publish.yml")["jobs"]["verify"]
        source = "\n".join(step.get("run", "") for step in verify["steps"])
        self.assertIn("AGENT_UPDATES_ENABLED", str(verify))
        self.assertIn('"$ACTIVATED" = true', source)
        self.assertIn("nix develop --no-update-lock-file .#ci -c bash -euo pipefail <<'PREPARE'", source)
        self.assertIn("publisher.py trust", source)
        self.assertIn("publisher.py prepare", source)
        self.assertIn("--public-key public-key.pub", source)
        self.assertIn("--version", source)
        self.assertIn("--run-id", source)
        self.assertIn("--linux", source)
        self.assertIn("--windows", source)
        self.assertIn("AGENT_WINDOWS_UPDATES_ENABLED", str(verify))
        self.assertIn('"$WINDOWS_ACTIVATED" = true', source)
        self.assertIn('"$(jq -er .artifacts.windows trust.json)"', source)
        self.assertNotIn("secrets.", str(verify))
        self.assertNotIn("--unsigned", source)
        self.assertNotIn("gh release", source)

    def test_retry_transfers_keep_stable_artifact_names(self):
        jobs = workflow("agent-updates-publish.yml")["jobs"]
        for producer, consumer in (("verify", "reserve"), ("reserve", "sign"), ("sign", "publish")):
            upload = next(step["with"] for step in jobs[producer]["steps"]
                          if step.get("uses", "").startswith("actions/upload-artifact@"))
            download = next(step["with"] for step in jobs[consumer]["steps"]
                            if step.get("uses", "").startswith("actions/download-artifact@"))
            self.assertEqual(upload["name"], download["name"])
            self.assertNotIn("run_attempt", upload["name"])
            self.assertIs(upload["overwrite"], True)
            self.assertEqual(upload["if-no-files-found"], "error")

    def test_credentialed_steps_never_execute_packages_or_deploy(self):
        jobs = workflow("agent-updates-publish.yml")["jobs"]
        for name in ("reserve", "sign", "publish"):
            source = "\n".join(step.get("run", "") for step in jobs[name]["steps"])
            self.assertNotRegex(source, r"(?:\.AppImage|tar\s|hdiutil|codesign|package-release|verify-release|gh\s+release|git\s+(?:push|tag)|wrangler|workers/domains)")
            self.assertNotIn("services/updates", source)
            self.assertNotIn("CLOUDFLARE_API_TOKEN", source)
        publish = str(jobs["publish"])
        self.assertIn("--base-url https://updates.maple.ai", publish)
        self.assertNotIn("--base-url https://updates.trymaple.ai", publish)
        self.assertNotIn("AGENT_UPDATES_ORIGIN", publish)

    def test_routes_and_worker_resources_require_separate_setup(self):
        source = (ROOT / "services/agent-updates/wrangler.jsonc").read_text()
        config = json.loads(re.sub(r",(\s*[}\]])", r"\1", re.sub(r"//[^\n]*", "", source)))
        self.assertFalse(config["workers_dev"])
        self.assertFalse(config["preview_urls"])
        self.assertNotIn("routes", config)
        self.assertNotIn("route", config)
        self.assertNotIn("assets", config)
        self.assertNotIn("env", config)
        self.assertEqual(config["r2_buckets"], [{
            "binding": "AGENT_RELEASES", "bucket_name": "maple-agent-updates",
        }])
        # Keep an accidental deploy out of the ordinary package commands.
        package = json.loads((ROOT / "services/agent-updates/package.json").read_text())
        self.assertNotIn("deploy", package["scripts"])
        self.assertEqual(package["scripts"]["build:dry-run"], "bun src/dry-run.ts")
        dry_run = (ROOT / "services/agent-updates/src/dry-run.ts").read_text()
        self.assertIn('"--dry-run"', dry_run)
        self.assertIn('"deploy"', dry_run)
        self.assertIn('bucket_name: "agent-updates-local-dry-run"', dry_run)


if __name__ == "__main__":
    unittest.main()
