#!/usr/bin/env python3
"""Validate Store measurements, documented limits and actual runner references.

Absolute results never establish a missing relative baseline. Native workload
diagnostics are explicit and cannot pass or record another platform's gate.
"""

import argparse
import hashlib
import json
import math
import re
from pathlib import Path


SEED = 0x6D61706C655F6167
MIB = 1024 * 1024
BOUNDARY = "production Store APIs; durable source commits; fresh SQLite caches, warm OS cache; no UI/live-network latency"
SCALES = {
    "ci": {"tasks": 40, "entries": 8000, "hot_entries": 2000, "long_entries": 1000},
    "full": {"tasks": 2000, "entries": 1000000, "hot_entries": 100000, "long_entries": 20000},
}
PROFILES = {"linux-ci": ("ci", "linux"), "macos-full": ("full", "macos")}
PLAN_INDEXES = {
    "list": "tasks_section", "source": "entries_seq", "project-section": "tasks_project_section",
    "pinned": "tasks_pinned", "title-search": "tasks_recent", "projects": "projects_navigation",
    "history": "timeline_rows_seq", "merge": "timeline_rows_key", "call": "timeline_rows_call",
    "hide": "timeline_rows_entry", "plan": "timeline_rows_plan", "attachment-unbound": "file_attachments_unbound",
    "attachment-collection": "file_attachments_task",
}
REPEATED = {
    "raw_connection_open", "list_50_warm", "open_100_warm", "project_groups_warm",
    "project_switcher_warm", "search_100_warm", "get_50_tasks_warm", "pinned_tasks_warm",
    "task_facts_warm", "history_forward_200_warm", "history_200_deep_warm",
}
FIXED_SAMPLES = {
    "model_context_boundary_cold": 5, "model_context_full_cold": 5,
    "durable_append_held_reader": 100, "passive_checkpoint_held_reader": 1,
    "truncate_checkpoint_after_reader": 1, "attachment_10mib_first_read": 1,
    "attachment_10mib_warm": 5, "durable_file_upload_10mib": 1,
    "durable_20mib_write": 1, "oversized_row_first_range": 1, "oversized_row_cached_range": 5,
    "timeline_rebuild_writer_chunk": 1, "timeline_rebuild_prepare_chunk": 1,
    "old_epoch_collection_chunk": 1, "oversized_task_collection_chunk": 1,
    "jsonl_full_load_os_cache_warm": 5, "jsonl_append_unsynced_historical_baseline": 100,
    "argon2_unlock": 1,
}
GAPS = [
    "open_100_cold measures Store paging and plan lookup; UI and live overlay merge are unmeasured",
    "attachment first read assembles plaintext file ranges with database metadata checks and the OS cache warm; cold disk and network are unmeasured",
    "the 20k-entry context fixture is identified, but its individual uncompressed payload byte count is not reported",
    "per-chunk WAL file-length deltas are observed growth/reuse, not physical WAL bytes written",
    "append median/p99/p99.9/maximum and over-5ms count are evidence; absolute FULL/platform-sync append budgets await review",
    "relative gates compare medians only; no approved relative tail, checkpoint or conversion limit is implied",
    "runner OS is checked; the JSON does not independently attest the scheduled runner label or hardware",
    "process peak RSS includes fixture generation, hashing and both modes, not an isolated operation's memory",
    "Argon2 uses the floor on small runs and current-host calibration on full runs when RAM is available; low-end cross-machine cost caps are unproven, and unavailable RAM (including the current Windows helper) falls back to the floor",
]


def integer(value, minimum=0):
    return isinstance(value, int) and not isinstance(value, bool) and value >= minimum


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0


def sample_values_match(sample, values):
    """Chunk observations must retain the same outliers as their summary."""
    ordered = sorted(values)
    if sample["samples"] != len(ordered):
        raise ValueError(f"chunk sample count differs: {sample['name']}")
    for key, percentile in (("median", 500), ("p99", 990), ("p999", 999)):
        index = max(0, math.ceil(len(ordered) * percentile / 1000) - 1)
        if sample[key] != ordered[index]:
            raise ValueError(f"chunk quantile differs: {sample['name']}/{key}")
    if sample["maximum"] != ordered[-1]:
        raise ValueError(f"chunk maximum differs: {sample['name']}")


def validate_wal(chunk):
    delta = chunk.get("wal_file_delta_bytes")
    if (not integer(chunk.get("wal_before_bytes")) or not integer(chunk.get("wal_after_bytes"))
            or not isinstance(delta, int) or isinstance(delta, bool)
            or delta != chunk["wal_after_bytes"] - chunk["wal_before_bytes"]):
        raise ValueError("chunk WAL file-length observation missing or inconsistent")


def attachment_collection_plan(details):
    # Payload unlinking is separate; metadata collection must seek one task.
    if any("USE TEMP B-TREE" in detail or re.search(r"\bSCAN\b", detail) for detail in details):
        return False
    return any(re.fullmatch(
        r"SEARCH file_attachments USING INDEX file_attachments_task \(task_id=\?\)", detail)
        for detail in details)


def validate(report):
    if report.get("format") != "maple-store-bench/1" or report.get("profile") not in SCALES:
        raise ValueError("missing benchmark result or unsupported format/profile")
    if report.get("generator_seed") != SEED or report.get("measurement_boundary") != BOUNDARY:
        raise ValueError("benchmark generator or measurement identity differs")
    runs = report.get("runs", [])
    if len(runs) != 2 or {run.get("mode") for run in runs} != {"plain", "keyed"}:
        raise ValueError("runner evidence requires exactly one plain and one keyed run")
    scenarios = {}
    full = report["profile"] == "full"
    for run in runs:
        mode = run["mode"]
        scale = SCALES[report["profile"]]
        if run.get("scale") != scale:
            raise ValueError("runner gate requires its canonical dataset scale")
        pragmas = run.get("pragmas", [])
        roles = {p["role"]: p for p in pragmas}
        if len(pragmas) != 5 or set(roles) != {"writer", "reader-1", "reader-2", "bulk", "checkpointer"}:
            raise ValueError("connection policy evidence missing or duplicated")
        writer = roles["writer"]
        if (writer["synchronous"], writer["fullfsync"], writer["wal_autocheckpoint"]) != (2, 1, 0):
            raise ValueError("source measurement does not use FULL/platform sync")
        if writer.get("secure_delete") != 1:
            raise ValueError("chunk measurements require secure_delete=ON")
        if any(p["checkpoint_fullfsync"] != 1 or p["wal_autocheckpoint"] != 0 or p["temp_store"] != 2
               or p.get("journal_mode", "").lower() != "wal" for p in pragmas):
            raise ValueError("a connection changed checkpoint/temp-file/WAL policy")
        seen_plans = set()
        for plan in run.get("plans", []):
            identity = (plan.get("name"), plan.get("statistics"))
            if identity in seen_plans or type(plan.get("statistics")) is not bool or plan.get("passed") is not True:
                raise ValueError("query plan guard failed, duplicated or missing")
            seen_plans.add(identity)
            expected_index = PLAN_INDEXES.get(plan["name"])
            details = plan.get("details", [])
            index_matches = (attachment_collection_plan(details) if plan["name"] == "attachment-collection"
                             else any(expected_index in detail for detail in details) if expected_index else True)
            if expected_index and (plan.get("expected_index") != expected_index
                                   or not index_matches
                                   or any("USE TEMP B-TREE" in detail for detail in details)):
                raise ValueError("query plan evidence differs from its required index")
        if not {(name, stats) for name in PLAN_INDEXES for stats in (False, True)} <= seen_plans:
            raise ValueError("query plan guard missing with or without statistics")
        seed = run.get("seed", {})
        if (seed.get("layout") != "globally timestamp-interleaved source and projector rows"
                or seed.get("tasks") != scale["tasks"] or seed.get("entries") != scale["entries"]
                or seed.get("seed_batch_entries") != 1000 or not integer(seed.get("payload_bytes"), 1)
                or seed.get("durability") != "FULL with fullfsync; synthetic fixture batches outside timed production source-append measurements"):
            raise ValueError("reference fixture identity/counts/interleaving differ")
        samples = {}
        for sample in run.get("scenarios", []):
            name = sample["name"]
            if name in samples:
                raise ValueError(f"duplicate scenario: {mode}/{name}")
            if sample.get("unit") != "microseconds" or not integer(sample.get("samples"), 1):
                raise ValueError("empty sample or unsupported timing unit")
            if any(not number(sample.get(key)) for key in ("median", "p99", "p999", "maximum", "total_micros")):
                raise ValueError("invalid timing result")
            if not sample["median"] <= sample["p99"] <= sample["p999"] <= sample["maximum"]:
                raise ValueError("inconsistent quantiles")
            if (not integer(sample.get("over_5ms")) or sample["over_5ms"] > sample["samples"]
                    or (sample["maximum"] <= 5000) != (sample["over_5ms"] == 0)
                    or not sample["maximum"] <= sample["total_micros"] <= sample["samples"] * sample["maximum"]):
                raise ValueError("inconsistent timing totals/tail count")
            samples[name] = sample
            scenarios[f"{mode}/{name}"] = sample["median"]
        required = dict(FIXED_SAMPLES)
        required.update({name: 100 if full else 20 for name in REPEATED})
        required.update({name: 20 if full else 5 for name in ("list_50_cold", "open_100_cold", "history_200_deep_cold")})
        required["durable_source_append"] = 2000 if full else 200
        if full and mode == "plain":
            required["plain_to_keyed_with_floor_kdf"] = 1
        for name, minimum in required.items():
            if name not in samples:
                raise ValueError(f"required scenario missing: {mode}/{name}")
            if samples[name]["samples"] < minimum:
                raise ValueError(f"scenario sample count reduced: {mode}/{name}")
        validate_observations(run, samples, full)
    return scenarios


def validate_observations(run, samples, full):
    obs = run.get("observations", {})
    if not integer(obs.get("history_depth_rows"), 40000 if full else 1):
        raise ValueError("required history depth missing (full run requires depth >=40000)")
    if obs.get("attachment_bytes") != 10 * MIB or not integer(obs.get("oversized_row_total_bytes"), 20 * MIB):
        raise ValueError("attachment/oversized-row fixture size differs")
    if obs.get("attachment_storage") != "plaintext session-owned file; database metadata only":
        raise ValueError("attachment storage boundary differs")
    for name in ("model_context_full_cold", "model_context_boundary_cold"):
        counts = obs.get(f"{name}_entries", [])
        if len(counts) != samples[name]["samples"] or not all(integer(count, 1) for count in counts):
            raise ValueError("context entry-count evidence missing")
        if name == "model_context_full_cold" and any(count != run["scale"]["long_entries"] for count in counts):
            raise ValueError("full context did not load the canonical long task")
        if name == "model_context_boundary_cold" and any(count >= run["scale"]["long_entries"] for count in counts):
            raise ValueError("boundary context did not exercise a partial load")
    chunks = obs.get("timeline_rebuild_chunks", [])
    if not chunks or chunks[-1].get("done") is not True or any(chunk.get("done") for chunk in chunks[:-1]):
        raise ValueError("complete rebuild chunk evidence missing")
    for chunk in chunks:
        validate_wal(chunk)
        if (not all(integer(chunk.get(key)) for key in ("writer_micros", "prepare_micros", "entries", "bytes"))
                or chunk["entries"] > 1000 or (chunk["bytes"] > MIB and chunk["entries"] != 1)):
            raise ValueError("rebuild chunk exceeds row/byte caps without an isolated row")
    if not any(chunk["entries"] > 0 for chunk in chunks):
        raise ValueError("rebuild processed no source entries")
    sample_values_match(samples["timeline_rebuild_writer_chunk"], [chunk["writer_micros"] for chunk in chunks])
    sample_values_match(samples["timeline_rebuild_prepare_chunk"], [chunk["prepare_micros"] for chunk in chunks])
    old_chunks = obs.get("old_epoch_collection_chunks", [])
    if not old_chunks:
        raise ValueError("old-epoch collection chunk evidence missing")
    for chunk in old_chunks:
        validate_wal(chunk)
        if (not all(integer(chunk.get(key)) for key in ("writer_micros", "rows", "bytes"))
                or chunk["rows"] < 1 or chunk["rows"] > 1000
                or (chunk["bytes"] > MIB and chunk["rows"] != 1)):
            raise ValueError("old-epoch collection exceeds row/byte caps without an isolated row")
    sample_values_match(samples["old_epoch_collection_chunk"], [chunk["writer_micros"] for chunk in old_chunks])
    collection = obs.get("oversized_collection_chunks", [])
    if (not collection or collection[-1].get("stage") != "TaskMetadata" or collection[-1].get("done") is not True
            or any(chunk.get("done") for chunk in collection[:-1])):
        raise ValueError("complete collection/final metadata evidence missing")
    stages = set()
    for chunk in collection:
        validate_wal(chunk)
        stages.add(chunk.get("stage"))
        if not all(integer(chunk.get(key)) for key in ("rows", "bytes", "elapsed_micros")):
            raise ValueError("collection chunk measurements missing")
        if chunk["stage"] == "Attachments":
            # One owned attachment subtree: at most 128 filesystem mutations
            # plus its metadata row. Bytes count the declared payload only on
            # final removal, not adjacent model outputs or rewritten SQL bytes.
            if not 1 <= chunk["rows"] <= 129 or chunk["bytes"] > 30 * MIB or chunk.get("oversized_row") is not False:
                raise ValueError("attachment cleanup exceeds its bounded subtree step")
            continue
        oversized = chunk["bytes"] > MIB
        if (type(chunk.get("oversized_row")) is not bool or chunk["oversized_row"] != oversized
                or chunk["rows"] > 1000 or (oversized and (chunk["rows"] != 1 or chunk["stage"] not in {"Timeline", "Entries", "Summaries"}))):
            raise ValueError("collection chunk exceeds caps without an isolated row")
    if not {"Timeline", "Entries", "Attachments", "TaskMetadata"} <= stages or not any(chunk["oversized_row"] for chunk in collection):
        raise ValueError("collection lost its oversized source-row/file fixture")
    if sum(chunk["bytes"] for chunk in collection if chunk["stage"] == "Attachments") != 10 * MIB:
        raise ValueError("collection lost its complete 10 MiB file fixture")
    summary = samples["oversized_task_collection_chunk"]
    if summary["samples"] != len(collection) or summary["maximum"] < max(chunk["elapsed_micros"] for chunk in collection):
        raise ValueError("collection summary lost a measured chunk")


def absolute_results(report):
    checks = []
    # Give only the full macOS list gate 25% hosted-runner headroom. Other
    # platforms/diagnostics retain the 1 ms target; relative comparisons remain
    # independent of this absolute budget.
    list_budget = 1250 if (report["profile"] == "full"
                           and report.get("machine_before", {}).get("os") == "macos") else 1000

    def check(mode, name, actual, budget, scope, statistic="median", oversized=False):
        status = "passed" if actual <= budget else ("needs_decision" if oversized else "failed")
        checks.append({"scenario": f"{mode}/{name}", "statistic": statistic, "actual_us": actual,
                       "budget_us": budget, "status": status, "gate_scope": scope})

    for run in report["runs"]:
        mode = run["mode"]
        samples = {sample["name"]: sample for sample in run["scenarios"]}
        obs = run["observations"]
        if mode == "keyed":
            for name, budget, scope in (
                    ("list_50_cold", list_budget, "Store list, fresh SQLite caches; OS cache warm"),
                    ("open_100_cold", 2000, "Store page and plan lookup; UI/live overlay unmeasured"),
                    ("raw_connection_open", 1000, "one raw-key SQLite connection; excludes host startup/password KDF"),
                    ("model_context_boundary_cold", 50000, "first partial context load including parse; OS cache warm"),
                    ("attachment_10mib_first_read", 25000, "first plaintext-file range assembly with keyed metadata; OS cache warm, no network")):
                check(mode, name, samples[name]["median"], budget, scope)
            if report["profile"] == "full":
                check(mode, "history_200_deep_cold", samples["history_200_deep_cold"]["median"], 3000,
                      f"Store history, fresh SQLite caches at depth {obs['history_depth_rows']}; OS cache warm")
        if report["profile"] == "full":
            check(mode, "model_context_full_cold", samples["model_context_full_cold"]["median"],
                  250000 if mode == "keyed" else 150000, "full canonical 20k-entry context including parse; OS cache warm")
        for name, chunks, field in (
                ("timeline_rebuild_writer_chunk", obs["timeline_rebuild_chunks"], "writer_micros"),
                ("old_epoch_collection_chunk", obs["old_epoch_collection_chunks"], "writer_micros"),
                ("oversized_task_collection_chunk", obs["oversized_collection_chunks"], "elapsed_micros")):
            capped = [chunk[field] for chunk in chunks if chunk["bytes"] <= MIB or chunk.get("stage") == "Attachments"]
            oversized = [chunk[field] for chunk in chunks if chunk["bytes"] > MIB and chunk.get("stage") != "Attachments"]
            if capped:
                check(mode, f"{name}/capped", max(capped), 50000,
                      "maximum capped chunk or bounded attachment-subtree cleanup call; includes admission/commit, bounds writer hold", "maximum")
            if oversized:
                check(mode, f"{name}/isolated_oversized_row", max(oversized), 50000,
                      "isolated oversized row; miss requires chunk storage or an approved measured alternative",
                      "maximum", oversized=True)
    status = "failed" if any(check["status"] == "failed" for check in checks) else (
        "needs_decision" if any(check["status"] == "needs_decision" for check in checks) else "passed")
    return {"status": status, "enforced": report["profile"] == "full", "checks": checks,
            "unmeasured": GAPS + (["CI fixture does not exercise 40k-depth history or a full 20k-entry context"]
                                    if report["profile"] == "ci" else [])}


def compare(report, reference, profile, factor, diagnostic=False):
    actual = validate(report)
    expected_profile, expected_os = PROFILES[profile]
    if report["profile"] != expected_profile:
        raise ValueError("runner reference and dataset profile differ")
    measured_os = report.get("machine_before", {}).get("os")
    if measured_os != expected_os and not diagnostic:
        raise ValueError(f"{profile} requires {expected_os}, measured {measured_os}; use --diagnostic for workload evidence only")
    absolute = absolute_results(report)
    if diagnostic:
        absolute["enforced"] = False
        return {"gate_status": "diagnostic_only", "profile": profile, "measured_os": measured_os,
                "expected_runner_os": expected_os, "relative_gate_status": "not_compared", "regressions": [],
                "absolute": absolute, "message": "Workload diagnostic only; no runner reference was compared or established."}
    expected = reference["profiles"][profile]["scenario_medians_us"]
    regressions = []
    for name, before in expected.items():
        if name not in actual:
            raise ValueError(f"recorded scenario missing: {name}")
        if not number(before) or before <= 0:
            raise ValueError(f"invalid reference median: {name}")
        if actual[name] > before * factor:
            regressions.append({"scenario": name, "reference_us": before, "actual_us": actual[name],
                                "factor": actual[name] / before, "allowed_factor": factor})
    pending = sorted(set(actual) - set(expected))
    relative = "failed" if regressions else ("baseline_pending" if pending else "passed")
    status = relative
    if absolute["enforced"] and absolute["status"] != "passed":
        status = "failed" if relative == "failed" else absolute["status"]
    return {"gate_status": status, "profile": profile, "measured_os": measured_os,
            "relative_gate_status": relative, "missing_reference_scenarios": pending,
            "regressions": regressions, "absolute": absolute,
            "message": ("Record actual runner measurements in a reviewed PR; absolute results do not establish a relative baseline."
                        if pending else "Relative medians and documented measured scopes evaluated independently.")}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("result", type=Path)
    parser.add_argument("--reference", type=Path, default=Path(__file__).parents[1] / "reference.json")
    parser.add_argument("--profile", choices=tuple(PROFILES), required=True)
    parser.add_argument("--diagnostic", action="store_true", help="Validate a native workload without comparing or recording a runner reference")
    parser.add_argument("--require-reference", action="store_true")
    parser.add_argument("--record-reference", action="store_true")
    parser.add_argument("--run-url", help="Required provenance for an explicit baseline record")
    args = parser.parse_args()
    if args.diagnostic and (args.record_reference or args.require_reference):
        raise ValueError("diagnostics cannot record or require a runner reference")
    payload = args.result.read_bytes()
    report = json.loads(payload)
    reference = json.loads(args.reference.read_text())
    outcome = compare(report, reference, args.profile, 10 if args.profile == "linux-ci" else 2, args.diagnostic)
    if args.record_reference:
        if not args.run_url or not args.run_url.startswith("https://github.com/"):
            raise ValueError("recording requires the actual GitHub runner result URL")
        if outcome["absolute"]["enforced"] and outcome["absolute"]["status"] != "passed":
            raise ValueError("unresolved absolute/oversized-row limits prevent recording an accepted full-run reference")
        reference["profiles"][args.profile] = {
            "status": "recorded", "scenario_medians_us": validate(report),
            "provenance": {"run_url": args.run_url, "result_sha256": hashlib.sha256(payload).hexdigest(),
                           "machine": report["machine_before"], "timestamp_unix": report["timestamp_unix"]}}
        args.reference.write_text(json.dumps(reference, indent=2, sort_keys=True) + "\n")
        print(json.dumps({"gate_status": "reference_recorded", "profile": args.profile}))
        return
    print(json.dumps(outcome, indent=2))
    if outcome["relative_gate_status"] == "baseline_pending":
        print("::notice::Runner baseline is pending. Absolute results do not establish a runner-relative latency pass.")
    if (outcome["gate_status"] in {"failed", "needs_decision"}
            or (args.require_reference and outcome["relative_gate_status"] == "baseline_pending")):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
