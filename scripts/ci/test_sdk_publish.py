#!/usr/bin/env python3
"""Offline tests for SDK registry, package, and publisher provenance boundaries."""

import base64
import copy
import gzip
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sdk_publish as publish


SHA = "a" * 40
VERSION = "3.7.0"
RUN_ID = 12345
ATTEMPT = 2
REPOSITORY = "MaplePrivacyLabs/Maple"
NAMES = {"npm": "@mapleai/sdk", "rust": "maple-sdk", "proxy": "maple-proxy"}
ARCHIVES = {"npm": "package.tgz", "rust": "package.crate", "proxy": "package.crate"}
# Where each crate lives in the repository, and the workflow that publishes it.
CRATE_SOURCES = {"rust": "sdk/rust", "proxy": "proxy"}
WORKFLOWS = {"npm": "sdk-publish-npm.yml", "rust": "sdk-publish-rust.yml", "proxy": "sdk-publish-rust.yml"}


def npm_manifest():
    return {
        "name": "@mapleai/sdk",
        "version": VERSION,
        "repository": {
            "type": "git",
            "url": "git+https://github.com/MaplePrivacyLabs/Maple.git",
            "directory": "sdk",
        },
        "type": "module",
        "main": "./dist/maple-sdk.umd.cjs",
        "module": "./dist/maple-sdk.es.js",
        "types": "./dist/index.d.ts",
        "exports": {
            ".": {
                "import": {"types": "./dist/index.d.ts", "default": "./dist/maple-sdk.es.js"},
                "require": {"types": "./dist/index.d.cts", "default": "./dist/maple-sdk.umd.cjs"},
            }
        },
    }


def rust_manifest(kind="rust"):
    return (
        f'[package]\nname = "{NAMES[kind]}"\nversion = "3.7.0"\nedition = "2021"\n'
        'repository = "https://github.com/MaplePrivacyLabs/Maple"\n'
    ).encode()


def package_entries(kind):
    if kind == "npm":
        return [
            ("package/", None),
            ("package/package.json", json.dumps(npm_manifest()).encode()),
            ("package/README.md", b"SDK package fixture\n"),
            ("package/LICENSE.md", b"MIT\n"),
            ("package/dist/maple-sdk.es.js", b"export const fixture = true;\n"),
            ("package/dist/maple-sdk.umd.cjs", b"module.exports = {};\n"),
            ("package/dist/index.d.ts", b"export declare const fixture: boolean;\n"),
            ("package/dist/index.d.cts", b"export declare const fixture: boolean;\n"),
        ]
    prefix = f"{NAMES[kind]}-{VERSION}"
    return [
        (prefix + "/", None),
        (prefix + "/Cargo.toml", rust_manifest(kind)),
        (prefix + "/src/lib.rs", b"pub fn fixture() {}\n"),
        (prefix + "/.cargo_vcs_info.json",
         json.dumps({"git": {"sha1": SHA}, "path_in_vcs": CRATE_SOURCES[kind]}).encode()),
    ]


def tar_bytes(entries, extra_member=None):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for name, data in entries:
            member = tarfile.TarInfo(name)
            if data is None:
                member.type = tarfile.DIRTYPE
            else:
                member.size = len(data)
            archive.addfile(member, None if data is None else io.BytesIO(data))
        if extra_member is not None:
            archive.addfile(extra_member)
    return output.getvalue()


def registry_inventory(kind, versions=("3.6.0",)):
    if kind == "npm":
        return {
            "name": "@mapleai/sdk",
            "versions": {
                version: {
                    "name": "@mapleai/sdk",
                    "version": version,
                    "dist": {
                        "tarball": f"https://registry.npmjs.org/@mapleai/sdk/-/sdk-{version}.tgz",
                        "shasum": "b" * 40,
                        "integrity": "sha512-" + base64.b64encode(b"b" * 64).decode(),
                    },
                }
                for version in versions
            },
            "dist-tags": {"latest": versions[-1]},
        }
    return {
        "crate": {"id": NAMES[kind], "name": NAMES[kind], "versions": list(range(1, len(versions) + 1)), "num_versions": len(versions)},
        "versions": [
            {"id": index, "crate": NAMES[kind], "num": version, "yanked": False, "checksum": "b" * 64}
            for index, version in enumerate(versions, start=1)
        ],
    }


class VersionAndRegistryTests(unittest.TestCase):
    def test_stable_versions_compare_numerically(self):
        self.assertEqual(publish.stable_version("0.0.0"), (0, 0, 0))
        self.assertGreater(publish.stable_version("3.10.0"), publish.stable_version("3.9.99"))

    def test_noncanonical_and_nonstable_versions_are_rejected(self):
        for version in ("", "v3.7.0", "03.7.0", "3.07.0", "3.7.00", "3.7", "3.7.0.1", "3.7.0-beta.1", "3.7.0+build", " 3.7.0", "3.7.0\n"):
            with self.subTest(version=version), self.assertRaises(publish.Rejected):
                publish.stable_version(version)

    def test_bootstrap_requires_confirmed_package_absence(self):
        for kind in NAMES:
            with self.subTest(kind=kind):
                publish.validate_registry(kind, VERSION, "bootstrap", None)
                with self.assertRaises(publish.Rejected):
                    publish.validate_registry(kind, VERSION, "bootstrap", registry_inventory(kind))

    def test_trusted_requires_existing_valid_inventory(self):
        for kind in NAMES:
            with self.subTest(kind=kind):
                publish.validate_registry(kind, VERSION, "trusted", registry_inventory(kind))
                with self.assertRaises(publish.Rejected):
                    publish.validate_registry(kind, VERSION, "trusted", None)

    def test_unknown_registry_responses_never_mean_absence(self):
        for kind in NAMES:
            for mode in ("bootstrap", "trusted"):
                for inventory in ({}, [], "Not found", {"error": "rate limit exceeded"}, {"versions": []}):
                    with self.subTest(kind=kind, mode=mode, inventory=inventory), self.assertRaises(publish.Rejected):
                        publish.validate_registry(kind, VERSION, mode, inventory)

    def test_existing_version_and_downgrade_are_rejected(self):
        for kind in NAMES:
            for existing in ((VERSION,), ("3.6.0", "3.10.0")):
                with self.subTest(kind=kind, existing=existing), self.assertRaises(publish.Rejected):
                    publish.validate_registry(kind, VERSION, "trusted", registry_inventory(kind, existing))

    def test_each_crate_reads_and_accepts_only_its_own_registry_inventory(self):
        self.assertEqual(publish.REGISTRIES["rust"], "https://crates.io/api/v1/crates/maple-sdk")
        self.assertEqual(publish.REGISTRIES["proxy"], "https://crates.io/api/v1/crates/maple-proxy")
        self.assertEqual((publish.SOURCES["rust"], publish.SOURCES["proxy"]), ("sdk/rust", "proxy"))
        for kind, other in (("rust", "proxy"), ("proxy", "rust")):
            publish.validate_registry(kind, VERSION, "trusted", registry_inventory(kind))
            foreign = registry_inventory(other)
            with self.subTest(kind=kind, inventory=other), self.assertRaises(publish.Rejected):
                publish.validate_registry(kind, VERSION, "trusted", foreign)
            # Either identity field alone is enough to reject it.
            for field in ("crate", "versions"):
                mixed = registry_inventory(kind)
                mixed[field] = foreign[field]
                with self.subTest(kind=kind, foreign_field=field), self.assertRaises(publish.Rejected):
                    publish.validate_registry(kind, VERSION, "trusted", mixed)

    def test_yanked_rust_releases_still_prevent_reuse_and_downgrade(self):
        for version in (VERSION, "4.0.0"):
            inventory = registry_inventory("rust", (version,))
            inventory["versions"][0]["yanked"] = True
            with self.subTest(version=version), self.assertRaises(publish.Rejected):
                publish.validate_registry("rust", VERSION, "trusted", inventory)

    def test_prereleases_do_not_prevent_a_newer_stable_release(self):
        for kind in NAMES:
            inventory = registry_inventory(kind, ("99.0.0-beta.1", "3.6.0"))
            with self.subTest(kind=kind):
                publish.validate_registry(kind, VERSION, "trusted", inventory)

    def test_incomplete_rust_inventory_is_rejected(self):
        for field, value in (("num_versions", 2), ("versions", [1, 2])):
            inventory = registry_inventory("rust")
            inventory["crate"][field] = value
            with self.subTest(field=field), self.assertRaises(publish.Rejected):
                publish.validate_registry("rust", VERSION, "trusted", inventory)

    def test_npm_latest_must_name_a_present_stable_version(self):
        for latest in ("4.0.0", "3.6.0-beta.1"):
            inventory = registry_inventory("npm", ("3.6.0-beta.1", "3.6.0"))
            inventory["dist-tags"]["latest"] = latest
            with self.subTest(latest=latest), self.assertRaises(publish.Rejected):
                publish.validate_registry("npm", VERSION, "trusted", inventory)

    def test_registry_package_identity_and_version_records_are_checked(self):
        npm = registry_inventory("npm")
        wrong_name = copy.deepcopy(npm)
        wrong_name["name"] = "@other/sdk"
        wrong_version = copy.deepcopy(npm)
        wrong_version["versions"]["3.6.0"]["version"] = "3.5.0"
        rust = registry_inventory("rust")
        wrong_crate = copy.deepcopy(rust)
        wrong_crate["crate"]["id"] = "other-sdk"
        duplicate = copy.deepcopy(rust)
        duplicate["versions"].append(copy.deepcopy(duplicate["versions"][0]))
        for kind, inventory in (("npm", wrong_name), ("npm", wrong_version), ("rust", wrong_crate), ("rust", duplicate)):
            with self.subTest(kind=kind, inventory=inventory), self.assertRaises(publish.Rejected):
                    publish.validate_registry(kind, VERSION, "trusted", inventory)


class RegistryTransportTests(unittest.TestCase):
    def test_only_http_404_means_package_absence(self):
        for kind in NAMES:
            for status in (301, 401, 403, 404, 429, 500, 503):
                error = HTTPError("https://registry.invalid", status, "fixture", {}, io.BytesIO(b"untrusted body"))
                with self.subTest(kind=kind, status=status), patch.object(publish, "build_opener") as opener:
                    opener.return_value.open.side_effect = error
                    if status == 404:
                        self.assertIsNone(publish.fetch_inventory(kind))
                    else:
                        with self.assertRaises(publish.Rejected):
                            publish.fetch_inventory(kind)

    def test_network_failure_and_malformed_or_oversized_json_are_rejected(self):
        for kind in NAMES:
            with self.subTest(kind=kind, failure="network"), patch.object(publish, "build_opener") as opener:
                opener.return_value.open.side_effect = URLError("fixture network failure")
                with self.assertRaises(publish.Rejected):
                    publish.fetch_inventory(kind)
            for payload in (b"not json", b'{"name":"one","name":"two"}', b" " * 65):
                with self.subTest(kind=kind, payload=payload), patch.object(publish, "build_opener") as opener, patch.object(publish, "MAX_REGISTRY", 64):
                    response = opener.return_value.open.return_value.__enter__.return_value
                    response.status = 200
                    response.read.return_value = payload
                    with self.assertRaises(publish.Rejected):
                        publish.fetch_inventory(kind)

    def test_successful_inventory_preserves_registry_metadata(self):
        for kind in NAMES:
            inventory = registry_inventory(kind)
            with self.subTest(kind=kind), patch.object(publish, "build_opener") as opener:
                response = opener.return_value.open.return_value.__enter__.return_value
                response.status = 200
                response.read.return_value = json.dumps(inventory).encode()
                self.assertEqual(publish.fetch_inventory(kind), inventory)


class PackageBoundaryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def archive(self, kind, entries=None, extra_member=None):
        path = self.root / ARCHIVES[kind]
        path.write_bytes(tar_bytes(package_entries(kind) if entries is None else entries, extra_member))
        return path

    def inspect(self, kind, entries=None, extra_member=None):
        return publish.inspect_archive(self.archive(kind, entries, extra_member), kind, VERSION, source_sha=SHA)

    def context(self, kind, mode="trusted"):
        return {"kind": kind, "name": NAMES[kind], "version": VERSION, "mode": mode, "source_sha": SHA, "run_id": RUN_ID, "run_attempt": ATTEMPT}

    def bundle(self, kind):
        archive = self.archive(kind)
        manifest = {"schema_version": 1, **self.context(kind), "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(), "archive_size": archive.stat().st_size}
        self.write_manifest(manifest)
        return manifest

    def write_manifest(self, manifest):
        (self.root / "manifest.json").write_text(json.dumps(manifest))

    def verify(self, kind):
        return publish.verify_bundle(self.root, kind, VERSION, "trusted", self.context(kind))

    def test_valid_archives_report_the_exact_bytes_and_decoded_metadata(self):
        for kind in NAMES:
            with self.subTest(kind=kind):
                archive = self.archive(kind)
                result = publish.inspect_archive(archive, kind, VERSION, source_sha=SHA)
                self.assertEqual(result["archive_sha256"], hashlib.sha256(archive.read_bytes()).hexdigest())
                self.assertEqual(result["archive_size"], archive.stat().st_size)
                self.assertIsInstance(result["metadata"], dict)

    def test_traversal_absolute_and_wrong_package_paths_are_rejected(self):
        for kind in NAMES:
            for name in ("../outside", "/absolute", "package/../../outside", "other-package/file"):
                with self.subTest(kind=kind, name=name), self.assertRaises(publish.Rejected):
                    self.inspect(kind, package_entries(kind) + [(name, b"untrusted")])

    def test_links_and_special_files_are_rejected(self):
        for kind in NAMES:
            prefix = "package" if kind == "npm" else f"{NAMES[kind]}-{VERSION}"
            for entry_type in (tarfile.SYMTYPE, tarfile.LNKTYPE, tarfile.FIFOTYPE, tarfile.CHRTYPE):
                member = tarfile.TarInfo(prefix + "/unexpected")
                member.type = entry_type
                member.linkname = "../../outside"
                with self.subTest(kind=kind, entry_type=entry_type), self.assertRaises(publish.Rejected):
                    self.inspect(kind, extra_member=member)

    def test_duplicate_members_and_missing_entrypoints_are_rejected(self):
        for kind in NAMES:
            entries = package_entries(kind)
            entrypoint = "package/dist/index.d.cts" if kind == "npm" else f"{NAMES[kind]}-{VERSION}/src/lib.rs"
            for invalid in (entries + [entries[1]], [(name, data) for name, data in entries if name != entrypoint]):
                with self.subTest(kind=kind, members=[name for name, _ in invalid]), self.assertRaises(publish.Rejected):
                    self.inspect(kind, invalid)

    def test_archive_compressed_expanded_member_and_metadata_bounds(self):
        for kind in NAMES:
            for constant in ("MAX_ARCHIVE", "MAX_EXPANDED", "MAX_MEMBERS", "MAX_METADATA"):
                with self.subTest(kind=kind, bound=constant), patch.object(publish, constant, 1), self.assertRaises(publish.Rejected):
                    self.inspect(kind)

    def test_embedded_npm_identity_repository_and_json_are_checked(self):
        mutations = (
            {"name": "@other/sdk"}, {"version": "3.6.0"}, {"repository": "https://github.com/MaplePrivacyLabs/Maple"},
            {"repository": {"type": "git", "url": "git+https://github.com/attacker/Maple.git", "directory": "sdk"}},
            {"repository": {"type": "git", "url": "git+https://github.com/MaplePrivacyLabs/Maple.git", "directory": "other"}},
        )
        payloads = [json.dumps({**npm_manifest(), **mutation}).encode() for mutation in mutations] + [b"{not json}"]
        for payload in payloads:
            entries = package_entries("npm")
            entries[1] = (entries[1][0], payload)
            with self.subTest(payload=payload), self.assertRaises(publish.Rejected):
                self.inspect("npm", entries)

    def test_npm_publication_settings_cannot_redirect_or_expand_the_package(self):
        mutations = (
            {"private": True}, {"workspaces": ["packages/*"]},
            {"bundledDependencies": ["untrusted"]}, {"bundleDependencies": ["untrusted"]},
            {"publishConfig": {"registry": "https://attacker.invalid"}},
            {"publishConfig": {"access": "restricted"}}, {"publishConfig": {"tag": "next"}},
            {"publishConfig": {"provenance": False}}, {"publishConfig": {"directory": "other"}},
        )
        for mutation in mutations:
            entries = package_entries("npm")
            entries[1] = (entries[1][0], json.dumps({**npm_manifest(), **mutation}).encode())
            with self.subTest(mutation=mutation), self.assertRaises(publish.Rejected):
                self.inspect("npm", entries)

    def test_npm_exports_cannot_drift_from_source_even_with_a_matching_bundle_digest(self):
        manifest = self.bundle("npm")
        publish.verify_bundle(self.root, "npm", VERSION, "trusted", self.context("npm"), source_metadata=npm_manifest())
        packed = npm_manifest()
        packed["exports"]["."]["require"]["default"] = "./README.md"
        entries = package_entries("npm")
        entries[1] = (entries[1][0], json.dumps(packed).encode())
        archive = self.archive("npm", entries)
        manifest["archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        manifest["archive_size"] = archive.stat().st_size
        self.write_manifest(manifest)
        with self.assertRaises(publish.Rejected):
            publish.verify_bundle(self.root, "npm", VERSION, "trusted", self.context("npm"), source_metadata=npm_manifest())

    def test_rust_metadata_and_vcs_source_are_bound_to_the_request(self):
        mutations = (
            ("Cargo.toml", rust_manifest().replace(b'"maple-sdk"', b'"other-sdk"')),
            ("Cargo.toml", rust_manifest().replace(b'"3.7.0"', b'"3.6.0"')),
            ("Cargo.toml", rust_manifest().replace(b"MaplePrivacyLabs", b"attacker")),
            ("Cargo.toml", b"not = valid = toml"),
            (".cargo_vcs_info.json", json.dumps({"git": {"sha1": "b" * 40}, "path_in_vcs": "sdk/rust"}).encode()),
            (".cargo_vcs_info.json", json.dumps({"git": {"sha1": SHA}, "path_in_vcs": "other"}).encode()),
            (".cargo_vcs_info.json", json.dumps({"git": {"sha1": SHA, "dirty": True}, "path_in_vcs": "sdk/rust"}).encode()),
        )
        for suffix, payload in mutations:
            entries = [(name, payload if name.endswith("/" + suffix) else data) for name, data in package_entries("rust")]
            with self.subTest(suffix=suffix, payload=payload), self.assertRaises(publish.Rejected):
                self.inspect("rust", entries)
        entries = [(name, data) for name, data in package_entries("rust") if not name.endswith("/.cargo_vcs_info.json")]
        with self.assertRaises(publish.Rejected):
            self.inspect("rust", entries)

    def test_a_crate_cannot_stand_in_for_the_other_crate(self):
        for kind, other in (("rust", "proxy"), ("proxy", "rust")):
            with self.subTest(kind=kind, archive=other), self.assertRaises(publish.Rejected):
                self.inspect(kind, package_entries(other))
            # The right name packed from the other crate's directory is rejected too.
            entries = [(name, json.dumps({"git": {"sha1": SHA}, "path_in_vcs": CRATE_SOURCES[other]}).encode()
                        if name.endswith("/.cargo_vcs_info.json") else data)
                       for name, data in package_entries(kind)]
            with self.subTest(kind=kind, path_in_vcs=CRATE_SOURCES[other]), self.assertRaises(publish.Rejected):
                self.inspect(kind, entries)
            # So is the other crate's manifest inside an otherwise correct archive.
            entries = [(name, rust_manifest(other) if name.endswith("/Cargo.toml") else data)
                       for name, data in package_entries(kind)]
            with self.subTest(kind=kind, manifest=other), self.assertRaises(publish.Rejected):
                self.inspect(kind, entries)

    def test_malformed_or_uncompressed_archives_are_rejected(self):
        for kind in NAMES:
            for payload in (b"not an archive", b"\x1f\x8btruncated", gzip.decompress(tar_bytes(package_entries(kind)))):
                path = self.root / ARCHIVES[kind]
                path.write_bytes(payload)
                with self.subTest(kind=kind, prefix=payload[:20]), self.assertRaises(publish.Rejected):
                    publish.inspect_archive(path, kind, VERSION, source_sha=SHA)

    def test_tar_contents_hidden_after_the_first_end_marker_are_rejected(self):
        for kind in NAMES:
            first = gzip.decompress(tar_bytes(package_entries(kind)))
            hidden = gzip.decompress(tar_bytes([("../hidden", b"untrusted")]))
            archive = self.root / ARCHIVES[kind]
            archive.write_bytes(gzip.compress(first + hidden))
            with self.subTest(kind=kind), self.assertRaises(publish.Rejected):
                publish.inspect_archive(archive, kind, VERSION, source_sha=SHA)

    def test_valid_bundle_verifies_for_both_package_kinds(self):
        for kind in NAMES:
            with self.subTest(kind=kind):
                self.bundle(kind)
                self.verify(kind)
                (self.root / ARCHIVES[kind]).unlink()

    def test_bundle_creation_preserves_the_inspected_archive_bytes(self):
        for kind in NAMES:
            with self.subTest(kind=kind):
                archive = self.archive(kind)
                directory = self.root / kind
                manifest = publish.bundle_archive(directory, archive, self.context(kind))
                self.assertEqual((directory / ARCHIVES[kind]).read_bytes(), archive.read_bytes())
                self.assertEqual(publish.verify_bundle(directory, kind, VERSION, "trusted", self.context(kind)), manifest)

    def test_bundle_rejects_extra_files_and_symlinked_inputs(self):
        for kind in NAMES:
            self.bundle(kind)
            extra = self.root / "unexpected"
            extra.write_text("untrusted")
            with self.subTest(kind=kind, invalid="extra"), self.assertRaises(publish.Rejected):
                self.verify(kind)
            extra.unlink()
            for filename in (ARCHIVES[kind], "manifest.json"):
                path = self.root / filename
                contents = path.read_bytes()
                with tempfile.TemporaryDirectory() as external:
                    target = Path(external) / "target"
                    target.write_bytes(contents)
                    path.unlink()
                    path.symlink_to(target)
                    with self.subTest(kind=kind, symlink=filename), self.assertRaises(publish.Rejected):
                        self.verify(kind)
                    path.unlink()
                    path.write_bytes(contents)
            (self.root / ARCHIVES[kind]).unlink()

    def test_bundle_rejects_changed_bytes_or_size(self):
        for kind in NAMES:
            self.bundle(kind)
            archive = self.root / ARCHIVES[kind]
            archive.write_bytes(archive.read_bytes() + b"tampered")
            with self.subTest(kind=kind), self.assertRaises(publish.Rejected):
                self.verify(kind)
            archive.unlink()

    def test_bundle_manifest_schema_and_provenance_cannot_be_substituted(self):
        mutations = (
            ("schema_version", True), ("schema_version", 2), ("unknown", "untrusted"),
            ("kind", "other"), ("name", "other-sdk"), ("version", "3.6.0"), ("mode", "bootstrap"),
            ("source_sha", "b" * 40), ("source_sha", "not-a-sha"),
            ("run_id", RUN_ID + 1), ("run_id", str(RUN_ID)), ("run_id", True),
            ("run_attempt", ATTEMPT + 1), ("run_attempt", 0),
            ("archive_sha256", "b" * 64), ("archive_sha256", "g" * 64),
            ("archive_size", 1), ("archive_size", True),
        )
        for kind in NAMES:
            manifest = self.bundle(kind)
            for key, value in mutations:
                self.write_manifest({**manifest, key: value})
                with self.subTest(kind=kind, key=key, value=value), self.assertRaises(publish.Rejected):
                    self.verify(kind)
            del manifest["source_sha"]
            self.write_manifest(manifest)
            with self.subTest(kind=kind, missing="source_sha"), self.assertRaises(publish.Rejected):
                self.verify(kind)
            (self.root / ARCHIVES[kind]).unlink()

    def published_inventory(self, kind, archive):
        inventory = registry_inventory(kind, ("3.6.0", VERSION))
        if kind == "npm":
            inventory["versions"][VERSION]["dist"]["integrity"] = "sha512-" + base64.b64encode(hashlib.sha512(archive.read_bytes()).digest()).decode()
        else:
            inventory["versions"][-1]["checksum"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        return inventory

    def test_registry_confirmation_requires_exact_uploaded_bytes(self):
        for kind in NAMES:
            archive = self.archive(kind)
            inventory = self.published_inventory(kind, archive)
            with self.subTest(kind=kind):
                publish.confirm_registry(kind, VERSION, inventory, archive)
                archive.write_bytes(archive.read_bytes() + b"different bytes")
                with self.assertRaises(publish.Rejected):
                    publish.confirm_registry(kind, VERSION, inventory, archive)

    def test_registry_confirmation_distinguishes_visibility_from_invalid_release(self):
        for kind in NAMES:
            archive = self.archive(kind)
            for inventory in (None, registry_inventory(kind)):
                with self.subTest(kind=kind, inventory=inventory), self.assertRaises(publish.NotVisible):
                    publish.confirm_registry(kind, VERSION, inventory, archive)
        archive = self.archive("npm")
        inventory = self.published_inventory("npm", archive)
        inventory["dist-tags"]["latest"] = "3.6.0"
        with self.assertRaises(publish.NotVisible):
            publish.confirm_registry("npm", VERSION, inventory, archive)
        archive = self.archive("rust")
        inventory = self.published_inventory("rust", archive)
        inventory["versions"][-1]["yanked"] = True
        with self.assertRaises(publish.Rejected) as rejected:
            publish.confirm_registry("rust", VERSION, inventory, archive)
        self.assertNotIsInstance(rejected.exception, publish.NotVisible)

    def test_confirmation_waits_for_visibility_and_reports_waiting_once(self):
        for kind in NAMES:
            self.bundle(kind)
            inventory = self.published_inventory(kind, self.root / ARCHIVES[kind])
            with (self.subTest(kind=kind),
                  patch.object(publish, "fetch_inventory", side_effect=[None, None, None, inventory]) as fetch,
                  patch.object(publish.time, "sleep") as sleep,
                  patch.object(publish.time, "monotonic", return_value=0),
                  patch.object(publish, "_report") as report):
                publish.confirm_publication(self.root, kind, VERSION, "trusted", self.context(kind))
                self.assertEqual(fetch.call_count, 4)
                self.assertEqual(sleep.call_count, 3)
                self.assertTrue(all(call.args == (30 if kind == "npm" else 5,)
                                    for call in sleep.call_args_list))
                self.assertEqual(report.call_count, 2)
                self.assertIn("Waiting for registry visibility", report.call_args_list[0].args[0])
                self.assertIn("no upload retry", report.call_args_list[0].args[0])
                self.assertIn("Verified archive SHA-256", report.call_args_list[1].args[0])
            (self.root / ARCHIVES[kind]).unlink()

    def test_confirmation_has_registry_specific_read_only_retry_bounds(self):
        for kind, attempts, interval in (("npm", 41, 30), ("rust", 25, 5), ("proxy", 25, 5)):
            self.bundle(kind)
            with (self.subTest(kind=kind),
                  patch.object(publish, "fetch_inventory", return_value=None) as fetch,
                  patch.object(publish.time, "sleep") as sleep,
                  patch.object(publish.time, "monotonic", return_value=0),
                  patch.object(publish, "_report") as report):
                with self.assertRaisesRegex(publish.Rejected, "inspect registry state"):
                    publish.confirm_publication(self.root, kind, VERSION, "trusted", self.context(kind))
                self.assertEqual(fetch.call_count, attempts)
                self.assertEqual(sleep.call_count, attempts - 1)
                self.assertTrue(all(call.args == (interval,) for call in sleep.call_args_list))
                report.assert_called_once()
                self.assertNotIn("Published", report.call_args.args[0])
            (self.root / ARCHIVES[kind]).unlink()

    def test_confirmation_deadline_includes_time_spent_reading_registry(self):
        self.bundle("npm")
        with (patch.object(publish, "fetch_inventory", return_value=None) as fetch,
              patch.object(publish.time, "sleep") as sleep,
              patch.object(publish.time, "monotonic", side_effect=[0, 1201]),
              patch.object(publish, "_report")):
            with self.assertRaises(publish.Rejected):
                publish.confirm_publication(self.root, "npm", VERSION, "trusted", self.context("npm"))
            fetch.assert_called_once()
            sleep.assert_not_called()

    def test_confirmation_does_not_retry_invalid_metadata_or_network_failures(self):
        for kind in NAMES:
            self.bundle(kind)
            for failure in ({}, publish.Rejected("Cannot read package registry")):
                with (self.subTest(kind=kind, failure=failure),
                      patch.object(publish, "fetch_inventory") as fetch,
                      patch.object(publish.time, "sleep") as sleep,
                      patch.object(publish, "_report") as report):
                    if isinstance(failure, Exception):
                        fetch.side_effect = failure
                    else:
                        fetch.return_value = failure
                    with self.assertRaises(publish.Rejected):
                        publish.confirm_publication(self.root, kind, VERSION, "trusted", self.context(kind))
                    fetch.assert_called_once()
                    sleep.assert_not_called()
                    report.assert_not_called()
            (self.root / ARCHIVES[kind]).unlink()


class SourceProvenanceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / "sdk/rust").mkdir(parents=True)
        (self.root / "proxy").mkdir()
        (self.root / "sdk/package.json").write_text(json.dumps(npm_manifest()))
        (self.root / "sdk/rust/Cargo.toml").write_bytes(rust_manifest())
        (self.root / "proxy/Cargo.toml").write_bytes(rust_manifest("proxy"))

    def environment(self, kind):
        return {
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_REPOSITORY": REPOSITORY,
            "GITHUB_REPOSITORY_ID": "923138240",
            "GITHUB_REPOSITORY_OWNER_ID": "322649754",
            "GITHUB_REF": "refs/heads/master",
            "GITHUB_SHA": SHA,
            "GITHUB_WORKFLOW_REF": f"{REPOSITORY}/.github/workflows/{WORKFLOWS[kind]}@refs/heads/master",
            "GITHUB_WORKFLOW_SHA": SHA,
            "GITHUB_RUN_ID": str(RUN_ID),
            "GITHUB_RUN_ATTEMPT": str(ATTEMPT),
        }

    def validate(self, kind, environment=None, head=SHA, dirty="", mode="trusted"):
        def git_result(arguments, **kwargs):
            if "rev-parse" in arguments:
                return subprocess.CompletedProcess(arguments, 0, stdout=head + "\n", stderr="")
            if "status" in arguments:
                return subprocess.CompletedProcess(arguments, 0, stdout=dirty, stderr="")
            raise AssertionError(f"Unexpected subprocess invocation: {arguments}")

        with patch.object(publish.subprocess, "run", side_effect=git_result):
            return publish.validate_source(kind, VERSION, mode, root=self.root, environ=self.environment(kind) if environment is None else environment)

    def test_canonical_dispatch_returns_the_bound_source_and_run(self):
        for kind in NAMES:
            for mode in ("bootstrap", "trusted"):
                with self.subTest(kind=kind, mode=mode):
                    context = self.validate(kind, mode=mode)
                    self.assertEqual(context, {"kind": kind, "name": NAMES[kind], "version": VERSION, "mode": mode, "source_sha": SHA, "run_id": RUN_ID, "run_attempt": ATTEMPT})

    def test_wrong_event_fork_ref_workflow_identity_and_run_are_rejected(self):
        mutations = (
            ("GITHUB_EVENT_NAME", "pull_request_target"), ("GITHUB_REPOSITORY", "attacker/Maple"),
            ("GITHUB_REPOSITORY_ID", "1"), ("GITHUB_REPOSITORY_OWNER_ID", "1"),
            ("GITHUB_REF", "refs/heads/feature"), ("GITHUB_SHA", "b" * 40),
            ("GITHUB_WORKFLOW_REF", f"{REPOSITORY}/.github/workflows/other.yml@refs/heads/master"),
            ("GITHUB_WORKFLOW_REF", f"{REPOSITORY}/.github/workflows/sdk-publish-npm.yml@refs/heads/feature"),
            ("GITHUB_WORKFLOW_SHA", "b" * 40), ("GITHUB_RUN_ID", "0"), ("GITHUB_RUN_ATTEMPT", "-1"),
        )
        for kind in NAMES:
            for key, value in mutations:
                environment = {**self.environment(kind), key: value}
                with self.subTest(kind=kind, key=key, value=value), self.assertRaises(publish.Rejected):
                    self.validate(kind, environment)

    def test_workflow_sha_explicit_context_is_required_to_match(self):
        for kind in NAMES:
            environment = self.environment(kind)
            environment["SDK_PUBLISH_WORKFLOW_SHA"] = environment.pop("GITHUB_WORKFLOW_SHA")
            with self.subTest(kind=kind):
                self.validate(kind, environment)
                environment["GITHUB_WORKFLOW_SHA"] = "b" * 40
                with self.assertRaises(publish.Rejected):
                    self.validate(kind, environment)
                del environment["GITHUB_WORKFLOW_SHA"]
                del environment["SDK_PUBLISH_WORKFLOW_SHA"]
                with self.assertRaises(publish.Rejected):
                    self.validate(kind, environment)

    def test_dirty_checkout_and_mismatched_package_version_are_rejected(self):
        for kind in NAMES:
            with self.subTest(kind=kind), self.assertRaises(publish.Rejected):
                self.validate(kind, dirty=" M sdk/package.json\n")
        package = npm_manifest()
        package["version"] = "3.6.0"
        (self.root / "sdk/package.json").write_text(json.dumps(package))
        with self.assertRaises(publish.Rejected):
            self.validate("npm")
        (self.root / "sdk/rust/Cargo.toml").write_bytes(rust_manifest().replace(b'"3.7.0"', b'"3.6.0"'))
        with self.assertRaises(publish.Rejected):
            self.validate("rust")
        # Each crate is bound to its own manifest: the SDK's stale version above
        # does not block the proxy, and the proxy's does not pass on the SDK's.
        self.validate("proxy")
        (self.root / "sdk/rust/Cargo.toml").write_bytes(rust_manifest())
        (self.root / "proxy/Cargo.toml").write_bytes(rust_manifest("proxy").replace(b'"3.7.0"', b'"3.6.0"'))
        self.validate("rust")
        with self.assertRaises(publish.Rejected):
            self.validate("proxy")

    def test_the_proxy_crate_publishes_only_through_the_rust_workflow(self):
        self.assertEqual(publish.WORKFLOWS["proxy"], publish.WORKFLOWS["rust"])
        environment = self.environment("proxy")
        environment["GITHUB_WORKFLOW_REF"] = f"{REPOSITORY}/.github/workflows/proxy-publish.yml@refs/heads/master"
        with self.assertRaises(publish.Rejected):
            self.validate("proxy", environment)


if __name__ == "__main__":
    unittest.main()
