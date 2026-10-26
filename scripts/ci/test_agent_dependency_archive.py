#!/usr/bin/env python3
"""Dependency pruning, artifact provenance, transport and extraction boundaries."""

import copy
from datetime import datetime, timedelta, timezone
import hashlib
import gzip
import io
import json
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.error
import zipfile

import agent_dependency_archive as cache


REPO = "MaplePrivacyLabs/Maple"
JOB = "Agent (windows-latest)"
REV = "a" * 40
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
STAMP = "1234567890abcdef"


class Fixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.home, self.target, self.bundle = self.root / "cargo", self.root / "target", self.root / "bundle"
        raw = {"profile": "ci", "platform": "Windows", "architecture": "AMD64", "native": {"compiler": "pinned"}}
        self.identity = {"compatibility": hashlib.sha256(json.dumps(raw, sort_keys=True, separators=(",", ":")).encode()).hexdigest(), **raw}
        self.producer = {"repository": REPO, "run_id": 41, "run_attempt": 2,
                         "head_sha": REV, "job_name": JOB, "workflow": "agent-ci.yml"}
        self.environment = {"GITHUB_REPOSITORY": REPO, "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/master",
                            "GITHUB_SHA": REV, "GITHUB_RUN_ID": "41", "GITHUB_RUN_ATTEMPT": "2"}
        selected = patch.dict(os.environ, self.environment, clear=True)
        selected.start()
        self.addCleanup(selected.stop)
        self.metadata = {"packages": [], "resolve": {"nodes": []}}
        self.package("public", "1.0.0", REGISTRY, self.home / "registry/src/index-abc/public-1.0.0/Cargo.toml")
        self.package("gitlib", "2.0.0", f"git+https://github.com/MaplePrivacyLabs/rusqlite?rev={REV}#{REV}", self.home / "git/checkouts/public-abc/1234567/Cargo.toml")
        self.package("maple-app", "0.0.3", None, self.root / "checkout/apps/maple-agent/app/Cargo.toml")
        self.package("maple-sdk", "0.4.0", None, self.root / "checkout/sdk/rust/Cargo.toml")
        self.write("registry/cache/index-abc/public-1.0.0.crate", b"released archive")
        self.write("registry/index/index-abc/.cache/pu/bl/public", b"public index record")
        self.write("registry/index/index-abc/config.json", b'{"dl":"https://static.crates.io/crates","api":"https://crates.io","private":"secret"}')
        self.write("credentials.toml", b"do not cache credentials")
        self.write("config.toml", b"do not cache config")
        self.write("bin/cargo-private", b"do not cache binaries")
        self.write("git/db/public-abc/config", b"[credential]\nhelper = secret\n")
        self.write("git/db/public-abc/objects/object", b"public Git object")
        self.write("git/checkouts/public-abc/1234567/.git/config", b"[remote]\nurl = https://user:secret@example.com\n")
        self.write("git/checkouts/public-abc/1234567/.git/hooks/post-checkout", b"do not execute hook")
        self.write("git/checkouts/public-abc/1234567/.git/objects/info/alternates", (str(self.home / "git/db/public-abc/objects") + "\n").encode())
        for prefix in ("debug", "build/debug"):
            for name in ("public", "gitlib", "maple-app", "maple-sdk"):
                self.write(f"{prefix}/deps/lib{name.replace('-', '_')}-{STAMP}.rlib", name.encode(), self.target)
                self.write(f"{prefix}/deps/{name.replace('-', '_')}-{STAMP}.d", b"dep-info", self.target)
                self.write(f"{prefix}/.fingerprint/{name}-{STAMP}/lib-fingerprint", b"freshness", self.target)
                self.write(f"{prefix}/build/{name}-{STAMP}/out/native.lib", b"native", self.target)
            self.write(f"{prefix}/incremental/private/data", b"incremental", self.target)
            self.write(f"{prefix}/maple-agent.exe", b"workspace executable", self.target)

    def write(self, relative, content, root=None):
        path = (root or self.home) / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        os.utime(path, (1_700_000_000, 1_700_000_000))
        return path

    def package(self, name, version, source, manifest):
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text(f'[package]\nname="{name}"\nversion="{version}"\n')
        package_id = f"{source or 'path+workspace'}#{name}@{version}"
        self.metadata["packages"].append({"name": name, "version": version, "source": source,
            "id": package_id, "manifest_path": str(manifest), "targets": [{"name": name, "kind": ["lib"]}]})
        self.metadata["resolve"]["nodes"].append({"id": package_id, "features": ["default"]})

    def pack(self, **options):
        return cache.pack(self.identity, self.metadata, self.target, self.home, self.bundle, JOB, "agent-ci.yml", **options)

    def tar_names(self):
        with tarfile.open(self.bundle / "dependencies.tar.gz", "r:gz") as archive:
            return archive.getnames()

    def unpack(self):
        return cache.unpack(self.bundle, self.identity, self.producer, self.root / "restored-cargo", self.root / "restored-target")

    def malicious_tar(self, members):
        self.pack()
        with tarfile.open(self.bundle / "dependencies.tar.gz", "w:gz") as archive:
            total = 0
            for name, kind, payload in members:
                info = tarfile.TarInfo(name)
                info.type = kind
                info.size = len(payload) if kind == tarfile.REGTYPE else 0
                info.linkname = "../../outside" if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE) else ""
                archive.addfile(info, io.BytesIO(payload) if kind == tarfile.REGTYPE else None)
                total += info.size
        manifest_path = self.bundle / "manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest.update(files=len(members), unpacked_bytes=total, archive_bytes=(self.bundle / "dependencies.tar.gz").stat().st_size,
                        archive_sha256=cache.sha256(self.bundle / "dependencies.tar.gz"))
        manifest_path.write_text(json.dumps(manifest))


class ArchiveTests(Fixture):
    def test_roundtrip_only_public_dependencies_preserving_freshness(self):
        manifest = self.pack()
        names = self.tar_names()
        self.assertTrue(any("build/debug/build/public-" in name for name in names))
        self.assertTrue(any("debug/deps/libpublic-" in name for name in names))
        self.assertFalse(any(any(part in name for part in ("maple_app", "maple-app", "maple_sdk", "maple-sdk", "incremental", "credentials", "cargo/bin", "hooks")) for name in names))
        self.assertNotIn("cargo/config.toml", names)
        self.unpack()
        restored = self.root / f"restored-target/debug/deps/libpublic-{STAMP}.rlib"
        self.assertEqual(restored.read_bytes(), b"public")
        self.assertEqual(restored.stat().st_mtime, 1_700_000_000)
        self.assertEqual(self.producer, manifest["producer"])
        self.assertNotIn("secret", (self.root / "restored-cargo/git/db/public-abc/config").read_text())
        alternate = self.root / "restored-cargo/git/checkouts/public-abc/1234567/.git/objects/info/alternates"
        self.assertFalse(Path(alternate.read_text().strip()).is_absolute())

    def test_path_package_name_collision_excludes_ambiguous_output(self):
        self.package("local-public", "1.0.0", None, self.root / "checkout/local-public/Cargo.toml")
        self.metadata["packages"][-1]["targets"][0]["name"] = "public"
        self.pack()
        self.assertFalse(any("deps/libpublic-" in name for name in self.tar_names()))

    def test_unreviewed_git_sources_outputs_and_name_collisions_are_excluded(self):
        self.package("private-helper", "1.0.0", f"git+https://github.com/unreviewed/private?rev={REV}#{REV}",
                     self.home / "git/checkouts/private-abc/1234567/Cargo.toml")
        self.metadata["packages"][-1]["targets"][0]["name"] = "public"
        self.write("git/checkouts/private-abc/1234567/secret-source.rs", b"private source")
        self.write(f"debug/build/private-helper-{STAMP}/out/private.lib", b"private native output", self.target)
        self.write(f"debug/.fingerprint/private-helper-{STAMP}/lib-fingerprint", b"private fingerprint", self.target)
        self.write(f"debug/deps/libprivate_helper-{STAMP}.rlib", b"private library", self.target)
        # A private package can also collide with a reviewed package name.
        self.package("gitlib", "3.0.0", f"git+https://github.com/unreviewed/another?rev={REV}#{REV}",
                     self.home / "git/checkouts/private-other/1234567/Cargo.toml")
        self.pack()
        names = self.tar_names()
        self.assertFalse(any("private-abc" in name or "private-other" in name or "private-helper" in name or "private_helper" in name for name in names))
        self.assertFalse(any("deps/libpublic-" in name or "deps/libgitlib-" in name or "/build/gitlib-" in name or "/.fingerprint/gitlib-" in name for name in names))

    def test_reviewed_git_repository_normalization_keeps_security_guards(self):
        original = self.metadata["packages"][1]["source"]
        for repository in ("MaplePrivacyLabs/rusqlite", "MAPLEPRIVACYLABS/RUSQLITE.git", "MaplePrivacyLabs/Rusqlite.GIT/"):
            self.metadata["packages"][1]["source"] = f"git+https://GitHub.com/{repository}?rev={REV}#{REV}"
            self.assertIn("gitlib", cache.packages(self.metadata)[1])
        for source in (f"git+http://github.com/MaplePrivacyLabs/rusqlite#{REV}",
                       f"git+https://user:secret@github.com/MaplePrivacyLabs/rusqlite#{REV}",
                       "git+https://github.com/MaplePrivacyLabs/rusqlite#short",
                       f"git+https://github.com/MaplePrivacyLabs/rusqlite?token=secret#{REV}"):
            self.metadata["packages"][1]["source"] = source
            self.assertNotIn("gitlib", cache.packages(self.metadata)[1])
        self.metadata["packages"][1]["source"] = original

    def test_unrelated_application_bundle_symlinks_are_not_traversed(self):
        bundle = self.target / "debug/Maple.app/Contents/Frameworks"
        bundle.mkdir(parents=True)
        (bundle / "system").symlink_to(self.root, target_is_directory=True)
        self.pack()
        self.assertFalse(any("Maple.app" in name for name in self.tar_names()))

    def test_link_outside_a_selected_dependency_root_is_refused(self):
        source = self.home / "registry/src/index-abc/public-1.0.0/private"
        source.symlink_to(self.home / "credentials.toml")
        with self.assertRaises(cache.Rejected):
            self.pack()

    def test_git_pointer_files_are_refused_on_pack_and_restore(self):
        checkout_git = self.home / "git/checkouts/public-abc/1234567/.git"
        shutil.rmtree(checkout_git)
        checkout_git.write_text("gitdir: /outside/private-object-store\n")
        with self.assertRaises(cache.Rejected):
            self.pack()
        shutil.rmtree(self.bundle)
        checkout_git.unlink()
        checkout_git.mkdir()
        self.malicious_tar([("cargo/git/checkouts/repo/rev/.git", tarfile.REGTYPE, b"gitdir: /outside/private-object-store\n")])
        with self.assertRaises(cache.Rejected):
            self.unpack()
        self.assertFalse((self.root / "restored-cargo").exists())

    def test_pr_and_manual_producers_cannot_pack(self):
        for event, ref in (("pull_request", "refs/pull/2/merge"), ("workflow_dispatch", "refs/heads/master"), ("push", "refs/heads/topic")):
            with self.subTest(event=event), patch.dict(os.environ, {"GITHUB_EVENT_NAME": event, "GITHUB_REF": ref}):
                with self.assertRaises(cache.Rejected):
                    self.pack()

    def test_recent_receipt_skips_compression_for_workspace_only_bump(self):
        public, _, _ = cache.packages(self.metadata)
        receipt = self.root / "receipt.json"
        receipt.write_text(json.dumps({"compatibility": self.identity["compatibility"], "dependency_fingerprint": "",
            "dependency_graph": cache.dependency_graph(self.metadata, public), "created_at": datetime.now(timezone.utc).isoformat(), "artifact_id": 17,
            "reader": cache.receipt_reader(JOB)}))
        self.metadata["packages"][-1]["version"] = "0.4.1"
        result = self.pack(receipt=receipt)
        self.assertFalse(result["needs_upload"])
        self.assertFalse(self.bundle.exists())
        self.metadata["resolve"]["nodes"][0]["features"].append("new-native-feature")
        self.assertTrue(self.pack(receipt=receipt)["needs_upload"])

    def test_old_receipt_and_changed_dependency_refresh_the_seed(self):
        public, _, _ = cache.packages(self.metadata)
        receipt = self.root / "receipt.json"
        receipt.write_text(json.dumps({"compatibility": self.identity["compatibility"], "dependency_fingerprint": "",
            "dependency_graph": cache.dependency_graph(self.metadata, public), "created_at": (datetime.now(timezone.utc) - timedelta(days=2)).isoformat(), "artifact_id": 17}))
        self.assertTrue(self.pack(receipt=receipt)["needs_upload"])

    def test_provider_requires_verified_test_only_graph(self):
        identity = {**self.identity, "profile": "provider"}
        with self.assertRaises(cache.Rejected):
            cache.artifact_name(identity)
        self.assertNotEqual(cache.artifact_name(identity, "b" * 64), cache.artifact_name(identity, "c" * 64))

    def test_environment_accepts_scoped_flat_identity_without_changing_old_helper(self):
        raw = {key: value for key, value in self.identity.items() if key != "compatibility"}
        raw["scope_schema"] = 1
        value = {**raw, "compatibility": hashlib.sha256(json.dumps(raw, sort_keys=True, separators=(",", ":")).encode()).hexdigest(),
                 "legacy_identity": {"old": "full raw identity"}, "legacy_compatibility": "b" * 64}
        path = self.root / "environment.json"
        path.write_text(json.dumps(value))
        self.assertEqual(cache.environment(path), value)
        value["native"]["compiler"] = "changed"
        path.write_text(json.dumps(value))
        with self.assertRaises(cache.Rejected):
            cache.environment(path)

    def test_archive_digest_and_manifest_identity_are_enforced(self):
        self.pack()
        with (self.bundle / "dependencies.tar.gz").open("ab") as archive:
            archive.write(b"tampered")
        with self.assertRaises(cache.Rejected):
            self.unpack()
        self.assertFalse((self.root / "restored-target").exists())

    def test_compressed_and_member_budgets_remain_separate(self):
        self.assertEqual(cache.MAX_ARCHIVE, 6 * 1024**3)
        self.assertEqual(cache.MAX_FILE, 3 * 1024**3)
        with patch.object(cache, "MAX_ARCHIVE", 1):
            with self.assertRaises(cache.Rejected):
                self.pack()
        shutil.rmtree(self.bundle)
        # A larger compressed-archive allowance cannot admit a single member
        # over its independent bound, even before tarfile reads its payload.
        header = tarfile.TarInfo("target/debug/deps/libpublic-" + STAMP + ".rlib")
        header.size = cache.MAX_FILE + 1
        archive = self.root / "oversized-member.tar.gz"
        archive.write_bytes(gzip.compress(header.tobuf(format=tarfile.USTAR_FORMAT)))
        self.assertLess(archive.stat().st_size, cache.MAX_ARCHIVE)
        with self.assertRaises(cache.Rejected):
            cache.tar_envelope(archive)

    def test_unsafe_paths_links_aliases_and_local_outputs_are_refused(self):
        attacks = ("../escape", "/absolute", "cargo/credentials.toml", "cargo/bin/tool", "cargo/registry/src/a/../escape",
                   "cargo/registry/src/a/CON.txt", "cargo/registry/src/a/file:stream", "cargo/registry/src/a/back\\slash",
                   "cargo/registry/src/a/trailing.", f"target/debug/deps/libmaple_app-{STAMP}.rlib")
        for name in attacks:
            with self.subTest(name=name):
                shutil.rmtree(self.bundle, ignore_errors=True)
                self.malicious_tar([(name, tarfile.REGTYPE, b"payload")])
                with self.assertRaises(cache.Rejected):
                    self.unpack()
                self.assertFalse((self.root / "restored-cargo").exists())
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE):
            with self.subTest(kind=kind):
                shutil.rmtree(self.bundle)
                self.malicious_tar([("cargo/registry/src/a/file", kind, b"")])
                with self.assertRaises(cache.Rejected):
                    self.unpack()

    def test_duplicate_case_alias_and_destination_symlink_fail_before_writes(self):
        self.malicious_tar([("cargo/registry/src/a/file", tarfile.REGTYPE, b"one"), ("cargo/registry/src/a/FILE", tarfile.REGTYPE, b"two")])
        with self.assertRaises(cache.Rejected):
            self.unpack()
        self.assertFalse((self.root / "restored-cargo").exists())
        shutil.rmtree(self.bundle)
        self.pack()
        destination = self.root / "restored-target/debug/deps"
        destination.parent.mkdir(parents=True)
        outside = self.root / "outside"
        outside.mkdir()
        destination.symlink_to(outside, target_is_directory=True)
        with self.assertRaises(cache.Rejected):
            self.unpack()
        self.assertEqual(list(outside.iterdir()), [])
        self.assertFalse((self.root / "restored-cargo").exists())

    def test_untrusted_git_configuration_and_unpack_bombs_are_rejected(self):
        self.malicious_tar([("cargo/git/db/public/config", tarfile.REGTYPE, b"[credential]\nhelper = malicious\n")])
        with self.assertRaises(cache.Rejected):
            self.unpack()
        self.assertFalse((self.root / "restored-cargo").exists())
        shutil.rmtree(self.bundle)
        self.pack()
        with patch.object(cache, "MAX_UNPACKED", 1):
            with self.assertRaises(cache.Rejected):
                self.unpack()

    def test_symlink_ancestor_and_unsafe_git_or_registry_metadata_are_rejected(self):
        self.pack()
        outside = self.root / "outside"
        outside.mkdir()
        parent = self.root / "linked-parent"
        parent.symlink_to(outside, target_is_directory=True)
        with self.assertRaises(cache.Rejected):
            cache.unpack(self.bundle, self.identity, self.producer, self.root / "restored-cargo", parent / "target")
        self.assertEqual(list(outside.iterdir()), [])
        for name, payload in (("cargo/git/checkouts/repo/rev/.git/objects/info/alternates", b"/outside/objects\n"),
                              ("cargo/registry/index/index/config.json", b'{"dl":"https://private.example/token","api":"https://crates.io"}')):
            shutil.rmtree(self.bundle)
            self.malicious_tar([(name, tarfile.REGTYPE, payload)])
            with self.subTest(name=name), self.assertRaises(cache.Rejected):
                self.unpack()
            self.assertFalse((self.root / "restored-cargo").exists())

    def test_oversized_pax_is_rejected_before_tarfile_reads_payload(self):
        self.pack()
        header = tarfile.TarInfo("PaxHeader")
        header.type, header.size = tarfile.XHDTYPE, 1024**3
        archive = self.bundle / "dependencies.tar.gz"
        archive.write_bytes(gzip.compress(header.tobuf(format=tarfile.USTAR_FORMAT)))
        manifest = json.loads((self.bundle / "manifest.json").read_text())
        manifest.update(archive_bytes=archive.stat().st_size, archive_sha256=cache.sha256(archive))
        (self.bundle / "manifest.json").write_text(json.dumps(manifest))
        with patch.object(cache.tarfile, "open", side_effect=AssertionError("unbounded PAX parser must not run")):
            with self.assertRaises(cache.Rejected):
                self.unpack()

    def test_receipt_cannot_be_replayed_from_restored_cache_or_prior_reader_job(self):
        with self.assertRaises(cache.Rejected):
            self.pack(receipt=self.target / "receipt.json")
        public, _, _ = cache.packages(self.metadata)
        receipt = self.root / "receipt.json"
        receipt.write_text(json.dumps({"compatibility": self.identity["compatibility"], "dependency_fingerprint": "",
            "dependency_graph": cache.dependency_graph(self.metadata, public), "created_at": datetime.now(timezone.utc).isoformat(), "artifact_id": 17,
            "reader": {**cache.receipt_reader(JOB), "run_id": "older-job"}}))
        self.assertTrue(self.pack(receipt=receipt)["needs_upload"])


class API:
    repository = REPO

    def __init__(self, artifact, run, jobs, zip_path=None):
        self.artifact, self.run, self.jobs, self.zip_path = artifact, run, jobs, zip_path

    def get(self, endpoint):
        if endpoint.startswith("actions/artifacts?"):
            return {"artifacts": [self.artifact]}
        if endpoint.startswith("actions/artifacts/"):
            return self.artifact
        if "/attempts/" in endpoint:
            return self.jobs
        return self.run

    def download(self, artifact_id, destination, digest):
        if cache.sha256(self.zip_path) != digest:
            raise cache.Rejected("ZIP digest mismatch")
        shutil.copyfile(self.zip_path, destination)
        return destination.stat().st_size


class ProvenanceTests(Fixture):
    def setUp(self):
        super().setUp()
        now = datetime.now(timezone.utc)
        self.artifact = {"id": 7, "name": cache.artifact_name(self.identity), "expired": False, "size_in_bytes": 50,
            "digest": "sha256:" + "b" * 64, "created_at": (now - timedelta(hours=1)).isoformat(),
            "workflow_run": {"id": 41, "head_sha": REV, "head_branch": "master", "repository_id": 2, "head_repository_id": 2}}
        self.run = {"id": 41, "run_attempt": 2, "event": "push", "head_sha": REV, "head_branch": "master",
            "path": ".github/workflows/agent-ci.yml", "repository": {"id": 2, "full_name": REPO},
            "head_repository": {"id": 2, "full_name": REPO}, "conclusion": "failure"}
        self.jobs = {"total_count": 1, "jobs": [{"id": 8, "name": JOB, "status": "completed", "conclusion": "success",
            "started_at": (now - timedelta(hours=2)).isoformat(), "completed_at": (now - timedelta(minutes=30)).isoformat(),
            "steps": [{"name": "build/test/provenance gates", "conclusion": "success"}]}]}

    def proof(self):
        return cache.source_proof(API(self.artifact, self.run, self.jobs), self.artifact,
            cache.artifact_name(self.identity), JOB, "agent-ci.yml", datetime.now(timezone.utc))

    def test_exact_successful_platform_job_is_usable_despite_other_job_failure(self):
        self.assertEqual(self.proof(), self.producer)

    def test_earlier_successful_platform_seed_survives_partial_later_rerun(self):
        with patch.dict(os.environ, {"GITHUB_RUN_ATTEMPT": "1"}):
            self.pack()
        self.producer["run_attempt"] = 1
        zipped = self.root / "earlier-attempt.zip"
        with zipfile.ZipFile(zipped, "w") as archive:
            for name in ("manifest.json", "dependencies.tar.gz"):
                archive.write(self.bundle / name, name)
        self.artifact["digest"] = "sha256:" + cache.sha256(zipped)
        api = API(self.artifact, self.run, self.jobs, zipped)
        events = []
        get, download = api.get, api.download

        def attempted(endpoint):
            events.append(endpoint)
            if "/attempts/2/jobs" in endpoint:
                return {"total_count": 1, "jobs": [{"name": "Agent (different-platform)", "conclusion": "failure"}]}
            return get(endpoint)

        def transferred(*arguments):
            events.append("verified-zip-download")
            return download(*arguments)

        receipt = self.root / "receipt.json"
        with patch.object(api, "get", side_effect=attempted), patch.object(api, "download", side_effect=transferred):
            self.assertTrue(cache.restore(api, self.identity, self.root / "restored-target", self.root / "restored-cargo",
                JOB, "agent-ci.yml", receipt=receipt))
        jobs_request = "actions/runs/41/attempts/1/jobs?per_page=100"
        self.assertIn(jobs_request, events)
        self.assertLess(events.index("verified-zip-download"), events.index(jobs_request))
        self.assertFalse(any("/attempts/2/jobs" in event for event in events))
        self.assertEqual(json.loads(receipt.read_text())["producer"]["run_attempt"], 1)

    def test_future_or_noninteger_manifest_attempt_is_rejected(self):
        api = API(self.artifact, self.run, self.jobs)
        for attempt in (0, 3, "1", True):
            with self.subTest(attempt=attempt), self.assertRaises(cache.Rejected):
                cache.source_proof(api, self.artifact, cache.artifact_name(self.identity), JOB,
                    "agent-ci.yml", datetime.now(timezone.utc), attempt=attempt)

    def test_pr_manual_fork_wrong_workflow_and_head_cannot_supply_seed(self):
        for change in ({"event": "pull_request"}, {"event": "workflow_dispatch"}, {"head_branch": "topic"},
                       {"head_repository": {"id": 3, "full_name": "fork/Maple"}}, {"path": ".github/workflows/other.yml"},
                       {"head_sha": "c" * 40}):
            with self.subTest(change=change):
                original = self.run
                self.run = {**self.run, **change}
                with self.assertRaises(cache.Rejected):
                    self.proof()
                self.run = original

    def test_wrong_attempt_timestamp_failed_or_duplicate_job_is_rejected(self):
        for conclusion in ("failure", "skipped", "cancelled"):
            self.jobs["jobs"][0]["conclusion"] = conclusion
            with self.assertRaises(cache.Rejected):
                self.proof()
        self.jobs["jobs"][0]["conclusion"] = "success"
        self.artifact["created_at"] = (datetime.now(timezone.utc) - timedelta(hours=4)).isoformat()
        with self.assertRaises(cache.Rejected):
            self.proof()
        self.artifact["created_at"] = (datetime.now(timezone.utc) - timedelta(hours=1)).isoformat()
        self.jobs["jobs"].append(copy.deepcopy(self.jobs["jobs"][0]))
        with self.assertRaises(cache.Rejected):
            self.proof()

    def test_missing_digest_expiry_and_age_fail_closed(self):
        for change in ({"digest": None}, {"expired": True}, {"created_at": (datetime.now(timezone.utc) - timedelta(days=8)).isoformat()}, {"size_in_bytes": cache.MAX_ARCHIVE * 2}):
            original = self.artifact
            self.artifact = {**self.artifact, **change}
            with self.subTest(change=change), self.assertRaises((cache.Rejected, TypeError)):
                self.proof()
            self.artifact = original

    def test_end_to_end_restore_writes_verified_receipt_and_never_executes(self):
        self.pack()
        zipped = self.root / "bundle.zip"
        with zipfile.ZipFile(zipped, "w") as archive:
            for name in ("manifest.json", "dependencies.tar.gz"):
                archive.write(self.bundle / name, name)
        self.artifact["digest"] = "sha256:" + cache.sha256(zipped)
        receipt = self.root / "receipt.json"
        with patch.object(cache.os, "system", side_effect=AssertionError("restore must not execute")):
            self.assertTrue(cache.restore(API(self.artifact, self.run, self.jobs, zipped), self.identity,
                self.root / "restored-target", self.root / "restored-cargo", JOB, "agent-ci.yml", receipt=receipt))
        proven = json.loads(receipt.read_text())
        self.assertEqual(proven["artifact_digest"], self.artifact["digest"])
        self.assertEqual(proven["producer"], self.producer)
        self.assertTrue((self.root / f"restored-target/debug/deps/libpublic-{STAMP}.rlib").is_file())

    def test_corrupt_service_zip_digest_is_a_cold_miss(self):
        self.pack()
        zipped = self.root / "corrupt.zip"
        zipped.write_bytes(b"corrupt bytes")
        self.assertFalse(cache.restore(API(self.artifact, self.run, self.jobs, zipped), self.identity,
            self.root / "restored-target", self.root / "restored-cargo", JOB, "agent-ci.yml"))
        self.assertFalse((self.root / "restored-target").exists())

    def test_zip_accepts_only_two_regular_bounded_expected_files(self):
        for name in ("../manifest.json", "unexpected", "directory/manifest.json"):
            zipped = self.root / "malicious.zip"
            with zipfile.ZipFile(zipped, "w") as archive:
                archive.writestr(name, b"bad")
                archive.writestr("dependencies.tar.gz", b"data")
            with self.subTest(name=name), self.assertRaises(cache.Rejected):
                cache.unzip(zipped, self.root)

    def test_scan_is_bounded_and_unavailable_service_never_blocks_build(self):
        api = API(self.artifact, self.run, self.jobs)
        requested = []

        def get(endpoint):
            requested.append(endpoint)
            if endpoint.startswith("actions/artifacts?"):
                return {"artifacts": [{"id": index} for index in range(100)]}
            return {"id": 0, "name": "wrong-name"}

        with patch.object(api, "get", side_effect=get):
            self.assertFalse(cache.restore(api, self.identity, self.target, self.home, JOB, "agent-ci.yml"))
        self.assertEqual(len(requested), cache.MAX_CANDIDATES + 1)
        identity = self.root / "identity.json"
        identity.write_text(json.dumps(self.identity))
        github_output = self.root / "github-output"
        with patch.dict(os.environ, {"GH_TOKEN": "read-only-token", "GITHUB_OUTPUT": str(github_output)}), \
             patch("sys.argv", ["agent_dependency_archive.py", "restore", "--environment", str(identity), "--target", str(self.root / "new-target"), "--job-name", JOB]), \
             patch.object(cache.GitHub, "get", side_effect=OSError("service unavailable")):
            cache.main()
        self.assertEqual(github_output.read_text(), "restored=false\n")
        self.assertFalse((self.root / "new-target").exists())


class Response(io.BytesIO):
    def __init__(self, value):
        super().__init__(value)
        self.headers = {"Content-Length": str(len(value))}


class TransportTests(unittest.TestCase):
    def test_api_auth_is_never_forwarded_to_artifact_blob(self):
        api = cache.GitHub(REPO, "private-token")
        observed = []
        data = b"verified ZIP bytes"

        class Opener:
            def open(self, request, timeout):
                observed.append(request)
                if len(observed) == 1:
                    raise urllib.error.HTTPError(request.full_url, 302, "redirect", {"Location": "https://safe.blob.core.windows.net/artifact?signed=opaque"}, None)
                return Response(data)

        with tempfile.TemporaryDirectory() as temporary, patch.object(cache.urllib.request, "build_opener", return_value=Opener()):
            self.assertEqual(api.download(7, Path(temporary) / "artifact.zip", hashlib.sha256(data).hexdigest()), len(data))
        self.assertEqual(observed[0].get_header("Authorization"), "Bearer private-token")
        self.assertIsNone(observed[1].get_header("Authorization"))
        self.assertNotIn("private-token", observed[1].full_url)

    def test_foreign_redirect_and_wrong_digest_are_refused(self):
        api = cache.GitHub(REPO, "private-token")

        class Redirect:
            def open(self, request, timeout):
                raise urllib.error.HTTPError(request.full_url, 302, "redirect", {"Location": "https://attacker.example/artifact"}, None)

        with tempfile.TemporaryDirectory() as temporary, patch.object(cache.urllib.request, "build_opener", return_value=Redirect()):
            with self.assertRaises(cache.Rejected):
                api.download(7, Path(temporary) / "artifact.zip", "b" * 64)
        with tempfile.TemporaryDirectory() as temporary, patch.object(cache.urllib.request, "build_opener") as opener:
            opener.return_value.open.return_value = Response(b"wrong ZIP")
            with self.assertRaises(cache.Rejected):
                api.download(7, Path(temporary) / "artifact.zip", "b" * 64)

    def test_download_bytes_are_bounded_even_when_header_lies(self):
        api = cache.GitHub(REPO, "private-token")
        response = Response(b"too many streamed bytes")
        response.headers["Content-Length"] = "1"
        with tempfile.TemporaryDirectory() as temporary, patch.object(cache.urllib.request, "build_opener") as opener, \
             patch.object(cache, "MAX_ARCHIVE", 1), patch.object(cache, "MAX_MANIFEST", 1), patch.object(cache, "CHUNK", 1):
            # Tighten the fixed transport-overhead margin by using an object
            # whose stream lies about producing enormous chunks.
            response.read = lambda size: b"x" * (1024**2 + 3)
            opener.return_value.open.return_value = response
            with self.assertRaises(cache.Rejected):
                api.download(7, Path(temporary) / "artifact.zip", "b" * 64)
if __name__ == "__main__":
    unittest.main()
