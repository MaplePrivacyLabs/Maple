"""Release authority, artifact binding and static publication failure cases.

The fake verifier models gh's authenticated output. These tests do not claim to
exercise Fulcio/Rekor or the browser/Rust verifier implementations.
"""

import copy
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest import mock
import zipfile

import opensecret_release_statement as release


class ReleaseStatementTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.candidate = self.root / "candidate"
        self.output = self.root / "release"
        self.sha = "d" * 40
        self.signer_sha = "e" * 40
        self.run = {
            "id": 123, "run_attempt": 2, "repository": {"full_name": release.REPOSITORY, "private": False},
            "head_repository": {"full_name": release.REPOSITORY}, "head_sha": self.sha,
            "path": release.BUILD_WORKFLOW, "event": "workflow_dispatch", "head_branch": "master",
            "status": "completed", "conclusion": "success",
        }
        self.measurements = {"HashAlgorithm": "Sha384 { ... }", "PCR0": "a" * 96,
                             "PCR1": "b" * 96, "PCR2": "c" * 96}
        self.image = b"synthetic EIF bytes, not a running enclave"
        self.handoff = {
            "environment": "dev", "source_sha": self.sha, "measurements": self.measurements,
            "eif_sha256": hashlib.sha256(self.image).hexdigest(), "approval_needed": True,
        }
        self.contents = {"image.eif": self.image, "pcr.json": json.dumps(self.measurements).encode(),
                         "handoff.json": json.dumps(self.handoff).encode()}
        self.contents["SHA256SUMS"] = self.checksums(self.contents)
        self.artifacts = [{"artifacts": [{"id": 456, "name": f"opensecret-eif-dev-{self.sha[:12]}",
                                         "expired": False, "size_in_bytes": 300,
                                         "workflow_run": {"id": 123}}]}]
        self.env = mock.patch.dict(os.environ, {"GITHUB_SHA": self.signer_sha,
                                  "ATTESTATIONS_R2_ACCOUNT_ID": "f" * 32,
                                  "ATTESTATIONS_R2_BUCKET": release.BUCKET})
        self.env.start()
        self.addCleanup(self.env.stop)

    @staticmethod
    def checksums(contents):
        return "".join(f"{hashlib.sha256(value).hexdigest()}  {name}\n"
                       for name, value in contents.items() if name != "SHA256SUMS").encode()

    def archive(self, contents=None):
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w") as archive:
            for name, value in (self.contents if contents is None else contents).items():
                archive.writestr(name, value)
        return buffer.getvalue()

    def materialize(self):
        self.candidate.mkdir()
        for name, value in self.contents.items():
            (self.candidate / name).write_bytes(value)
        self.output.mkdir()
        predicate = release.validate_candidate(self.candidate, self.run)
        release.write_json(self.output / "candidate.json", {
            "predicate": predicate, "eifSha256": self.handoff["eif_sha256"], "signerCommit": self.signer_sha,
        })
        # Shape intentionally synthetic: gh is mocked, never advertised as a
        # cryptographically valid fixture.
        release.write_json(self.output / "bundle.json", {"syntheticBundle": True})

    def authenticated(self, path, workflow, sha, predicate_type, bundle=None):
        if predicate_type == release.SLSA_TYPE:
            predicate = {"runDetails": {"metadata": {"invocationId":
                f"{release.SOURCE_REPOSITORY}/actions/runs/123/attempts/2"}}}
        else:
            predicate = release.read_json(self.output / "candidate.json")["predicate"]
        return [{"verificationResult": {"statement": {
            "_type": "https://in-toto.io/Statement/v1", "predicateType": predicate_type,
            "subject": [{"name": "image.eif", "digest": {"sha256": release.digest(path)}}],
            "predicate": predicate,
        }}}]

    def prepare_mocks(self, *, evidence=None):
        def run(arguments, **kwargs):
            if "stdout" in kwargs:
                kwargs["stdout"].write(self.archive())
            return subprocess.CompletedProcess(arguments, 0)

        stack = contextlib.ExitStack()
        stack.enter_context(mock.patch.object(release, "api", return_value=self.run))
        stack.enter_context(mock.patch.object(release, "command_json", return_value=self.artifacts))
        stack.enter_context(mock.patch.object(release.subprocess, "run", side_effect=run))
        verifier = stack.enter_context(mock.patch.object(release, "verify", side_effect=evidence or self.authenticated))
        return stack, verifier

    def test_prepares_all_three_authenticated_files_and_full_tuple(self):
        stack, verifier = self.prepare_mocks()
        with stack:
            release.prepare("123", self.candidate, self.output)
        self.assertEqual({call.args[0].name for call in verifier.call_args_list},
                         {"image.eif", "pcr.json", "handoff.json"})
        predicate = release.read_json(self.output / "predicate.json")
        self.assertEqual(predicate["environment"], "development")
        self.assertEqual(predicate["source"]["commit"], self.sha)
        self.assertEqual(predicate["measurements"], {name: self.measurements[name] for name in ("PCR0", "PCR1", "PCR2")})
        self.assertEqual(predicate["build"], {"runId": "123", "runAttempt": 2})
        self.assertNotEqual(predicate["source"]["commit"], self.signer_sha)

    def test_rejects_wrong_repository_workflow_ref_event_result_and_attempt(self):
        for field, value in (("repository", {"full_name": "attacker/Maple", "private": False}),
                             ("head_repository", {"full_name": "attacker/Maple"}),
                             ("repository", {"full_name": release.REPOSITORY, "private": True}),
                             ("path", release.SIGN_WORKFLOW), ("head_branch", "feature"),
                             ("event", "pull_request"), ("conclusion", "failure"),
                             ("status", "in_progress"), ("head_sha", "short"), ("run_attempt", True)):
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                release.validate_run({**self.run, field: value}, "123")

    def test_invalid_run_id_never_reaches_api_or_shell(self):
        with mock.patch.object(release, "api") as api:
            for run_id in ("0", "-1", "123\n456", "$(id)", "123/../../"):
                with self.subTest(run_id=run_id), self.assertRaises(ValueError):
                    release.prepare(run_id, self.candidate, self.output)
            api.assert_not_called()

    def test_provenance_from_another_run_attempt_is_rejected(self):
        def wrong_attempt(*args):
            results = self.authenticated(*args)
            results[0]["verificationResult"]["statement"]["predicate"]["runDetails"]["metadata"]["invocationId"] += "9"
            return results

        stack, _ = self.prepare_mocks(evidence=wrong_attempt)
        with stack, self.assertRaisesRegex(ValueError, "selected run attempt"):
            release.prepare("123", self.candidate, self.output)
        self.assertFalse((self.output / "predicate.json").exists())

    def test_no_unsigned_mode_fallback_for_old_builds(self):
        def missing_handoff(*args):
            if args[0].name == "handoff.json":
                raise subprocess.CalledProcessError(1, "gh")
            return self.authenticated(*args)

        stack, _ = self.prepare_mocks(evidence=missing_handoff)
        with stack, self.assertRaises(subprocess.CalledProcessError):
            release.prepare("123", self.candidate, self.output)

    def test_missing_duplicate_expired_or_cross_run_artifact_is_rejected(self):
        base = copy.deepcopy(self.artifacts)
        for artifacts in ([{"artifacts": []}], [{"artifacts": base[0]["artifacts"] * 2}],
                          [{"artifacts": [{**base[0]["artifacts"][0], "expired": True}]}],
                          [{"artifacts": [{**base[0]["artifacts"][0], "workflow_run": {"id": 999}}]}]):
            with self.subTest(artifacts=artifacts):
                self.artifacts = artifacts
                stack, _ = self.prepare_mocks()
                with stack, self.assertRaises(ValueError):
                    release.prepare("123", self.candidate, self.output)

    def test_rejects_traversal_extra_files_duplicate_paths_symlinks_and_oversize(self):
        variants = []
        variants.append(self.archive({**self.contents, "../escape": b"bad"}))
        variants.append(self.archive({name: value for name, value in self.contents.items() if name != "handoff.json"}))
        for mode in (stat.S_IFLNK | 0o777, stat.S_IFDIR | 0o755):
            buffer = io.BytesIO()
            with zipfile.ZipFile(buffer, "w") as archive:
                for name, value in self.contents.items():
                    entry = zipfile.ZipInfo(name)
                    entry.external_attr = mode << 16
                    archive.writestr(entry, value)
            variants.append(buffer.getvalue())
        for index, value in enumerate(variants):
            path = self.root / f"{index}.zip"
            path.write_bytes(value)
            with self.subTest(index=index), self.assertRaises(ValueError):
                release.extract_candidate(path, self.root / f"out-{index}")
        self.assertFalse((self.root / "escape").exists())
        path = self.root / "oversize.zip"
        path.write_bytes(self.archive())
        with mock.patch.object(release, "MAX_EIF", 3), self.assertRaisesRegex(ValueError, "size limit"):
            release.extract_candidate(path, self.root / "oversize")
        path = self.root / "duplicate.zip"
        with zipfile.ZipFile(path, "w") as archive:
            for name in ("image.eif", "image.eif", "pcr.json", "handoff.json"):
                archive.writestr(name, b"duplicate fixture")
        with self.assertRaisesRegex(ValueError, "exactly the four"):
            release.extract_candidate(path, self.root / "duplicate")

    def test_changed_files_checksums_mode_source_and_measurements_are_rejected(self):
        self.materialize()
        for field, value in (("environment", "prod"), ("source_sha", "f" * 40),
                             ("eif_sha256", "f" * 64), ("measurements", {**self.measurements, "PCR1": "0" * 96})):
            with self.subTest(field=field):
                contents = {**self.contents, "handoff.json": json.dumps({**self.handoff, field: value}).encode()}
                contents["SHA256SUMS"] = self.checksums(contents)
                for name, data in contents.items():
                    (self.candidate / name).write_bytes(data)
                with self.assertRaises(ValueError):
                    release.validate_candidate(self.candidate, self.run)
        (self.candidate / "image.eif").write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "checksum"):
            release.validate_candidate(self.candidate, self.run)

    def test_verifier_uses_exact_identity_issuer_digest_ref_and_predicate(self):
        with mock.patch.object(release, "command_json", return_value=[{}]) as command:
            release.verify(Path("image.eif"), release.SIGN_WORKFLOW, self.signer_sha, release.PREDICATE_TYPE,
                           Path("bundle.json"))
        args = command.call_args.args[0]
        for flag, expected in (("--cert-identity", f"{release.SOURCE_REPOSITORY}/{release.SIGN_WORKFLOW}@refs/heads/master"),
                               ("--cert-oidc-issuer", release.ISSUER), ("--source-ref", "refs/heads/master"),
                               ("--source-digest", self.signer_sha), ("--signer-digest", self.signer_sha),
                               ("--predicate-type", release.PREDICATE_TYPE), ("--bundle", "bundle.json")):
            self.assertEqual(args[args.index(flag) + 1], expected)
        self.assertIn("--deny-self-hosted-runners", args)
        self.assertNotIn("--cert-identity-regex", args)

    def test_release_rejects_changed_predicate_and_subject(self):
        self.materialize()
        for field, replacement in (("predicate", {"environment": "production"}),
                                   ("subject", [{"digest": {"sha256": "f" * 64}}]),
                                   ("predicateType", release.SLSA_TYPE)):
            def wrong(*args):
                evidence = self.authenticated(*args)
                evidence[0]["verificationResult"]["statement"][field] = replacement
                return evidence

            with self.subTest(field=field), mock.patch.object(release, "verify", side_effect=wrong), self.assertRaises(ValueError):
                release.verify_release(self.candidate, self.output, self.output / "bundle.json")

    def test_failed_crypto_verification_cannot_retain_or_publish(self):
        self.materialize()
        with mock.patch.object(release, "verify", side_effect=subprocess.CalledProcessError(1, "gh")), \
                mock.patch.object(release.subprocess, "run") as upload, self.assertRaises(subprocess.CalledProcessError):
            release.publish(self.candidate, self.output)
        upload.assert_not_called()

    def test_static_put_retains_history_and_verifies_public_bytes(self):
        self.materialize()
        old = "f" * 96 + ".json"
        objects = {old: b"historical approval"}
        commands = []

        def upload(args, **kwargs):
            commands.append(args)
            self.assertEqual(args[:3], ["aws", "s3api", "put-object"])
            objects[args[args.index("--key") + 1]] = Path(args[args.index("--body") + 1]).read_bytes()

        def fetch(url, method):
            return objects[url.rsplit("/", 1)[1]] if method == "GET" else b""

        with mock.patch.object(release, "verify", side_effect=self.authenticated) as verifier, \
                mock.patch.object(release.subprocess, "run", side_effect=upload), \
                mock.patch.object(release, "public_request", side_effect=fetch):
            release.publish(self.candidate, self.output)
            release.publish(self.candidate, self.output)
        self.assertEqual(objects[old], b"historical approval")
        self.assertEqual(len(objects), 2)
        self.assertEqual(len(commands), 2)
        self.assertEqual(len(verifier.call_args_list), 4)  # Local and downloaded evidence, on each run.
        self.assertEqual(release.read_json(self.output / "publication.json")["bundleSha256"],
                         release.digest(self.output / "bundle.json"))

    def test_wrong_bucket_failed_upload_and_stale_public_bytes_never_report_success(self):
        self.materialize()
        with mock.patch.object(release, "verify", side_effect=self.authenticated), \
                mock.patch.object(release.subprocess, "run") as upload:
            with mock.patch.dict(os.environ, {"ATTESTATIONS_R2_BUCKET": "maple-attestations-prod"}), self.assertRaises(ValueError):
                release.publish(self.candidate, self.output)
            upload.assert_not_called()
            upload.side_effect = subprocess.CalledProcessError(1, "aws")
            with self.assertRaises(subprocess.CalledProcessError):
                release.publish(self.candidate, self.output)
            upload.side_effect = None
            with mock.patch.object(release, "public_request", return_value=b"stale bundle"), \
                    mock.patch.object(release.time, "sleep"), self.assertRaises(ValueError):
                release.publish(self.candidate, self.output)
        self.assertFalse((self.output / "publication.json").exists())


if __name__ == "__main__":
    unittest.main()
