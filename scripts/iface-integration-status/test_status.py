#!/usr/bin/env python3
"""Synthetic offline controls: no credentials, network, product or model calls."""
import copy
import json
import tempfile
import unittest
from pathlib import Path

import extension as e
import root_apply as a

ROOT = Path(__file__).resolve().parents[2]


def ledger_fixture():
    source = e.load(ROOT / "state/iface-integration.json")
    contract = copy.deepcopy(source["contracts"][0])
    contract["taskIds"] = ["iface.test.task"]
    contract["evidence"] = []
    contract["audit"] = {layer: {"status": "UNKNOWN", "evidence": []} for layer in e.AUDIT_LAYERS}
    task = copy.deepcopy(source["tasks"][0])
    task.update(id="iface.test.task", phase="P2", status=None, delivery=None, runIds=[], evidence=[], verificationKind="runtime", contractIds=[contract["id"]], dependsOn=[], acceptanceCaseIds=["iface.test.case.success", "iface.test.case.cancel"])
    source.update(contracts=[contract], tasks=[task], runs=[])
    source.pop("sourceCatalog", None)
    source.pop("terminologySource", None)
    source.pop("displayOverrides", None)
    source["sourceInventories"] = []
    source["upstreamDecisions"] = []
    source["kernelChanges"] = []
    source["trackingSources"] = []
    return source


def run_fixture(ident="iface.test.run.success", case="iface.test.case.success", result="PASS"):
    return {"id": ident, "title": "合成运行", "caseId": case, "batchId": "iface.test.batch", "scope": "仅合成控制", "verificationLayer": "真实宿主", "platform": "synthetic", "entrypoint": "synthetic-only", "taskIds": ["iface.test.task"], "contractIds": ["iface.contract.assembly"], "executionKind": "executed", "result": result, "startedAt": "2026-11-15T10:00:00+08:00", "finishedAt": "2026-11-15T10:00:01+08:00", "command": "synthetic-no-execution", "configSha256": "d" * 64, "actualVersions": [{"component": "synthetic-composition", "commit": "a" * 40, "scope": "synthetic-only"}], "evidence": [{"path": "synthetic/no-real-execution.json", "sha256": "b" * 64}]}


def binding_fixture():
    binding = {"schema": "iface-table-binding/v1", "projectId": e.PROJECT, "baseToken": "synthetic-base", "schemaConfirmedByRoot": True, "protectedTableIds": ["protected-" + str(n) for n in range(6)], "tables": {}}
    for kind, (name, key) in e.TABLES.items():
        binding["tables"][kind] = {"name": name, "tableId": "managed-" + kind, "fields": [key] + e.FIELDS[kind] + list(e.LINKS[kind]), "linkFields": {label: {"link_table": "managed-" + target, "bidirectional": False, "targetKind": target, "sourceField": field} for label, (target, field) in e.LINKS[kind].items()}}
    return binding


class FakeClient:
    def __init__(self, binding):
        self.binding = binding
        self.rows = {table: [{"id": "existing-" + table, "fields": {"原记录": "不改"}}] for table in binding["protectedTableIds"]}
        self.rows.update({table["tableId"]: [] for table in binding["tables"].values()})
        self.calls = []
        self.next_id = 0
        self.fail_mode = None

    def records(self, table):
        return copy.deepcopy(self.rows[table])

    def call(self, shortcut, table, body):
        self.calls.append((shortcut, table, copy.deepcopy(body)))
        if self.fail_mode == "before_create" and shortcut == "+record-batch-create":
            self.fail_mode = None
            raise TimeoutError("synthetic uncertain result")
        if shortcut == "+record-batch-create":
            ids = []
            for fields in body["create_records"]:
                self.next_id += 1
                rid = "synthetic-record-" + str(self.next_id)
                self.rows[table].append({"id": rid, "fields": copy.deepcopy(fields)})
                ids.append(rid)
            if self.fail_mode == "after_create":
                self.fail_mode = None
                raise TimeoutError("synthetic lost acknowledgment")
            return {"record_id_list": ids}
        if shortcut == "+record-batch-update":
            for rid, fields in body["update_records"].items():
                record = next(row for row in self.rows[table] if row["id"] == rid)
                record["fields"].update(copy.deepcopy(fields))
            return {}
        raise AssertionError("unexpected mutation")


class Controls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="iface-table-control-")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.ledger_path = self.directory / "ledger.json"
        self.ledger = ledger_fixture()
        self.binding = binding_fixture()
        self.client = FakeClient(self.binding)
        self.journal = self.directory / "binding.write-journal.json"
        self.receipt = self.directory / "receipt.json"

    def projection(self):
        e.save(self.ledger_path, self.ledger)
        return e.project(self.ledger_path)

    def prepared(self):
        projection = self.projection()
        remote = a.snapshot(self.client, self.binding)
        return projection, e.plan(projection, remote)

    def passing_delivery(self):
        first = run_fixture()
        second = run_fixture("iface.test.run.cancel", "iface.test.case.cancel")
        self.ledger["runs"] = [first, second]
        task = self.ledger["tasks"][0]
        task.update(status="delivered", runIds=[first["id"], second["id"]], delivery={"runIds": [first["id"], second["id"]], "reviewer": "synthetic", "reviewedAt": "2026-11-15T10:01:00+08:00", "scope": "both cases", "appliesToVersions": copy.deepcopy(first["actualVersions"])})

    def test_real_ledger_has_independent_denominator(self):
        real = e.load(ROOT / "state/iface-integration.json")
        summary = e.check(real)
        self.assertEqual(summary["legacyBackendDenominator"], 127)
        self.assertFalse(summary["productPassInferredFromManagement"])

    def test_tracking_records_are_distinct_and_cannot_upgrade_claims(self):
        row = {"id": "iface.upstream.example", "title": "example", "sourceRepository": "PilotDeck", "sourceVersion": "a" * 40, "sourcePath": "src/example.rs", "reviewDepth": "路径清单", "decision": "暂缓", "reason": "未审阅生产语义", "implementationStatus": "未接收或暂缓", "testStatus": "未实测", "mergeStatus": "未核实", "taskIds": ["iface.test.task"], "contractIds": [], "runIds": []}
        self.ledger["upstreamDecisions"] = [row]
        projected = self.projection()
        self.assertEqual(projected["summary"]["counts"]["tasks"], 1)
        self.assertEqual(projected["summary"]["trackingCounts"]["upstreamDecisions"], 1)
        self.assertEqual(projected["summary"]["upstreamCountsBySourceVersion"], {"a" * 40: 1})
        self.assertEqual([r["记录类型"] for r in projected["tables"]["tasks"]["rows"]], ["交付任务", "上游增量处置"])
        remote = a.snapshot(self.client, self.binding)
        for table in e.plan(projected, remote)["tables"]:
            for op in table["operations"]:
                self.client.call(op["shortcut"], op["tableId"], op["body"])
        visible = a.snapshot(self.client, self.binding)
        links = e.link_operations(projected, visible)
        task_table = visible["tables"]["tasks"]
        identities = {r["fields"]["任务ID"]: r["id"] for r in task_table["records"]}
        relation = next(op["body"]["update_records"][identities["iface.upstream.example"]]["关联交付任务"] for op in links if identities["iface.upstream.example"] in op["body"]["update_records"])
        self.assertEqual(relation, [{"id": identities["iface.test.task"]}])
        for field, value, error in [("taskIds", ["iface.upstream.example"], "taskIds"), ("dependsOn", ["iface.test.task"], "prerequisites"), ("testStatus", "局部实测", "actual test"), ("mergeStatus", "远端main已合入", "remote merge")]:
            bad = copy.deepcopy(self.ledger)
            bad["upstreamDecisions"][0][field] = value
            with self.assertRaisesRegex(ValueError, error):
                e.check(bad)
        bad = copy.deepcopy(self.ledger)
        bad["tasks"][0]["dependsOn"] = ["iface.upstream.example"]
        with self.assertRaisesRegex(ValueError, "dependsOn"):
            e.check(bad)

    def test_terminology_changes_display_not_historical_facts(self):
        run = run_fixture()
        self.ledger["runs"] = [run]
        source = self.directory / "terms.json"
        e.save(source, {"records": [{"kind": "runs", "id": run["id"], "originalTitle": run["title"], "suggestedDisplayTitle": "限定范围标题", "suggestedDisplayNote": "仅局部测试", "suggestedDisplayLayer": "局部"}]})
        self.ledger["terminologySource"] = {"path": str(source), "sha256": e.sha(source.read_bytes())}
        row = self.projection()["tables"]["runs"]["rows"][0]
        self.assertEqual(row["标题"], "限定范围标题")
        self.assertEqual(row["验证层"], "局部")
        self.assertEqual(json.loads(row["事实JSON"]), run)
        source.write_text("{}")
        with self.assertRaisesRegex(ValueError, "terminology source drift"):
            self.projection()

    def test_mixed_static_status_requires_both_check_results(self):
        row = {"id": "iface.change.static", "title": "静态检查", "sourceRepository": "agent-kernel", "sourceVersion": "a" * 40, "sourcePath": "x.rs", "reviewDepth": "源码", "decision": "追踪", "reason": "待检", "implementationStatus": "已实现", "testStatus": "静态检查PASS与FAIL并存", "mergeStatus": "未核实", "taskIds": ["iface.test.task"], "contractIds": [], "runIds": []}
        self.ledger["kernelChanges"] = [row]
        with self.assertRaisesRegex(ValueError, "mixed static checks"):
            e.check(self.ledger)
        good, bad = run_fixture(), run_fixture("iface.test.run.failed", result="FAIL")
        good["executionKind"] = bad["executionKind"] = "build"
        self.ledger["runs"] = [good, bad]
        row["runIds"] = [good["id"], bad["id"]]
        self.assertTrue(e.check(self.ledger)["ok"])

    def test_display_override_is_checked_and_does_not_change_run_fact(self):
        run = run_fixture()
        self.ledger["runs"] = [run]
        source = self.directory / "overrides.json"
        e.save(source, {"overrides": [{"kind": "runs", "id": run["id"], "field": "验证层", "expected": "真实宿主", "value": "局部运行"}]})
        self.ledger["displayOverrides"] = {"path": str(source), "sha256": e.sha(source.read_bytes())}
        row = self.projection()["tables"]["runs"]["rows"][0]
        self.assertEqual(row["验证层"], "局部运行")
        self.assertEqual(json.loads(row["事实JSON"]), run)
        run["verificationLayer"] = "不同层"
        with self.assertRaisesRegex(ValueError, "baseline drift"):
            self.projection()

    def test_journal_migration_preserves_history_and_rejects_uncertainty(self):
        old = copy.deepcopy(self.binding)
        for kind in e.TABLES:
            old["tables"][kind]["fields"].remove("术语说明")
        old["tables"]["contracts"]["fields"].remove("能力实现")
        old["tables"]["contracts"]["fields"].append("Kernel实现")
        for name in ("记录类型", "来源仓库", "来源版本", "来源路径", "审阅层级", "处置结论", "处置理由", "实现状态", "实现提交", "实测状态", "合入状态", "关联交付任务"):
            old["tables"]["tasks"]["fields"].remove(name)
        old["tables"]["tasks"]["linkFields"].pop("关联交付任务")
        state = {"schema": "iface-write-journal/v1", "bindingSha256": e.sha(e.canonical(old).encode()), "pendingOperations": [], "attempts": [{"status": "PASS_READBACK"}]}
        updated = a.migrate_journal(old, self.binding, state)
        self.assertEqual(updated["attempts"], state["attempts"])
        self.assertEqual(updated["bindingSha256"], e.sha(e.canonical(self.binding).encode()))
        bad = copy.deepcopy(old)
        bad["tables"]["contracts"]["fields"].remove("标题")
        bad["tables"]["contracts"]["fields"].append("随意更名")
        forged_state = copy.deepcopy(state)
        forged_state["bindingSha256"] = e.sha(e.canonical(bad).encode())
        with self.assertRaisesRegex(ValueError, "unexpected removal or rename"):
            a.migrate_journal(bad, self.binding, forged_state)
        state["pendingOperations"] = [{"unknown": True}]
        with self.assertRaisesRegex(ValueError, "unresolved"):
            a.migrate_journal(old, self.binding, state)

    def test_new_task_is_unknown_until_verified(self):
        self.ledger["tasks"][0].update(status=None, runIds=[], delivery=None)
        self.ledger["runs"] = []
        summary = e.check(self.ledger)
        self.assertEqual(summary["taskStatuses"]["已交付"], 0)
        self.assertEqual(summary["taskStatuses"]["未核实"], 1)

    def test_delivered_requires_own_evidence(self):
        self.ledger["tasks"][0]["status"] = "delivered"
        with self.assertRaises(ValueError):
            e.check(self.ledger)

    def test_old_backend_status_not_inherited(self):
        self.ledger["tasks"][0]["legacyStatus"] = "delivered"
        with self.assertRaises(ValueError):
            e.check(self.ledger)
        self.ledger["tasks"][0].pop("legacyStatus")
        self.ledger["inheritLegacyStatus"] = True
        with self.assertRaises(ValueError):
            e.check(self.ledger)

    def test_complete_same_version_pass_can_close(self):
        self.passing_delivery()
        self.assertTrue(e.check(self.ledger)["ok"])

    def test_one_pass_cannot_close_multi_case_task(self):
        self.passing_delivery()
        self.ledger["tasks"][0]["delivery"]["runIds"].pop()
        with self.assertRaisesRegex(ValueError, "coverage"):
            e.check(self.ledger)

    def test_other_candidate_cannot_close(self):
        self.passing_delivery()
        self.ledger["tasks"][0]["delivery"]["appliesToVersions"][0]["commit"] = "c" * 40
        with self.assertRaisesRegex(ValueError, "version differs"):
            e.check(self.ledger)

    def test_unknown_source_failure_is_preserved_without_fake_artifact(self):
        run = run_fixture(result="FAIL")
        run.update(executionKind="build", actualVersions=[{"component": "unfrozen-source", "scope": "original compile failure", "identityStatus": "UNCONFIRMED", "reason": "full source snapshot was not saved"}])
        self.ledger["runs"] = [run]
        self.assertTrue(e.check(self.ledger)["ok"])
        run["actualVersions"][0]["artifactSha256"] = run["evidence"][0]["sha256"]
        with self.assertRaisesRegex(ValueError, "unconfirmed identity"):
            e.check(self.ledger)

    def test_unknown_or_current_file_only_cannot_close_delivery(self):
        self.passing_delivery()
        self.ledger["runs"][0]["actualVersions"] = [{"component": "unfrozen-source", "scope": "unknown", "identityStatus": "UNCONFIRMED", "reason": "not captured"}]
        with self.assertRaisesRegex(ValueError, "unconfirmed identity"):
            e.check(self.ledger)
        self.passing_delivery()
        self.ledger["runs"][0]["actualVersions"][0]["identityStatus"] = "CURRENT_FILE_ONLY_NOT_BOUND_TO_RUN"
        with self.assertRaisesRegex(ValueError, "unbound identity"):
            e.check(self.ledger)

    def test_valid_unknown_identity_time_projects_to_text_and_plans(self):
        run = run_fixture(result="FAIL")
        run.update(executionKind="build", actualVersions=[{"component": "unfrozen-source", "scope": "original failure", "identityStatus": "UNCONFIRMED", "reason": "source was not frozen"}], timingStatus="UNCONFIRMED_LOG_ONLY", recordedAt="2026-11-15T10:03:00+08:00", startedAt=None, finishedAt=None, configSha256=None, configurationStatus="UNCONFIRMED", configurationIdentityReason="environment was not captured", command=None, commandStatus="UNCONFIRMED", commandIdentityReason="argv was not captured")
        self.ledger["runs"] = [run]
        projection = self.projection()
        row = projection["tables"]["runs"]["rows"][0]
        self.assertTrue(all(isinstance(value, str) for value in row.values()))
        for field in ("配置SHA256", "开始时间", "结束时间", "实际版本"):
            self.assertIn("UNCONFIRMED", row[field])
        self.assertEqual(json.loads(row["事实JSON"]), run)
        planned = e.plan(projection, a.snapshot(self.client, self.binding))
        self.assertEqual(next(t for t in planned["tables"] if t["kind"] == "runs")["create"], 1)

    def test_not_run_nulls_and_review_nulls_are_explicit_text(self):
        self.assertEqual(e.run_identity_text({"executionKind": "not_run", "startedAt": None}, "startedAt"), "NOT_RUN（未执行）")
        self.assertEqual(e.run_identity_text({"executionKind": "evidence_review", "configSha256": None}, "configSha256"), "不适用（证据审阅）")

    def test_management_review_cannot_close_runtime(self):
        self.passing_delivery()
        self.ledger["runs"][0]["executionKind"] = "evidence_review"
        with self.assertRaisesRegex(ValueError, "review-only"):
            e.check(self.ledger)

    def test_failed_case_cannot_close(self):
        self.passing_delivery()
        self.ledger["runs"][0]["result"] = "FAIL"
        with self.assertRaisesRegex(ValueError, "passing"):
            e.check(self.ledger)

    def test_not_run_cannot_claim_actual_versions(self):
        run = run_fixture()
        run.update(executionKind="not_run", result="NOT_RUN")
        self.ledger["runs"] = [run]
        with self.assertRaises(ValueError):
            e.check(self.ledger)

    def test_inherited_evidence_is_not_new_execution(self):
        run = run_fixture()
        run["inheritance"] = {"scope": "old", "reason": "synthetic", "sourceEvidence": run["evidence"], "appliesToVersions": run["actualVersions"]}
        self.ledger["runs"] = [run]
        with self.assertRaisesRegex(ValueError, "not a new execution"):
            e.check(self.ledger)

    def test_joint_verified_requires_runtime_run(self):
        layer = self.ledger["contracts"][0]["audit"]["jointlyVerified"]
        layer.update(status="PRESENT", evidence=run_fixture()["evidence"], runIds=[])
        with self.assertRaisesRegex(ValueError, "actual passing"):
            e.check(self.ledger)

    def test_historical_runs_and_first_failure_immutable(self):
        failure = run_fixture(result="FAIL")
        self.ledger["runs"] = [failure]
        before = copy.deepcopy(self.ledger)
        self.ledger["runs"][0]["result"] = "PASS"
        with self.assertRaisesRegex(ValueError, "immutable"):
            e.check_transition(before, self.ledger)
        self.ledger = before
        projection, approved = self.prepared()
        a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        self.ledger["runs"] = []
        with self.assertRaisesRegex(ValueError, "first failure|unknown remote"):
            e.plan(self.projection(), a.snapshot(self.client, self.binding))

    def test_dependency_cycle_and_unknown_refs(self):
        self.ledger["tasks"][0]["dependsOn"] = ["iface.test.task"]
        with self.assertRaises(ValueError):
            e.check(self.ledger)
        self.ledger["tasks"][0]["dependsOn"] = ["iface.missing"]
        with self.assertRaises(ValueError):
            e.check(self.ledger)

    def test_binding_cannot_target_any_old_table(self):
        e.validate_binding(self.binding, "synthetic-base")
        for protected in self.binding["protectedTableIds"]:
            bad = copy.deepcopy(self.binding)
            bad["tables"]["tasks"]["tableId"] = protected
            with self.assertRaises(ValueError):
                e.validate_binding(bad, "synthetic-base")

    def test_binding_cannot_link_outside_new_tables(self):
        self.binding["tables"]["contracts"]["linkFields"]["接入任务"]["link_table"] = "protected-0"
        with self.assertRaisesRegex(ValueError, "link target"):
            e.validate_binding(self.binding, "synthetic-base")

    def test_dry_run_never_writes_or_creates_journal(self):
        projection, approved = self.prepared()
        result = a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal)
        self.assertEqual(result["status"], "DRY_RUN")
        self.assertEqual(self.client.calls, [])
        self.assertFalse(self.journal.exists())
        self.assertFalse(self.receipt.exists())

    def test_sync_real_ids_old_tables_unchanged_and_repeat_noop(self):
        projection, approved = self.prepared()
        protected = copy.deepcopy({key: value for key, value in self.client.rows.items() if key.startswith("protected-")})
        result = a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        self.assertEqual(result["status"], "PASS_READBACK")
        self.assertTrue(result["resolvedLinkPlan"])
        for row in self.client.rows["managed-contracts"]:
            links = row["fields"]["接入任务"]
            self.assertEqual(links, [{"id": self.client.rows["managed-tasks"][0]["id"]}])
        self.assertEqual(protected, {key: value for key, value in self.client.rows.items() if key.startswith("protected-")})
        for shortcut, table_id, _ in self.client.calls:
            self.assertIn(shortcut, ("+record-batch-create", "+record-batch-update"))
            self.assertTrue(table_id.startswith("managed-"))
        before = len(self.client.calls)
        plan = e.plan(projection, a.snapshot(self.client, self.binding))
        repeat = a.apply(self.client, self.binding, projection, plan, self.directory / "repeat.json", self.journal, True)
        self.assertEqual(repeat["status"], "NO_CHANGE")
        self.assertEqual(before, len(self.client.calls))

    def test_unknown_create_cannot_retry_under_new_receipt(self):
        projection, approved = self.prepared()
        self.client.fail_mode = "before_create"
        with self.assertRaises(TimeoutError):
            a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        self.assertEqual(e.load(self.receipt)["status"], "STOP_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK")
        before = len(self.client.calls)
        fresh = e.plan(projection, a.snapshot(self.client, self.binding))
        with self.assertRaisesRegex(ValueError, "prior write outcome"):
            a.apply(self.client, self.binding, projection, fresh, self.directory / "new-receipt.json", self.journal, True)
        self.assertEqual(before, len(self.client.calls))

    def test_lost_create_ack_can_resume_only_after_visibility(self):
        projection, approved = self.prepared()
        self.client.fail_mode = "after_create"
        with self.assertRaises(TimeoutError):
            a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        fresh = e.plan(projection, a.snapshot(self.client, self.binding))
        result = a.apply(self.client, self.binding, projection, fresh, self.directory / "resume.json", self.journal, True)
        self.assertEqual(result["status"], "PASS_READBACK")
        self.assertEqual(len(self.client.rows["managed-contracts"]), 1)
        self.assertFalse(e.load(self.journal)["pendingOperations"])

    def test_tampered_projection_rejected_before_writes(self):
        projection, approved = self.prepared()
        projection["tables"]["tasks"]["rows"][0]["状态"] = "已交付"
        with self.assertRaisesRegex(ValueError, "forged"):
            a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        self.assertEqual(self.client.calls, [])

    def test_ledger_drift_rejected_before_writes(self):
        projection, approved = self.prepared()
        self.ledger["tasks"][0]["nextAction"] = "changed"
        e.save(self.ledger_path, self.ledger)
        with self.assertRaisesRegex(ValueError, "drift"):
            a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        self.assertEqual(self.client.calls, [])

    def test_remote_drift_and_unknown_rows_fail_closed(self):
        projection, approved = self.prepared()
        self.client.rows["protected-0"][0]["fields"]["原记录"] = "concurrent edit"
        with self.assertRaisesRegex(ValueError, "plan drift"):
            a.apply(self.client, self.binding, projection, approved, self.receipt, self.journal, True)
        self.client.rows["managed-tasks"] = [{"id": "unknown-real-record", "fields": {"任务ID": "unknown-stable"}}]
        with self.assertRaisesRegex(ValueError, "unknown remote"):
            e.plan(projection, a.snapshot(self.client, self.binding))
        self.assertEqual(self.client.calls, [])

    def test_incomplete_duplicate_and_nontext_snapshot_rejected(self):
        projection = self.projection()
        remote = a.snapshot(self.client, self.binding)
        remote["complete"] = False
        with self.assertRaises(ValueError):
            e.plan(projection, remote)
        remote["complete"] = True
        row = {"id": "duplicate-record", "fields": projection["tables"]["tasks"]["rows"][0]}
        remote["tables"]["tasks"]["records"] = [row, copy.deepcopy(row)]
        with self.assertRaisesRegex(ValueError, "duplicate"):
            e.plan(projection, remote)

    def test_stable_text_is_not_a_real_link(self):
        with self.assertRaises(ValueError):
            e.link_ids(["iface.test.task"])
        projection = self.projection()
        with self.assertRaisesRegex(ValueError, "not visible"):
            e.link_operations(projection, a.snapshot(self.client, self.binding))

    def test_evidence_file_hash_verification(self):
        path = self.directory / "actual.json"
        path.write_text('{"synthetic":true}\n')
        run = run_fixture()
        run["evidence"] = [{"path": str(path), "sha256": e.sha(path.read_bytes())}]
        self.ledger["runs"] = [run]
        self.assertTrue(e.check(self.ledger, self.directory)["ok"])
        path.write_text("changed")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            e.check(self.ledger, self.directory)


if __name__ == "__main__":
    unittest.main(verbosity=2)
