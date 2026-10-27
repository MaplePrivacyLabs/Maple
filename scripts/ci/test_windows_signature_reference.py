"""Exact historical signature reference selection, without network or PE execution."""

import copy
import hashlib
import io
import json
from pathlib import Path
import stat
import subprocess
import unittest
import zipfile

import windows_signature_reference as probe


def selected():
    return probe.selection({
        "MAPLE_PROBE_RUN": "123", "MAPLE_PROBE_ATTEMPT": "1",
        "MAPLE_PROBE_ARTIFACT": "456", "MAPLE_PROBE_ZIP_BYTES": "1000",
        "MAPLE_PROBE_SOURCE": "a" * 40, "MAPLE_PROBE_ZIP_SHA": "b" * 64,
        "MAPLE_PROBE_PE_MEMBER": "setup.exe", "MAPLE_PROBE_PE_SHA": "c" * 64,
    })


def metadata():
    run = {"id": 123, "run_attempt": 1, "head_sha": "a" * 40,
           "head_branch": "master", "event": "push", "path": probe.PRODUCER,
           "repository": {"full_name": probe.REPOSITORY, "id": 1},
           "head_repository": {"full_name": probe.REPOSITORY},
           "status": "completed", "conclusion": "success"}
    artifact = {"id": 456, "expired": False,
                "name": "maple-agent-prod-windows-x86_64-123", "size_in_bytes": 1000,
                "digest": "sha256:" + "b" * 64, "created_at": "2026-10-27T00:01:00Z",
                "workflow_run": {"id": 123, "head_sha": "a" * 40,
                                 "repository_id": 1, "head_repository_id": 1}}
    jobs = {"total_count": 3, "jobs": [
        {"id": index + 1, "name": name, "status": "completed", "conclusion": "success",
         "labels": ["windows-2025"], "started_at": "2026-10-27T00:00:00Z",
         "completed_at": "2026-10-27T00:02:00Z"} for index, name in enumerate(probe.JOBS)
    ]}
    return run, artifact, jobs


def archive(extra=None, changed_source=False):
    selection = selected()
    executable = b"MZ ordinary data fixture; never executable"
    build = {"source_sha": "a" * 40, "profile": "prod", "version": "0.0.7",
             "bundle_id": "cloud.opensecret.maple.agent"}
    manifest = {"source_sha": "d" * 40 if changed_source else "a" * 40,
                "profile": "prod", "version": "0.0.7", "platform": "windows-x86_64",
                "distribution": "master", "windows_signing": "authenticode", "package": "setup.exe"}
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w") as payload:
        payload.writestr("build-info.json", json.dumps(build))
        payload.writestr("manifest.json", json.dumps(manifest))
        payload.writestr("setup.exe", executable)
        if extra is not None:
            payload.writestr(extra, b"untrusted extra")
    data = output.getvalue()
    selection.update(ZIP_BYTES=len(data), ZIP_SHA=hashlib.sha256(data).hexdigest(),
                     PE_SHA=hashlib.sha256(executable).hexdigest())
    return selection, data, executable


class WindowsSignatureReferenceTests(unittest.TestCase):
    def test_exact_successful_producer_and_three_native_jobs(self):
        self.assertEqual(len(probe.validate_producer(selected(), *metadata())), 3)
        for mutate in (
            lambda r, a, j: r.update(head_branch="feature"),
            lambda r, a, j: r.update(run_attempt=2),
            lambda r, a, j: r["head_repository"].update(full_name="someone/fork"),
            lambda r, a, j: a.update(digest="sha256:" + "d" * 64),
            lambda r, a, j: a.update(created_at="2026-10-27T00:03:00Z"),
            lambda r, a, j: j["jobs"][1].update(conclusion="failure"),
            lambda r, a, j: j["jobs"][2].update(conclusion="skipped"),
            lambda r, a, j: j["jobs"][0].update(labels=["self-hosted", "windows"]),
            lambda r, a, j: j["jobs"][1].update(labels=["blacksmith-8vcpu-windows-2025"]),
            lambda r, a, j: j["jobs"][2].update(labels=["blacksmith-8vcpu-windows-2025"]),
        ):
            values = copy.deepcopy(metadata())
            mutate(*values)
            with self.assertRaises(ValueError):
                probe.validate_producer(selected(), *values)

    def test_the_unsigned_build_may_run_on_blacksmith(self):
        run, artifact, jobs = metadata()
        jobs["jobs"][0]["labels"] = ["blacksmith-8vcpu-windows-2025"]
        self.assertEqual(len(probe.validate_producer(selected(), run, artifact, jobs)), 3)

    def test_selectors_cannot_be_paths_urls_or_unbounded_sizes(self):
        for key, value in (("RUN", "123/other"), ("ATTEMPT", "0"),
                           ("ZIP_BYTES", str(probe.MAX_ZIP + 1)),
                           ("SOURCE", "a" * 39), ("PE_MEMBER", "../setup.exe"),
                           ("PE_MEMBER", "https://example.test/setup.exe")):
            environment = {"MAPLE_PROBE_" + name: str(value) for name, value in selected().items()}
            environment["MAPLE_PROBE_" + key] = value
            with self.assertRaises(ValueError):
                probe.selection(environment)

    def test_exact_archive_and_pe_digest_are_required(self):
        selection, data, executable = archive()
        self.assertEqual(probe.reference_pe(selection, data), executable)
        for field in ("ZIP_SHA", "PE_SHA"):
            altered = dict(selection, **{field: "0" * 64})
            with self.assertRaises(ValueError):
                probe.reference_pe(altered, data)
        selection, data, _ = archive(changed_source=True)
        with self.assertRaises(ValueError):
            probe.reference_pe(selection, data)

    def test_unsafe_inventory_is_refused_before_extracting_any_member(self):
        link = zipfile.ZipInfo("link")
        link.external_attr = (stat.S_IFLNK | 0o777) << 16
        for extra in ("../outside", "C:outside", "other\\file", "SETUP.EXE", link):
            selection, data, _ = archive(extra)
            with self.assertRaises(ValueError):
                probe.reference_pe(selection, data)

    def test_probe_workflow_is_manual_read_only_and_has_no_implicit_reference(self):
        root = Path(__file__).resolve().parents[2]
        workflow = json.loads(subprocess.check_output([
            "yq", "-o=json", ".", str(root / ".github/workflows/agent-windows-signature-probe.yml")
        ], text=True))
        self.assertEqual(set(workflow["on"]), {"workflow_dispatch"})
        self.assertEqual(workflow["permissions"], {"contents": "read"})
        inputs = workflow["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual(len(inputs), 8)
        self.assertTrue(all(value["required"] and "default" not in value for value in inputs.values()))
        job = workflow["jobs"]["signature-probe"]
        self.assertEqual(job["permissions"], {"contents": "read", "actions": "read"})
        self.assertEqual(job["runs-on"], "windows-2025")
        self.assertNotIn("environment", job)
        self.assertNotIn("secrets", json.dumps(workflow))
        self.assertNotIn("${{ inputs.", "\n".join(step.get("run", "") for step in job["steps"]))
        actions = [step["uses"] for step in job["steps"] if "uses" in step]
        self.assertTrue(all(len(action.split("@")[1]) == 40 for action in actions))
        self.assertFalse(any("cache" in action or "download-artifact" in action for action in actions))


if __name__ == "__main__":
    unittest.main()
