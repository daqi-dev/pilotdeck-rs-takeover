#!/usr/bin/env python3
"""唯一外部写入人使用的适配器；默认预览，复用显式指定的已有 Lark CLI 封装。"""
import argparse
import copy
import fcntl
import importlib.util
import json
import os
import tempfile
from contextlib import contextmanager
from pathlib import Path

import extension as e


def atomic_save(path, value):
    path = Path(path)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, prefix=path.name + ".", delete=False) as stream:
        temporary = Path(stream.name)
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


@contextmanager
def exclusive_lock(path):
    with Path(path).open("a") as stream:
        fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)


def load_client(engine_path, base, binding):
    spec = importlib.util.spec_from_file_location("iface_project_status_engine", engine_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    expected = {table["tableId"]: table for table in binding["tables"].values()}

    class CheckedLark(module.Lark):
        def call(self, shortcut, table=None, body=None, extra=()):
            result = super().call(shortcut, table, body, extra)
            if shortcut == "+record-list" and table in expected:
                config = expected[table]
                e.require(set(result.get("fields", [])) == set(config["fields"]), "live fields differ from confirmed binding")
                types = result.get("field_type_list")
                e.require(isinstance(types, list) and len(types) == len(result["fields"]), "live field types required")
                for field, field_type in zip(result["fields"], types):
                    wanted = "LINK" if field in config["linkFields"] else "TEXT"
                    e.require(str(field_type).upper() == wanted, "live field type differs: " + field)
            return result

    return CheckedLark(base)


def snapshot(client, binding):
    result = {"schema": "iface-table-remote/v1", "projectId": e.PROJECT, "complete": True, "baseToken": binding["baseToken"], "protectedTableIds": binding["protectedTableIds"], "protectedSnapshots": {}, "tables": {}}
    for table_id in binding["protectedTableIds"]:
        records = client.records(table_id)
        result["protectedSnapshots"][table_id] = {"count": len(records), "sha256": e.sha(e.canonical(sorted(records, key=lambda r: r["id"])).encode())}
    for kind in e.TABLES:
        table = binding["tables"][kind]
        records = client.records(table["tableId"])
        for record in records:
            for field, value in record["fields"].items():
                if field in table["linkFields"]:
                    e.link_ids(value)
                else:
                    e.require(isinstance(value, (str, type(None))), "unexpected non-text value")
                    if value is None:
                        record["fields"][field] = ""
        result["tables"][kind] = {**table, "records": records}
    return result


def verify_projection(projection):
    actual = e.project(projection["source"], projection.get("factReferenceSource"))
    e.require(actual == projection, "ledger drift or forged/stale projection")


def pending_visible(operation, remote):
    table = next((value for value in remote["tables"].values() if value["tableId"] == operation["tableId"]), None)
    e.require(table is not None, "journal writes outside managed tables")
    kind = next(kind for kind in e.TABLES if remote["tables"][kind]["tableId"] == operation["tableId"])
    key = e.TABLES[kind][1]
    by_record = {record["id"]: record["fields"] for record in table["records"]}
    by_stable = {record["fields"][key]: record["fields"] for record in table["records"]}
    if operation["shortcut"] == "+record-batch-create":
        pairs = [(by_stable.get(fields[key]), fields) for fields in operation["body"]["create_records"]]
    else:
        pairs = [(by_record.get(rid), fields) for rid, fields in operation["body"]["update_records"].items()]
    for actual, expected in pairs:
        if actual is None:
            return False
        for field, value in expected.items():
            if field in e.LINKS[kind]:
                if e.link_ids(actual.get(field)) != e.link_ids(value):
                    return False
            elif (actual.get(field) or "") != value:
                return False
    return True


def journal_state(path, binding):
    identity = e.sha(e.canonical(binding).encode())
    if Path(path).exists():
        state = e.load(path)
        e.require(state.get("schema") == "iface-write-journal/v1" and state.get("bindingSha256") == identity, "journal/binding mismatch; inspect prior writes before rebinding")
        return state
    return {"schema": "iface-write-journal/v1", "bindingSha256": identity, "pendingOperations": [], "attempts": []}


def migrate_journal(old_binding, new_binding, state):
    """Migrate only an empty-pending journal across an additive schema change."""
    e.validate_binding(new_binding, new_binding["baseToken"])
    e.require(state.get("schema") == "iface-write-journal/v1" and state.get("bindingSha256") == e.sha(e.canonical(old_binding).encode()), "old journal/binding mismatch")
    e.require(state.get("pendingOperations") == [], "unresolved write journal cannot migrate")
    for field in ("schema", "projectId", "baseToken", "protectedTableIds"):
        e.require(old_binding.get(field) == new_binding.get(field), "binding identity changed")
    e.require(set(old_binding.get("tables", {})) == set(e.TABLES), "old tables mismatch")
    for kind in e.TABLES:
        old, new = old_binding["tables"][kind], new_binding["tables"][kind]
        e.require(old.get("name") == new.get("name") and old.get("tableId") == new.get("tableId"), "table identity changed")
        added = {"术语说明"}
        if kind == "tasks":
            added |= {"记录类型", "来源仓库", "来源版本", "来源路径", "审阅层级", "处置结论", "处置理由", "实现状态", "实现提交", "实测状态", "合入状态", "关联交付任务"}
        old_expected = set(new["fields"]) - added
        if kind == "contracts":
            old_expected = (old_expected - {"能力实现"}) | {"Kernel实现"}
        e.require(set(old.get("fields", [])) == old_expected, "unexpected removal or rename in old schema")
        old_links = dict(new["linkFields"])
        if kind == "tasks":
            old_links.pop("关联交付任务")
        e.require(old.get("linkFields") == old_links, "old relationship changed")
    migrated = copy.deepcopy(state)
    migrated.setdefault("bindingMigrations", []).append({"from": state["bindingSha256"], "to": e.sha(e.canonical(new_binding).encode())})
    migrated["bindingSha256"] = migrated["bindingMigrations"][-1]["to"]
    return migrated


def apply(client, binding, projection, approved, out, journal_path, execute=False):
    """A receipt name cannot bypass a prior unknown write: journal is binding-scoped."""
    verify_projection(projection)
    current_remote = snapshot(client, binding)
    current = e.plan(projection, current_remote)
    e.require(current == approved, "remote/source plan drift")
    # Preflight the complete text plan before journal mutation or the first write.
    for table in current["tables"]:
        for operation in table["operations"]:
            e.validate_operation_payload(operation)
    state = journal_state(journal_path, binding)
    e.require(all(pending_visible(op, current_remote) for op in state["pendingOperations"]), "prior write outcome remains unknown; STOP, inspect service history, never blindly recreate")
    if not execute:
        return {"status": "DRY_RUN", "writes": 0, "plan": current, "priorUnknownWritesNowVisible": len(state["pendingOperations"])}
    e.require(not Path(out).exists(), "receipt already exists; use a new receipt after reviewing the old one")
    receipt = {"status": "STARTED", "sourceSha256": projection["sourceSha256"], "planSha256": e.sha(e.canonical(approved).encode()), "protectedBefore": current_remote["protectedSnapshots"], "operations": []}
    state["pendingOperations"] = []
    state["attempts"].append({"receipt": str(Path(out).resolve()), "sourceSha256": projection["sourceSha256"], "status": "STARTED"})
    atomic_save(journal_path, state)
    atomic_save(out, receipt)

    def perform(operation):
        allowed = {t["tableId"] for t in binding["tables"].values()}
        e.require(operation["tableId"] in allowed and operation["shortcut"] in ("+record-batch-create", "+record-batch-update"), "mutation outside managed write boundary")
        e.validate_operation_payload(operation)
        verify_projection(projection)
        state["pendingOperations"].append(copy.deepcopy(operation))
        atomic_save(journal_path, state)
        receipt["operations"].append({"operation": operation, "status": "UNKNOWN_UNTIL_READBACK"})
        atomic_save(out, receipt)
        result = client.call(operation["shortcut"], operation["tableId"], operation["body"])
        if operation["shortcut"] == "+record-batch-create":
            ids = result.get("record_id_list")
            count = len(operation["body"]["create_records"])
            e.require(isinstance(ids, list) and len(ids) == count and len(set(ids)) == count and all(e.text(v) for v in ids), "unknown create acknowledgment")
        receipt["operations"][-1]["status"] = "ACK_PENDING_READBACK"
        atomic_save(out, receipt)

    try:
        for table in current["tables"]:
            for operation in table["operations"]:
                perform(operation)
        records_remote = snapshot(client, binding)
        e.require(records_remote["protectedSnapshots"] == receipt["protectedBefore"], "protected tables changed; no further writes")
        remaining = e.plan(projection, records_remote)
        e.require(not any(t["operations"] for t in remaining["tables"]), "record readback mismatch")
        link_plan = e.link_operations(projection, records_remote)
        receipt["resolvedLinkPlan"] = link_plan
        atomic_save(out, receipt)
        for operation in link_plan:
            perform(operation)
        final = snapshot(client, binding)
        e.require(final["protectedSnapshots"] == receipt["protectedBefore"], "protected tables changed")
        e.require(not any(t["operations"] for t in e.plan(projection, final)["tables"]), "final record readback mismatch")
        e.require(not e.link_operations(projection, final), "final relation readback mismatch")
        e.require(all(pending_visible(op, final) for op in state["pendingOperations"]), "write acknowledgment not visible")
        verify_projection(projection)
        for operation in receipt["operations"]:
            operation["status"] = "VERIFIED_READBACK"
        receipt.update(status="PASS_READBACK" if receipt["operations"] else "NO_CHANGE", protectedAfter=final["protectedSnapshots"], counts={kind: len(t["records"]) for kind, t in final["tables"].items()}, remoteSha256=e.sha(e.canonical(final).encode()), productValidationPerformed=False)
        state["pendingOperations"] = []
        state["attempts"][-1]["status"] = receipt["status"]
        atomic_save(journal_path, state)
        atomic_save(out, receipt)
        return receipt
    except Exception as error:
        receipt["status"] = "STOP_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK"
        receipt["errorType"] = type(error).__name__
        state["attempts"][-1]["status"] = receipt["status"]
        atomic_save(journal_path, state)
        atomic_save(out, receipt)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("export", "apply"))
    parser.add_argument("--engine", required=True, help="显式指定已核实的project_status.py；不读取凭据文件")
    parser.add_argument("--base-token", required=True)
    parser.add_argument("--tables", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--projection")
    parser.add_argument("--plan")
    parser.add_argument("--execute", action="store_true")
    args = parser.parse_args()
    binding = e.load(args.tables)
    e.validate_binding(binding, args.base_token)
    client = load_client(args.engine, args.base_token, binding)
    if args.action == "export":
        e.require(not args.execute, "export is read-only")
        e.save(args.out, snapshot(client, binding))
        return
    e.require(args.projection and args.plan, "projection/plan required")
    projection, approved = e.load(args.projection), e.load(args.plan)
    journal = Path(args.tables).with_suffix(".write-journal.json")
    if args.execute:
        with exclusive_lock(Path(args.tables).with_suffix(".write.lock")):
            result = apply(client, binding, projection, approved, args.out, journal, True)
    else:
        result = apply(client, binding, projection, approved, args.out, journal, False)
        e.save(args.out, result)
    print(json.dumps({"status": result["status"], "productValidationPerformed": False}, ensure_ascii=False))


if __name__ == "__main__":
    main()
