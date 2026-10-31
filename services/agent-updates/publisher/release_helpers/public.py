"""Bounded public qualification of the bound all-platform Agent publication.

This verifier uses no credentials, follows no redirects, writes no payload
copies, and executes no downloaded application. The default performs no HTTP.
"""

import concurrent.futures
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class PublicVerifier:
    HOSTS = ("updates.maple.ai", "updates.trymaple.ai")
    NATIVE, CURL = "Maple-Agent-Updater/1", "curl/8.7.1"
    SELECT_HEADERS = ("Content-Type", "Content-Length", "Content-Encoding",
                      "Content-Range", "ETag", "Cache-Control", "CDN-Cache-Control",
                      "Location", "Accept-Ranges", "CF-Ray")
    RESEARCH_KEYS = ("macos", "windows", "linux-appimage", "linux-deb", "linux-rpm", "android")
    PLATFORMS = ("macos", "linux", "windows")
    ARM_PLATFORM = "windows-arm64"
    MANIFEST_NAMES = {"macos": "manifest.json", "linux": "linux-manifest.json",
                      "windows": "windows-manifest.json", "windows-arm64": "windows-arm64-manifest.json"}
    DOWNLOAD_KEYS = {"macos": "macos", "linux": "linux-bundle", "windows": "windows",
                     "windows-arm64": "windows-arm64"}
    FIXED_REQUESTS, MAX_REQUESTS, FULL_STREAMS = 97, 104, 10
    IMMUTABLE_CACHE = "public, max-age=31536000, immutable"

    def __init__(self, ctx):
        self.ctx = ctx
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        self.state_lock = threading.Lock()
        self.requests = 0
        self.deadline = None
        self.enabled = False
        inventory = self.inventory(ctx.bindings)
        self.platforms, self.previous_platforms = inventory["platforms"], inventory["previous_platforms"]
        self.FULL_STREAMS = inventory["full_streams"]
        self.FIXED_REQUESTS = inventory["fixed_requests"]
        self.MAX_REQUESTS = inventory["max_requests"]

    @classmethod
    def inventory(cls, bindings):
        platforms = bindings.get("platforms")
        if platforms not in (list(cls.PLATFORMS), [*cls.PLATFORMS, cls.ARM_PLATFORM]):
            raise ValueError("public verification requires the complete selected platform inventory")
        platforms = tuple(platforms)
        references = bindings.get("previous_signed_envelopes")
        if not isinstance(references, dict):
            raise ValueError("public verification requires the previous signed envelope inventory")
        previous_names = {key.rsplit("/", 1)[-1] for key in references}
        if previous_names not in ({cls.MANIFEST_NAMES[p] for p in cls.PLATFORMS}, set(cls.MANIFEST_NAMES.values())):
            raise ValueError("public verification requires the complete previous signed envelope inventory")
        previous_platforms = cls.PLATFORMS + ((cls.ARM_PLATFORM,) if
                             cls.MANIFEST_NAMES[cls.ARM_PLATFORM] in previous_names else ())
        if not set(previous_platforms) <= set(platforms):
            raise ValueError("previous platform may not be dropped")
        payload_count = 5 + (2 if cls.ARM_PLATFORM in platforms else 0)
        # Per host: four current metadata requests and four aliases per selected
        # platform, one immutable per previous platform, and HEAD/range/full per
        # payload. Research retains its independent fixed thirteen requests.
        fixed = 2 * (8 * len(platforms) + len(previous_platforms) + 3 * payload_count) + 13
        return {"platforms": platforms, "previous_platforms": previous_platforms,
                "fixed_requests": fixed, "full_streams": 2 * payload_count, "max_requests": fixed + 7}

    @staticmethod
    def add_arguments(parser):
        parser.add_argument("--execute-live", action="store_true")
        parser.add_argument("--output", type=Path)

    def need(self, ok, reason):
        if not ok:
            raise ValueError(reason)

    def sha(self, data):
        return hashlib.sha256(data).hexdigest()

    def unique_pairs(self, items):
        result = {}
        for key, value in items:
            self.need(key not in result, "duplicate JSON field")
            result[key] = value
        return result

    def plain_json(self, data):
        return json.loads(data, object_pairs_hook=self.unique_pairs)

    def schema(self, actual, generic, legacy):
        return actual == generic or (isinstance(actual, str) and re.fullmatch(legacy, actual) is not None)

    def identity_matches(self, manifest, identity):
        return all(manifest[name] == identity[name] for name in ("version", "build", "source_sha"))

    def verify_envelope(self, data, platform, expected_inner):
        envelope, manifest = self.ctx.core.parse_envelope(data)
        inner = envelope["manifest"].encode("utf-8")
        self.need(inner == expected_inner, "signed manifest differs from frozen exact bytes")
        self.need(manifest["format"] == {"macos": "maple-agent-update-v1",
                  "linux": "maple-agent-update-v2", "windows": "maple-agent-update-v3",
                  "windows-arm64": "maple-agent-update-v4"}[platform],
                  "wrong platform envelope")
        with tempfile.TemporaryDirectory(prefix="agent-public-signature-") as temporary:
            message, signature = Path(temporary) / "manifest.json", Path(temporary) / "signature.minisig"
            message.write_bytes(inner)
            signature.write_text(envelope["signature"], encoding="utf-8")
            checked = subprocess.run([str(self.ctx.tool("minisign")), "-V", "-H", "-q",
                                      "-m", str(message), "-x", str(signature), "-P", self.primary],
                                     stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        self.need(checked.returncode == 0, "primary minisign verification failed")
        return manifest

    def setup(self):
        bindings, core = self.ctx.bindings, self.ctx.core
        self.need(bindings["schema"] == "agent-release-bindings-v1", "wrong public bindings schema")
        self.publication = self.ctx.publication
        success = self.plain_json(self.ctx.pinned(bindings["successful_publication_run"]))
        self.need(success["id"] == self.publication["run_id"] and
                  success["run_attempt"] == self.publication["run_attempt"] and
                  success["head_sha"] == self.publication["source_sha"] and
                  success["head_branch"] == "master" and
                  success["path"] == ".github/workflows/agent-updates-publish.yml" and
                  success["status"] == "completed" and success["conclusion"] == "success",
                  "publication has not actually succeeded")
        freeze = self.plain_json(self.ctx.pinned(bindings["freeze"]))
        self.freeze = freeze
        self.need(freeze["schema"] == "agent-release-prepared-freeze-v1" and
                  freeze["source"] == self.ctx.source and freeze["platforms"] == list(self.platforms) and
                  freeze["producer"]["run_id"] == self.ctx.producer_run and
                  freeze["producer"]["run_attempt"] == self.ctx.producer_attempt and
                  freeze["producer"]["build"] == self.ctx.build,
                  "included producer/freeze differs")
        producer = self.ctx.identity
        self.need(all(freeze["producer"][name] == value for name, value in producer.items()),
                  "frozen producer identity differs from bindings")
        proof = self.plain_json(self.ctx.pinned(bindings["signed_stage_proof"]))
        self.need(proof["schema"] == "agent-release-bundle-proof-v1" and proof["stage"] == "signed" and
                  proof["publication"] == self.publication and proof["producer"] == producer and
                  proof["tooling_sha256"] == self.ctx.tooling_sha256 and
                  proof["policy_sha256"] == self.ctx.policy_sha256 and
                  proof["core_sha256"] == self.ctx.core_sha256 and
                  proof["verified_primary_signatures"] == sorted(self.MANIFEST_NAMES[p].replace("manifest", "envelope")
                                                                  for p in self.platforms),
                  "actual signed-stage proof differs")
        prepared, bundle = Path(bindings["prepared_bundle"]), Path(bindings["signed_bundle"])
        for directory in (prepared, bundle):
            self.need(directory.is_absolute() and directory == directory.resolve() and directory.is_dir()
                      and not directory.is_symlink(), "unsafe prepared or signed bundle")
        key_path = Path(bindings["public_key"])
        key = self.ctx.read(key_path)
        self.need(self.sha(key) == freeze["files"]["public-key.pub"]["sha256"], "frozen primary key changed")
        self.primary = core.public_key_text(key_path)
        profile = core.decode_json(self.ctx.read(core.ROOT / "apps/maple-agent/release-profiles.json"))
        self.need(self.primary == profile["prod"]["update_public_keys"][0], "public verifier requires the trusted primary key")
        arm = {"windows_arm64": True} if self.ARM_PLATFORM in self.platforms else {}
        core.validate_pins(self.primary, profile, profile, linux=True, windows=True, **arm)
        self.signed, self.manifests, self.publications = {}, {}, {}
        for publication in core.bundle_manifests(prepared):
            platform = {name: p for p, name in self.MANIFEST_NAMES.items()}[publication["name"]]
            name, envelope_name, inner = publication["name"], publication["envelope_name"], publication["data"]
            data = self.ctx.read(bundle / envelope_name)
            self.need(self.sha(data) == bindings["envelope_sha256"][platform] == proof["files"][envelope_name]["sha256"],
                      "actual signed envelope pin differs")
            self.need(self.sha(inner) == freeze["files"][name]["sha256"], "frozen unsigned manifest changed")
            manifest = self.verify_envelope(data, platform, inner)
            self.need(self.identity_matches(manifest, producer) and manifest["pub_date"] == producer["pub_date"],
                      "signed release identity differs")
            self.signed[platform], self.manifests[platform] = data, manifest
            self.publications[platform] = publication
        self.need(set(self.signed) == set(self.platforms), "all selected platforms are required")
        self.load_previous()
        research = self.plain_json(self.ctx.pinned(bindings["research_preservation"]))
        self.need(research["passed"] is True and research["request_count"] == 7 and research["writes"] == 0,
                  "Research preservation receipt differs")
        self.recent_research = research
        self.payloads = {a["path"]: a for manifest in self.manifests.values()
                         for group in ("artifacts", "downloads") for a in manifest[group].values()}
        self.need(len(self.payloads) == self.FULL_STREAMS // 2, "selected platform payload inventory differs")
        self.prefixes = {}
        for path, artifact in self.payloads.items():
            filename = path.rsplit("/", 1)[-1]
            stem = re.match(r"/agent/stable/releases/" + re.escape(self.ctx.version) +
                            r"/maple-agent-prod-" + re.escape(self.ctx.version) + r"-([0-9a-f]{7,40})-", path)
            self.need(stem and self.ctx.source.startswith(stem[1]) and
                      freeze["files"]["payloads/" + filename] == {"sha256": artifact["sha256"], "size": artifact["size"]},
                      "payload differs from frozen release")
            local = prepared / "payloads" / filename
            self.need(local == local.resolve() and not local.is_symlink(), "unsafe local prefix path")
            with os.fdopen(os.open(local, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as source:
                info = os.fstat(source.fileno())
                self.need(stat.S_ISREG(info.st_mode) and info.st_size == artifact["size"], "local payload size changed")
                self.prefixes[path] = source.read(1024)

    def load_previous(self):
        bindings = self.ctx.bindings
        previous = bindings["previous"]
        self.previous_identity = previous
        baseline = self.plain_json(self.ctx.pinned(bindings["previous_public"]))
        self.need(self.schema(baseline["schema"], "agent-release-public-postpublication-v1",
                             r"agent[0-9]+-all-platforms-public-postpublication-v1") and
                  baseline["all_checks_passed"] is True and baseline["version"] == previous["version"] and
                  baseline["build"] == previous["build"] and baseline["source"] == previous["source_sha"] and
                  baseline["publication"]["run_id"] == previous["publisher_run"] and
                  baseline["publication"]["run_attempt"] == previous["publisher_attempt"],
                  "accepted previous public baseline failed")
        acceptance = self.plain_json(self.ctx.pinned(bindings["previous_root_acceptance"]))
        self.need(self.schema(acceptance["schema"], "agent-release-root-acceptance-v1",
                             r"agent-?[0-9]+-root-acceptance-v1") and
                  acceptance["release_published"] is True and acceptance["version"] == previous["version"] and
                  acceptance["build"] == previous["build"] and acceptance["source"] == previous["source_sha"] and
                  acceptance["publisher_run"] == previous["publisher_run"] and
                  acceptance["publisher_attempt"] == previous["publisher_attempt"] and
                  acceptance["public_receipt_sha256"] == bindings["previous_public"]["sha256"] and
                  acceptance["envelopes"] == baseline["envelope_sha256"],
                  "accepted previous all-platform authority differs")
        self.baseline, self.old, self.old_manifests = baseline, {}, {}
        self.need(set(baseline["envelope_sha256"]) == set(self.previous_platforms),
                  "accepted previous platform inventory differs")
        expected_keys = {f"stable/releases/{previous['version']}/{self.MANIFEST_NAMES[p]}"
                         for p in self.previous_platforms}
        self.need(set(bindings["previous_signed_envelopes"]) == expected_keys, "previous envelope inventory differs")
        for platform in self.previous_platforms:
            name = self.publications[platform]["name"]
            reference = bindings["previous_signed_envelopes"][f"stable/releases/{previous['version']}/{name}"]
            data = self.ctx.pinned(reference)
            self.need(self.sha(data) == baseline["envelope_sha256"][platform], "accepted previous envelope changed")
            envelope, _ = self.ctx.core.parse_envelope(data)
            manifest = self.verify_envelope(data, platform, envelope["manifest"].encode("utf-8"))
            self.need(self.identity_matches(manifest, previous), "accepted previous identity differs")
            self.old[platform], self.old_manifests[platform] = data, manifest

    def build_tasks(self):
        tasks = []
        for host in self.HOSTS:
            origin = "https://" + host
            for platform in self.platforms:
                publication = self.publications[platform]
                for method, ua in (("GET", self.NATIVE), ("GET", self.CURL), ("HEAD", self.NATIVE)):
                    tasks.append(dict(kind="current-envelope", platform=platform,
                                      url=origin + "/agent/" + publication["pointer"], method=method, ua=ua))
                tasks.append(dict(kind="current-envelope", platform=platform, immutable=True,
                                  url=origin + f"/agent/stable/releases/{self.ctx.version}/" + publication["name"]))
                if platform in self.previous_platforms:
                    tasks.append(dict(kind="previous-envelope", platform=platform,
                                      url=origin + f"/agent/stable/releases/{self.previous_identity['version']}/" + publication["name"]))
            for platform in self.platforms:
                key = self.DOWNLOAD_KEYS[platform]
                for ua in (self.NATIVE, self.CURL):
                    for method in ("GET", "HEAD"):
                        tasks.append(dict(kind="agent-alias", url=origin + "/download/agent/stable/" + platform,
                                          method=method, ua=ua, expected_location=self.manifests[platform]["downloads"][key]["path"]))
            for path, artifact in self.payloads.items():
                for mode in ("head", "range", "full"):
                    tasks.append(dict(kind="payload-" + mode, url=origin + path, path=path, artifact=artifact))
        tasks.append(dict(kind="research-latest", url="https://updates.trymaple.ai/latest.json", ua=self.CURL))
        for key in self.RESEARCH_KEYS:
            for method in ("GET", "HEAD"):
                tasks.append(dict(kind="research-alias", key=key,
                                  url="https://updates.trymaple.ai/download/research/stable/" + key, method=method, ua=self.CURL))
        self.need(len(tasks) == self.FIXED_REQUESTS and
                  sum(t["kind"] == "payload-full" for t in tasks) == self.FULL_STREAMS,
                  "fixed public request inventory changed")
        return tasks

    def approved_url(self, url):
        parsed = urllib.parse.urlsplit(url)
        self.need(parsed.scheme == "https" and parsed.netloc in (*self.HOSTS, "api.github.com") and
                  not parsed.username and not parsed.fragment, "unapproved URL")
        if parsed.netloc == "api.github.com":
            prefix = "/repos/MaplePrivacyLabs/Maple/"
            tail = parsed.path.removeprefix(prefix)
            allowed = (tail == "releases/latest" and not parsed.query or
                       re.fullmatch(r"git/ref/tags/v[0-9]+\.[0-9]+\.[0-9]+", tail) and not parsed.query or
                       re.fullmatch(r"git/tags/[0-9a-f]{40}", tail) and not parsed.query or
                       tail == "actions/workflows/release.yml/runs" and parsed.query == "event=release&per_page=30")
            self.need(parsed.path.startswith(prefix) and allowed, "unapproved Research evidence URL")
        else:
            self.need(not parsed.query, "unexpected public query")
        return parsed

    def fetch(self, url, method="GET", ua=NATIVE, limit=262144, range_header=None, full=False):
        self.need(self.enabled, "public verifier is disabled")
        self.need(method in {"GET", "HEAD"}, "public verifier allows only GET and HEAD")
        parsed = self.approved_url(url)
        with self.state_lock:
            self.requests += 1
            self.need(self.requests <= self.MAX_REQUESTS, "fixed public request budget exceeded")
        self.need(time.monotonic() < self.deadline, "public check deadline exceeded")
        headers = {"User-Agent": ua, "Accept-Encoding": "identity", "X-Maple-Download-Probe": "1"}
        if parsed.netloc == "api.github.com":
            headers.update({"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"})
        if range_header:
            headers["Range"] = range_header
        request = urllib.request.Request(url, method=method, headers=headers)
        record = {"url": url, "method": method, "user_agent": ua, "range": range_header,
                  "full_payload_stream": full, "analytics_probe_header": "1"}
        body, first = bytearray(), bytearray()
        try:
            try:
                response = self.opener.open(request, timeout=45)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                record.update(status=response.status, headers={key.lower(): response.headers.get(key)
                              for key in self.SELECT_HEADERS if response.headers.get(key) is not None})
                self.need(record["headers"].get("content-encoding", "identity") == "identity", "unexpected encoding")
                digest, size = hashlib.sha256(), 0
                if method != "HEAD":
                    while True:
                        self.need(time.monotonic() < self.deadline, "public stream deadline exceeded")
                        chunk = response.read(min(65536, limit + 1 - size))
                        if not chunk:
                            break
                        size += len(chunk)
                        self.need(size <= limit, "response exceeds exact bound")
                        digest.update(chunk)
                        first.extend(chunk[:max(0, 1024 - len(first))])
                        if not full:
                            body.extend(chunk)
                record.update(bytes_read=size, body_sha256=digest.hexdigest(), first_1024_sha256=self.sha(first))
        except Exception as error:
            record["error"] = str(error)
        return record, bytes(body), bytes(first)

    def previous(self, url, method="GET"):
        matches = [r for r in self.baseline["requests"] if r["url"] == url and r["method"] == method]
        self.need(matches, "no accepted own-URL predecessor baseline")
        self.need(all(r.get("check_passed") is True for r in matches), "failed predecessor baseline request")
        return matches[0]

    def execute(self, task):
        kind = task["kind"]
        method = "HEAD" if kind == "payload-head" else task.get("method", "GET")
        limit = task["artifact"]["size"] if kind == "payload-full" else 1024 if kind == "payload-range" else 262144
        try:
            record, body, first = self.fetch(task["url"], method=method, ua=task.get("ua", self.NATIVE), limit=limit,
                                             range_header="bytes=0-1023" if kind == "payload-range" else None,
                                             full=kind == "payload-full")
        except Exception as error:
            return {"kind": kind, "url": task["url"], "method": method, "analytics_probe_header": "1",
                    "check_passed": False, "error": str(error)}
        record.update(kind=kind)
        if "key" in task:
            record["key"] = task["key"]
        try:
            self.need("error" not in record, record.get("error", "HTTP failed"))
            headers = record["headers"]
            if kind in ("current-envelope", "previous-envelope"):
                expected = self.signed[task["platform"]] if kind == "current-envelope" else self.old[task["platform"]]
                self.need(record["status"] == 200 and headers.get("content-length") == str(len(expected)) and
                          (method == "HEAD" or body == expected), "public signed envelope bytes/size differ")
                immutable = "/releases/" in task["url"]
                self.need(headers.get("cache-control") == (self.IMMUTABLE_CACHE if immutable else "no-store"),
                          "metadata cache policy differs")
                if not immutable:
                    self.need(headers.get("cdn-cache-control") == "no-store", "mutable feed CDN policy differs")
                if kind == "previous-envelope":
                    accepted = self.previous(task["url"])
                    self.need(self.sha(expected) == accepted["body_sha256"] and headers.get("etag") == accepted["headers"].get("etag"),
                              "previous metadata bytes/own ETag changed")
            elif kind in ("agent-alias", "research-alias"):
                self.need(record["status"] == 302 and headers.get("cache-control") == "no-store" and
                          headers.get("cdn-cache-control") == "no-store" and not body, "alias status/cache/body differs")
                if kind == "agent-alias":
                    self.need(headers.get("location") == task["expected_location"], "Agent host-relative alias differs")
                else:
                    self.need(headers.get("location", "").startswith("https://github.com/MaplePrivacyLabs/Maple/releases/download/v"),
                              "Research alias origin differs")
            elif kind.startswith("payload-"):
                artifact = task["artifact"]
                self.need(headers.get("cache-control") == self.IMMUTABLE_CACHE and
                          headers.get("accept-ranges") == "bytes" and headers.get("etag"), "payload cache/range/ETag policy differs")
                if kind == "payload-head":
                    self.need(record["status"] == 200 and headers.get("content-length") == str(artifact["size"]) and
                              record["bytes_read"] == 0, "payload HEAD size differs")
                elif kind == "payload-range":
                    self.need(record["status"] == 206 and headers.get("content-range") == "bytes 0-1023/" + str(artifact["size"]) and
                              headers.get("content-length") == "1024" and body == self.prefixes[task["path"]], "payload range differs")
                else:
                    self.need(record["status"] == 200 and record["bytes_read"] == artifact["size"] and
                              record["body_sha256"] == artifact["sha256"] and first == self.prefixes[task["path"]], "payload full hash/size differs")
            elif kind == "research-latest":
                self.need(record["status"] == 200, "Research metadata unavailable")
                record["research_version"] = self.plain_json(body)["version"]
                self.research_metadata = body
            record["check_passed"] = True
        except Exception as error:
            record.update(check_passed=False, check_error=str(error))
        return record

    def research_api(self, path, evidence):
        record, body, _ = self.fetch("https://api.github.com/repos/MaplePrivacyLabs/Maple/" + path,
                                     ua=self.CURL, limit=2097152)
        evidence.append(record)
        self.need("error" not in record and record["status"] == 200, "independent Research public API lookup failed")
        return self.plain_json(body)

    def research_asset(self, release, name, tag):
        matches = [a for a in release["assets"] if a["name"] == name]
        self.need(len(matches) == 1, "Research asset identity is ambiguous")
        asset = matches[0]
        self.need(type(asset["id"]) is int and asset["id"] > 0 and type(asset["size"]) is int and
                  asset["size"] > 0 and asset["state"] == "uploaded" and
                  re.fullmatch(r"sha256:[0-9a-f]{64}", asset["digest"]) and
                  asset["browser_download_url"] == "https://github.com/MaplePrivacyLabs/Maple/releases/download/" + tag + "/" + name,
                  "invalid Research uploaded asset")
        return asset

    def research_changed_evidence(self, metadata, records):
        version = self.plain_json(metadata)["version"]
        self.need(isinstance(version, str) and re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version), "not a stable Research version")
        evidence = []
        release = self.research_api("releases/latest", evidence)
        tag = "v" + version
        self.need(release["tag_name"] == tag and release["draft"] is False and release["prerelease"] is False and
                  type(release["id"]) is int and release["id"] > 0, "Research metadata does not match latest stable GitHub release")
        latest = self.research_asset(release, "latest.json", tag)
        self.need(latest["size"] == len(metadata) <= 65536 and latest["digest"] == "sha256:" + self.sha(metadata),
                  "Research metadata bytes differ from released asset digest")
        expected = {"macos": f"Maple_{version}_universal.dmg", "windows": f"Maple_{version}_x64-setup.exe",
                    "linux-appimage": f"Maple_{version}_amd64.AppImage", "linux-deb": f"Maple_{version}_amd64.deb",
                    "android": "app-universal-release.apk"}
        rpms = [a["name"] for a in release["assets"] if re.fullmatch(r"Maple-" + re.escape(version) + r"-[1-9][0-9]*\.x86_64\.rpm", a["name"])]
        self.need(len(rpms) == 1, "ambiguous Research RPM")
        expected["linux-rpm"] = rpms[0]
        for key, name in expected.items():
            asset = self.research_asset(release, name, tag)
            aliases = [r for r in records if r.get("kind") == "research-alias" and r["key"] == key]
            self.need(len(aliases) == 2 and all(r.get("status") == 302 and r["headers"].get("location") == asset["browser_download_url"] for r in aliases),
                      "Research alias differs from independent release asset")
        obj = self.research_api("git/ref/tags/" + tag, evidence)["object"]
        for _ in range(4):
            if obj["type"] == "commit":
                break
            self.need(obj["type"] == "tag" and re.fullmatch(r"[0-9a-f]{40}", obj["sha"]), "unexpected Research tag type")
            obj = self.research_api("git/tags/" + obj["sha"], evidence)["object"]
        self.need(obj["type"] == "commit" and re.fullmatch(r"[0-9a-f]{40}", obj["sha"]), "unresolved Research tag")
        runs = self.research_api("actions/workflows/release.yml/runs?event=release&per_page=30", evidence)
        matches = [r for r in runs["workflow_runs"] if r.get("path") == ".github/workflows/release.yml" and
                   r.get("event") == "release" and r.get("status") == "completed" and r.get("conclusion") == "success" and
                   r.get("head_branch") == tag and r.get("head_sha") == obj["sha"] and
                   r.get("head_repository", {}).get("full_name") == "MaplePrivacyLabs/Maple" and type(r.get("id")) is int and r["id"] > 0]
        self.need(matches, "no successful Research Release run matches independent tag+commit")
        summary = {"state": "independent_release_verified", "version": version, "release_id": release["id"],
                   "tag": tag, "source_sha": obj["sha"], "successful_release_run_ids": [r["id"] for r in matches],
                   "metadata_sha256": self.sha(metadata), "api_requests": evidence}
        self.ctx.emit(self.output / "research-independent-release.json", summary)
        return summary

    def qualify_research(self, records):
        try:
            current = next(r for r in records if r["kind"] == "research-latest")
            self.need(current["check_passed"], "Research metadata request failed")
            original = self.previous(current["url"])
            recent = next(r for r in self.recent_research["requests"] if r["url"] == current["url"])
            self.need(original["body_sha256"] == recent["body_sha256"], "accepted Research baselines disagree")
            if current["body_sha256"] == original["body_sha256"]:
                previous_version = self.baseline["research"]["version"]
                self.need(current["research_version"] == previous_version and
                          current["headers"].get("etag") == original["headers"].get("etag"), "unchanged Research own ETag differs")
                for record in records:
                    if record["kind"] == "research-alias":
                        self.need(record["headers"].get("location") == self.previous(record["url"], record["method"])["headers"].get("location"),
                                  "Research route changed with unchanged metadata")
                return {"state": "unchanged", "version": previous_version, "metadata_sha256": current["body_sha256"]}
            return self.research_changed_evidence(self.research_metadata, records)
        except Exception as error:
            return {"state": "requires_review", "error": str(error)}

    def run(self, args):
        if not args.execute_live:
            return {"status": "prepared_only_no_http", "source": self.ctx.source, "version": self.ctx.version,
                    "build": self.ctx.build, "producer": self.ctx.producer_run,
                    "producer_attempt": self.ctx.producer_attempt, "fixed_requests": self.FIXED_REQUESTS,
                    "maximum_requests": self.MAX_REQUESTS, "full_payload_streams": self.FULL_STREAMS}
        self.output = Path(args.output) if args.output is not None else None
        self.need(self.output and self.output.is_absolute() and not self.output.exists() and
                  not self.output.is_symlink() and self.output.parent == self.output.parent.resolve() and
                  self.output.is_relative_to(self.ctx.root), "fresh absolute output in owned state required")
        self.output.mkdir(mode=0o700)
        self.setup()
        tasks = self.build_tasks()
        self.requests, self.deadline, self.enabled = 0, time.monotonic() + 900, True
        records = []
        started = datetime.datetime.now(datetime.timezone.utc).isoformat()
        try:
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                for future in concurrent.futures.as_completed([pool.submit(self.execute, task) for task in tasks]):
                    records.append(future.result())
            research = self.qualify_research(records)
        finally:
            self.enabled = False
        for path in self.payloads:
            for host in self.HOSTS:
                same = [r for r in records if r["url"] == "https://" + host + path]
                if all(r["check_passed"] for r in same) and len({r["headers"].get("etag") for r in same}) != 1:
                    for record in same:
                        record.update(check_passed=False, check_error="one payload URL changed ETag across HEAD/range/full")
        agent_passed = all(r["check_passed"] for r in records if not r["kind"].startswith("research"))
        research_passed = all(r["check_passed"] for r in records if r["kind"].startswith("research")) and research["state"] in ("unchanged", "independent_release_verified")
        bindings = self.ctx.bindings
        receipt = {"schema": "agent-release-public-postpublication-v1", "started_at": started,
                   "completed_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                   "source": self.ctx.source, "version": self.ctx.version, "build": self.ctx.build,
                   "producer_run": self.ctx.producer_run, "producer_attempt": self.ctx.producer_attempt,
                   "publication": self.publication, "bindings_sha256": self.ctx.bindings_sha256,
                   "policy_sha256": self.ctx.policy_sha256, "core_sha256": self.ctx.core_sha256,
                   "tooling_sha256": self.ctx.tooling_sha256,
                   "freeze_sha256": bindings["freeze"]["sha256"], "signed_stage_proof": bindings["signed_stage_proof"],
                   "envelope_sha256": bindings["envelope_sha256"], "all_checks_passed": agent_passed and research_passed,
                   "agent_checks_passed": agent_passed, "research_checks_passed": research_passed, "research": research,
                   "fixed_request_count": len(records), "total_request_count": self.requests,
                   "full_payload_streams": sum(r["kind"] == "payload-full" for r in records),
                   "successful_full_payload_streams": sum(r["kind"] == "payload-full" and r["check_passed"] for r in records),
                   "duplicate_full_payload_files_saved": 0, "analytics_probe_header": {"X-Maple-Download-Probe": "1"},
                   "included_platforms": list(self.platforms), "preserved_previous_immutables": list(self.previous_platforms),
                   "previous": self.previous_identity, "previous_public_sha256": bindings["previous_public"]["sha256"],
                   "credentials_used": False, "redirects_followed": False, "producer_attestations_verified_here": False,
                   "requests": records,
                   "scope": "Exact signed local/public bytes and delivery only; no app execution, device/install/update lifecycle or announcement."}
        self.need(len(records) == self.FIXED_REQUESTS and receipt["full_payload_streams"] == self.FULL_STREAMS,
                  "public task inventory incomplete")
        self.ctx.emit(self.output / "receipt.json", receipt)
        return {"receipt": str(self.output / "receipt.json"),
                "receipt_sha256": self.sha(self.ctx.read(self.output / "receipt.json")),
                "all_checks_passed": receipt["all_checks_passed"], "request_count": self.requests,
                "full_payload_streams": receipt["full_payload_streams"], "research": research,
                "failures": [r for r in records if not r["check_passed"]]}
