"""Offline dispatch/reconciliation behavior; never talks to GitHub."""
from copy import deepcopy
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import Mock

from release_helpers.context import Context, OperatorError, decode
from release_helpers.dispatch import Dispatcher


class FakeContext:
    emit = Context.emit

    def __init__(self, root, version, run):
        self.root = root.resolve()
        self.version, self.producer_run = version, run
        self.source, self.publisher_source = "a" * 40, "b" * 40
        self.producer_attempt, self.build = 2, "987"
        self.repository = "MaplePrivacyLabs/Maple"
        self.workflow = ".github/workflows/agent-updates-publish.yml"
        self.bindings_sha256, self.policy_sha256, self.tooling_sha256 = (char * 64 for char in "cde")
        self.bindings = {"platforms": ["macos", "linux", "windows"],
                         "publisher_source": self.publisher_source,
                         "freeze": {"sha256": "f" * 64}, "qualification": {"sha256": "9" * 64}}
        self.posts, self.gets, self.revisions = [], [], []
        self.runs = []
        self.artifact = {"id": 321, "name": f"maple-agent-prod-macos-aarch64-{run}",
                         "size_in_bytes": 42, "digest": "sha256:" + "8" * 64, "expired": False,
                         "workflow_run": {"id": run, "head_sha": self.source}}
        self.master = self.publisher_source
        self.producer = {"run_attempt": 2, "head_sha": self.source,
                         "status": "completed", "conclusion": "success"}
        self.fail_post = False

    def read(self, path): return Path(path).read_bytes()
    def verify_code_revision(self, revision): self.revisions.append(revision)

    def gh_get(self, path, **kwargs):
        self.gets.append(path)
        if path == "git/ref/heads/master": return {"object": {"sha": self.master}}
        if path.startswith("actions/workflows/"):
            page = int(path.rsplit("page=", 1)[1])
            return {"total_count": len(self.runs), "workflow_runs": deepcopy(self.runs[(page - 1) * 100:page * 100])}
        if path.endswith("/artifacts?per_page=100"):
            return {"total_count": 1, "artifacts": [deepcopy(self.artifact)]}
        return deepcopy(self.producer)

    def gh_post(self, path, body):
        self.posts.append((path, body))
        if self.fail_post: raise OSError("simulated ambiguous transport")
        return None


class DispatchTests(unittest.TestCase):
    def context(self, path, version="1.2.3", run=1234):
        path.mkdir(mode=0o700)
        ctx = FakeContext(path, version, run)
        operator = Dispatcher(ctx)
        operator.inputs = Mock(return_value=(
            {"selected_final_artifact_metadata": [deepcopy(ctx.artifact)]},
            {"ref": "master", "inputs": {"build_run_id": str(run), "version": version,
             "notes": "Reviewed notes\n", "include_linux": True, "include_windows": True}}))
        return ctx, operator

    def test_two_releases_use_inputs_and_dispatch_exactly_once(self):
        with TemporaryDirectory() as directory:
            for index, (version, run) in enumerate((("1.2.3", 1234), ("7.8.9", 9876))):
                with self.subTest(version=version):
                    ctx, op = self.context(Path(directory) / str(index), version, run)
                    result = op.run(SimpleNamespace(dispatch_once=True))
                    self.assertTrue(result["request_accepted"])
                    self.assertEqual(len(ctx.posts), 1)
                    body = ctx.posts[0][1]
                    self.assertEqual(body["inputs"]["version"], version)
                    self.assertEqual(body["inputs"]["build_run_id"], str(run))
                    intent = decode((ctx.root / "dispatch-intent.json").read_bytes())
                    self.assertEqual(intent["expected_publisher_source"], ctx.publisher_source)
                    self.assertNotEqual(intent["expected_publisher_source"], intent["producer_source"])
                    with self.assertRaises(OperatorError): op.run(SimpleNamespace(dispatch_once=True))
                    self.assertEqual(len(ctx.posts), 1)

    def test_preview_validates_local_inputs_but_makes_no_requests(self):
        with TemporaryDirectory() as directory:
            ctx, op = self.context(Path(directory) / "state")
            result = op.run(SimpleNamespace(dispatch_once=False))
            self.assertEqual(result["request"]["inputs"]["notes"], "Reviewed notes\n")
            self.assertEqual(ctx.gets, [])
            self.assertEqual(ctx.posts, [])
            self.assertFalse((ctx.root / "dispatch-intent.json").exists())

    def test_changed_master_active_duplicate_attempt_and_artifact_deny_before_post(self):
        mutations = (
            lambda c: setattr(c, "master", "0" * 40),
            lambda c: c.runs.append({"id": 7, "status": "waiting"}),
            lambda c: c.runs.append({"id": 7, "status": "completed", "display_title": f"Publish Agent {c.version} from verified build {c.producer_run}"}),
            lambda c: c.producer.update(run_attempt=3),
            lambda c: c.producer.update(status="in_progress"),
            lambda c: c.artifact.update(id=99),
            lambda c: c.artifact.update(expired=True),
            lambda c: c.artifact.update(digest="sha256:" + "0" * 64),
        )
        with TemporaryDirectory() as directory:
            for index, mutation in enumerate(mutations):
                with self.subTest(index=index):
                    ctx, op = self.context(Path(directory) / str(index))
                    mutation(ctx)
                    with self.assertRaises(OperatorError): op.run(SimpleNamespace(dispatch_once=True))
                    self.assertEqual(ctx.posts, [])
                    self.assertFalse((ctx.root / "dispatch-intent.json").exists())

    def test_complete_history_can_span_pages_and_incomplete_or_changing_history_fails(self):
        with TemporaryDirectory() as directory:
            ctx, op = self.context(Path(directory) / "state")
            ctx.runs = [{"id": i + 1, "status": "completed"} for i in range(105)]
            self.assertEqual(len(op.history()), 105)
            ctx.gh_get = Mock(side_effect=[{"total_count": 105, "workflow_runs": ctx.runs[:100]},
                                          {"total_count": 106, "workflow_runs": ctx.runs[100:]}])
            with self.assertRaises(OperatorError): op.history()
            self.assertEqual(ctx.posts, [])
            ctx.gh_get = Mock(return_value={"total_count": 105, "workflow_runs": ctx.runs[:99]})
            with self.assertRaises(OperatorError): op.history()

    def test_ambiguous_post_retains_intent_and_failure_and_never_retries(self):
        with TemporaryDirectory() as directory:
            ctx, op = self.context(Path(directory) / "state")
            ctx.fail_post = True
            with self.assertRaises(OSError): op.run(SimpleNamespace(dispatch_once=True))
            self.assertTrue((ctx.root / "dispatch-intent.json").exists())
            result = decode((ctx.root / "dispatch-result.json").read_bytes())
            self.assertIsNone(result["request_accepted"])
            self.assertTrue(result["requires_reconciliation"])
            self.assertEqual(result["error_type"], "OSError")
            with self.assertRaises(OperatorError): op.run(SimpleNamespace(dispatch_once=True))
            self.assertEqual(len(ctx.posts), 1)

    def test_reconcile_is_read_only_and_preserves_distinct_sources(self):
        with TemporaryDirectory() as directory:
            ctx, op = self.context(Path(directory) / "state")
            op.run(SimpleNamespace(dispatch_once=True))
            ctx.runs = [{"id": 4567, "run_attempt": 1, "head_sha": ctx.publisher_source,
                         "head_branch": "master", "event": "workflow_dispatch", "path": ctx.workflow,
                         "display_title": f"Publish Agent {ctx.version} from verified build {ctx.producer_run}",
                         "repository": {"full_name": ctx.repository}, "head_repository": {"full_name": ctx.repository}}]
            output = ctx.root / "publication.json"
            with self.assertRaises(OperatorError): op.reconcile(SimpleNamespace(network=False, output=output))
            result = op.reconcile(SimpleNamespace(network=True, output=output))
            self.assertEqual(result["run_id"], 4567)
            selected = decode(output.read_bytes())
            self.assertEqual(selected["source_sha"], ctx.publisher_source)
            self.assertEqual(selected["payload_source"], ctx.source)
            self.assertEqual(len(ctx.posts), 1)

    def test_ambiguous_reconciliation_and_wrong_source_cannot_select(self):
        with TemporaryDirectory() as directory:
            ctx, op = self.context(Path(directory) / "state")
            op.run(SimpleNamespace(dispatch_once=True))
            with self.assertRaises(OperatorError): op.reconcile(SimpleNamespace(network=True, output=ctx.root / "selected.json"))
            self.assertEqual(len(ctx.posts), 1)


if __name__ == "__main__":
    unittest.main()
