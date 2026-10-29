"""Offline trust-input tests; synthetic policy IDs and mocked local Git only."""

from copy import deepcopy
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from release_helpers import context


class ContextFixture:
    def __init__(self, root, generation=1):
        self.root, self.repo = root, Path(context.__file__).resolve().parents[4]
        self.state = root / "state"
        self.state.mkdir(mode=0o700)
        self.policy_path, self.bindings_path = root / "policy.json", root / "bindings.json"
        self.tools = {}
        for name in ("gh", "minisign"):
            path = root / ("fixture-" + name)
            path.write_bytes(b"offline executable fixture: " + name.encode())
            path.chmod(0o700)
            self.tools[name] = self.ref(path)
        self.route = root / "credential-route.json"
        self.route.write_text('{"fixture":"references only, no account secrets"}\n')
        self.route.chmod(0o600)
        self.policy = {"schema": "agent-release-policy-v1", "environment": {"id": 101, "name": "updates-production", "reviewer_id": 102},
                       "r2": {"endpoint": "https://" + "0" * 32 + ".r2.cloudflarestorage.com", "bucket": "fixture-releases", "route": self.ref(self.route)},
                       "tools": deepcopy(self.tools)}
        profiles = context.decode((self.repo / "apps/maple-agent/release-profiles.json").read_bytes())
        self.key = root / "public-key.pub"
        self.key.write_text(profiles["prod"]["update_public_keys"][0] + "\n")
        selected_version, previous_version = (("2.4.6", "2.4.5") if generation == 1 else ("3.8.12", "3.8.11"))
        source, publisher_source = str(generation) * 40, str(generation + 2) * 40
        previous_ref = {"path": str(root / "previous-fixture.json"), "sha256": "5" * 64}
        self.bindings = {"schema": context.SCHEMA_BINDINGS, "repo": str(self.repo), "platforms": list(context.PLATFORMS),
                         "producer_identity": {"run_id": 201 + generation, "run_attempt": 1, "source_sha": source,
                                               "version": selected_version, "build": str(300 + generation), "pub_date": "2026-10-01T01:02:03Z"},
                         "publication_identity": {"run_id": 401 + generation, "run_attempt": 2,
                                                  "source_sha": publisher_source, "workflow": context.WORKFLOW},
                         "payload_tree": "4" * 40, "public_key": str(self.key),
                         "previous": {"version": previous_version, "build": "299", "source_sha": "6" * 40,
                                      "publisher_run": 501, "publisher_attempt": 1},
                         "previous_signed_envelopes": {f"stable/releases/{previous_version}/{name}": deepcopy(previous_ref)
                                                       for name in ("manifest.json", "linux-manifest.json", "windows-manifest.json")},
                         "artifacts": {platform: {"id": index + 601, "name": f"maple-agent-prod-{context.TARGETS[platform]}-{201 + generation}",
                                                   "size": 1024, "zip_sha256": "7" * 64}
                                       for index, platform in enumerate(context.PLATFORMS)},
                         "code_pins": {name: context.sha((self.repo / name).read_bytes()) for name in context.CODE_PATHS}}
        for name in ("qualification", "notes", "previous_private_r2", "previous_root_acceptance", "previous_public", "research_preservation"):
            self.bindings[name] = deepcopy(previous_ref)
        self.write()

    def ref(self, path):
        return {"path": str(path), "sha256": context.sha(path.read_bytes())}

    def write(self, refresh_tooling=True):
        self.policy_path.write_bytes(self.encode(self.policy))
        self.policy_sha = context.sha(self.policy_path.read_bytes())
        self.bindings["policy_sha256"] = self.policy_sha
        if refresh_tooling:
            self.bindings["tooling_sha256"] = context.tooling_digest(self.repo)
        self.bindings_path.write_bytes(self.encode(self.bindings))
        self.bindings_sha = context.sha(self.bindings_path.read_bytes())

    def encode(self, value):
        return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()

    def load(self):
        return context.Context(self.state, self.bindings_path, self.bindings_sha, self.policy_path, self.policy_sha)

    def load_policy(self):
        return context.Policy(self.policy_path, self.policy_sha)


class ContextTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.fixture = ContextFixture(self.root)

    def load_without_git(self):
        with patch.object(context.Context, "verify_code_revision"):
            return self.fixture.load()

    def rejected_binding(self, change):
        original = deepcopy(self.fixture.bindings)
        try:
            change(self.fixture.bindings)
            self.fixture.write(refresh_tooling=False)
            with self.assertRaises((context.OperatorError, ValueError, TypeError, KeyError)):
                self.load_without_git()
        finally:
            self.fixture.bindings = original

    def rejected_policy(self, change):
        original = deepcopy(self.fixture.policy)
        try:
            change(self.fixture.policy)
            self.fixture.write(refresh_tooling=False)
            with self.assertRaises((context.OperatorError, ValueError, TypeError, KeyError)):
                self.fixture.load_policy()
        finally:
            self.fixture.policy = original

    def test_two_release_identities_load_trusted_checkout_and_primary(self):
        for generation in (1, 2):
            with self.subTest(generation=generation):
                if generation == 2:
                    root = self.root / "second"
                    root.mkdir()
                    self.fixture = ContextFixture(root, generation=2)
                loaded = self.load_without_git()
                self.assertEqual(loaded.repo, loaded.core.ROOT)
                self.assertEqual(loaded.source, str(generation) * 40)
                self.assertEqual(loaded.publication["source_sha"], str(generation + 2) * 40)
                self.assertEqual(loaded.tooling_sha256, context.tooling_digest(loaded.repo))
                self.assertEqual(loaded.tool("gh"), self.fixture.tools["gh"]["path"])

    def test_duplicate_fields_and_nonfinite_json_are_rejected(self):
        for raw in (b'{"x":1,"x":2}', b'{"outer":{"x":1,"x":2}}', b'{"x":NaN}', b'{"x":Infinity}',
                    b'{"x":-Infinity}', b'{"x":1e400}', b'{"nested":[-1e400]}'):
            with self.subTest(raw=raw), self.assertRaises(context.OperatorError):
                context.decode(raw)

    def test_policy_and_bindings_loaders_apply_strict_json_after_hash_check(self):
        for policy in (False, True):
            with self.subTest(policy=policy):
                path = self.fixture.policy_path if policy else self.fixture.bindings_path
                raw = b'{"schema":"first","schema":"second"}'
                path.write_bytes(raw)
                if policy:
                    with self.assertRaisesRegex(context.OperatorError, "duplicate"):
                        context.Policy(path, context.sha(raw))
                else:
                    with self.assertRaisesRegex(context.OperatorError, "duplicate"):
                        context.Context(self.fixture.state, path, context.sha(raw), self.fixture.policy_path, self.fixture.policy_sha)
                self.fixture.write()

    def test_state_requires_owned_canonical_directory_and_0700(self):
        for mode in (0o755, 0o770, 0o777):
            self.fixture.state.chmod(mode)
            with self.subTest(mode=oct(mode)), self.assertRaisesRegex(context.OperatorError, "private0700"):
                self.load_without_git()
        self.fixture.state.chmod(0o700)
        with patch("release_helpers.context.os.getuid", return_value=os.getuid() + 1):
            with self.assertRaisesRegex(context.OperatorError, "owned"):
                self.load_without_git()
        link = self.root / "state-link"
        link.symlink_to(self.fixture.state, target_is_directory=True)
        with self.assertRaisesRegex(context.OperatorError, "canonical"):
            context.Context(link, self.fixture.bindings_path, self.fixture.bindings_sha, self.fixture.policy_path, self.fixture.policy_sha)

    def test_bindings_and_policy_expected_sha_are_required_and_exact(self):
        f = self.fixture
        cases = (("wrong bindings", "0" * 64, f.policy_sha), ("wrong policy", f.bindings_sha, "0" * 64),
                 ("invalid digest", "bad", f.policy_sha), ("invalid policy digest", f.bindings_sha, True))
        for name, binding_sha, policy_sha in cases:
            with self.subTest(case=name), self.assertRaises(context.OperatorError):
                context.Context(f.state, f.bindings_path, binding_sha, f.policy_path, policy_sha)
        with self.assertRaises(context.OperatorError):
            context.Policy(f.policy_path, "0" * 64)
        with self.assertRaises(context.OperatorError):
            context.Policy(f.policy_path, "bad")

    def test_policy_exact_environment_and_tool_roles(self):
        for change in (lambda p: p.update(extra=True), lambda p: p.update(schema="other"),
                       lambda p: p["environment"].update(name="updates-staging"),
                       lambda p: p["environment"].update(extra=True),
                       lambda p: p["tools"].update(extra=deepcopy(p["tools"]["gh"])),
                       lambda p: p["tools"].pop("minisign"),
                       lambda p: p["tools"]["gh"].update(path="relative-tool"),
                       lambda p: p["tools"]["gh"].update(path=True),
                       lambda p: p["tools"]["gh"].update(sha256="F" * 64),
                       lambda p: p["tools"]["gh"].update(extra=True)):
            self.rejected_policy(change)
        for name in ("id", "reviewer_id"):
            for value in (True, False, 0, -1, 2**64, "101"):
                with self.subTest(field=name, value=value):
                    self.rejected_policy(lambda p: p["environment"].update({name: value}))

    def test_r2_origin_rejects_userinfo_ports_paths_queries_and_other_hosts(self):
        host = "0" * 32 + ".r2.cloudflarestorage.com"
        invalid = ("http://" + host, "https://user@" + host, "https://user:password@" + host,
                   "https://@" + host, "https://:@" + host, "https://" + host + ":0", "https://" + host + ":",
                   "https://" + host + ":443", "https://" + host + "/bucket", "https://" + host + "?x=1",
                   "https://" + host + "#fragment", "https://" + host + ".example", "https://example.com",
                   "https://" + "0" * 31 + ".r2.cloudflarestorage.com")
        for endpoint in invalid:
            with self.subTest(endpoint=endpoint):
                self.rejected_policy(lambda p: p["r2"].update(endpoint=endpoint))
        for bucket in ("", "A-bucket", "a/b", "x", "a" * 64):
            with self.subTest(bucket=bucket):
                self.rejected_policy(lambda p: p["r2"].update(bucket=bucket))

    def test_tool_hash_executable_bit_and_symlink_target_are_checked_at_use(self):
        loaded = self.fixture.load_policy()
        tool = Path(self.fixture.tools["gh"]["path"])
        original = tool.read_bytes()
        tool.write_bytes(b"changed executable")
        with self.assertRaisesRegex(context.OperatorError, "pinned tool changed"):
            loaded.tool("gh")
        tool.write_bytes(original)
        tool.chmod(0o600)
        with self.assertRaisesRegex(context.OperatorError, "pinned tool changed"):
            loaded.tool("gh")
        tool.chmod(0o700)
        link = self.root / "tool-link"
        link.symlink_to(tool)
        self.fixture.policy["tools"]["gh"]["path"] = str(link)
        self.fixture.write()
        loaded = self.fixture.load_policy()
        self.assertEqual(loaded.tool("gh"), str(tool))
        other = self.root / "other-tool"
        other.write_bytes(b"different target")
        other.chmod(0o700)
        link.unlink()
        link.symlink_to(other)
        with self.assertRaisesRegex(context.OperatorError, "pinned tool changed"):
            loaded.tool("gh")
        link.unlink()
        link.symlink_to(self.fixture.state, target_is_directory=True)
        with self.assertRaises((context.OperatorError, OSError)):
            loaded.tool("gh")

    def test_invalid_producer_identity_and_missing_platforms_fail_closed(self):
        fields = {"run_id": (True, 0, -1, 2**64), "run_attempt": (False, 0, "1"),
                  "source_sha": ("a" * 39, "A" * 40, "g" * 40),
                  "build": (True, 0, "0", "01", "1" * 19),
                  "version": ("1.2", "v1.2.3", "01.2.3", "1.2.3-rc1", "1.2.3+build", str(2**64) + ".0.0"),
                  "pub_date": (True, "2026-01-01", "2026-10-01T01:02:03+00:00", "2026-99-99T99:99:99Z", "2026-02-30T01:02:03Z")}
        for field, values in fields.items():
            for value in values:
                with self.subTest(field=field, value=value):
                    self.rejected_binding(lambda b: b["producer_identity"].update({field: value}))
        for platforms in (["macos", "linux"], ["macos", "linux", "windows", "other"], ["windows", "linux", "macos"]):
            self.rejected_binding(lambda b: b.update(platforms=platforms))
        self.rejected_binding(lambda b: b.update(payload_tree="x" * 40))
        self.rejected_binding(lambda b: b.update(repo=str(self.root)))
        self.rejected_binding(lambda b: b.update(optional_note="UNBOUND placeholder"))

    def test_artifact_exact_name_identity_size_and_digest(self):
        for field, values in {"id": (True, 0, -1, 2**64), "name": ("other", "maple-agent-prod-linux-x86_64-202"),
                              "size": (True, 0, -1, 512 * 1024**2 + 1, "1024"),
                              "zip_sha256": ("g" * 64, "A" * 64, "a" * 63)}.items():
            for value in values:
                with self.subTest(field=field, value=value):
                    self.rejected_binding(lambda b: b["artifacts"]["macos"].update({field: value}))
        self.rejected_binding(lambda b: b["artifacts"].pop("windows"))
        self.rejected_binding(lambda b: b["artifacts"]["linux"].update(extra=True))

    def test_predecessor_version_and_identity_must_be_valid_and_older(self):
        for field, values in {"version": ("2.4.6", "2.4.7", "02.4.5", "2.4", str(2**64) + ".0.0"),
                              "build": (True, 0, "0", "01", "invalid"),
                              "source_sha": ("a" * 39, "A" * 40), "publisher_run": (True, 0, -1),
                              "publisher_attempt": (False, 0, "1")}.items():
            for value in values:
                with self.subTest(field=field, value=value):
                    self.rejected_binding(lambda b: b["previous"].update({field: value}))
        self.rejected_binding(lambda b: b["previous_signed_envelopes"].pop(next(iter(b["previous_signed_envelopes"]))))
        self.rejected_binding(lambda b: b["previous_signed_envelopes"].update({"stable/releases/wrong/manifest.json": b["notes"]}))

    def test_publisher_identity_remains_separate_and_strict(self):
        for field, values in {"run_id": (True, 0, -1, "401"), "run_attempt": (False, 0, 2**64),
                              "source_sha": ("g" * 40, "A" * 40, "1" * 39),
                              "workflow": (".github/workflows/release.yml", "other")}.items():
            for value in values:
                with self.subTest(field=field, value=value):
                    self.rejected_binding(lambda b: b["publication_identity"].update({field: value}))
        self.rejected_binding(lambda b: b["publication_identity"].update(extra=True))

    def test_selected_payload_and_different_publisher_both_check_code_revisions(self):
        calls = []

        def git_show(args, **kwargs):
            self.assertEqual(args[:2], ["git", "show"])
            self.assertEqual(kwargs["cwd"], self.fixture.repo)
            revision, name = args[2].split(":", 1)
            calls.append((revision, name))
            return SimpleNamespace(returncode=0, stdout=(self.fixture.repo / name).read_bytes())

        with patch("release_helpers.context.subprocess.run", side_effect=git_show):
            loaded = self.fixture.load()
        self.assertCountEqual(calls, [(revision, name) for revision in (loaded.source, loaded.publication["source_sha"]) for name in context.CODE_PATHS])
        with patch("release_helpers.context.subprocess.run", return_value=SimpleNamespace(returncode=0, stdout=b"changed code")):
            with self.assertRaisesRegex(context.OperatorError, "publisher inputs differ"):
                self.fixture.load()

        def changed_publisher(args, **kwargs):
            revision, name = args[2].split(":", 1)
            raw = (self.fixture.repo / name).read_bytes()
            if revision == self.fixture.bindings["publication_identity"]["source_sha"]:
                raw += b"\nchanged publisher revision\n"
            return SimpleNamespace(returncode=0, stdout=raw)

        with patch("release_helpers.context.subprocess.run", side_effect=changed_publisher):
            with self.assertRaisesRegex(context.OperatorError, "publisher inputs differ"):
                self.fixture.load()
        self.rejected_binding(lambda b: b["code_pins"].update({context.CODE_PATHS[0]: "0" * 64}))

    def test_pinned_receipts_and_canonical_regular_reads_are_bounded(self):
        loaded = self.load_without_git()
        path = self.root / "small-receipt.json"
        path.write_bytes(b"{}\n")
        reference = self.fixture.ref(path)
        self.assertEqual(loaded.pinned(reference), b"{}\n")
        path.write_bytes(b"changed")
        with self.assertRaisesRegex(context.OperatorError, "pinned local input changed"):
            loaded.pinned(reference)
        with self.assertRaisesRegex(context.OperatorError, "exceeds bound"):
            loaded.read(path, maximum=2)
        link = self.root / "receipt-link"
        link.symlink_to(path)
        with self.assertRaisesRegex(context.OperatorError, "noncanonical"):
            loaded.read(link)
        fifo = self.root / "not-a-receipt"
        os.mkfifo(fifo)
        with self.assertRaisesRegex(context.OperatorError, "regular"):
            loaded.read(fifo)

    def test_tooling_digest_changes_and_changed_bound_tooling_refuses(self):
        # A minimal disposable checkout exercises the same fixed sibling-path
        # relation as production; no production source files are modified.
        repo = self.root / "copy"
        names = set(context.CODE_PATHS) | {"services/agent-updates/publisher/agent_release.py"}
        names |= {str(p.relative_to(self.fixture.repo)) for p in (self.fixture.repo / "services/agent-updates/publisher/release_helpers").glob("*.py")}
        for name in names:
            target = repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.fixture.repo / name, target)
        before = context.tooling_digest(repo)
        self.fixture.repo = repo
        self.fixture.bindings["repo"] = str(repo)
        self.fixture.write()
        self.assertEqual(self.fixture.bindings["tooling_sha256"], before)
        fake_context_path = repo / "services/agent-updates/publisher/release_helpers/context.py"
        with patch("release_helpers.context.__file__", str(fake_context_path)):
            loaded = self.load_without_git()
        self.assertEqual(loaded.repo, loaded.core.ROOT)
        helper = repo / "services/agent-updates/publisher/release_helpers/public.py"
        helper.write_bytes(helper.read_bytes() + b"\n# changed local tooling\n")
        self.assertNotEqual(context.tooling_digest(repo), before)
        with patch("release_helpers.context.__file__", str(fake_context_path)):
            with self.assertRaisesRegex(context.OperatorError, "tooling changed"):
                self.load_without_git()

    def test_emit_is_fresh_private_and_confined_to_owned_state(self):
        loaded = self.load_without_git()
        path = self.fixture.state / "receipt.json"
        emitted = loaded.emit(path, {"schema": "fixture", "ok": True})
        self.assertEqual(emitted, context.sha(path.read_bytes()))
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        with self.assertRaises(FileExistsError):
            loaded.emit(path, {"overwritten": True})
        with self.assertRaisesRegex(context.OperatorError, "outside owned state"):
            loaded.emit(self.root / "outside.json", {"outside": True})

    def test_isolated_cli_help_lists_all_operations_without_loading_context(self):
        script = self.fixture.repo / "services/agent-updates/publisher/agent_release.py"
        result = subprocess.run([sys.executable, "-I", str(script), "--help"], capture_output=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        for operation in ("plan", "freeze", "bind-publication", "bind-public", "download-producer", "download-stage",
                          "dispatch", "reconcile-dispatch", "approve", "verify-public", "verify-bundle", "accept", "check-policy"):
            self.assertIn(operation, result.stdout.decode())

    def test_route_preflight_checks_bytes_and_mode_without_executing_tools(self):
        policy = self.fixture.load_policy()
        self.assertEqual(policy.verify_route(), self.fixture.ref(self.fixture.route))
        self.fixture.route.chmod(0o644)
        with self.assertRaises(context.OperatorError):
            policy.verify_route()
        self.fixture.route.chmod(0o600)
        self.fixture.route.write_text("changed references\n")
        with self.assertRaises(context.OperatorError):
            policy.verify_route()

    def test_check_policy_cli_has_no_network_or_effectful_process(self):
        import agent_release
        with patch("subprocess.run", side_effect=AssertionError("no process allowed")), patch("builtins.print"):
            self.assertEqual(agent_release.main(["check-policy", "--policy", str(self.fixture.policy_path),
                                                "--policy-sha256", self.fixture.policy_sha]), 0)


if __name__ == "__main__":
    unittest.main()
