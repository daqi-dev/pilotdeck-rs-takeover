"""Synthetic offline controls for reference transport and guarded empty-cell repair."""
import copy
import json
import tempfile
import unittest
from pathlib import Path
import extension as e
import root_apply as a
import repair_fact_cells as r
import test_status as base


class FactCellControls(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="iface-fact-cell-control-"); self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name); self.path = self.dir / "ledger.json"; self.policy_path = self.dir / "policy.json"
        self.baseline = base.ledger_fixture(); old = base.run_fixture("iface.test.old")
        self.baseline["runs"] = [old]; self.baseline["tasks"][0]["runIds"] = [old["id"]]
        self.ledger = copy.deepcopy(self.baseline)
        large = base.run_fixture("iface.test.new.large"); large["scope"] = "汉" * 20000
        small = base.run_fixture("iface.test.new.small")
        self.ledger["runs"] += [large, small]; self.ledger["tasks"][0]["runIds"] += [large["id"], small["id"]]
        e.save(self.path, self.ledger); self.original = e.project(self.path)
        self.policy = {"schema": "iface-fact-reference-policy/v1", "factSource": {"path": str(self.path), "collection": "runs"}, "protectedRunFacts": [{"id": old["id"], "canonicalFactSha256": e.sha(e.canonical(old).encode())}], "entries": [{"id": large["id"], "canonicalFactSha256": e.sha(e.canonical(large).encode()), "canonicalFactUtf8Bytes": len(e.canonical(large).encode())}]}
        self.target = self.make_target(); self.binding = base.binding_fixture(); self.client = base.FakeClient(self.binding)
        for kind, table in self.original["tables"].items():
            self.client.rows[self.binding["tables"][kind]["tableId"]] = [{"id": "synthetic-" + kind + "-" + str(i), "fields": copy.deepcopy(row)} for i, row in enumerate(table["rows"])]
        record = self.client.rows["managed-runs"][1]; record["fields"]["事实JSON"] = ""
        self.remote = a.snapshot(self.client, self.binding); self.plan = r.make_plan(self.original, self.target, self.remote, self.baseline)
        self.out = self.dir / "repair.json"; self.repair_journal = self.dir / "repair-journal.json"
        self.original_journal = self.dir / "original-journal.json"; self.original_journal.write_text('{"pendingOperations":["untouched-original-unknown"]}')

    def make_target(self):
        e.save(self.policy_path, self.policy)
        return e.project(self.path, {"path": str(self.policy_path), "sha256": e.sha(self.policy_path.read_bytes())})

    def test_reference_changes_only_new_fact_transport_and_preserves_full_source(self):
        before = self.path.read_bytes(); old = self.original["tables"]["runs"]["rows"][0]
        self.assertEqual(old, self.target["tables"]["runs"]["rows"][0])
        first = self.original["tables"]["runs"]["rows"][1]; second = self.target["tables"]["runs"]["rows"][1]
        self.assertEqual({k for k in first if first[k] != second[k]}, {"事实JSON", "行SHA256"})
        envelope = json.loads(second["事实JSON"]); self.assertEqual(envelope["canonicalFactSha256"], e.sha(first["事实JSON"].encode()))
        self.assertEqual(envelope["canonicalFactUtf8Bytes"], len(first["事实JSON"].encode()))
        self.assertEqual(self.path.read_bytes(), before)

    def test_bad_fact_hash_byte_count_and_source_path_fail_closed(self):
        for field, value in [("canonicalFactSha256", "0" * 64), ("canonicalFactUtf8Bytes", 1)]:
            policy = copy.deepcopy(self.policy); policy["entries"][0][field] = value; e.save(self.policy_path, policy)
            with self.assertRaisesRegex(ValueError, "referenced full fact changed"):
                e.project(self.path, {"path": str(self.policy_path), "sha256": e.sha(self.policy_path.read_bytes())})
        policy = copy.deepcopy(self.policy); policy["factSource"]["path"] = str(self.dir / "other.json"); e.save(self.policy_path, policy)
        with self.assertRaisesRegex(ValueError, "current complete ledger"):
            e.project(self.path, {"path": str(self.policy_path), "sha256": e.sha(self.policy_path.read_bytes())})

    def test_protected_fact_cannot_be_referenced_or_modified(self):
        self.policy["entries"][0]["id"] = "iface.test.old"
        with self.assertRaisesRegex(ValueError, "protected historical run"): self.make_target()
        self.policy["entries"][0]["id"] = "iface.test.new.large"
        self.ledger["runs"][0]["scope"] = "changed"; e.save(self.path, self.ledger)
        with self.assertRaisesRegex(ValueError, "protected historical run changed"): self.make_target()

    def test_new_cell_budget_has_exact_utf8_boundary_and_update_guard(self):
        e.validate_text_cells({"x": "a" * 50000})
        with self.assertRaisesRegex(ValueError, "text cell exceeds"):e.validate_text_cells({"x": "a" * 50001})
        with self.assertRaisesRegex(ValueError, "text cell exceeds"):
            e.batch_operations("+record-batch-update", "synthetic", {"record": {"事实JSON": "汉" * 16667}})

    def test_normal_immutable_guard_and_original_journal_remain_unchanged(self):
        before = self.original_journal.read_bytes()
        with self.assertRaisesRegex(ValueError, "historical execution is immutable"):
            e.plan(self.target, self.remote)
        self.assertEqual(before, self.original_journal.read_bytes())

    def test_repair_requires_exact_baseline_record_id_and_full_before_image(self):
        wrong = copy.deepcopy(self.baseline); wrong["runs"] = []
        with self.assertRaisesRegex(ValueError, "baseline must exactly match"):
            r.make_plan(self.original, self.target, self.remote, wrong)
        for mutate in [lambda row: row["fields"].__setitem__("事实JSON", "nonempty"), lambda row: row["fields"].__setitem__("标题", "changed")]:
            remote = copy.deepcopy(self.remote); mutate(remote["tables"]["runs"]["records"][1])
            with self.assertRaisesRegex(ValueError, "before-image"):
                r.make_plan(self.original, self.target, remote, self.baseline)
        self.client.rows["managed-runs"][1]["id"] = "changed-record-id"
        with self.assertRaisesRegex(ValueError, "approved-plan drift"):
            r.repair(self.client, self.binding, self.original, self.target, self.baseline, self.plan, self.out, self.repair_journal, True)
        self.assertEqual(self.client.calls, []); self.assertFalse(self.out.exists()); self.assertFalse(self.repair_journal.exists())

    def test_targeted_repair_is_update_only_and_does_not_resolve_original_pending(self):
        before_journal = self.original_journal.read_bytes(); before_ledger = self.path.read_bytes()
        receipt = r.repair(self.client, self.binding, self.original, self.target, self.baseline, self.plan, self.out, self.repair_journal, True)
        self.assertEqual(receipt["status"], "PASS_TARGETED_REPAIR_READBACK")
        self.assertFalse(receipt["originalPendingResolved"]); self.assertFalse(receipt["originalJournalTouched"])
        self.assertEqual(before_journal, self.original_journal.read_bytes()); self.assertEqual(before_ledger, self.path.read_bytes())
        self.assertTrue(all(call[0] == "+record-batch-update" for call in self.client.calls))
        self.assertTrue(all(set(values) == {"事实JSON", "行SHA256"} for call in self.client.calls for values in call[2]["update_records"].values()))
        self.assertEqual(len(self.plan["originalCreatedRunClassification"]), 2)
        self.assertEqual(sum(row["originalCreateReadback"] == "EXACT_ALL_TEXT_FIELDS" for row in self.plan["originalCreatedRunClassification"]), 1)
        final = a.snapshot(self.client, self.binding); self.assertFalse(any(t["operations"] for t in e.plan(self.target, final)["tables"]))

    def test_unknown_repair_stays_pending_and_same_binding_journal_blocks_retry(self):
        original_call = self.client.call
        def fail(shortcut, table, body):
            self.assertEqual(e.load(self.repair_journal)["pendingOperations"][-1]["body"], body)
            raise TimeoutError("synthetic uncertain repair dispatch")
        self.client.call = fail
        with self.assertRaises(TimeoutError):r.repair(self.client, self.binding, self.original, self.target, self.baseline, self.plan, self.out, self.repair_journal, True)
        self.assertEqual(len(e.load(self.repair_journal)["pendingOperations"]), 1)
        self.assertEqual(e.load(self.out)["status"], "STOP_REPAIR_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK")
        self.client.call = original_call
        with self.assertRaisesRegex(ValueError, "repair attempt exists"):
            r.repair(self.client, self.binding, self.original, self.target, self.baseline, self.plan, self.dir / "another.json", self.repair_journal, True)
        self.assertEqual(len(e.load(self.repair_journal)["pendingOperations"]), 1)

    def test_full_snapshot_protected_table_and_policy_drift_block_before_writes(self):
        self.client.rows["protected-0"][0]["fields"]["原记录"] = "changed"
        with self.assertRaisesRegex(ValueError, "approved-plan drift"):
            r.repair(self.client, self.binding, self.original, self.target, self.baseline, self.plan, self.out, self.repair_journal, True)
        self.assertFalse(self.repair_journal.exists())
        self.policy_path.write_text("{}")
        with self.assertRaisesRegex(ValueError, "fact reference source drift"):
            r.repair(self.client, self.binding, self.original, self.target, self.baseline, self.plan, self.out, self.repair_journal, True)
        self.assertEqual(self.client.calls, [])


if __name__ == "__main__": unittest.main(verbosity=2)
