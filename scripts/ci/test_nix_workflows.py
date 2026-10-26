"""Enforce one immutable, security-patched Determinate Nix runtime in CI."""

import functools
import json
from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
PINNED_ACTION = (
    "DeterminateSystems/determinate-nix-action@"
    "96c5ee93a9dddf2830d31e1da6ec719077336c28"
)
PINNED_INPUTS = {"source-tag": "v3.23.3", "github-token": ""}
VERIFY_STEP = {
    "name": "Verify pinned Nix runtime",
    "run": "bash scripts/ci/verify-nix-runtime.sh",
}
# Independent Agent provider, Linux release and attestation jobs retain pinned setup.
EXPECTED_INSTALLS = 82


@functools.cache
def workflow(path):
    result = subprocess.run(
        ["yq", "-o=json", ".", str(path)],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


class NixWorkflowPinTests(unittest.TestCase):
    def test_every_nix_installer_is_exactly_pinned_after_checkout(self):
        installs = []
        source = "\n".join(path.read_text() for path in sorted(WORKFLOWS.glob("*.yml")))
        self.assertNotIn("DeterminateSystems/nix-installer-action@", source)

        for path in sorted(WORKFLOWS.glob("*.yml")):
            config = workflow(path)
            workflow_directory = (
                config.get("defaults", {}).get("run", {}).get("working-directory", ".")
            )
            for job_name, job in config.get("jobs", {}).items():
                job_directory = (
                    job.get("defaults", {}).get("run", {}).get(
                        "working-directory", workflow_directory
                    )
                )
                steps = job.get("steps", [])
                for index, step in enumerate(steps):
                    action = step.get("uses", "")
                    if not action.startswith("DeterminateSystems/determinate-nix-action@"):
                        continue
                    with self.subTest(workflow=path.name, job=job_name):
                        self.assertEqual(action, PINNED_ACTION)
                        self.assertEqual(step.get("with"), PINNED_INPUTS)
                        prior_actions = [item.get("uses", "") for item in steps[:index]]
                        self.assertTrue(any(
                            item.startswith("actions/checkout@") for item in prior_actions
                        ))
                        self.assertLess(index + 1, len(steps))
                        verify = steps[index + 1]
                        self.assertEqual(
                            {key: verify.get(key) for key in VERIFY_STEP}, VERIFY_STEP
                        )
                        self.assertLessEqual(
                            set(verify), {"name", "run", "if", "working-directory"}
                        )
                        self.assertEqual(verify.get("if"), step.get("if"))
                        self.assertEqual(
                            verify.get("working-directory", job_directory), "."
                        )
                    installs.append((path.name, job_name))

        self.assertEqual(len(installs), EXPECTED_INSTALLS)


if __name__ == "__main__":
    unittest.main()
