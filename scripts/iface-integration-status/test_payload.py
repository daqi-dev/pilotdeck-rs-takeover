"""Offline regression controls for exact UTF-8 batching; synthetic writes only."""
import copy
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import extension as e
import root_apply as a
import test_status as base


class PayloadControls(unittest.TestCase):
    def test_unicode_budget_measures_encoded_body_and_preserves_rows(self):
        rows = [{"验收ID": "iface.synthetic." + str(i), "事实JSON": "汉" * 30} for i in range(5)]
        body = {"create_records": rows[:2]}
        self.assertLess(len(e.canonical(body)), 250)
        self.assertGreater(e.body_utf8_bytes(body), 250)
        with patch.object(e, "BATCH_MAX_BODY_BYTES", 250):
            operations = e.batch_operations("+record-batch-create", "synthetic", rows)
            self.assertEqual([r for op in operations for r in op["body"]["create_records"]], rows)
            self.assertTrue(all(e.validate_operation_payload(op) <= 250 for op in operations))

    def test_exact_singleton_limit_and_one_byte_over_are_explicit(self):
        rows = [{"验收ID": "iface.synthetic.large", "事实JSON": "汉" * 100}]
        size = e.body_utf8_bytes({"create_records": rows})
        with patch.object(e, "BATCH_MAX_BODY_BYTES", size):
            self.assertEqual(len(e.batch_operations("+record-batch-create", "synthetic", rows)), 1)
        with patch.object(e, "BATCH_MAX_BODY_BYTES", size - 1):
            with self.assertRaisesRegex(ValueError, "single record.*iface.synthetic.large"):
                e.batch_operations("+record-batch-create", "synthetic", rows)

    def test_count_cap_is_retained_independently_of_byte_cap(self):
        rows = [{"x": i} for i in range(501)]
        operations = e.batch_operations("+record-batch-create", "synthetic", rows)
        self.assertEqual([len(op["body"]["create_records"]) for op in operations], [200, 200, 101])

    def test_update_keys_and_full_values_survive_byte_splitting(self):
        records = {"record-汉-" + str(i): {"事实JSON": "完整" * 80} for i in range(8)}
        with patch.object(e, "BATCH_MAX_BODY_BYTES", 1100):
            operations = e.batch_operations("+record-batch-update", "synthetic", records)
            self.assertGreater(len(operations), 1)
            reconstructed = {rid: values for op in operations for rid, values in op["body"]["update_records"].items()}
            self.assertEqual(reconstructed, records)
            self.assertTrue(all(e.validate_operation_payload(op) <= 1100 for op in operations))

    def fixture(self, directory):
        ledger = base.ledger_fixture()
        path = directory / "ledger.json"
        e.save(path, ledger)
        return ledger, path, base.binding_fixture()

    def test_oversized_late_table_blocks_before_first_write_or_journal(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            ledger, path, binding = self.fixture(directory)
            run = base.run_fixture(); run["scope"] = "汉" * 50000
            ledger["runs"] = [run]; e.save(path, ledger); projection = e.project(path)
            client = base.FakeClient(binding)
            with patch.object(e, "CELL_MAX_TEXT_BYTES", 1000000): approved = e.plan(projection, a.snapshot(client, binding))
            with patch.object(e, "BATCH_MAX_BODY_BYTES", 100000), patch.object(e, "CELL_MAX_TEXT_BYTES", 1000000):
                with self.assertRaisesRegex(ValueError, "single record payload"):
                    a.apply(client, binding, projection, approved, directory / "receipt.json", directory / "journal.json", True)
            self.assertEqual(client.calls, [])
            self.assertFalse((directory / "journal.json").exists())
            self.assertFalse((directory / "receipt.json").exists())

    def test_link_updates_share_budget_and_preserve_all_relations(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp); ledger, path, binding = self.fixture(directory)
            template = ledger["tasks"][0]
            ledger["tasks"] = [dict(copy.deepcopy(template), id="iface.test.task." + str(i)) for i in range(12)]
            ledger["contracts"][0]["taskIds"] = [t["id"] for t in ledger["tasks"]]
            e.save(path, ledger); projection = e.project(path); client = base.FakeClient(binding)
            for table in e.plan(projection, a.snapshot(client, binding))["tables"]:
                for op in table["operations"]:client.call(op["shortcut"], op["tableId"], op["body"])
            remote = a.snapshot(client, binding); expected = e.link_operations(projection, remote)
            def flatten(operations):
                return {rid: values for op in operations for rid, values in op["body"]["update_records"].items()}
            with patch.object(e, "BATCH_MAX_BODY_BYTES", 700):
                actual = e.link_operations(projection, remote)
                self.assertEqual(flatten(actual), flatten(expected))
                self.assertTrue(all(e.validate_operation_payload(op) <= 700 for op in actual))
                self.assertGreater(len(actual), len(expected))
            calls_before = copy.deepcopy(client.calls)
            with patch.object(e, "BATCH_MAX_BODY_BYTES", 100):
                with self.assertRaisesRegex(ValueError, "single record payload"):
                    e.link_operations(projection, remote)
            self.assertEqual(client.calls, calls_before)

    def test_each_actual_small_operation_is_journaled_before_dispatch(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp); ledger, path, binding = self.fixture(directory)
            ledger["runs"] = [base.run_fixture("iface.test.run." + str(i)) for i in range(4)]
            ledger["tasks"][0]["runIds"] = [r["id"] for r in ledger["runs"]]
            e.save(path, ledger); projection = e.project(path)
            journal = directory / "journal.json"; receipt = directory / "receipt.json"
            class JournalClient(base.FakeClient):
                def call(self, shortcut, table, body):
                    state = e.load(journal)
                    operation = {"shortcut": shortcut, "tableId": table, "body": body}
                    self_test.assertEqual(state["pendingOperations"][-1], operation)
                    self_test.assertLessEqual(e.body_utf8_bytes(body), e.BATCH_MAX_BODY_BYTES)
                    # Stop on the second split run operation, after preserving its own pending entry.
                    if table == "managed-runs" and len([x for x in self.calls if x[1] == table]) == 1:
                        self.calls.append((shortcut, table, copy.deepcopy(body)))
                        raise TimeoutError("synthetic unknown after small-operation dispatch")
                    return super().call(shortcut, table, body)
            self_test = self; client = JournalClient(binding)
            with patch.object(e, "BATCH_MAX_RECORDS", 1):
                approved = e.plan(projection, a.snapshot(client, binding))
                with self.assertRaises(TimeoutError):a.apply(client, binding, projection, approved, receipt, journal, True)
            state = e.load(journal)
            self.assertEqual(len(state["pendingOperations"]), 4)
            self.assertEqual([len(op["body"].get("create_records", op["body"].get("update_records"))) for op in state["pendingOperations"]], [1, 1, 1, 1])
            self.assertEqual(e.load(receipt)["operations"][-1]["status"], "UNKNOWN_UNTIL_READBACK")
            # An absent uncertain second create still blocks; batching does not classify it as undispatched.
            with patch.object(e, "BATCH_MAX_RECORDS", 1):
                reviewed = e.plan(projection, a.snapshot(client, binding))
                with self.assertRaisesRegex(ValueError, "prior write outcome remains unknown"):
                    a.apply(client, binding, projection, reviewed, directory / "retry.json", journal, True)
            self.assertFalse((directory / "retry.json").exists())


if __name__ == "__main__":unittest.main(verbosity=2)
