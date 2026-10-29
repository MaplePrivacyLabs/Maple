"""Offline public-delivery qualification with two synthetic Agent releases.

Signatures are structural fixtures and the minisign process is an injected
boundary. Every HTTP request is captured by an in-memory transport.
"""

import base64
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit

from release_helpers.public import NoRedirect, PublicVerifier


SPEC = importlib.util.spec_from_file_location("public_fixture_publisher", Path(__file__).with_name("publisher.py"))
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


class FixtureContext:
    def __init__(self, root):
        self.root, self.repo, self.core = root, publisher.ROOT, publisher
        self.source, self.version, self.build = "b" * 40, "7.8.10", "103"
        self.producer_run, self.producer_attempt = 110, 2
        self.identity = {"source_sha": self.source, "version": self.version, "build": self.build,
                         "run_id": self.producer_run, "run_attempt": self.producer_attempt,
                         "pub_date": "2026-01-02T03:04:05Z"}
        self.bindings_sha256, self.policy_sha256 = "c" * 64, "d" * 64
        self.core_sha256, self.tooling_sha256 = "e" * 64, "f" * 64
        self.publication = {"run_id": 120, "run_attempt": 3, "source_sha": self.source,
                            "workflow": ".github/workflows/agent-updates-publish.yml"}
        self.bindings = {"schema": "agent-release-bindings-v1", "publication": self.publication,
                         "previous": {"version": "7.8.9", "build": "102", "source_sha": "a" * 40,
                                      "publisher_run": 100, "publisher_attempt": 1}}

    def digest(self, data):
        return hashlib.sha256(data).hexdigest()

    def read(self, path, maximum=4 * 1024 * 1024):
        path = Path(path)
        if not path.is_absolute() or path != path.resolve():
            raise ValueError("unsafe fixture path")
        data = path.read_bytes()
        if len(data) > maximum:
            raise ValueError("oversized fixture file")
        return data

    def pinned(self, reference, maximum=4 * 1024 * 1024):
        data = self.read(reference["path"], maximum)
        if self.digest(data) != reference["sha256"]:
            raise ValueError("pinned fixture changed")
        return data

    def emit(self, path, value):
        with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as target:
            target.write(publisher.encode_json(value))
            target.flush()
            os.fsync(target.fileno())

    def tool(self, name):
        if name != "minisign":
            raise ValueError("unexpected fixture tool")
        return self.root / "pinned-minisign"

    def pin_bytes(self, name, data):
        path = self.root / name
        path.write_bytes(data)
        return {"path": str(path), "sha256": self.digest(data)}

    def pin(self, name, value):
        return self.pin_bytes(name, publisher.encode_json(value))


class Response(io.BytesIO):
    def __init__(self, body=b"", status=200, headers=None):
        super().__init__(body)
        self.status, self.headers = status, headers or {}


class RecordedTransport:
    def __init__(self, fixture):
        self.fixture, self.calls = fixture, []
        self.envelope_etag_override = {}
        self.payload_etag_override = {}
        self.alias_override = {}
        self.encoding = None
        self.extra_body = b""
        self.allow_advance = False
        self.tag_depth = 0

    def open(self, request, timeout):
        f = self.fixture
        headers = {k.lower(): v for k, v in request.header_items()}
        self.calls.append({"url": request.full_url, "method": request.get_method(), "headers": headers, "timeout": timeout})
        parts, method = urlsplit(request.full_url), request.get_method()
        if parts.netloc == "api.github.com":
            return self.api(parts.path.removeprefix("/repos/MaplePrivacyLabs/Maple/"), parts.query)
        if parts.netloc not in PublicVerifier.HOSTS:
            raise AssertionError("unexpected network origin")
        path = parts.path
        if path in f.envelope_urls:
            data = f.envelope_urls[path]
            immutable = "/releases/" in path
            result_headers = {"Content-Length": str(len(data)), "Content-Type": "application/json",
                              "Cache-Control": PublicVerifier.IMMUTABLE_CACHE if immutable else "no-store"}
            if immutable:
                result_headers["ETag"] = self.envelope_etag_override.get(request.full_url, f.etag(request.full_url))
            else:
                result_headers["CDN-Cache-Control"] = "no-store"
            if self.encoding:
                result_headers["Content-Encoding"] = self.encoding
            return Response(b"" if method == "HEAD" else data + self.extra_body, headers=result_headers)
        if path in f.payloads:
            data = f.payloads[path]
            result_headers = {"Content-Length": str(len(data)), "Cache-Control": PublicVerifier.IMMUTABLE_CACHE,
                              "Accept-Ranges": "bytes", "ETag": self.payload_etag_override.get((request.full_url, method, headers.get("range")), f.etag(request.full_url))}
            if method == "HEAD":
                return Response(headers=result_headers)
            if headers.get("range") == "bytes=0-1023":
                result_headers.update({"Content-Range": f"bytes 0-1023/{len(data)}", "Content-Length": "1024"})
                return Response(data[:1024], status=206, headers=result_headers)
            return Response(data, headers=result_headers)
        if path.startswith("/download/agent/stable/"):
            platform = path.rsplit("/", 1)[1]
            manifest = f.current[platform]["manifest"]
            key = {"macos": "macos", "linux": "linux-bundle", "windows": "windows"}[platform]
            location = manifest["downloads"][key]["path"]
            return self.alias(request.full_url, location)
        if path == "/latest.json":
            data = f.research_metadata
            return Response(data, headers={"Content-Length": str(len(data)), "ETag": f.etag(request.full_url), "Cache-Control": "no-store"})
        if path.startswith("/download/research/stable/"):
            key = path.rsplit("/", 1)[1]
            return self.alias(request.full_url, f.research_url(key))
        raise AssertionError("unexpected public request: " + request.full_url)

    def alias(self, url, location):
        return Response(status=302, headers={"Cache-Control": "no-store", "CDN-Cache-Control": "no-store",
                                             "Location": self.alias_override.get(url, location)})

    def api(self, path, query):
        f = self.fixture
        if not self.allow_advance:
            return Response(b"{}", status=404)
        if path == "releases/latest":
            names = ["latest.json", *f.research_names().values()]
            assets = [{"id": i + 1, "name": name, "size": len(f.research_metadata) if name == "latest.json" else 100,
                       "state": "uploaded", "digest": "sha256:" + (f.ctx.digest(f.research_metadata) if name == "latest.json" else "0" * 64),
                       "browser_download_url": "https://github.com/MaplePrivacyLabs/Maple/releases/download/v" + f.research_version + "/" + name}
                      for i, name in enumerate(names)]
            data = {"id": 200, "tag_name": "v" + f.research_version, "draft": False, "prerelease": False, "assets": assets}
        elif path == "git/ref/tags/v" + f.research_version:
            data = {"object": {"type": "tag" if self.tag_depth else "commit", "sha": "1" * 40 if self.tag_depth else "9" * 40}}
        elif path.startswith("git/tags/"):
            index = int(path.rsplit("/", 1)[1][0])
            data = {"object": {"type": "tag" if index < self.tag_depth else "commit",
                               "sha": str(index + 1) * 40 if index < self.tag_depth else "9" * 40}}
        elif path == "actions/workflows/release.yml/runs" and query == "event=release&per_page=30":
            data = {"workflow_runs": [{"id": 300, "path": ".github/workflows/release.yml", "event": "release",
                                       "status": "completed", "conclusion": "success", "head_branch": "v" + f.research_version,
                                       "head_sha": "9" * 40, "head_repository": {"full_name": "MaplePrivacyLabs/Maple"}}]}
        else:
            raise AssertionError("unexpected Research evidence API")
        return Response(publisher.encode_json(data))


class PublicFixture:
    def __init__(self, root, revision_length=7):
        self.ctx = FixtureContext(root)
        self.revision_length = revision_length
        self.research_version = "5.6.7"
        self.research_metadata = publisher.encode_json({"version": self.research_version})
        self.old = self.bundle("previous", "7.8.9", "102", "a" * 40)
        self.current = self.bundle("current", self.ctx.version, self.ctx.build, self.ctx.source)
        self.envelope_urls, self.payloads = {}, {}
        for platform in PublicVerifier.PLATFORMS:
            for records in (self.old, self.current):
                record = records[platform]
                self.envelope_urls[f"/agent/stable/releases/{record['manifest']['version']}/{record['name']}"] = record["envelope"]
            record = self.current[platform]
            pointer = {"macos": "latest.json", "linux": "linux/latest.json", "windows": "windows/latest.json"}[platform]
            self.envelope_urls["/agent/stable/" + pointer] = record["envelope"]
            self.payloads.update(record["payloads"])
        prepared = root / "current-prepared"
        key = publisher.decode_json((publisher.ROOT / "apps/maple-agent/release-profiles.json").read_bytes())["prod"]["update_public_keys"][0]
        (prepared / "public-key.pub").write_text(key + "\n", encoding="ascii")
        files = {name: {"sha256": self.ctx.digest(path.read_bytes()), "size": path.stat().st_size}
                 for path in prepared.rglob("*") if path.is_file() for name in [str(path.relative_to(prepared))]}
        producer = {"run_id": self.ctx.producer_run, "run_attempt": self.ctx.producer_attempt, "source_sha": self.ctx.source,
                    "version": self.ctx.version, "build": self.ctx.build, "pub_date": "2026-01-02T03:04:05Z"}
        self.freeze = {"schema": "agent-release-prepared-freeze-v1", "source": self.ctx.source,
                       "platforms": list(PublicVerifier.PLATFORMS), "producer": producer, "files": files}
        proof = {"schema": "agent-release-bundle-proof-v1", "stage": "signed", "publication": self.ctx.publication,
                 "tooling_sha256": self.ctx.tooling_sha256, "policy_sha256": self.ctx.policy_sha256,
                 "core_sha256": self.ctx.core_sha256,
                 "producer": producer, "verified_primary_signatures": ["envelope.json", "linux-envelope.json", "windows-envelope.json"],
                 "files": {r["envelope_name"]: {"sha256": self.ctx.digest(r["envelope"])} for r in self.current.values()}}
        baseline_requests = []
        for host in PublicVerifier.HOSTS:
            for record in self.old.values():
                url = f"https://{host}/agent/stable/releases/7.8.9/{record['name']}"
                baseline_requests.append({"url": url, "method": "GET", "check_passed": True,
                                          "body_sha256": self.ctx.digest(record["envelope"]), "headers": {"etag": self.etag(url)}})
        research_url = "https://updates.trymaple.ai/latest.json"
        research_record = {"url": research_url, "method": "GET", "check_passed": True,
                           "body_sha256": self.ctx.digest(self.research_metadata), "headers": {"etag": self.etag(research_url)}}
        baseline_requests.append(research_record)
        for key in PublicVerifier.RESEARCH_KEYS:
            for method in ("GET", "HEAD"):
                baseline_requests.append({"url": "https://updates.trymaple.ai/download/research/stable/" + key, "method": method,
                                          "check_passed": True, "headers": {"location": self.research_url(key)}})
        envelope_hashes = {p: self.ctx.digest(r["envelope"]) for p, r in self.old.items()}
        baseline = {"schema": "agent-release-public-postpublication-v1", "all_checks_passed": True, "version": "7.8.9",
                    "build": "102", "source": "a" * 40, "envelope_sha256": envelope_hashes,
                    "publication": {"run_id": 100, "run_attempt": 1, "source_sha": "c" * 40},
                    "research": {"version": self.research_version}, "requests": baseline_requests}
        public_ref = self.ctx.pin("previous-public.json", baseline)
        acceptance = {"schema": "agent-release-root-acceptance-v1", "release_published": True, "version": "7.8.9",
                      "build": "102", "source": "a" * 40, "publisher_run": 100, "publisher_attempt": 1,
                      "public_receipt_sha256": public_ref["sha256"], "envelopes": envelope_hashes}
        self.ctx.bindings.update({"prepared_bundle": str(prepared), "signed_bundle": str(root / "current-signed"),
                                 "public_key": str(prepared / "public-key.pub"), "freeze": self.ctx.pin("freeze.json", self.freeze),
                                 "signed_stage_proof": self.ctx.pin("signed-proof.json", proof),
                                 "envelope_sha256": {p: self.ctx.digest(r["envelope"]) for p, r in self.current.items()},
                                 "successful_publication_run": self.ctx.pin("publication.json", {"id": 120, "run_attempt": 3,
                                     "head_sha": self.ctx.source, "head_branch": "master", "path": ".github/workflows/agent-updates-publish.yml",
                                     "status": "completed", "conclusion": "success"}),
                                 "previous_public": public_ref, "previous_root_acceptance": self.ctx.pin("acceptance.json", acceptance),
                                 "previous_signed_envelopes": {f"stable/releases/7.8.9/{r['name']}": self.ctx.pin_bytes("previous-" + r["envelope_name"], r["envelope"]) for r in self.old.values()},
                                 "research_preservation": self.ctx.pin("research.json", {"passed": True, "request_count": 7, "writes": 0, "requests": [research_record]})})
        self.transport = RecordedTransport(self)

    def etag(self, url):
        return '"' + hashlib.sha256(url.encode()).hexdigest()[:20] + '"'

    def research_names(self):
        v = self.research_version
        return {"macos": f"Maple_{v}_universal.dmg", "windows": f"Maple_{v}_x64-setup.exe",
                "linux-appimage": f"Maple_{v}_amd64.AppImage", "linux-deb": f"Maple_{v}_amd64.deb",
                "linux-rpm": f"Maple-{v}-1.x86_64.rpm", "android": "app-universal-release.apk"}

    def research_url(self, key):
        return "https://github.com/MaplePrivacyLabs/Maple/releases/download/v" + self.research_version + "/" + self.research_names()[key]

    def bundle(self, label, version, build, source):
        prepared, signed = self.ctx.root / (label + "-prepared"), self.ctx.root / (label + "-signed")
        prepared.mkdir()
        signed.mkdir()
        (prepared / "payloads").mkdir()
        result = {}
        for platform, format_version, name, suffixes in (
            ("macos", "v1", "manifest.json", ("macos-aarch64.app.tar.gz", "macos-universal.dmg")),
            ("linux", "v2", "linux-manifest.json", ("linux-x86_64.tar.gz",)),
            ("windows", "v3", "windows-manifest.json", ("windows-x86_64.tar.gz", "windows-x86_64.exe")),
        ):
            payloads, artifacts = {}, []
            for i, suffix in enumerate(suffixes):
                filename = f"maple-agent-prod-{version}-{source[:self.revision_length]}-{suffix}"
                data = (platform.encode() + bytes([i])) * 350
                path = publisher.public_path(version, filename)
                (prepared / "payloads" / filename).write_bytes(data)
                payloads[path] = data
                artifacts.append({"path": path, "size": len(data), "sha256": self.ctx.digest(data)})
            artifact_key = {"macos": "darwin-aarch64", "linux": "linux-x86_64", "windows": "windows-x86_64"}[platform]
            download_key = {"macos": "macos", "linux": "linux-bundle", "windows": "windows"}[platform]
            manifest = {"format": "maple-agent-update-" + format_version, "product": "maple-agent", "channel": "stable",
                        "version": version, "build": build, "source_sha": source, "pub_date": "2026-01-02T03:04:05Z",
                        "notes": "Synthetic offline release", "artifacts": {artifact_key: artifacts[0]}, "downloads": {download_key: artifacts[-1]}}
            inner = publisher.encode_json(manifest)
            publisher.validate_manifest(inner)
            signature = "\n".join(("untrusted comment: structural fixture", base64.b64encode(b"ED" + bytes(72)).decode(),
                                    "trusted comment: " + publisher.trusted_comment(version), base64.b64encode(bytes(64)).decode(), ""))
            envelope = publisher.encode_json({"manifest": inner.decode(), "signature": signature})
            envelope_name = name.replace("manifest", "envelope")
            (prepared / name).write_bytes(inner)
            (signed / envelope_name).write_bytes(envelope)
            result[platform] = {"manifest": manifest, "envelope": envelope, "name": name,
                                "envelope_name": envelope_name, "payloads": payloads}
        return result


class PublicVerifierTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.fixture = PublicFixture(self.root)
        self.verifier = PublicVerifier(self.fixture.ctx)
        self.verifier.opener = self.fixture.transport
        self.signatures = []

    def minisign(self, args, **kwargs):
        self.assertEqual(Path(args[0]), self.root / "pinned-minisign")
        message = Path(args[args.index("-m") + 1]).read_bytes()
        self.assertTrue(Path(args[args.index("-x") + 1]).is_file())
        self.assertEqual(args[-2:], ["-P", self.verifier.primary])
        self.signatures.append(publisher.validate_manifest(message)["version"])
        return SimpleNamespace(returncode=0)

    def run_live(self, output="public"):
        with patch("release_helpers.public.subprocess.run", side_effect=self.minisign):
            return self.verifier.run(SimpleNamespace(execute_live=True, output=self.root / output))

    def receipt(self, output="public"):
        return json.loads((self.root / output / "receipt.json").read_bytes())

    def repin(self, binding, change):
        reference = self.fixture.ctx.bindings[binding]
        value = json.loads(Path(reference["path"]).read_bytes())
        change(value)
        Path(reference["path"]).write_bytes(publisher.encode_json(value))
        reference["sha256"] = self.fixture.ctx.digest(Path(reference["path"]).read_bytes())

    def test_default_is_no_network_and_creates_no_output(self):
        summary = self.verifier.run(SimpleNamespace(execute_live=False, output=None))
        self.assertEqual(summary["status"], "prepared_only_no_http")
        self.assertEqual(self.fixture.transport.calls, [])
        self.assertEqual(summary["fixed_requests"], 97)
        self.assertEqual(summary["maximum_requests"], 104)

    def test_output_outside_owned_state_fails_before_http(self):
        with self.assertRaisesRegex(ValueError, "owned state"):
            self.verifier.run(SimpleNamespace(execute_live=True, output=self.root.parent / (self.root.name + "-outside")))
        self.assertEqual(self.fixture.transport.calls, [])

    def test_two_bound_versions_urls_qa_headers_matrix_and_no_payload_copies(self):
        summary = self.run_live()
        self.assertTrue(summary["all_checks_passed"], summary)
        receipt = self.receipt()
        self.assertEqual(receipt["schema"], "agent-release-public-postpublication-v1")
        self.assertEqual((receipt["fixed_request_count"], receipt["total_request_count"], receipt["full_payload_streams"]), (97, 97, 10))
        self.assertEqual(sorted(self.signatures), ["7.8.10"] * 3 + ["7.8.9"] * 3)
        self.assertEqual(set(p.name for p in (self.root / "public").iterdir()), {"receipt.json"})
        calls = self.fixture.transport.calls
        self.assertEqual(len(calls), 97)
        for call in calls:
            self.assertEqual(call["headers"]["x-maple-download-probe"], "1")
            self.assertEqual(call["headers"]["accept-encoding"], "identity")
            self.assertEqual(call["timeout"], 45)
        for host in PublicVerifier.HOSTS:
            host_calls = [r for r in calls if urlsplit(r["url"]).netloc == host]
            old = [r for r in host_calls if "/releases/7.8.9/" in r["url"]]
            current = [r for r in host_calls if "/releases/7.8.10/" in r["url"]]
            self.assertEqual(len(old), 3)
            self.assertEqual(len(current), 18)
            aliases = [r for r in host_calls if "/download/agent/" in r["url"]]
            self.assertEqual({(r["method"], r["headers"]["user-agent"]) for r in aliases},
                             {("GET", PublicVerifier.NATIVE), ("HEAD", PublicVerifier.NATIVE), ("GET", PublicVerifier.CURL), ("HEAD", PublicVerifier.CURL)})
        self.assertEqual(receipt["research"]["version"], "5.6.7")

    def test_previous_own_url_etag_cannot_be_replaced_by_other_host_etag(self):
        path = "/agent/stable/releases/7.8.9/manifest.json"
        url = "https://updates.maple.ai" + path
        self.fixture.transport.envelope_etag_override[url] = self.fixture.etag("https://updates.trymaple.ai" + path)
        self.assertFalse(self.run_live()["all_checks_passed"])
        failure = next(r for r in self.receipt()["requests"] if r["url"] == url)
        self.assertIn("own ETag", failure["check_error"])

    def test_canonical_longer_git_abbreviation_is_supported(self):
        root = self.root / "longer-revision"
        root.mkdir()
        self.fixture = PublicFixture(root, revision_length=9)
        self.verifier = PublicVerifier(self.fixture.ctx)
        self.verifier.opener = self.fixture.transport
        with patch("release_helpers.public.subprocess.run", return_value=SimpleNamespace(returncode=0)):
            summary = self.verifier.run(SimpleNamespace(execute_live=True, output=root / "public"))
        self.assertTrue(summary["all_checks_passed"], summary)

    def test_freeze_date_must_match_bound_producer(self):
        self.repin("freeze", lambda value: value["producer"].update(pub_date="2026-01-03T03:04:05Z"))
        with self.assertRaisesRegex(ValueError, "frozen producer identity"):
            self.run_live()
        self.assertEqual(self.fixture.transport.calls, [])

    def test_payload_etags_must_match_across_head_range_and_full(self):
        url = "https://updates.maple.ai" + next(iter(self.fixture.payloads))
        self.fixture.transport.payload_etag_override[(url, "GET", "bytes=0-1023")] = '"changed"'
        self.assertFalse(self.run_live()["all_checks_passed"])
        failures = [r for r in self.receipt()["requests"] if r["url"] == url]
        self.assertEqual(len(failures), 3)
        self.assertTrue(all(not r["check_passed"] for r in failures))

    def test_unknown_research_advance_fails_closed(self):
        self.fixture.research_version = "5.6.8"
        self.fixture.research_metadata = publisher.encode_json({"version": "5.6.8"})
        summary = self.run_live()
        self.assertFalse(summary["all_checks_passed"])
        self.assertEqual(summary["research"]["state"], "requires_review")
        self.assertTrue(self.receipt()["agent_checks_passed"])
        self.assertEqual(summary["request_count"], 98)

    def test_verified_research_advance_at_maximum_budget_is_allowed(self):
        self.fixture.research_version = "5.6.8"
        self.fixture.research_metadata = publisher.encode_json({"version": "5.6.8"})
        self.fixture.transport.allow_advance, self.fixture.transport.tag_depth = True, 4
        summary = self.run_live()
        self.assertTrue(summary["all_checks_passed"], summary)
        self.assertEqual(summary["request_count"], 104)
        self.assertEqual(summary["research"]["state"], "independent_release_verified")
        evidence = json.loads((self.root / "public" / "research-independent-release.json").read_bytes())
        self.assertEqual(evidence["successful_release_run_ids"], [300])
        self.assertEqual(len(evidence["api_requests"]), 7)

    def test_legacy_schemas_require_all_bound_identities_and_hash_pins(self):
        self.repin("previous_public", lambda value: value.update(schema="agent013-all-platforms-public-postpublication-v1"))
        public_ref = self.fixture.ctx.bindings["previous_public"]
        self.repin("previous_root_acceptance", lambda value: value.update(schema="agent-013-root-acceptance-v1", public_receipt_sha256=public_ref["sha256"]))
        self.assertTrue(self.run_live()["all_checks_passed"])

    def test_wrong_legacy_identity_is_rejected_before_network(self):
        self.repin("previous_public", lambda value: value.update(schema="agent014-all-platforms-public-postpublication-v1", build="999"))
        with self.assertRaisesRegex(ValueError, "baseline"):
            self.run_live()
        self.assertEqual(self.fixture.transport.calls, [])

    def test_failed_primary_signature_stops_before_http(self):
        with patch("release_helpers.public.subprocess.run", return_value=SimpleNamespace(returncode=1)):
            with self.assertRaisesRegex(ValueError, "primary minisign"):
                self.verifier.run(SimpleNamespace(execute_live=True, output=self.root / "public"))
        self.assertEqual(self.fixture.transport.calls, [])

    def test_unchanged_research_cannot_change_alias(self):
        url = "https://updates.trymaple.ai/download/research/stable/macos"
        self.fixture.transport.alias_override[url] = self.fixture.research_url("windows")
        summary = self.run_live()
        self.assertFalse(summary["all_checks_passed"])
        self.assertIn("Research route changed", summary["research"]["error"])

    def test_transport_encoding_bounds_and_no_redirect_policy(self):
        self.fixture.transport.encoding = "gzip"
        self.assertFalse(self.run_live()["all_checks_passed"])
        self.assertTrue(any("encoding" in r.get("check_error", "") for r in self.receipt()["requests"]))
        self.verifier.enabled = True
        self.verifier.deadline = float("inf")
        self.fixture.transport.encoding = None
        url = "https://updates.maple.ai/agent/stable/latest.json"
        record, _, _ = self.verifier.fetch(url, limit=10)
        self.assertIn("response exceeds exact bound", record["error"])
        self.assertIsNone(next(h for h in PublicVerifier(self.fixture.ctx).opener.handlers if isinstance(h, NoRedirect)).redirect_request(None))

    def test_allowlist_and_request_budget_fail_before_transport(self):
        self.verifier.enabled, self.verifier.deadline = True, float("inf")
        for url in ("http://updates.maple.ai/latest.json", "https://updates.maple.ai.evil/latest.json",
                    "https://updates.maple.ai/latest.json?token=bad", "https://api.github.com/repos/other/repo/releases/latest",
                    "https://api.github.com/repos/MaplePrivacyLabs/Maple/issues"):
            with self.subTest(url=url), self.assertRaisesRegex(ValueError, "unapproved|query"):
                self.verifier.fetch(url)
        self.verifier.requests = 104
        with self.assertRaisesRegex(ValueError, "budget"):
            self.verifier.fetch("https://updates.maple.ai/latest.json")
        self.assertEqual(self.fixture.transport.calls, [])


if __name__ == "__main__":
    unittest.main()
