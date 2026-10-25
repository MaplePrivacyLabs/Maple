import copy
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "scripts/evaluate.py"
SPEC = importlib.util.spec_from_file_location("evaluate", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def sample(name, values):
    values = sorted(values)
    percentile = lambda n: values[max(0, (len(values) * n + 999) // 1000 - 1)]
    return {"name": name, "unit": "microseconds", "samples": len(values),
            "median": percentile(500), "p99": percentile(990), "p999": percentile(999),
            "maximum": values[-1], "total_micros": sum(values),
            "over_5ms": sum(value > 5000 for value in values)}


def wal(before=100, after=120):
    return {"wal_before_bytes": before, "wal_after_bytes": after, "wal_file_delta_bytes": after - before}


def report(full=False):
    profile = "full" if full else "ci"
    scale = MODULE.SCALES[profile]
    runs = []
    for mode in ("plain", "keyed"):
        counts = dict(MODULE.FIXED_SAMPLES)
        counts.update({name: 100 if full else 20 for name in MODULE.REPEATED})
        counts.update({name: 20 if full else 5 for name in ("list_50_cold", "open_100_cold", "history_200_deep_cold")})
        counts["durable_source_append"] = 2000 if full else 200
        if full and mode == "plain":
            counts["plain_to_keyed_with_floor_kdf"] = 1
        scenarios = {name: sample(name, [100] * count) for name, count in counts.items()}
        rebuild = [{"prepare_micros": 100, "writer_micros": 100, "entries": 100, "bytes": 1000, "done": False, **wal()},
                   {"prepare_micros": 100, "writer_micros": 100, "entries": 0, "bytes": 0, "done": True, **wal()}]
        old = [{"writer_micros": 100, "rows": 100, "bytes": 1000, **wal()}]
        collection = [{"stage": stage, "rows": 3 if stage == "Attachments" else 1, "bytes": size, "oversized_row": size > MODULE.MIB and stage != "Attachments",
                       "elapsed_micros": 100, "done": stage == "TaskMetadata", **wal()}
                      for stage, size in (("Timeline", 20 * MODULE.MIB), ("Entries", 20 * MODULE.MIB),
                                          ("Attachments", 10 * MODULE.MIB), ("TaskMetadata", 0))]
        scenarios["timeline_rebuild_writer_chunk"] = sample("timeline_rebuild_writer_chunk", [100, 100])
        scenarios["timeline_rebuild_prepare_chunk"] = sample("timeline_rebuild_prepare_chunk", [100, 100])
        scenarios["oversized_task_collection_chunk"] = sample("oversized_task_collection_chunk", [100] * 4)
        runs.append({"mode": mode, "scale": copy.deepcopy(scale),
                     "pragmas": [{"role": role, "synchronous": 2, "fullfsync": 1, "wal_autocheckpoint": 0,
                                  "checkpoint_fullfsync": 1, "temp_store": 2, "secure_delete": 1, "journal_mode": "wal"}
                                 for role in ("writer", "reader-1", "reader-2", "bulk", "checkpointer")],
                     "plans": [{"name": name, "statistics": stats, "expected_index": index,
                                "passed": True, "details": [
                                    f"SEARCH file_attachments USING INDEX {index} (task_id=?)"
                                    if name == "attachment-collection" else f"SEARCH USING INDEX {index}"]}
                               for name, index in MODULE.PLAN_INDEXES.items() for stats in (False, True)],
                     "seed": {"layout": "globally timestamp-interleaved source and projector rows",
                              "tasks": scale["tasks"], "entries": scale["entries"], "payload_bytes": scale["entries"] * 2000,
                              "seed_batch_entries": 1000,
                              "durability": "FULL with fullfsync; synthetic fixture batches outside timed production source-append measurements"},
                     "scenarios": list(scenarios.values()),
                     "observations": {"history_depth_rows": 40000 if full else 1800, "attachment_bytes": 10 * MODULE.MIB,
                                      "attachment_storage": "plaintext session-owned file; database metadata only",
                                      "oversized_row_total_bytes": 20 * MODULE.MIB,
                                      "model_context_full_cold_entries": [scale["long_entries"]] * 5,
                                      "model_context_boundary_cold_entries": [100] * 5,
                                      "timeline_rebuild_chunks": rebuild, "old_epoch_collection_chunks": old,
                                      "oversized_collection_chunks": collection}})
    return {"format": "maple-store-bench/1", "profile": profile, "generator_seed": MODULE.SEED,
            "measurement_boundary": MODULE.BOUNDARY, "timestamp_unix": 1,
            "machine_before": {"os": "macos" if full else "linux"}, "runs": runs}


def reference(actual=None, profile="linux-ci"):
    return {"profiles": {profile: {"scenario_medians_us": MODULE.validate(actual) if actual else {}}}}


def set_sample(actual, mode, name, values):
    run = next(run for run in actual["runs"] if run["mode"] == mode)
    previous = next(item for item in run["scenarios"] if item["name"] == name)
    run["scenarios"][run["scenarios"].index(previous)] = sample(name, values)


def check(outcome, name):
    return next(item for item in outcome["absolute"]["checks"] if item["scenario"] == name)


class EvidenceGateTests(unittest.TestCase):
    def test_weaker_durability_cannot_pass_without_a_baseline(self):
        actual = report()
        actual["runs"][0]["pragmas"][0]["synchronous"] = 1
        with self.assertRaisesRegex(ValueError, "FULL"):
            MODULE.validate(actual)

    def test_missing_reference_is_pending_even_when_absolute_diagnostics_pass(self):
        outcome = MODULE.compare(report(), reference(), "linux-ci", 10)
        self.assertEqual(outcome["gate_status"], "baseline_pending")
        self.assertEqual(outcome["absolute"]["status"], "passed")
        self.assertFalse(outcome["absolute"]["enforced"])

    def test_full_absolute_pass_does_not_establish_a_relative_baseline(self):
        outcome = MODULE.compare(report(True), reference(profile="macos-full"), "macos-full", 2)
        self.assertEqual(outcome["gate_status"], "baseline_pending")
        self.assertTrue(outcome["absolute"]["enforced"])

    def test_recorded_runner_baseline_and_absolute_limits_can_pass(self):
        actual = report(True)
        self.assertEqual(MODULE.compare(actual, reference(actual, "macos-full"), "macos-full", 2)["gate_status"], "passed")

    def test_regression_and_recorded_missing_scenario_fail(self):
        baseline = reference(report())
        actual = report()
        set_sample(actual, "keyed", "durable_source_append", [1001] * 200)
        self.assertEqual(MODULE.compare(actual, baseline, "linux-ci", 10)["gate_status"], "failed")
        baseline["profiles"]["linux-ci"]["scenario_medians_us"]["keyed/missing"] = 1
        with self.assertRaisesRegex(ValueError, "missing"):
            MODULE.compare(actual, baseline, "linux-ci", 10)

    def test_partial_relative_baseline_remains_pending(self):
        baseline = {"profiles": {"linux-ci": {"scenario_medians_us": {"keyed/durable_source_append": 100}}}}
        outcome = MODULE.compare(report(), baseline, "linux-ci", 10)
        self.assertEqual(outcome["gate_status"], "baseline_pending")
        self.assertIn("plain/list_50_cold", outcome["missing_reference_scenarios"])

    def test_documented_absolute_limits_are_independent_of_relative_factor(self):
        cases = (("list_50_cold", 1251, 20), ("open_100_cold", 2001, 20),
                 ("history_200_deep_cold", 3001, 20), ("raw_connection_open", 1001, 100),
                 ("model_context_full_cold", 250001, 5), ("model_context_boundary_cold", 50001, 5),
                 ("attachment_10mib_first_read", 25001, 1))
        for name, value, count in cases:
            with self.subTest(scenario=name):
                actual = report(True)
                set_sample(actual, "keyed", name, [value] * count)
                outcome = MODULE.compare(actual, reference(actual, "macos-full"), "macos-full", 2)
                self.assertEqual(outcome["relative_gate_status"], "passed")
                self.assertEqual(outcome["gate_status"], "failed")
                self.assertEqual(check(outcome, f"keyed/{name}")["status"], "failed")

    def test_full_macos_list_budget_accepts_headroom_but_rejects_over_budget(self):
        for median, status in ((1019, "passed"), (1250, "passed"), (1251, "failed")):
            with self.subTest(median=median):
                actual = report(True)
                set_sample(actual, "keyed", "list_50_cold", [median] * 20)
                outcome = MODULE.compare(actual, reference(actual, "macos-full"), "macos-full", 2)
                self.assertEqual(check(outcome, "keyed/list_50_cold")["budget_us"], 1250)
                self.assertEqual(outcome["gate_status"], status)

    def test_linux_list_diagnostic_retains_one_millisecond_target(self):
        actual = report()
        set_sample(actual, "keyed", "list_50_cold", [1001] * 5)
        outcome = MODULE.compare(actual, reference(actual), "linux-ci", 10)
        self.assertEqual(check(outcome, "keyed/list_50_cold")["budget_us"], 1000)
        self.assertEqual(check(outcome, "keyed/list_50_cold")["status"], "failed")
        self.assertFalse(outcome["absolute"]["enforced"])

    def test_macos_list_relative_regression_still_fails_within_absolute_headroom(self):
        baseline = report(True)
        set_sample(baseline, "keyed", "list_50_cold", [400] * 20)
        actual = copy.deepcopy(baseline)
        set_sample(actual, "keyed", "list_50_cold", [1019] * 20)
        outcome = MODULE.compare(actual, reference(baseline, "macos-full"), "macos-full", 2)
        self.assertEqual(check(outcome, "keyed/list_50_cold")["status"], "passed")
        self.assertEqual(outcome["relative_gate_status"], "failed")
        self.assertEqual(outcome["gate_status"], "failed")
        self.assertEqual(outcome["regressions"][0]["scenario"], "keyed/list_50_cold")

    def test_plain_full_context_has_150ms_limit(self):
        actual = report(True)
        set_sample(actual, "plain", "model_context_full_cold", [150001] * 5)
        outcome = MODULE.compare(actual, reference(actual, "macos-full"), "macos-full", 2)
        self.assertEqual(check(outcome, "plain/model_context_full_cold")["budget_us"], 150000)
        self.assertEqual(outcome["gate_status"], "failed")

    def test_small_dataset_does_not_claim_full_context_or_depth_gate(self):
        outcome = MODULE.compare(report(), reference(), "linux-ci", 10)
        names = {item["scenario"] for item in outcome["absolute"]["checks"]}
        self.assertNotIn("keyed/history_200_deep_cold", names)
        self.assertNotIn("keyed/model_context_full_cold", names)
        self.assertIn("UI/live overlay", check(outcome, "keyed/open_100_cold")["gate_scope"])
        self.assertIn("OS cache warm", check(outcome, "keyed/attachment_10mib_first_read")["gate_scope"])

    def test_single_slow_capped_chunk_cannot_hide_behind_median(self):
        for family, field in (("timeline_rebuild_chunks", "writer_micros"),
                              ("old_epoch_collection_chunks", "writer_micros"),
                              ("oversized_collection_chunks", "elapsed_micros")):
            with self.subTest(family=family):
                actual = report(True)
                obs = actual["runs"][1]["observations"]
                if family == "oversized_collection_chunks":
                    obs[family][-1][field] = 50001
                    set_sample(actual, "keyed", "oversized_task_collection_chunk", [100, 100, 100, 50001])
                elif family == "timeline_rebuild_chunks":
                    obs[family] = [dict(obs[family][0]) for _ in range(20)] + [obs[family][-1]]
                    obs[family][0][field] = 50001
                    set_sample(actual, "keyed", "timeline_rebuild_writer_chunk", [100] * 20 + [50001])
                    set_sample(actual, "keyed", "timeline_rebuild_prepare_chunk", [100] * 21)
                else:
                    obs[family] = [dict(obs[family][0]) for _ in range(20)]
                    obs[family][0][field] = 50001
                    set_sample(actual, "keyed", "old_epoch_collection_chunk", [100] * 19 + [50001])
                outcome = MODULE.compare(actual, reference(actual, "macos-full"), "macos-full", 2)
                self.assertEqual(outcome["gate_status"], "failed")

    def test_oversized_miss_requires_decision_separately_from_capped_chunks(self):
        actual = report(True)
        actual["runs"][1]["observations"]["oversized_collection_chunks"][0]["elapsed_micros"] = 50001
        set_sample(actual, "keyed", "oversized_task_collection_chunk", [100, 100, 100, 50001])
        outcome = MODULE.compare(actual, reference(profile="macos-full"), "macos-full", 2)
        self.assertEqual(outcome["gate_status"], "needs_decision")
        self.assertEqual(outcome["relative_gate_status"], "baseline_pending")
        self.assertEqual(check(outcome, "keyed/oversized_task_collection_chunk/capped")["status"], "passed")
        self.assertEqual(check(outcome, "keyed/oversized_task_collection_chunk/isolated_oversized_row")["status"], "needs_decision")

    def test_old_epoch_oversized_chunk_has_separate_decision(self):
        actual = report(True)
        chunk = actual["runs"][1]["observations"]["old_epoch_collection_chunks"][0]
        chunk.update(rows=1, bytes=2 * MODULE.MIB, writer_micros=50001)
        set_sample(actual, "keyed", "old_epoch_collection_chunk", [50001])
        outcome = MODULE.compare(actual, reference(profile="macos-full"), "macos-full", 2)
        self.assertEqual(check(outcome, "keyed/old_epoch_collection_chunk/isolated_oversized_row")["status"], "needs_decision")

    def test_full_platform_sync_append_has_no_invented_absolute_budget(self):
        actual = report(True)
        set_sample(actual, "keyed", "durable_source_append", [1000000] * 2000)
        outcome = MODULE.compare(actual, reference(profile="macos-full"), "macos-full", 2)
        self.assertEqual(outcome["gate_status"], "baseline_pending")
        self.assertFalse(any(item["scenario"].endswith("/durable_source_append") for item in outcome["absolute"]["checks"]))

    def test_required_modes_scenarios_counts_and_data_cannot_be_removed(self):
        mutations = (
            (lambda actual: actual["runs"].pop(), "plain and one keyed"),
            (lambda actual: actual["runs"][1]["scenarios"].pop(), "required scenario missing"),
            (lambda actual: actual["runs"][0]["scenarios"].append(copy.deepcopy(actual["runs"][0]["scenarios"][0])), "duplicate scenario"),
            (lambda actual: actual["runs"][0]["scenarios"][0].update(samples=1, total_micros=100), "sample count reduced"),
            (lambda actual: actual["runs"][0]["scale"].update(entries=800), "canonical dataset"),
            (lambda actual: actual["runs"][0]["seed"].update(entries=800), "fixture identity"),
            (lambda actual: actual.update(generator_seed=1), "generator"),
            (lambda actual: actual["runs"][0]["plans"].pop(), "without statistics"),
            (lambda actual: actual["runs"][0]["pragmas"][0].update(secure_delete=0), "secure_delete"),
        )
        for mutate, message in mutations:
            with self.subTest(message=message):
                actual = report()
                mutate(actual)
                with self.assertRaisesRegex(ValueError, message):
                    MODULE.validate(actual)

    def test_depth_and_context_identity_cannot_bypass_full_gate(self):
        for field, value, message in (("history_depth_rows", 39999, "depth"),
                                      ("model_context_full_cold_entries", [1000] * 5, "canonical long"),
                                      ("model_context_boundary_cold_entries", [20000] * 5, "partial")):
            with self.subTest(field=field):
                actual = report(True)
                actual["runs"][1]["observations"][field] = value
                with self.assertRaisesRegex(ValueError, message):
                    MODULE.validate(actual)

    def test_caps_and_observed_maxima_are_validated(self):
        actual = report()
        actual["runs"][1]["observations"]["old_epoch_collection_chunks"][0].update(rows=2, bytes=2 * MODULE.MIB)
        with self.assertRaisesRegex(ValueError, "without an isolated row"):
            MODULE.validate(actual)
        actual = report()
        actual["runs"][1]["observations"]["timeline_rebuild_chunks"][0]["writer_micros"] = 50001
        with self.assertRaisesRegex(ValueError, "chunk quantile|chunk maximum"):
            MODULE.validate(actual)

    def test_attachment_collection_seeks_task_metadata(self):
        MODULE.validate(report())

    def test_attachment_collection_rejects_wrong_or_unbounded_plans(self):
        bad_details = (
            ["SEARCH file_attachments USING INDEX file_attachments_task (task_id>?)"],
            ["SEARCH file_attachments USING INDEX unrelated (task_id=?)"],
            ["SEARCH file_attachments USING INDEX file_attachments_task_extra (task_id=?)"],
            ["SCAN file_attachments USING INDEX file_attachments_task"],
            ["SEARCH other_table USING INDEX file_attachments_task (task_id=?)"],
            ["SEARCH file_attachments USING INDEX file_attachments_task (task_id=?)", "SCAN file_attachments"],
            ["SEARCH file_attachments USING INDEX file_attachments_task (task_id=?)", "USE TEMP B-TREE FOR ORDER BY"],
        )
        for details in bad_details:
            with self.subTest(details=details):
                actual = report()
                plan = next(plan for plan in actual["runs"][1]["plans"] if plan["name"] == "attachment-collection")
                plan["details"] = details
                with self.assertRaisesRegex(ValueError, "required index"):
                    MODULE.validate(actual)

    def test_attachment_collection_keeps_expected_field_and_actual_pass_guard(self):
        for changes in ({"expected_index": "wrong_index"}, {"passed": False}):
            with self.subTest(changes=changes):
                actual = report()
                plan = next(plan for plan in actual["runs"][1]["plans"] if plan["name"] == "attachment-collection")
                plan.update(changes)
                with self.assertRaises(ValueError):
                    MODULE.validate(actual)

    def test_file_collection_is_bounded_without_an_oversized_row_exception(self):
        for rows in (1, 3, 128, 129):
            actual = report()
            chunk = next(chunk for chunk in actual["runs"][0]["observations"]["oversized_collection_chunks"]
                         if chunk["stage"] == "Attachments")
            chunk["rows"] = rows
            MODULE.validate(actual)
        for changes in ({"rows": 0}, {"rows": 130}, {"bytes": 30 * MODULE.MIB + 1}, {"oversized_row": True}):
            with self.subTest(changes=changes):
                actual = report()
                chunk = next(chunk for chunk in actual["runs"][0]["observations"]["oversized_collection_chunks"]
                             if chunk["stage"] == "Attachments")
                chunk.update(changes)
                with self.assertRaisesRegex(ValueError, "bounded subtree step"):
                    MODULE.validate(actual)
        actual = report()
        chunk = next(chunk for chunk in actual["runs"][0]["observations"]["oversized_collection_chunks"]
                     if chunk["stage"] == "Attachments")
        chunk["bytes"] = MODULE.MIB
        with self.assertRaisesRegex(ValueError, "complete 10 MiB file"):
            MODULE.validate(actual)

    def test_file_cleanup_keeps_the_normal_latency_gate(self):
        actual = report(True)
        chunk = next(chunk for chunk in actual["runs"][0]["observations"]["oversized_collection_chunks"]
                     if chunk["stage"] == "Attachments")
        chunk["rows"] = 129
        chunk["elapsed_micros"] = 50001
        set_sample(actual, "plain", "oversized_task_collection_chunk", [100, 100, 100, 50001])
        MODULE.validate(actual)
        outcome = MODULE.absolute_results(actual)
        result = next(item for item in outcome["checks"]
                      if item["scenario"] == "plain/oversized_task_collection_chunk/capped")
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["actual_us"], 50001)

    def test_old_blob_storage_evidence_cannot_pass_as_file_storage(self):
        actual = report()
        del actual["runs"][0]["observations"]["attachment_storage"]
        with self.assertRaisesRegex(ValueError, "storage boundary"):
            MODULE.validate(actual)

    def test_signed_wal_growth_is_observation_not_bytes_written(self):
        actual = report()
        actual["runs"][0]["observations"]["timeline_rebuild_chunks"][0].update(wal(1000, 100))
        MODULE.validate(actual)
        actual["runs"][0]["observations"]["timeline_rebuild_chunks"][0]["wal_file_delta_bytes"] = 1
        with self.assertRaisesRegex(ValueError, "WAL file-length"):
            MODULE.validate(actual)

    def test_platform_mismatch_requires_explicit_diagnostic(self):
        actual = report()
        actual["machine_before"]["os"] = "macos"
        with self.assertRaisesRegex(ValueError, "linux-ci requires linux"):
            MODULE.compare(actual, reference(), "linux-ci", 10)
        outcome = MODULE.compare(actual, reference(), "linux-ci", 10, diagnostic=True)
        self.assertEqual(outcome["gate_status"], "diagnostic_only")
        self.assertEqual(outcome["relative_gate_status"], "not_compared")
        self.assertFalse(outcome["absolute"]["enforced"])

    def test_cli_requires_reference_and_does_not_record_unresolved_full_limits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            actual = report(True)
            actual["runs"][1]["observations"]["oversized_collection_chunks"][0]["elapsed_micros"] = 50001
            set_sample(actual, "keyed", "oversized_task_collection_chunk", [100, 100, 100, 50001])
            result, baseline = root / "result.json", root / "reference.json"
            result.write_text(json.dumps(actual))
            baseline.write_text(json.dumps(reference(profile="macos-full")))
            before = baseline.read_bytes()
            command = [sys.executable, str(SCRIPT), str(result), "--reference", str(baseline), "--profile", "macos-full"]
            failed = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(failed.returncode, 1)
            self.assertEqual(json.loads(failed.stdout.split("\n::notice::")[0])["gate_status"], "needs_decision")
            recorded = subprocess.run(command + ["--record-reference", "--run-url", "https://github.com/example/actions/runs/1"], capture_output=True, text=True)
            self.assertNotEqual(recorded.returncode, 0)
            self.assertEqual(baseline.read_bytes(), before)
            result.write_text(json.dumps(report(True)))
            required = subprocess.run(command + ["--require-reference"], capture_output=True, text=True)
            self.assertEqual(required.returncode, 1)
            forbidden = subprocess.run(command + ["--diagnostic", "--record-reference"], capture_output=True, text=True)
            self.assertNotEqual(forbidden.returncode, 0)


if __name__ == "__main__":
    unittest.main()
