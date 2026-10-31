"""Hash-bound release facts and private operator policy; neither is executable code."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import math
import os
from datetime import datetime
from pathlib import Path
import re
import stat
import subprocess
from urllib.parse import urlsplit

REPOSITORY = "MaplePrivacyLabs/Maple"
WORKFLOW = ".github/workflows/agent-updates-publish.yml"
DESKTOP_WORKFLOW = ".github/workflows/agent-desktop-build.yml"
PLATFORMS = ("macos", "linux", "windows")
ALL_PLATFORMS = (*PLATFORMS, "windows-arm64")
TARGETS = {"macos": "macos-aarch64", "linux": "linux-x86_64", "windows": "windows-x86_64",
           "windows-arm64": "windows-aarch64"}
MANIFESTS = dict(zip(ALL_PLATFORMS, ("manifest.json", "linux-manifest.json", "windows-manifest.json",
                                  "windows-arm64-manifest.json")))
ENVELOPES = dict(zip(ALL_PLATFORMS, ("envelope.json", "linux-envelope.json", "windows-envelope.json",
                                  "windows-arm64-envelope.json")))
POINTERS = dict(zip(ALL_PLATFORMS, ("stable/latest.json", "stable/linux/latest.json", "stable/windows/latest.json",
                                 "stable/windows-arm64/latest.json")))
SCHEMA_BINDINGS = "agent-release-bindings-v1"
SCHEMA_FREEZE = "agent-release-prepared-freeze-v1"
SCHEMA_BUNDLE = "agent-release-bundle-proof-v1"
SCHEMA_R2 = "agent-release-r2-proof-v1"
SCHEMA_DOWNLOAD = "agent-release-stage-download-v1"
SCHEMA_PUBLIC = "agent-release-public-postpublication-v1"
CODE_PATHS = (
    ".github/workflows/agent-updates-publish.yml",
    "services/agent-updates/publisher/publisher.py",
    "apps/maple-agent/release-profiles.json",
    "apps/maple-agent/scripts/release-info.py",
)


class OperatorError(RuntimeError):
    """A bounded diagnostic which does not include transport or credential output."""


def need(ok, message):
    if not ok:
        raise OperatorError(message)


def selected_platforms(value):
    """Existing snapshots retain three platforms; new releases include ARM64."""
    need(isinstance(value, (list, tuple)) and tuple(value) in (PLATFORMS, ALL_PLATFORMS),
         "release must include the exact ordered platform inventory")
    return tuple(value)


def manifest_names(platforms):
    return tuple(MANIFESTS[name] for name in selected_platforms(platforms))


def envelope_names(platforms):
    return tuple(ENVELOPES[name] for name in selected_platforms(platforms))


def pointer_names(platforms):
    return tuple(POINTERS[name] for name in selected_platforms(platforms))


def predecessor_platforms(bindings):
    refs = bindings.get("previous_signed_envelopes")
    need(isinstance(refs, dict), "predecessor immutable inventory differs")
    prefix = f"stable/releases/{bindings['previous']['version']}/"
    for platforms in (PLATFORMS, ALL_PLATFORMS):
        if set(refs) == {prefix + name for name in manifest_names(platforms)}:
            return platforms
    raise OperatorError("predecessor immutable inventory differs")


def platform_counts(platforms):
    arm = "windows-arm64" in selected_platforms(platforms)
    return {"prepared": 23 + 6 * arm, "reserved": 24 + 6 * arm, "signed": 27 + 7 * arm}


def r2_request_count(bindings):
    current, previous = selected_platforms(bindings["platforms"]), predecessor_platforms(bindings)
    need(set(previous) <= set(current), "release cannot remove an accepted platform")
    return 3 * len(current) + len(previous) + 1


def sha(data):
    return hashlib.sha256(data).hexdigest()


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result, "duplicate JSON key")
        result[key] = value
    return result


def decode(data):
    try:
        return json.loads(data, object_pairs_hook=_pairs, parse_float=_finite_float,
                          parse_constant=lambda _: (_ for _ in ()).throw(OperatorError("non-finite JSON")))
    except (ValueError, UnicodeError, RecursionError) as error:
        raise OperatorError("invalid JSON document") from error


def _finite_float(value):
    result = float(value)
    need(math.isfinite(result), "non-finite JSON")
    return result


def read_regular(path, maximum=4 * 1024**2):
    path = Path(path)
    need(path.is_absolute() and path.resolve(strict=True) == path, "noncanonical local path")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as handle:
        need(stat.S_ISREG(os.fstat(handle.fileno()).st_mode), "not a regular local file")
        raw = handle.read(maximum + 1)
    need(len(raw) <= maximum, "local file exceeds bound")
    return raw


def positive(value):
    return type(value) is int and 0 < value < 2**64


def digest(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def source(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None


def version(value):
    return (isinstance(value, str) and len(value) <= 64
            and re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value) is not None
            and all(int(part) < 2**64 for part in value.split('.')))


def build(value):
    return isinstance(value, str) and re.fullmatch(r"[1-9][0-9]{0,17}", value) is not None


def reference(value):
    need(isinstance(value, dict) and set(value) == {"path", "sha256"}
         and isinstance(value["path"], str) and Path(value["path"]).is_absolute()
         and digest(value["sha256"]), "invalid pinned reference")
    return value


def tooling_files(repo):
    publisher = repo / "services/agent-updates/publisher"
    files = [publisher / "agent_release.py", publisher / "publisher.py",
             repo / "apps/maple-agent/release-profiles.json"]
    files += sorted((publisher / "release_helpers").glob("*.py"))
    return {str(path.relative_to(repo)): sha(read_regular(path)) for path in files}


def tooling_digest(repo):
    return sha(json.dumps(tooling_files(repo), sort_keys=True, separators=(",", ":")).encode())


class Context:
    """One immutable input snapshot and one separately pinned private policy.

    Later facts (freeze, publication selection, signed proof, success) produce a
    new binding snapshot, never an edit to executable helper code.
    """

    repository = REPOSITORY
    workflow = WORKFLOW
    desktop_workflow = DESKTOP_WORKFLOW

    def __init__(self, root, bindings_path, bindings_sha256, policy_path, policy_sha256):
        self.root = Path(root)
        info = self.root.lstat()
        need(self.root.is_absolute() and self.root.resolve(strict=True) == self.root
             and stat.S_ISDIR(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o700
             and info.st_uid == os.getuid(), "state directory must be owned, canonical and private0700")
        need(digest(bindings_sha256) and digest(policy_sha256), "expected input digests required")
        self.bindings_path = Path(bindings_path)
        self.bindings_sha256, self.policy_sha256 = bindings_sha256, policy_sha256
        raw = read_regular(self.bindings_path)
        need(sha(raw) == bindings_sha256, "release bindings changed")
        self.bindings = decode(raw)
        policy_raw = read_regular(Path(policy_path))
        need(sha(policy_raw) == policy_sha256, "operator policy changed")
        self.policy = decode(policy_raw)
        self.validate_policy()
        self.validate_bindings()
        self.repo = Path(self.bindings["repo"])
        expected_repo = Path(__file__).resolve().parents[4]
        need(self.repo == expected_repo, "tools must run from their bound trusted checkout")
        self.tooling_sha256 = tooling_digest(self.repo)
        need(self.bindings["tooling_sha256"] == self.tooling_sha256, "release tooling changed during operation")
        self.code_pins = self.bindings["code_pins"]
        need(set(self.code_pins) == set(CODE_PATHS), "publisher code pin inventory differs")
        for name, expected in self.code_pins.items():
            need(digest(expected) and sha(read_regular(self.repo / name)) == expected,
                 "trusted publisher input changed")
        self.verify_code_revision(self.source)
        if self.bindings.get("publication_identity"):
            self.verify_code_revision(self.publication["source_sha"])
        core_path = self.repo / "services/agent-updates/publisher/publisher.py"
        self.core_sha256 = self.code_pins["services/agent-updates/publisher/publisher.py"]
        self.profile_sha256 = self.code_pins["apps/maple-agent/release-profiles.json"]
        # The module path is fixed by this installed source, never supplied as
        # an executable file in the release data or private policy.
        spec = importlib.util.spec_from_file_location("agent_release_canonical_publisher", core_path)
        self.core = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.core)
        profiles = decode(read_regular(self.repo / "apps/maple-agent/release-profiles.json"))
        self.primary = profiles["prod"]["update_public_keys"][0]
        self.primary_sha256 = sha((self.primary + "\n").encode("ascii"))
        need(self.core.public_key_text(self.bindings["public_key"]) == self.primary,
             "release public key is not the compiled primary")
        self.core.validate_pins(self.primary, profiles, profiles, linux=True, windows=True,
                                **({"windows_arm64": True} if "windows-arm64" in self.platforms else {}))

    def validate_policy(self):
        p = self.policy
        need(isinstance(p, dict) and set(p) == {"schema", "environment", "r2", "tools"}
             and p["schema"] == "agent-release-policy-v1", "invalid operator policy schema")
        env = p["environment"]
        need(isinstance(env, dict) and set(env) == {"id", "name", "reviewer_id"}
             and positive(env["id"]) and positive(env["reviewer_id"])
             and env["name"] == "updates-production", "invalid protected environment policy")
        self.environment_id, self.reviewer_id, self.envname = env["id"], env["reviewer_id"], env["name"]
        r2 = p["r2"]
        need(isinstance(r2, dict) and set(r2) == {"endpoint", "bucket", "route"}, "invalid R2 policy")
        need(isinstance(r2["endpoint"], str), "invalid fixed R2 origin")
        try:
            url = urlsplit(r2["endpoint"])
            port = url.port
        except ValueError as error:
            raise OperatorError("invalid fixed R2 origin") from error
        need(url.scheme == "https" and url.username is None and url.password is None and port is None
             and url.netloc == url.hostname
             and url.path in {"", "/"} and not url.query and not url.fragment
             and re.fullmatch(r"[0-9a-f]{32}\.r2\.cloudflarestorage\.com", url.hostname or ""),
             "invalid fixed R2 origin")
        need(isinstance(r2["bucket"], str) and re.fullmatch(r"[a-z0-9][a-z0-9-]{2,62}", r2["bucket"]),
             "invalid R2 bucket")
        reference(r2["route"])
        need(isinstance(p["tools"], dict) and set(p["tools"]) == {"gh", "minisign"}, "invalid tool roles")
        for tool in p["tools"].values():
            reference(tool)

    def validate_bindings(self):
        b = self.bindings
        need(isinstance(b, dict) and b.get("schema") == SCHEMA_BINDINGS, "invalid release binding schema")
        need(b.get("policy_sha256") == self.policy_sha256, "release uses a different operator policy")
        need("UNBOUND" not in json.dumps(b), "release bindings contain unbound placeholders")
        platforms = selected_platforms(b.get("platforms"))
        identity = b.get("producer_identity")
        need(isinstance(identity, dict) and set(identity) == {"run_id", "run_attempt", "source_sha", "version", "build", "pub_date"},
             "invalid producer identity")
        need(positive(identity["run_id"]) and positive(identity["run_attempt"]) and source(identity["source_sha"])
             and build(identity["build"]),
             "invalid producer identity fields")
        need(version(identity["version"]),
             "invalid stable version")
        need(isinstance(identity["pub_date"], str) and re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", identity["pub_date"]),
             "invalid producer date")
        try:
            datetime.strptime(identity["pub_date"], "%Y-%m-%dT%H:%M:%SZ")
        except ValueError as error:
            raise OperatorError("invalid producer date") from error
        need(source(b.get("payload_tree")), "invalid payload tree")
        need(set(b.get("artifacts", {})) == set(platforms), "missing exact final package records")
        for platform, item in b["artifacts"].items():
            need(set(item) == {"id", "name", "size", "zip_sha256"} and positive(item["id"])
                 and type(item["size"]) is int and 0 < item["size"] <= 512 * 1024**2
                 and digest(item["zip_sha256"])
                 and item["name"] == f"maple-agent-prod-{TARGETS[platform]}-{identity['run_id']}",
                 "invalid selected artifact record")
        for key in ("qualification", "notes", "previous_private_r2", "previous_root_acceptance", "previous_public", "research_preservation"):
            reference(b[key])
        need(isinstance(b.get("previous"), dict) and set(b["previous"]) == {"version", "build", "source_sha", "publisher_run", "publisher_attempt"},
             "invalid predecessor identity")
        previous = b["previous"]
        need(version(previous["version"]) and build(previous["build"])
             and source(previous["source_sha"]) and positive(previous["publisher_run"]) and positive(previous["publisher_attempt"]),
             "invalid predecessor identity fields")
        need(tuple(map(int, previous["version"].split('.'))) < tuple(map(int, identity["version"].split('.'))),
             "release version does not advance predecessor")
        previous_platforms = predecessor_platforms(b)
        need(set(previous_platforms) <= set(platforms), "release cannot remove an accepted platform")
        names = manifest_names(previous_platforms)
        need(set(b.get("previous_signed_envelopes", {})) == {f"stable/releases/{previous['version']}/{n}" for n in names},
             "predecessor immutable inventory differs")
        for item in b["previous_signed_envelopes"].values():
            reference(item)
        publication = b.get("publication_identity")
        if publication is not None:
            need(set(publication) == {"run_id", "run_attempt", "source_sha", "workflow"}
                 and positive(publication["run_id"]) and positive(publication["run_attempt"])
                 and source(publication["source_sha"]) and publication["workflow"] == WORKFLOW,
                 "invalid publisher identity")
        self.identity = identity

    @property
    def source(self): return self.identity["source_sha"]
    @property
    def platforms(self): return selected_platforms(self.bindings["platforms"])
    @property
    def version(self): return self.identity["version"]
    @property
    def build(self): return self.identity["build"]
    @property
    def tree(self): return self.bindings["payload_tree"]
    @property
    def producer_run(self): return self.identity["run_id"]
    @property
    def producer_attempt(self): return self.identity["run_attempt"]
    @property
    def publication(self):
        value = self.bindings.get("publication_identity")
        need(value is not None, "actual publication selection required")
        return value

    @property
    def route(self): return Path(self.policy["r2"]["route"]["path"])
    @property
    def route_sha256(self): return self.policy["r2"]["route"]["sha256"]
    @property
    def endpoint(self): return self.policy["r2"]["endpoint"]
    @property
    def bucket(self): return self.policy["r2"]["bucket"]

    def read(self, path, maximum=4 * 1024**2):
        return read_regular(path, maximum)

    def pinned(self, ref, maximum=4 * 1024**2):
        reference(ref)
        data = read_regular(Path(ref["path"]), maximum)
        need(sha(data) == ref["sha256"], "pinned local input changed")
        return data

    def tool(self, name):
        item = self.policy["tools"][name]
        path = Path(item["path"]).resolve(strict=True)
        need(sha(read_regular(path, 64 * 1024**2)) == item["sha256"] and os.access(path, os.X_OK), "pinned tool changed")
        return str(path)

    def emit(self, path, value):
        path = Path(path)
        need(path.is_absolute() and path.parent.resolve(strict=True) == path.parent and path.is_relative_to(self.root),
             "receipt outside owned state")
        data = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
        return sha(data)

    def _github(self, path, body=None, maximum=8 * 1024**2):
        need(isinstance(path, str) and not path.startswith('/') and '://' not in path and '..' not in path,
             "invalid canonical GitHub API path")
        path = path if path.startswith(f"repos/{REPOSITORY}/") or path == "user" else f"repos/{REPOSITORY}/{path}"
        args = [self.tool("gh"), "api", "--hostname", "github.com", "--method", "GET" if body is None else "POST", path]
        if body is not None:
            args += ["--input", "-"]
        result = subprocess.run(args, input=None if body is None else json.dumps(body).encode(),
                                capture_output=True, timeout=60)
        need(result.returncode == 0 and len(result.stdout) <= maximum,
             "GitHub request failed; preserve intent before any retry")
        return decode(result.stdout) if result.stdout else None

    def gh_get(self, path, maximum=8 * 1024**2): return self._github(path, maximum=maximum)
    def gh_post(self, path, body, maximum=8 * 1024**2): return self._github(path, body, maximum)

    def verify_code_revision(self, revision):
        need(source(revision), "invalid trusted source revision")
        for name, expected in self.code_pins.items():
            result = subprocess.run(["git", "show", revision + ":" + name], cwd=self.repo,
                                    capture_output=True, timeout=30)
            need(result.returncode == 0 and sha(result.stdout) == expected,
                 "selected source publisher inputs differ")


class Policy:
    """Validate the same private policy before release facts have been selected."""

    def __init__(self, path, expected_sha256):
        need(digest(expected_sha256), "expected private policy digest required")
        raw = read_regular(Path(path))
        need(sha(raw) == expected_sha256, "operator policy changed")
        self.policy_sha256 = expected_sha256
        self.policy = decode(raw)
        Context.validate_policy(self)

    tool = Context.tool

    def verify_route(self):
        route = self.policy["r2"]["route"]
        path = Path(route["path"])
        info = path.lstat()
        need(stat.S_ISREG(info.st_mode) and stat.S_IMODE(info.st_mode) == 0o600
             and sha(read_regular(path)) == route["sha256"], "references-only credential route changed")
        return route
