"""One explicit dispatch and read-only reconciliation of its durable intent."""
from datetime import datetime, timezone
from pathlib import Path

from .bundle import BundleVerifier
from .context import need, sha, decode, source, SCHEMA_FREEZE


class Dispatcher:
    def __init__(self, ctx):
        self.ctx = ctx

    def history(self):
        """Complete bounded history, not a last-page guess about active work."""
        result, total = [], None
        for page in range(1, 11):
            data = self.ctx.gh_get(f"actions/workflows/agent-updates-publish.yml/runs?per_page=100&page={page}", maximum=2 * 1024**2)
            need(type(data.get("total_count")) is int and 0 <= data["total_count"] <= 1000,
                 "publication history exceeds bounded reconciliation")
            if total is None:
                total = data["total_count"]
            need(total == data["total_count"] and isinstance(data.get("workflow_runs"), list),
                 "publication history changed or is incomplete")
            result += data["workflow_runs"]
            need(len({item["id"] for item in result}) == len(result), "duplicate publication history")
            if len(result) == total:
                return result
            need(len(data["workflow_runs"]) == 100 and len(result) < total, "incomplete publication history")
        raise RuntimeError("publication history bound exceeded")

    def inputs(self):
        ctx = self.ctx
        b = ctx.bindings
        frozen = decode(ctx.pinned(b["freeze"]))
        need(frozen.get("schema") == SCHEMA_FREEZE and frozen.get("canonical_prepare_completed") is True
             and frozen["source"] == ctx.source and frozen["tree"] == ctx.tree
             and frozen["producer"]["run_id"] == ctx.producer_run
             and frozen["producer"]["run_attempt"] == ctx.producer_attempt
             and frozen["producer"]["build"] == ctx.build
             and frozen["platforms"] == ["macos", "linux", "windows"], "frozen release identity differs")
        need(frozen["files"] == b["prepared_files"] and frozen["notes"]["sha256"] == b["notes"]["sha256"],
             "frozen files or notes differ")
        BundleVerifier(ctx).verify_bundle(b, ctx.core, Path(b["prepared_bundle"]), "prepared", None)
        qualification = decode(ctx.pinned(b["qualification"]))
        need(qualification["required_included_jobs_all_success"] is True and qualification["producer_terminal"] is True
             and (qualification["run_id"], qualification["attempt"], qualification["source"], qualification["version"], qualification["build"])
             == (ctx.producer_run, ctx.producer_attempt, ctx.source, ctx.version, int(ctx.build)),
             "qualified producer identity differs")
        notes = ctx.pinned(b["notes"]).decode("utf-8")
        return qualification, {"ref": "master", "inputs": {"build_run_id": str(ctx.producer_run),
                "version": ctx.version, "notes": notes, "include_linux": True, "include_windows": True}}

    def run(self, args):
        ctx = self.ctx
        if not args.dispatch_once:
            _, body = self.inputs()
            return {"status": "no_dispatch", "request": body,
                    "source": ctx.source, "freeze_sha256": ctx.bindings["freeze"]["sha256"],
                    "policy_sha256": ctx.policy_sha256,
                    "protected_approvals": "separate reserve/sign/publish"}
        intent_path, result_path = ctx.root / "dispatch-intent.json", ctx.root / "dispatch-result.json"
        need(not intent_path.exists(), "dispatch intent exists; reconcile without redispatch")
        qualification, body = self.inputs()
        publisher_source = ctx.bindings.get("publisher_source")
        need(source(publisher_source), "reviewed publisher source required")
        ctx.verify_code_revision(publisher_source)
        need(ctx.gh_get("git/ref/heads/master")["object"]["sha"] == publisher_source,
             "dispatch master changed; select the actual publisher source")
        runs = self.history()
        need(all(run["status"] == "completed" for run in runs), "another publication is active")
        title = f"Publish Agent {ctx.version} from verified build {ctx.producer_run}"
        need(not any(run.get("display_title") == title for run in runs), "publication already exists; reconcile")
        producer = ctx.gh_get(f"actions/runs/{ctx.producer_run}")
        need(producer["run_attempt"] == ctx.producer_attempt and producer["head_sha"] == ctx.source
             and producer["status"] == "completed" and producer["conclusion"] == "success",
             "selected producer changed before dispatch")
        artifacts = ctx.gh_get(f"actions/runs/{ctx.producer_run}/artifacts?per_page=100")
        need(artifacts["total_count"] == len(artifacts["artifacts"]), "incomplete selected artifact inventory")
        for selected in qualification["selected_final_artifact_metadata"]:
            matches = [item for item in artifacts["artifacts"] if item["name"] == selected["name"]]
            need(len(matches) == 1 and matches[0]["expired"] is False and
                 all(matches[0].get(k) == selected.get(k) for k in ("id", "name", "digest", "size_in_bytes", "workflow_run")),
                 "selected artifact changed before dispatch")
        intent = {"at": datetime.now(timezone.utc).isoformat(), "expected_publisher_source": publisher_source,
                  "producer_source": ctx.source, "bindings_sha256": ctx.bindings_sha256,
                  "policy_sha256": ctx.policy_sha256, "tooling_sha256": ctx.tooling_sha256,
                  "frozen_prepared_sha256": ctx.bindings["freeze"]["sha256"],
                  "qualification_sha256": ctx.bindings["qualification"]["sha256"], "body": body,
                  "previous_run_ids": [run["id"] for run in runs]}
        ctx.emit(intent_path, intent)
        outcome = {"at": datetime.now(timezone.utc).isoformat(), "request_accepted": None}
        try:
            outcome["response"] = ctx.gh_post("actions/workflows/agent-updates-publish.yml/dispatches", body)
            outcome["request_accepted"] = True
        except Exception as error:
            outcome["error_type"] = type(error).__name__
            outcome["requires_reconciliation"] = True
            ctx.emit(result_path, outcome)
            raise
        ctx.emit(result_path, outcome)
        return {"request_accepted": True, "intent": str(intent_path),
                "scope": "one dispatch; protected approvals remain separate"}

    def reconcile(self, args):
        need(args.network, "read-only network reconciliation requires --network")
        ctx = self.ctx
        raw = ctx.read(ctx.root / "dispatch-intent.json")
        intent = decode(raw)
        need(intent["bindings_sha256"] == ctx.bindings_sha256 and intent["tooling_sha256"] == ctx.tooling_sha256
             and intent["policy_sha256"] == ctx.policy_sha256, "dispatch intent belongs to different inputs")
        title = f"Publish Agent {ctx.version} from verified build {ctx.producer_run}"
        matches = [run for run in self.history() if run["id"] not in intent["previous_run_ids"] and run.get("display_title") == title]
        need(len(matches) == 1, "publication outcome is absent or ambiguous; do not redispatch")
        run = matches[0]
        need(run["head_sha"] == intent["expected_publisher_source"] and run["head_branch"] == "master"
             and run["event"] == "workflow_dispatch" and run["path"] == ctx.workflow
             and run["repository"]["full_name"] == ctx.repository and run["head_repository"]["full_name"] == ctx.repository,
             "untrusted returned publication")
        selected = {"run_id": run["id"], "run_attempt": run["run_attempt"], "source_sha": run["head_sha"],
                    "workflow": ctx.workflow, "payload_source": ctx.source, "producer_run": ctx.producer_run,
                    "producer_attempt": ctx.producer_attempt, "version": ctx.version,
                    "platforms": ["macos", "linux", "windows"], "inputs": intent["body"]["inputs"],
                    "dispatch_intent_sha256": sha(raw)}
        digest = ctx.emit(args.output, selected)
        return {"selection": str(args.output), "sha256": digest, "run_id": run["id"], "attempt": run["run_attempt"]}


def add_arguments(parser, operation):
    if operation == "dispatch":
        parser.add_argument("--dispatch-once", action="store_true")
    else:
        parser.add_argument("--network", action="store_true")
        parser.add_argument("--output", type=Path, required=True)
