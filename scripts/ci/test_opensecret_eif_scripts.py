"""Behaviour of the OpenSecret EIF build, rootfs, comparison and approval scripts."""

import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import verify_opensecret_eif_rootfs as rootfs_diagnostic


ROOT = Path(__file__).resolve().parents[2]


VALID_MEASUREMENTS = {"HashAlgorithm": "Sha384 { ... }", "PCR0": "a" * 96, "PCR1": "b" * 96, "PCR2": "c" * 96}


class EifReleaseBuildCommandTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.repo = self.root / "repo"
        self.component = self.repo / "services/opensecret"
        self.component.mkdir(parents=True)
        scripts = self.repo / "scripts/ci"
        scripts.mkdir(parents=True)
        self.script = scripts / "build_opensecret_eif.sh"
        shutil.copyfile(ROOT / "scripts/ci/build_opensecret_eif.sh", self.script)
        self.binaries = self.root / "bin"
        self.binaries.mkdir()
        self.scratch = self.root / "scratch"
        self.scratch.mkdir()
        for tool in ("dirname", "mktemp", "rm", "ls", "mkdir", "cp", "chmod", "jq", "sha256sum", "cut", "cmp"):
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
            "output = Path(args[args.index('--out-link') + 1])\n"
            "output.mkdir()\n"
            "(output / 'image.eif').write_bytes(b'fixture EIF, not a real image')\n"
            "(output / 'pcr.json').write_text(os.environ['TEST_MEASUREMENTS'])\n"
        )
        nix.chmod(0o755)
        self.measurements = json.dumps(VALID_MEASUREMENTS) + "\n"
        (self.component / "pcrDev.json").write_text(json.dumps({**VALID_MEASUREMENTS, "PCR0": "d" * 96}) + "\n")
        (self.component / "pcrProd.json").write_text(self.measurements)
        for name in ("pcrDevHistory.json", "pcrProdHistory.json"):
            (self.component / name).write_text("untouched history fixture\n")
        self.existing_result = self.component / "result"
        self.existing_result.symlink_to(self.root / "operator-owned-output")
        self.sentinel = self.root / "dotenv-was-loaded"
        (self.component / ".env").write_text(f"touch '{self.sentinel}'\n")
        self.before_files = {p.name: p.read_bytes() for p in self.component.iterdir() if p.is_file()}
        self.github_output = self.root / "github-output"

    def run_build(self, mode="dev", output="candidate", **extra_env):
        env = {
            "PATH": str(self.binaries), "HOME": str(self.root), "TMPDIR": str(self.scratch),
            "TEST_TRACE": str(self.root / "trace"), "TEST_MEASUREMENTS": self.measurements,
            "GITHUB_SHA": "1" * 40, "GITHUB_OUTPUT": str(self.github_output), **extra_env,
        }
        result = subprocess.run(
            [shutil.which("bash"), "--noprofile", "--norc", str(self.script), mode, output],
            cwd=self.root, env=env, capture_output=True, text=True,
        )
        self.assertFalse(self.sentinel.exists())
        self.assertEqual(os.readlink(self.existing_result), str(self.root / "operator-owned-output"))
        self.assertEqual({p.name: p.read_bytes() for p in self.component.iterdir() if p.is_file()},
                         self.before_files)
        self.assertEqual(list(self.scratch.iterdir()), [])
        return result

    def outputs(self):
        return dict(line.split("=", 1) for line in self.github_output.read_text().splitlines())

    def test_new_measurements_produce_an_attestable_candidate_and_request_approval(self):
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        candidate = self.root / "candidate"
        self.assertEqual(sorted(p.name for p in candidate.iterdir()),
                         ["SHA256SUMS", "handoff.json", "image.eif", "pcr.json"])
        for name in ("image.eif", "pcr.json"):
            self.assertFalse((candidate / name).is_symlink())
            self.assertEqual((candidate / name).stat().st_mode & 0o777, 0o644)
        digest = hashlib.sha256(b"fixture EIF, not a real image").hexdigest()
        self.assertEqual((candidate / "SHA256SUMS").read_text().splitlines()[0], f"{digest}  image.eif")
        handoff = json.loads((candidate / "handoff.json").read_text())
        self.assertEqual(handoff, {
            "environment": "dev", "source_sha": "1" * 40, "measurements": VALID_MEASUREMENTS,
            "eif_sha256": digest, "approval_needed": True,
        })
        self.assertEqual(self.outputs(), {
            "approval_needed": "true", "pcr0": "a" * 96, "eif_sha256": digest,
            "artifact": "opensecret-eif-dev-111111111111",
        })
        trace = json.loads((self.root / "trace").read_text())
        self.assertIn(".?submodules=1#eif-dev", trace["args"])
        self.assertEqual(trace["cwd"], str(self.component))

    def test_already_approved_measurements_need_no_signing(self):
        result = self.run_build("prod")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outputs()["approval_needed"], "false")
        self.assertEqual(self.outputs()["artifact"], "opensecret-eif-prod-111111111111")

    def test_malformed_measurements_and_unsupported_hosts_fail_before_output(self):
        bad = json.dumps({**VALID_MEASUREMENTS, "PCR0": "0" * 96})
        result = self.run_build(TEST_MEASUREMENTS=bad)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "candidate/SHA256SUMS").exists())
        self.assertFalse((self.root / "candidate/handoff.json").exists())
        self.assertFalse(self.github_output.exists())
        (self.root / "trace").unlink()
        result = self.run_build(output="candidate-x86", TEST_ARCH="x86_64")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "trace").exists())

    def test_refuses_a_nonempty_output_directory(self):
        (self.root / "candidate").mkdir()
        (self.root / "candidate/stale").write_text("")
        result = self.run_build()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "trace").exists())


class EifRootfsDiagnosticTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.entrypoint_store = "/nix/store/" + "a" * 32 + "-entrypoint"
        self.ca_store = "/nix/store/" + "b" * 32 + "-nss-cacert-3.131.1"
        self.ca_bundle = b"synthetic certificate bundle"
        self.archive = self.root / "user-initramfs.cpio.gz"
        self.script = (
            "#!/bin/bash\n"
            f"ln -sf {self.ca_store}/etc/ssl/certs/ca-bundle.crt "
            "/etc/ssl/certs/ca-bundle.crt\n"
            "export SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt\n"
            "export AWS_CA_BUNDLE=/etc/ssl/certs/ca-bundle.crt\n"
        ).encode()
        self.entries = [
            ("cmd", stat.S_IFREG | 0o444, b"/bin/entrypoint"),
            ("rootfs/bin/entrypoint", stat.S_IFLNK | 0o777,
             f"{self.entrypoint_store}/bin/entrypoint".encode()),
            (f"rootfs{self.entrypoint_store}/bin/entrypoint", stat.S_IFREG | 0o755,
             self.script),
            (f"rootfs{self.ca_store}/etc/ssl/certs/ca-bundle.crt", stat.S_IFREG | 0o444,
             self.ca_bundle),
        ]
        self.write_archive()

    @staticmethod
    def newc_entry(name, mode, data):
        name = name.encode() + b"\0"
        fields = (1, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name), 0)
        header = b"070701" + b"".join(f"{field:08x}".encode() for field in fields)
        return (header + name + b"\0" * (-(len(header) + len(name)) % 4)
                + data + b"\0" * (-len(data) % 4))

    def write_archive(self):
        with gzip.open(self.archive, "wb") as archive:
            for entry in self.entries:
                archive.write(self.newc_entry(*entry))
            archive.write(self.newc_entry("TRAILER!!!", 0, b""))

    def candidate(self):
        candidate = self.root / "candidate"
        output = self.root / "nix-output"
        candidate.mkdir()
        output.mkdir()
        measurements = {
            "HashAlgorithm": "Sha384 { ... }", "PCR0": "a" * 96,
            "PCR1": "b" * 96, "PCR2": "c" * 96,
        }
        image = b"synthetic EIF bytes"
        pcr = (json.dumps(measurements) + "\n").encode()
        for directory in (candidate, output):
            (directory / "image.eif").write_bytes(image)
            (directory / "pcr.json").write_bytes(pcr)
        eif_sha = hashlib.sha256(image).hexdigest()
        pcr_sha = hashlib.sha256(pcr).hexdigest()
        (candidate / "SHA256SUMS").write_text(
            f"{eif_sha}  image.eif\n{pcr_sha}  pcr.json\n"
        )
        (candidate / "handoff.json").write_text(json.dumps({
            "environment": "dev", "source_sha": "d" * 40,
            "eif_sha256": eif_sha, "measurements": measurements,
            "approval_needed": True,
        }))
        ca_file = self.root / self.ca_store.lstrip("/") / "etc/ssl/certs/ca-bundle.crt"
        ca_file.parent.mkdir(parents=True)
        ca_file.write_bytes(self.ca_bundle)
        return candidate, output

    def test_exact_user_ramdisk_entrypoint_resolves_to_its_archived_ca_bundle(self):
        store, bundle = rootfs_diagnostic.inspect_archive(self.archive)
        self.assertEqual(store, self.ca_store)
        self.assertEqual(bundle, self.ca_bundle)
        candidate, output = self.candidate()
        user_drv = Path("/nix/store/" + "e" * 32 + "-user-initramfs.cpio.gz.drv")
        with mock.patch.object(rootfs_diagnostic, "eif_derivation",
                               return_value=(output, self.archive, user_drv)), mock.patch.object(
                                   rootfs_diagnostic, "realize_user_ramdisk"
                               ), mock.patch.object(
                                   rootfs_diagnostic, "fingerprints", return_value={"F" * 64}
                               ), mock.patch.object(
                                   rootfs_diagnostic, "verify_aaa_tls_policy",
                                   return_value="retained_rejected_for_openssl_tls"
                               ):
            report = rootfs_diagnostic.verify_candidate(
                "dev", candidate, "d" * 40, "openssl", store_root=self.root
            )
        self.assertEqual(report["kind"], "PR_DIAGNOSTIC_UNAPPROVED")
        self.assertEqual(report["ca_store_path"], self.ca_store)
        self.assertEqual(report["ca_bundle_sha256"], hashlib.sha256(self.ca_bundle).hexdigest())
        self.assertEqual(report["ca_store_comparison"], "matched")
        self.assertEqual(report["ca_certificate_count"], 1)
        self.assertEqual(report["comodo_aaa_tls_policy"], "retained_rejected_for_openssl_tls")

    def test_archived_ca_remains_inspectable_when_cached_store_input_is_absent(self):
        candidate, output = self.candidate()
        ca_file = self.root / self.ca_store.lstrip("/") / "etc/ssl/certs/ca-bundle.crt"
        ca_file.unlink()
        user_drv = Path("/nix/store/" + "e" * 32 + "-user-initramfs.cpio.gz.drv")
        with mock.patch.object(rootfs_diagnostic, "eif_derivation",
                               return_value=(output, self.archive, user_drv)), mock.patch.object(
                                   rootfs_diagnostic, "realize_user_ramdisk"
                               ), mock.patch.object(
                                   rootfs_diagnostic, "fingerprints", return_value={"F" * 64}
                               ), mock.patch.object(
                                   rootfs_diagnostic, "verify_aaa_tls_policy", return_value="absent"
                               ):
            report = rootfs_diagnostic.verify_candidate(
                "dev", candidate, "d" * 40, "openssl", store_root=self.root
            )
        self.assertEqual(report["ca_store_comparison"], "not_materialized")
        self.assertEqual(report["ca_bundle_sha256"], hashlib.sha256(self.ca_bundle).hexdigest())

    def test_affected_root_and_candidate_byte_drift_fail_closed(self):
        candidate, output = self.candidate()
        user_drv = Path("/nix/store/" + "e" * 32 + "-user-initramfs.cpio.gz.drv")
        with mock.patch.object(rootfs_diagnostic, "eif_derivation",
                               return_value=(output, self.archive, user_drv)), mock.patch.object(
                                   rootfs_diagnostic, "realize_user_ramdisk"
                               ), mock.patch.object(
                                   rootfs_diagnostic, "fingerprints",
                                   return_value={rootfs_diagnostic.EMSIGN_G3}
                               ), mock.patch.object(
                                   rootfs_diagnostic, "verify_aaa_tls_policy",
                                   return_value="retained_rejected_for_openssl_tls"
                               ):
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "affected emSign root"):
                rootfs_diagnostic.verify_candidate(
                    "dev", candidate, "d" * 40, "openssl", store_root=self.root
                )
        (candidate / "image.eif").write_bytes((output / "image.eif").read_bytes())
        ca_file = self.root / self.ca_store.lstrip("/") / "etc/ssl/certs/ca-bundle.crt"
        ca_file.write_bytes(b"different Nix CA bundle")
        with mock.patch.object(rootfs_diagnostic, "eif_derivation",
                               return_value=(output, self.archive, user_drv)), mock.patch.object(
                                   rootfs_diagnostic, "realize_user_ramdisk"
                               ):
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "archived CA bundle differs"):
                rootfs_diagnostic.verify_candidate(
                    "dev", candidate, "d" * 40, "openssl", store_root=self.root
                )
        (candidate / "image.eif").write_bytes(b"different candidate")
        with mock.patch.object(rootfs_diagnostic, "eif_derivation",
                               return_value=(output, self.archive, user_drv)), mock.patch.object(
                                   rootfs_diagnostic, "realize_user_ramdisk"
                               ):
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "differs from derivation"):
                rootfs_diagnostic.verify_candidate(
                    "dev", candidate, "d" * 40, "openssl", store_root=self.root
                )

    def test_missing_effective_ca_and_unsafe_entrypoint_symlink_fail_closed(self):
        self.entries.pop()
        self.write_archive()
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "bundle missing"):
            rootfs_diagnostic.inspect_archive(self.archive)
        self.entries[1] = ("rootfs/bin/entrypoint", stat.S_IFLNK | 0o777,
                           b"../../outside/entrypoint")
        self.write_archive()
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "leaves reviewed store path"):
            rootfs_diagnostic.inspect_archive(self.archive)

    def test_malformed_and_duplicate_cpio_entries_fail_closed(self):
        self.archive.write_bytes(b"not a gzip archive")
        with self.assertRaises(OSError):
            rootfs_diagnostic.inspect_archive(self.archive)
        self.entries.append(self.entries[0])
        self.write_archive()
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "duplicate"):
            rootfs_diagnostic.inspect_archive(self.archive)

    def test_entrypoint_must_export_both_ca_variables(self):
        self.entries[2] = (
            self.entries[2][0], self.entries[2][1],
            self.script.replace(b"export AWS_CA_BUNDLE=", b"export DISABLED_CA_BUNDLE="),
        )
        self.write_archive()
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "AWS_CA_BUNDLE"):
            rootfs_diagnostic.inspect_archive(self.archive)

    def test_nitro_init_command_must_select_reviewed_entrypoint(self):
        self.entries.pop(0)
        self.write_archive()
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "Nitro init command missing"):
            rootfs_diagnostic.inspect_archive(self.archive)
        self.entries.insert(0, ("cmd", stat.S_IFREG | 0o444, b"/bin/unused"))
        self.write_archive()
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "does not select"):
            rootfs_diagnostic.inspect_archive(self.archive)

    def test_retained_aaa_requires_explicit_openssl_tls_rejection_and_no_ordinary_pem(self):
        bundle = self.root / "ca-bundle.crt"
        ordinary = b"-----BEGIN CERTIFICATE-----\n" + base64.b64encode(b"synthetic root") + b"\n-----END CERTIFICATE-----\n"
        bundle.write_bytes(ordinary * 100)
        self.assertEqual(rootfs_diagnostic.verify_aaa_tls_policy("openssl", bundle, set()), "absent")
        rejected = subprocess.CompletedProcess([], 2, b"", b"error 28 at 0 depth lookup: certificate rejected")
        with mock.patch.object(rootfs_diagnostic.subprocess, "run", return_value=rejected):
            self.assertEqual(rootfs_diagnostic.verify_aaa_tls_policy(
                "openssl", bundle, {rootfs_diagnostic.COMODO_AAA}
            ), "retained_rejected_for_openssl_tls")
        unsuitable = subprocess.CompletedProcess([], 2, b"", b"error 26: unsuitable certificate purpose")
        with mock.patch.object(rootfs_diagnostic.subprocess, "run", return_value=unsuitable):
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "did not explicitly reject"):
                rootfs_diagnostic.verify_aaa_tls_policy(
                    "openssl", bundle, {rootfs_diagnostic.COMODO_AAA}
                )
        bundle.write_bytes(bundle.read_bytes() + rootfs_diagnostic.AAA_ROOT.read_bytes())
        with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "entered the ordinary TLS root"):
            rootfs_diagnostic.verify_aaa_tls_policy(
                "openssl", bundle, {rootfs_diagnostic.COMODO_AAA}
            )

    def test_eif_derivation_selects_one_user_ramdisk_input(self):
        eif = "/nix/store/" + "c" * 32 + "-opensecret-eif-dev"
        system = "/nix/store/" + "d" * 32 + "-bootstrap-initramfs.cpio.gz"
        user = "/nix/store/" + "e" * 32 + "-user-initramfs.cpio.gz"
        user_drv_name = "f" * 32 + "-user-initramfs.cpio.gz.drv"
        user_drv = Path("/nix/store") / user_drv_name
        derivation = {"derivations": {"candidate.drv": {"env": {
            "out": eif, "ramdisksArgs": f"--ramdisk {system} --ramdisk {user} ",
        }, "inputs": {"drvs": {user_drv_name: {"outputs": ["out"]}}}}}}
        with mock.patch.object(rootfs_diagnostic.subprocess, "run") as run:
            run.return_value.stdout = json.dumps(derivation)
            self.assertEqual(rootfs_diagnostic.eif_derivation("dev"), (Path(eif), Path(user), user_drv))
            self.assertEqual(run.call_args.kwargs["cwd"], rootfs_diagnostic.COMPONENT)
            derivation["derivations"]["candidate.drv"]["inputs"]["drvs"] = {
                str(user_drv): {"outputs": ["out"]}
            }
            run.return_value.stdout = json.dumps(derivation)
            self.assertEqual(rootfs_diagnostic.eif_derivation("dev"), (Path(eif), Path(user), user_drv))
            derivation["derivations"]["candidate.drv"]["env"]["ramdisksArgs"] += f"--ramdisk {user}"
            run.return_value.stdout = json.dumps(derivation)
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "ramdisk inputs"):
                rootfs_diagnostic.eif_derivation("dev")

    def test_explicit_realization_binds_direct_drv_to_selected_archive(self):
        user_drv = Path("/nix/store/" + "f" * 32 + "-user-initramfs.cpio.gz.drv")
        selected = Path("/nix/store/" + "e" * 32 + "-user-initramfs.cpio.gz")
        derivation = {"derivations": {user_drv.name: {
            "env": {"out": str(selected)}, "outputs": {"out": {"path": selected.name}},
        }}}
        with mock.patch.object(rootfs_diagnostic.subprocess, "run") as run, mock.patch.object(
            rootfs_diagnostic, "regular_file", return_value=True
        ):
            run.side_effect = [
                subprocess.CompletedProcess([], 0, json.dumps(derivation), ""),
                subprocess.CompletedProcess([], 0, str(selected) + "\n", ""),
            ]
            rootfs_diagnostic.realize_user_ramdisk(selected, user_drv)
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args.args[0], [
                "nix", "build", "--no-link", "--no-update-lock-file", "--print-out-paths",
                f"{user_drv}^out",
            ])
            derivation["derivations"][user_drv.name]["env"]["out"] = "/nix/store/other"
            run.side_effect = [subprocess.CompletedProcess([], 0, json.dumps(derivation), "")]
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "does not produce"):
                rootfs_diagnostic.realize_user_ramdisk(selected, user_drv)
            self.assertEqual(run.call_count, 3)

    def test_explicit_realization_rejects_wrong_reported_output(self):
        user_drv = Path("/nix/store/" + "f" * 32 + "-user-initramfs.cpio.gz.drv")
        selected = Path("/nix/store/" + "e" * 32 + "-user-initramfs.cpio.gz")
        derivation = {"derivations": {user_drv.name: {
            "env": {"out": str(selected)}, "outputs": {"out": {"path": selected.name}},
        }}}
        with mock.patch.object(rootfs_diagnostic.subprocess, "run") as run:
            run.side_effect = [
                subprocess.CompletedProcess([], 0, json.dumps(derivation), ""),
                subprocess.CompletedProcess([], 0, "/nix/store/wrong\n", ""),
            ]
            with self.assertRaisesRegex(rootfs_diagnostic.VerificationError, "different user ramdisk"):
                rootfs_diagnostic.realize_user_ramdisk(selected, user_drv)


class ApprovalPublishCommandTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.origin = self.root / "origin.git"
        self.git("init", "-q", "--bare", "--initial-branch=master", str(self.origin))
        self.repo = self.root / "repo"
        self.git("clone", "-q", str(self.origin), str(self.repo))
        component = self.repo / "services/opensecret"
        component.mkdir(parents=True)
        for name in ("pcrDev.json", "pcrProd.json", "pcrDevHistory.json", "pcrProdHistory.json"):
            (component / name).write_text(f"{name} fixture\n")
        (component / "other.rs").write_text("fn main() {}\n")
        scripts = self.repo / "scripts/ci"
        scripts.mkdir(parents=True)
        shutil.copyfile(ROOT / "scripts/ci/publish_opensecret_approval.sh", scripts / "publish_opensecret_approval.sh")
        self.git("-C", str(self.repo), "add", "-A")
        self.git("-C", str(self.repo), "commit", "-q", "-m", "fixture")
        self.git("-C", str(self.repo), "push", "-q", "origin", "HEAD:refs/heads/master")
        self.source_sha = self.git("-C", str(self.repo), "rev-parse", "HEAD")
        self.artifact = self.root / "artifact"
        self.artifact.mkdir()
        self.write_handoff()
        self.summary = self.root / "summary.md"

    def git(self, *arguments):
        return subprocess.run(
            ["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "commit.gpgsign=false", *arguments],
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def write_handoff(self, **overrides):
        handoff = {"environment": "dev", "source_sha": self.source_sha,
                   "measurements": VALID_MEASUREMENTS, "eif_sha256": "f" * 64, **overrides}
        (self.artifact / "handoff.json").write_text(json.dumps(handoff))

    def change_approvals(self, mode="dev"):
        prefix = "pcrDev" if mode == "dev" else "pcrProd"
        for name in (f"{prefix}.json", f"{prefix}History.json"):
            (self.repo / "services/opensecret" / name).write_text(f"{name} approved\n")

    def branch(self):
        return f"opensecret/pcr-approval-{self.source_sha[:12]}"

    def run_publish(self, mode="dev", path=None):
        env = {"PATH": path or os.environ["PATH"], "HOME": str(self.root), "GITHUB_TOKEN": "fixture-token",
               "GITHUB_REPOSITORY": "fixture/repo", "GITHUB_RUN_ID": "7", "GITHUB_STEP_SUMMARY": str(self.summary)}
        return subprocess.run(
            [shutil.which("bash"), "--noprofile", "--norc",
             str(self.repo / "scripts/ci/publish_opensecret_approval.sh"), mode, str(self.artifact)],
            cwd=self.repo, env=env, capture_output=True, text=True,
        )

    def origin_branches(self):
        return self.git("-C", str(self.origin), "for-each-ref", "--format=%(refname:short)", "refs/heads")

    def test_pushes_only_the_approval_files_to_a_review_branch(self):
        self.change_approvals()
        result = self.run_publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        branch = self.branch()
        self.assertEqual(sorted(self.origin_branches().split()), sorted(["master", branch]))
        self.assertEqual(self.git("-C", str(self.origin), "rev-parse", "master"), self.source_sha)
        changed = self.git("-C", str(self.origin), "diff", "--name-only", "master", branch)
        self.assertEqual(changed.split(), ["services/opensecret/pcrDev.json", "services/opensecret/pcrDevHistory.json"])
        message = self.git("-C", str(self.origin), "log", "-1", "--format=%an%n%B", branch)
        self.assertIn("github-actions[bot]", message)
        self.assertIn(self.source_sha, message)
        self.assertIn("a" * 96, message)
        self.assertIn(f"compare/master...{branch}", self.summary.read_text())
        self.assertNotIn("fixture-token", (self.repo / ".git/config").read_text())
        self.assertNotIn("fixture-token", result.stdout + result.stderr)

    def test_refuses_unexpected_changes_and_a_repeated_environment(self):
        self.change_approvals()
        (self.repo / "services/opensecret/other.rs").write_text("fn main() { changed }\n")
        result = self.run_publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.origin_branches(), "master")
        self.git("-C", str(self.repo), "checkout", "-q", "--", "services/opensecret/other.rs")
        self.assertEqual(self.run_publish().returncode, 0)
        self.git("-C", str(self.repo), "reset", "-q", "--hard", self.source_sha)
        self.change_approvals()
        result = self.run_publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already carries the dev approval", result.stderr)
        self.assertEqual(len(self.git("-C", str(self.origin), "rev-list", f"master..{self.branch()}").split()), 1)

    def test_second_environment_lands_on_the_same_branch(self):
        self.change_approvals("dev")
        self.assertEqual(self.run_publish("dev").returncode, 0)
        self.git("-C", str(self.repo), "reset", "-q", "--hard", self.source_sha)
        self.change_approvals("prod")
        self.write_handoff(environment="prod")
        result = self.run_publish("prod")
        self.assertEqual(result.returncode, 0, result.stderr)
        branch = self.branch()
        self.assertEqual(sorted(self.origin_branches().split()), sorted(["master", branch]))
        self.assertEqual(self.git("-C", str(self.origin), "rev-parse", "master"), self.source_sha)
        self.assertEqual(len(self.git("-C", str(self.origin), "rev-list", f"master..{branch}").split()), 2)
        changed = self.git("-C", str(self.origin), "diff", "--name-only", "master", branch)
        self.assertEqual(changed.split(), ["services/opensecret/pcrDev.json", "services/opensecret/pcrDevHistory.json",
                                           "services/opensecret/pcrProd.json", "services/opensecret/pcrProdHistory.json"])
        subjects = self.git("-C", str(self.origin), "log", "--format=%s", f"master..{branch}")
        self.assertEqual(subjects.splitlines(), [f"Approve prod OpenSecret EIF measurements from {self.source_sha[:12]}",
                                                 f"Approve dev OpenSecret EIF measurements from {self.source_sha[:12]}"])

    def test_lost_push_race_is_replayed_on_top_of_the_other_environment(self):
        # A git shim lets the prod run push to the branch after the dev run has
        # fetched and just before it pushes, so the first push is rejected.
        rival = self.root / "rival"
        self.git("clone", "-q", str(self.origin), str(rival))
        for name in ("pcrProd.json", "pcrProdHistory.json"):
            (rival / "services/opensecret" / name).write_text(f"{name} approved\n")
        self.git("-C", str(rival), "commit", "-q", "-am", f"Approve prod OpenSecret EIF measurements from {self.source_sha[:12]}")
        shim_dir = self.root / "shim"
        shim_dir.mkdir()
        marker = self.root / "raced"
        real_git = shutil.which("git")
        (shim_dir / "git").write_text(
            f"#!{shutil.which('bash')}\n"
            "for arg in \"$@\"; do\n"
            f"  if [ \"$arg\" = push ] && [ ! -e '{marker}' ]; then touch '{marker}'; "
            f"'{real_git}' -C '{rival}' push -q origin HEAD:refs/heads/{self.branch()}; fi\n"
            "done\n"
            f"exec '{real_git}' \"$@\"\n")
        (shim_dir / "git").chmod(0o755)
        self.change_approvals("dev")
        result = self.run_publish("dev", path=f"{shim_dir}:{os.environ['PATH']}")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rejected (attempt 1)", result.stderr)
        branch = self.branch()
        self.assertEqual(len(self.git("-C", str(self.origin), "rev-list", f"master..{branch}").split()), 2)
        changed = self.git("-C", str(self.origin), "diff", "--name-only", "master", branch)
        self.assertEqual(changed.split(), ["services/opensecret/pcrDev.json", "services/opensecret/pcrDevHistory.json",
                                           "services/opensecret/pcrProd.json", "services/opensecret/pcrProdHistory.json"])
        subjects = self.git("-C", str(self.origin), "log", "--format=%s", f"master..{branch}")
        self.assertEqual(subjects.splitlines()[0], f"Approve dev OpenSecret EIF measurements from {self.source_sha[:12]}")

    def test_nothing_to_publish_and_handoff_mismatches(self):
        result = self.run_publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.origin_branches(), "master")
        self.change_approvals()
        self.write_handoff(source_sha="0" * 40)
        self.assertNotEqual(self.run_publish().returncode, 0)
        self.write_handoff(environment="prod")
        self.assertNotEqual(self.run_publish().returncode, 0)
        self.assertEqual(self.origin_branches(), "master")


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
        for tool in ("dirname", "mktemp", "rm", "diff", "cat"):
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
            "    sys.exit(int(os.environ['TEST_FAIL_BUILD']))\n"
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
                # macOS resolves /tmp to /private/tmp; compare canonical paths.
                self.assertEqual(Path(trace["cwd"]).resolve(), self.component.resolve())
                self.assertEqual(trace["args"][:4], [
                    "build", "--no-update-lock-file", "--print-build-logs", "--out-link",
                ])
                self.assertEqual(trace["args"][-1], f".?submodules=1#eif-{mode}")

    def test_measurement_mismatch_exits_3_with_the_built_measurements(self):
        built = '{"PCR0":"not approved"}\n'
        for mode in ("dev", "prod"):
            with self.subTest(mode=mode):
                result = self.run_check(mode, TEST_MEASUREMENTS=built)
                self.assertEqual(result.returncode, 3, result.stderr)
                self.assertIn(f"EIF/PCR approval mismatch ({mode})", result.stderr)
                self.assertTrue(result.stdout.endswith(f"Built measurements ({mode}, pcr.json):\n{built}"))

    def test_build_failure_and_missing_generated_measurements_fail(self):
        # Status 1, never the mismatch status 3, whatever Nix exits with.
        for status in ("3", "42"):
            with self.subTest(nix_status=status):
                result = self.run_check(TEST_FAIL_BUILD=status)
                self.assertEqual(result.returncode, 1)
                self.assertIn("EIF build failed (dev)", result.stderr)
        for failure in ("TEST_MISSING_PCR", "TEST_MISSING_IMAGE", "TEST_PCR_DIRECTORY"):
            with self.subTest(failure=failure):
                result = self.run_check(**{failure: "1"})
                self.assertEqual(result.returncode, 1)
                self.assertIn("did not produce", result.stderr)

    def test_a_comparison_error_is_not_a_mismatch(self):
        diff = self.binaries / "diff"
        diff.unlink()
        diff.write_text("#!/bin/sh\nexit 2\n")
        diff.chmod(0o755)
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("Could not compare pcrDev.json", result.stderr)

    def test_invalid_mode_and_wrong_architecture_do_not_build(self):
        for mode, extra_env, status in (("preview", {}, 2), ("dev", {"TEST_ARCH": "x86_64"}, 1)):
            with self.subTest(mode=mode, extra_env=extra_env):
                result = self.run_check(mode, **extra_env)
                self.assertEqual(result.returncode, status)
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


if __name__ == "__main__":
    unittest.main()
