#!/usr/bin/env python3
"""Offline operator fixtures. No trusted signing key, BWS login, or EIF build."""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec

from test_pcr_compatibility import SOURCE, fixture


class OperatorBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="pcr-operator-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.backend = self.root / "backend"
        self.backend.mkdir()
        (self.backend / "scripts").mkdir()
        (self.backend / "secretspec").mkdir()
        for relative in ("justfile", "pcr_sign.js", "scripts/pcr_compatibility.py", "scripts/ci_sign_pcr.sh",
                         "secretspec/pcr-signing.toml"):
            shutil.copyfile(SOURCE / relative, self.backend / relative)
        self.home = self.root / "home"
        (self.home / ".config/secretspec").mkdir(parents=True)
        self.values = self.root / "fixture.values"
        self.values.write_text("signing_private_key=fixture_signing_only\n")
        # Point the copied manifest's committed alias at a dummy provider; the
        # recipes and BWS item names are exercised unchanged.
        manifest = self.backend / "secretspec/pcr-signing.toml"
        patched, count = re.subn(r"(?m)^opensecret_pcr_signing = .*$",
                                 f'opensecret_pcr_signing = "dotenv:{self.values}"', manifest.read_text())
        self.assertEqual(count, 1)
        manifest.write_text(patched)
        self.env = {
            "PATH": os.environ["PATH"], "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
        }
        for name in ("SIGNING_PRIVATE_KEY", "BWS_ACCESS_TOKEN", "SECRETSPEC_FILE",
                     "SECRETSPEC_PROVIDER", "BWS_CONFIG_FILE", "AWS_SECRET_ACCESS_KEY",
                     "TINFOIL_API_KEY", "CLOUDFLARE_API_TOKEN", "NODE_OPTIONS"):
            self.env[name] = "fixture_ambient_poison"
        (self.backend / ".env").write_text("INVALID DOTENV CONTENT MUST NOT BE PARSED\n")
        blobs = fixture(2)
        for name, data in blobs.items():
            (self.backend / name).write_bytes(data)
        for environment in ("Dev", "Prod"):
            (self.backend / f"pcr{environment}History.json").write_bytes(fixture(1)[f"pcr{environment}History.json"])

    def run_recipe(self, *args):
        return subprocess.run(
            ["just", "--no-dotenv", "--justfile", str(self.backend / "justfile"), *args],
            cwd=self.backend, env=self.env, text=True, capture_output=True, timeout=30,
        )

    def test_native_signing_resolution_and_verified_public_append(self):
        # Replay an existing PUBLIC signature, never access the trusted key.
        # Real Node checks the native SecretSpec child boundary before replaying it.
        entry = json.loads(fixture(2)["pcrDevHistory.json"])[1]
        (self.backend / "pcr_sign.js").write_text(
            "const assert = require('node:assert/strict');\n"
            "assert.equal(process.env.SIGNING_PRIVATE_KEY, 'fixture_signing_only');\n"
            "for (const key of ['BWS_ACCESS_TOKEN', 'AWS_SECRET_ACCESS_KEY', "
            "'TINFOIL_API_KEY', 'CLOUDFLARE_API_TOKEN', 'NODE_OPTIONS']) "
            "assert.equal(process.env[key], undefined);\n"
            f"assert.equal(process.argv[3], {json.dumps(entry['PCR0'])});\n"
            f"console.log({json.dumps(entry['signature'])});\n"
        )
        result = self.run_recipe("append-pcr-dev")
        self.assertEqual(result.returncode, 0, result.stderr)
        history = json.loads((self.backend / "pcrDevHistory.json").read_bytes())
        self.assertEqual(len(history), 2)
        self.assertEqual(history[0], json.loads(fixture(1)["pcrDevHistory.json"])[0])
        self.assertEqual(history[1]["signature"], entry["signature"])
        self.assertNotIn("fixture_signing_only", result.stdout + result.stderr)
        # No provider lookup when the measurements are already approved.
        self.values.unlink()
        result = self.run_recipe("append-pcr-dev")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no key lookup", result.stdout)

    def test_missing_key_is_not_replaced_by_ambient_or_dotenv(self):
        self.values.write_text("")
        before = (self.backend / "pcrDevHistory.json").read_bytes()
        result = self.run_recipe("append-pcr-dev")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.backend / "pcrDevHistory.json").read_bytes(), before)
        self.assertNotIn("fixture_ambient_poison", result.stdout + result.stderr)
        self.assertNotIn("generate-keys", result.stdout + result.stderr)

    def test_wrong_key_signature_never_changes_history(self):
        # Synthetic test key only, unrelated to the SDK-trusted identity.
        key = ec.generate_private_key(ec.SECP384R1())
        encoded = base64.b64encode(key.private_bytes(
            serialization.Encoding.DER, serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )).decode()
        self.values.write_text(f"signing_private_key={encoded}\n")
        before = (self.backend / "pcrDevHistory.json").read_bytes()
        result = self.run_recipe("append-pcr-dev")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("signature is invalid", result.stderr)
        self.assertEqual((self.backend / "pcrDevHistory.json").read_bytes(), before)
        self.assertNotIn(encoded, result.stdout + result.stderr)

    def ci_artifact(self):
        artifact = self.root / "artifact"
        artifact.mkdir()
        (artifact / "pcr.json").write_bytes(fixture(2)["pcrDev.json"])
        (artifact / "image.eif").write_bytes(b"public synthetic EIF fixture")
        (artifact / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256((artifact / name).read_bytes()).hexdigest()}  {name}\n"
            for name in ("image.eif", "pcr.json")))
        (artifact / "handoff.json").write_text(json.dumps({"environment": "dev", "source_sha": "0" * 40}))
        return artifact

    def run_ci_signing(self, artifact, **env):
        return subprocess.run(
            ["bash", str(self.backend / "scripts/ci_sign_pcr.sh"), "dev", str(artifact)],
            cwd=self.root, env={**self.env, **env}, text=True, capture_output=True, timeout=60,
        )

    def test_ci_signing_entrypoint_hands_the_gated_key_to_the_signer_only(self):
        # The gated workflow step supplies the key; the recipe still verifies the
        # public signature before the history changes. Replay a public signature.
        entry = json.loads(fixture(2)["pcrDevHistory.json"])[1]
        (self.backend / "pcr_sign.js").write_text(
            "const assert = require('node:assert/strict');\n"
            "assert.equal(process.env.SIGNING_PRIVATE_KEY, 'fixture_ci_key');\n"
            f"assert.equal(process.argv[3], {json.dumps(entry['PCR0'])});\n"
            f"console.log({json.dumps(entry['signature'])});\n"
        )
        # A prod snapshot that the prod history already covers keeps the final
        # four-file check meaningful.
        (self.backend / "pcrProd.json").write_bytes(fixture(1)["pcrProd.json"])
        artifact = self.ci_artifact()
        before = (self.backend / "pcrDevHistory.json").read_bytes()
        result = self.run_ci_signing(artifact)
        self.assertNotEqual(result.returncode, 0)  # the ambient poison key is not accepted
        self.assertEqual((self.backend / "pcrDevHistory.json").read_bytes(), before)
        result = self.run_ci_signing(artifact, SIGNING_PRIVATE_KEY="fixture_ci_key")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.backend / "pcrDev.json").read_bytes(), fixture(2)["pcrDev.json"])
        history = json.loads((self.backend / "pcrDevHistory.json").read_bytes())
        self.assertEqual(len(history), 2)
        self.assertEqual(history[1]["signature"], entry["signature"])
        self.assertNotIn("fixture_ci_key", result.stdout + result.stderr)

    def test_ci_signing_entrypoint_rejects_tampered_or_mismatched_candidates(self):
        artifact = self.ci_artifact()
        before = {name: (self.backend / name).read_bytes() for name in ("pcrDev.json", "pcrDevHistory.json")}
        (artifact / "image.eif").write_bytes(b"tampered after attestation")
        result = self.run_ci_signing(artifact, SIGNING_PRIVATE_KEY="fixture_ci_key")
        self.assertNotEqual(result.returncode, 0)
        (artifact / "image.eif").write_bytes(b"public synthetic EIF fixture")
        (artifact / "handoff.json").write_text(json.dumps({"environment": "prod", "source_sha": "0" * 40}))
        result = self.run_ci_signing(artifact, SIGNING_PRIVATE_KEY="fixture_ci_key")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual({name: (self.backend / name).read_bytes() for name in before}, before)

    def test_build_does_not_receive_any_operator_credentials(self):
        binary = self.root / "bin"
        binary.mkdir()
        (self.backend / "result").mkdir()
        (self.backend / "result/pcr.json").write_text("{}\n")
        mock = binary / "nix"
        mock.write_text(
            # Absolute interpreter so the fixture also runs in a Nix sandbox.
            f"#!{sys.executable}\nimport os\n"
            "assert not any(key in os.environ for key in "
            "['SIGNING_PRIVATE_KEY','BWS_ACCESS_TOKEN','AWS_SECRET_ACCESS_KEY',"
            "'TINFOIL_API_KEY','CLOUDFLARE_API_TOKEN'])\n"
        )
        mock.chmod(0o755)
        self.env["PATH"] = str(binary) + ":" + self.env["PATH"]
        for environment in ("dev", "prod", "preview"):
            result = self.run_recipe(f"build-eif-{environment}")
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_manifest_is_separate_from_local_runtime(self):
        signing = tomllib.loads((SOURCE / "secretspec/pcr-signing.toml").read_text())
        local = tomllib.loads((SOURCE / "secretspec.toml").read_text())
        self.assertEqual(signing["project"]["name"], "opensecret-pcr-signing")
        self.assertEqual(set(signing["profiles"]["default"]), {"SIGNING_PRIVATE_KEY"})
        self.assertNotIn("SIGNING_PRIVATE_KEY", local["profiles"]["default"])
        # Committed project IDs: distinct projects, keyring-only credential for signing.
        self.assertEqual(signing["providers"], {"opensecret_pcr_signing": {
            "uri": "bws://2305d292-179b-477e-b6a8-b4c4007eac20", "credentials": {"access_token": "keyring"}}})
        self.assertEqual(local["providers"]["opensecret_local"],
                         {"uri": "bws://9a8c5b99-b00c-4403-beb1-b4c400050810", "credentials": {"access_token": "keyring"}})
        self.assertEqual(local["providers"]["opensecret_local_headless"], "bws://9a8c5b99-b00c-4403-beb1-b4c400050810")


if __name__ == "__main__":
    unittest.main()
