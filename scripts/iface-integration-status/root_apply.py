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
        directory = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
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


def load_client(engine_path, base, binding, schema_migration=False):
    spec = importlib.util.spec_from_file_location("iface_project_status_engine", engine_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    expected = {table["tableId"]: table for table in binding["tables"].values()}

    class CheckedLark(module.Lark):
        def call(self, shortcut, table=None, body=None, extra=()):
            result = super().call(shortcut, table, body, extra)
            if shortcut == "+record-list" and table in expected and not schema_migration:
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
    actual = e.project(projection["source"], projection.get("factReferenceSource"), projection.get("featureMetadataSource"))
    e.require(actual == projection, "ledger drift or forged/stale projection")


def verify_persisted_schema(client, binding, state):
    expected = state.get("verifiedSchemaFields")
    e.require(isinstance(expected, dict) and set(expected) == {t["tableId"] for t in binding["tables"].values()}, "completed schema journal lacks actual field identity pins")
    for table_id, values in expected.items():
        actual = client.call("+field-list", table_id).get("fields")
        e.require(isinstance(actual, list) and sorted(actual, key=lambda field: field["id"]) == values, "actual field identity/properties drift after migration")


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
    schema_state, schema_path = None, None
    journal_name = Path(journal_path).name
    if journal_name.endswith(".write-journal.json"):
        schema_path = Path(journal_path).with_name(journal_name[:-len(".write-journal.json")] + ".schema-journal.json")
        if schema_path.exists():
            schema_state = e.load(schema_path)
            e.require(schema_state.get("schema") == "iface-schema-journal/v1" and schema_state.get("status") == "PASS_READBACK" and schema_state.get("pendingOperations") == [] and schema_state.get("newBindingSha256") == e.sha(e.canonical(binding).encode()), "unresolved schema journal blocks record writes")
            verify_persisted_schema(client, binding, schema_state)
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
        if schema_state is not None:
            e.require(e.load(schema_path) == schema_state, "schema journal changed during record apply")
            verify_persisted_schema(client, binding, schema_state)
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
        if schema_state is not None:
            e.require(e.load(schema_path) == schema_state, "schema journal changed during record apply")
            verify_persisted_schema(client, binding, schema_state)
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


def proposed_feature_binding(old_binding):
    proposed = copy.deepcopy(old_binding)
    for kind, (_, key) in e.TABLES.items():
        proposed["tables"][kind]["fields"] = [key] + e.FIELDS[kind] + list(e.LINKS[kind])
        proposed["tables"][kind]["linkFields"] = {label: {"link_table": proposed["tables"][target]["tableId"], "bidirectional": False, "targetKind": target, "sourceField": field} for label, (target, field) in e.LINKS[kind].items()}
    validate_feature_transition(old_binding, proposed)
    proposed["schemaConfirmedByRoot"] = False
    return proposed


def validate_feature_transition(old_binding, new_binding):
    """Exactly this additive transition from the 0042 schema, never arbitrary edits."""
    e.validate_binding(new_binding, new_binding["baseToken"])
    for field in ("schema", "projectId", "baseToken", "protectedTableIds", "schemaConfirmedByRoot"):
        e.require(old_binding.get(field) == new_binding.get(field), "binding identity changed")
    e.require(set(old_binding.get("tables", {})) == set(e.TABLES), "old tables mismatch")
    for kind, (name, key) in e.TABLES.items():
        old, new = old_binding["tables"][kind], new_binding["tables"][kind]
        e.require(old.get("name") == name and old.get("tableId") == new.get("tableId"), "table identity changed")
        e.require(set(old.get("fields", [])) == {key, *e.LEGACY_FIELDS[kind], *e.LEGACY_LINKS[kind]}, "old schema is not the exact pre-feature schema")
        links = {label: {"link_table": old_binding["tables"][target]["tableId"], "bidirectional": False, "targetKind": target, "sourceField": field} for label, (target, field) in e.LEGACY_LINKS[kind].items()}
        e.require(old.get("linkFields") == links, "old relationship changed")


def schema_snapshot(client, binding):
    """Read actual field IDs/types/link destinations, all rows and six protected hashes."""
    def fields(table_id, managed=True):
        response = client.call("+field-list", table_id)
        values = response.get("fields")
        e.require(isinstance(values, list) and bool(values), "complete actual field list required")
        ids, names = set(), set()
        for value in values:
            e.require(isinstance(value, dict) and e.text(value.get("id")) and e.text(value.get("name")) and value["id"] not in ids and value["name"] not in names, "duplicate/missing actual field identity")
            e.require(e.text(value.get("type")) and (not managed or value["type"].lower() in ("text", "link")), "unexpected actual field type")
            ids.add(value["id"]); names.add(value["name"])
        return sorted(values, key=lambda value: value["id"])
    result = {"schema": "iface-schema-snapshot/v1", "bindingSha256": e.sha(e.canonical(binding).encode()), "protectedSnapshots": {}, "tables": {}}
    for table_id in binding["protectedTableIds"]:
        schema_before = fields(table_id, managed=False)
        records = client.records(table_id)
        e.require(schema_before == fields(table_id, managed=False), "protected schema changed while reading")
        result["protectedSnapshots"][table_id] = {"count": len(records), "sha256": e.sha(e.canonical(sorted(records, key=lambda row: row["id"])).encode()), "schemaSha256": e.sha(e.canonical(schema_before).encode())}
    for kind, (_, key) in e.TABLES.items():
        table_id = binding["tables"][kind]["tableId"]
        actual = fields(table_id)
        records = client.records(table_id)
        e.require(actual == fields(table_id), "schema changed while reading records")
        e.require(len({row["id"] for row in records}) == len(records) and all(e.text(row.get("id")) for row in records), "duplicate/missing record IDs")
        e.unique([row["fields"] for row in records], key)
        result["tables"][kind] = {"tableId": table_id, "fields": actual, "records": sorted(records, key=lambda row: row["id"])}
    return result


def validate_actual_schema(actual, table, expected_names):
    by_name = {value["name"]: value for value in actual}
    e.require(set(by_name) == set(expected_names), "actual schema names differ from approved transition")
    for name, value in by_name.items():
        wanted = "link" if name in table["linkFields"] else "text"
        e.require(value["type"].lower() == wanted, "actual field type differs: " + name)
        if wanted == "link":
            config = table["linkFields"][name]
            e.require(value.get("link_table") == config["link_table"] and value.get("bidirectional") is False, "actual link destination differs: " + name)


def feature_schema_plan(old_binding, new_binding, before, predecessor_sha256=None):
    validate_feature_transition(old_binding, new_binding)
    old_sha = e.sha(e.canonical(old_binding).encode())
    e.require(before.get("schema") == "iface-schema-snapshot/v1" and before.get("bindingSha256") == old_sha and set(before.get("tables", {})) == set(e.TABLES), "wrong complete schema snapshot")
    e.require(set(before.get("protectedSnapshots", {})) == set(old_binding["protectedTableIds"]), "protected schema snapshot missing")
    operations = []
    for kind in e.TABLES:
        old, new, observed = old_binding["tables"][kind], new_binding["tables"][kind], before["tables"][kind]
        e.require(observed["tableId"] == old["tableId"], "schema snapshot table mismatch")
        validate_actual_schema(observed["fields"], old, old["fields"])
        for name in sorted(set(new["fields"]) - set(old["fields"])):
            field = {"name": name, "type": "link" if name in new["linkFields"] else "text"}
            if field["type"] == "link":
                field.update({k: new["linkFields"][name][k] for k in ("link_table", "bidirectional")})
            operations.append({"shortcut": "+field-create", "tableId": old["tableId"], "body": field})
    result = {"schema": "iface-feature-schema-plan/v1", "oldBindingSha256": old_sha, "newBindingSha256": e.sha(e.canonical(new_binding).encode()), "beforeSnapshotSha256": e.sha(e.canonical(before).encode()), "operations": operations, "recordWrites": 0, "protectedTableWrites": 0, "noDeletesOrRenames": True}
    if predecessor_sha256 is not None:
        e.require(e.digest(predecessor_sha256, 64), "invalid predecessor schema journal SHA")
        result["predecessorSchemaJournalSha256"] = predecessor_sha256
    return result


def verify_schema_progress(before, after, old_binding, new_binding, completed):
    e.require(after["bindingSha256"] == before["bindingSha256"] and after["protectedSnapshots"] == before["protectedSnapshots"], "protected table or binding changed during schema migration")
    for kind in e.TABLES:
        old, new = old_binding["tables"][kind], new_binding["tables"][kind]
        prior, current = before["tables"][kind], after["tables"][kind]
        added = {operation["body"]["name"] for operation in completed if operation["tableId"] == old["tableId"]}
        validate_actual_schema(current["fields"], new, set(old["fields"]) | added)
        old_ids = {field["id"]: field for field in prior["fields"]}
        now = {field["id"]: field for field in current["fields"]}
        e.require(all(now.get(ident) == value for ident, value in old_ids.items()), "old actual field identity/properties changed")
        stripped = []
        for row in current["records"]:
            values = row["fields"]
            e.require(set(values) <= set(old["fields"]) | added, "unexpected record field during migration")
            e.require(all(value in (None, "", []) for name, value in values.items() if name in added), "new schema field already has concurrent content")
            stripped.append({"id": row["id"], "fields": {name: value for name, value in values.items() if name in old["fields"]}})
        e.require(stripped == prior["records"], "old record identity/value changed during schema migration")


def verify_created_field_identities(observed, verified_fields):
    by_table = {table["tableId"]: {field["id"]: field for field in table["fields"]} for table in observed["tables"].values()}
    e.require(all(by_table[item["tableId"]].get(item["field"]["id"]) == item["field"] for item in verified_fields), "previously created field identity/properties changed")


def migrate_feature_journal(old_binding, new_binding, state, receipt_path, before_sha, after_sha):
    validate_feature_transition(old_binding, new_binding)
    e.require(state.get("schema") == "iface-write-journal/v1" and state.get("bindingSha256") == e.sha(e.canonical(old_binding).encode()), "old journal/binding mismatch")
    e.require(state.get("pendingOperations") == [], "unresolved write journal cannot migrate")
    migrated = copy.deepcopy(state)
    new_sha = e.sha(e.canonical(new_binding).encode())
    migrated.setdefault("bindingMigrations", []).append({"from": state["bindingSha256"], "to": new_sha, "kind": "feature-schema-additions/v1", "receipt": str(Path(receipt_path).resolve()), "beforeSnapshotSha256": before_sha, "afterSnapshotSha256": after_sha})
    migrated["bindingSha256"] = new_sha
    return migrated


def validate_completed_fact_repair(repair, binding):
    """Accept only the successful journal shape emitted by repair_fact_cells.py."""
    e.require(repair.get("schema") == "iface-fact-repair-journal/v1" and repair.get("status") == "PASS_TARGETED_REPAIR_READBACK" and repair.get("pendingOperations") == [] and e.digest(repair.get("repairPlanSha256"), 64), "unresolved fact repair blocks schema migration")
    operations = repair.get("verifiedOperations")
    e.require(isinstance(operations, list) and bool(operations), "unresolved fact repair: verified operation history required")
    for operation in operations:
        e.require(operation.get("shortcut") == "+record-batch-update" and operation.get("tableId") == binding["tables"]["runs"]["tableId"], "unresolved fact repair: unexpected verified operation")
        e.validate_operation_payload(operation)
        e.require(all(set(values) == {"事实JSON", "行SHA256"} and e.text(values["事实JSON"]) and e.digest(values["行SHA256"], 64) for values in operation["body"]["update_records"].values()), "unresolved fact repair: invalid verified fact repair fields")


def validate_legacy_schema_predecessor(prior, binding, actual, main):
    """Recognize only the completed, documented pre-feature schema transition."""
    e.require(prior.get("schema") == "iface-schema-migration/v1" and prior.get("status") == "PASS_READBACK" and prior.get("pendingOperations", []) == [], "legacy schema outcome unknown/partial")
    current_sha = e.sha(e.canonical(binding).encode())
    e.require(prior.get("newBindingSha256") == current_sha and e.digest(prior.get("oldBindingSha256"), 64) and prior["oldBindingSha256"] != current_sha, "legacy schema binding mismatch")
    e.require(main.get("bindingSha256") == current_sha and any(item.get("from") == prior["oldBindingSha256"] and item.get("to") == current_sha for item in main.get("bindingMigrations", [])), "legacy schema migration missing from write journal history")
    protected = prior.get("protectedBefore")
    e.require(isinstance(protected, dict) and protected == prior.get("protectedAfter") and set(protected) == set(binding["protectedTableIds"]), "legacy protected snapshots differ/missing")
    for table_id, value in protected.items():
        e.require(type(value.get("count")) is int and value["count"] >= 0 and e.digest(value.get("sha256"), 64), "invalid legacy protected snapshot")
        e.require(all(actual["protectedSnapshots"][table_id].get(key) == item for key, item in value.items()), "legacy protected table differs from current readback")
    operations = prior.get("operations")
    e.require(isinstance(operations, list) and len(operations) == 4 and all(op.get("status") == "VERIFIED_READBACK" for op in operations), "legacy schema operations not completely verified")
    rename = operations[0]
    before, after = rename.get("before"), rename.get("after")
    e.require(isinstance(before, dict) and isinstance(after, dict) and before.get("name") == "Kernel实现" and after.get("name") == "能力实现" and {k: v for k, v in before.items() if k != "name"} == {k: v for k, v in after.items() if k != "name"}, "legacy schema rename identity/properties changed")
    expected = [
        {"kind": "contracts", "rename": {"from": "Kernel实现", "to": "能力实现", "knownFieldId": after.get("id")}},
        {"kind": "contracts", "createFields": [{"name": "术语说明", "type": "text"}]},
        {"kind": "tasks", "createFields": [{"name": name, "type": "text"} for name in ("术语说明", "记录类型", "来源仓库", "来源版本", "来源路径", "审阅层级", "处置结论", "处置理由", "实现状态", "实现提交", "实测状态", "合入状态")] + [{"name": "关联交付任务", "type": "link", "link_table": binding["tables"]["tasks"]["tableId"], "bidirectional": False}]},
        {"kind": "runs", "createFields": [{"name": "术语说明", "type": "text"}]},
    ]
    e.require([op.get("requested") for op in operations] == expected, "unrecognized legacy schema transition")
    for index, operation in enumerate(operations):
        kind = operation["requested"]["kind"]
        fields = operation["after"] if index else [operation["after"]]
        requested = operation["requested"].get("createFields", [{"name": "能力实现", "type": "text"}])
        e.require(isinstance(fields, list) and len(fields) == len(requested), "legacy field readback shape mismatch")
        actual_fields = {field["id"]: field for field in actual["tables"][kind]["fields"]}
        e.require(len({field.get("id") for field in fields}) == len(fields), "duplicate legacy field identity")
        for field, specification in zip(fields, requested):
            e.require(e.text(field.get("id")) and actual_fields.get(field["id"]) == field and all(field.get(key) == value for key, value in specification.items()), "legacy actual field identity/properties mismatch")


def apply_feature_schema(client, old_binding, new_binding, before, approved, binding_path, out, execute=False):
    """One additive schema attempt; uncertain field creates always require root resolution."""
    binding_path, out = Path(binding_path), Path(out)
    e.require(e.load(binding_path) == old_binding, "binding file drift")
    schema_path = binding_path.with_suffix(".schema-journal.json")
    prior_raw = schema_path.read_bytes() if schema_path.exists() else None
    prior = json.loads(prior_raw) if prior_raw is not None else None
    legacy = prior is not None and prior.get("schema") == "iface-schema-migration/v1"
    predecessor_sha = e.sha(prior_raw) if legacy else None
    e.require(feature_schema_plan(old_binding, new_binding, before, predecessor_sha) == approved, "forged schema plan or predecessor journal drift")
    actual = schema_snapshot(client, old_binding)
    e.require(actual == before, "schema/record snapshot drift before migration")
    main_path = binding_path.with_suffix(".write-journal.json")
    e.require(main_path.exists(), "existing binding write journal required for migration")
    main = journal_state(main_path, old_binding)
    e.require(main.get("pendingOperations") == [], "unresolved write journal cannot migrate")
    repair_path = binding_path.with_suffix(".fact-repair-journal.json")
    if repair_path.exists():
        repair = e.load(repair_path)
        validate_completed_fact_repair(repair, old_binding)
    predecessor = None
    if prior is not None:
        if legacy:
            validate_legacy_schema_predecessor(prior, old_binding, actual, main)
            predecessor = {"sha256": predecessor_sha, "rawUtf8": prior_raw.decode("utf-8"), "document": prior}
        else:
            e.require(prior.get("schema") == "iface-schema-journal/v1" and prior.get("status") == "PASS_READBACK" and prior.get("pendingOperations") == [], "prior schema outcome unknown; STOP, never recreate fields under a new receipt")
            raise ValueError("feature schema migration already attempted; inspect retained receipt")
    if not execute:
        return {"status": "DRY_RUN", "writes": 0, "plan": approved, "predecessorSchemaJournalSha256": predecessor_sha}
    e.require(not out.exists(), "schema receipt already exists")
    receipt = {"schema": "iface-feature-schema-receipt/v1", "status": "STARTED", "approvedPlan": approved, "oldBinding": old_binding, "newBinding": new_binding, "oldWriteJournal": main, "operations": [], "productValidationPerformed": False}
    journal = {"schema": "iface-schema-journal/v1", "status": "STARTED", "oldBindingSha256": approved["oldBindingSha256"], "newBindingSha256": approved["newBindingSha256"], "receipt": str(out.resolve()), "pendingOperations": [], "verifiedFields": []}
    if predecessor is not None:
        journal["predecessor"] = copy.deepcopy(predecessor)
        receipt["predecessor"] = copy.deepcopy(predecessor)
    e.require((schema_path.read_bytes() if schema_path.exists() else None) == prior_raw, "schema journal changed before attempt")
    atomic_save(schema_path, journal); atomic_save(out, receipt)
    completed = []
    try:
        for operation in approved["operations"]:
            e.require(e.load(binding_path) == old_binding and e.load(main_path) == main, "binding/write journal changed during migration")
            current_schema = schema_snapshot(client, old_binding)
            verify_schema_progress(before, current_schema, old_binding, new_binding, completed)
            verify_created_field_identities(current_schema, journal["verifiedFields"])
            journal["pendingOperations"].append(copy.deepcopy(operation)); atomic_save(schema_path, journal)
            receipt["operations"].append({"operation": operation, "status": "UNKNOWN_UNTIL_READBACK"}); atomic_save(out, receipt)
            client.call(operation["shortcut"], operation["tableId"], operation["body"])
            completed.append(operation)
            observed = schema_snapshot(client, old_binding)
            verify_schema_progress(before, observed, old_binding, new_binding, completed)
            verify_created_field_identities(observed, journal["verifiedFields"])
            table = next(value for value in observed["tables"].values() if value["tableId"] == operation["tableId"])
            field = next(value for value in table["fields"] if value["name"] == operation["body"]["name"])
            journal["verifiedFields"].append({"tableId": operation["tableId"], "field": copy.deepcopy(field)})
            atomic_save(schema_path, journal)
            receipt["operations"][-1].update(status="VERIFIED_FIELD_AND_RECORD_READBACK", actualField=field)
            atomic_save(out, receipt)
        after = schema_snapshot(client, old_binding)
        verify_schema_progress(before, after, old_binding, new_binding, completed)
        verify_created_field_identities(after, journal["verifiedFields"])
        e.require(e.load(binding_path) == old_binding and e.load(main_path) == main, "binding/write journal drift before commit")
        after_sha = e.sha(e.canonical(after).encode())
        migrated = migrate_feature_journal(old_binding, new_binding, main, out, approved["beforeSnapshotSha256"], after_sha)
        receipt.update(status="READY_TO_REBIND", afterSnapshot=after, newWriteJournal=migrated)
        atomic_save(out, receipt)
        e.require(schema_snapshot(client, old_binding) == after, "schema/record drift before local binding install")
        # Two-file install is fail-closed: an interruption leaves an explicit binding/journal mismatch.
        atomic_save(binding_path, new_binding); atomic_save(main_path, migrated)
        e.require(e.load(binding_path) == new_binding and e.load(main_path) == migrated, "binding/journal install readback mismatch")
        e.require(schema_snapshot(client, old_binding) == after, "schema/record drift after local binding install")
        receipt["status"] = "PASS_READBACK"
        journal.update(status="PASS_READBACK", verifiedSchemaFields={table["tableId"]: table["fields"] for table in after["tables"].values()}, verifiedOperations=copy.deepcopy(journal["pendingOperations"]), pendingOperations=[])
        atomic_save(schema_path, journal); atomic_save(out, receipt)
        return receipt
    except Exception as error:
        journal["status"] = receipt["status"] = "STOP_UNKNOWN_OR_PARTIAL_SCHEMA_REQUIRES_ROOT_RESOLUTION"
        receipt["errorType"] = type(error).__name__
        atomic_save(schema_path, journal); atomic_save(out, receipt)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("export", "apply", "schema-target", "schema-export", "schema-plan", "schema-apply"))
    parser.add_argument("--engine", help="显式指定已核实的project_status.py；不读取凭据文件")
    parser.add_argument("--base-token", required=True)
    parser.add_argument("--tables", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--new-tables", help="reviewed additive target binding, same table IDs")
    parser.add_argument("--before", help="complete actual schema/record before snapshot")
    parser.add_argument("--projection")
    parser.add_argument("--plan")
    parser.add_argument("--execute", action="store_true")
    args = parser.parse_args()
    binding = e.load(args.tables)
    if args.action == "schema-target":
        e.require(not args.execute and binding["baseToken"] == args.base_token, "target generation is read-only and base-bound")
        e.save(args.out, proposed_feature_binding(binding)); return
    if args.action.startswith("schema-"):
        e.require(args.new_tables, "reviewed target binding required")
        new_binding = e.load(args.new_tables)
        validate_feature_transition(binding, new_binding)
        e.require(binding["baseToken"] == args.base_token, "binding/base mismatch")
        e.require(args.engine or args.action == "schema-plan", "explicit verified engine required")
        client = load_client(args.engine, args.base_token, binding, schema_migration=True) if args.engine else None
        if args.action == "schema-export":
            e.require(not args.execute, "schema export is read-only")
            e.save(args.out, schema_snapshot(client, binding)); return
        e.require(args.before, "schema before snapshot required")
        before = e.load(args.before)
        if args.action == "schema-plan":
            e.require(not args.execute, "schema plan is read-only")
            prior_path = Path(args.tables).with_suffix(".schema-journal.json")
            prior = e.load(prior_path) if prior_path.exists() else None
            prior_sha = e.sha(prior_path.read_bytes()) if prior is not None and prior.get("schema") == "iface-schema-migration/v1" else None
            e.save(args.out, feature_schema_plan(binding, new_binding, before, prior_sha)); return
        e.require(args.plan, "approved schema plan required")
        if args.execute:
            with exclusive_lock(Path(args.tables).with_suffix(".write.lock")):
                result = apply_feature_schema(client, binding, new_binding, before, e.load(args.plan), args.tables, args.out, True)
        else:
            result = apply_feature_schema(client, binding, new_binding, before, e.load(args.plan), args.tables, args.out)
            e.save(args.out, result)
        print(json.dumps({"status": result["status"], "productValidationPerformed": False}, ensure_ascii=False)); return
    e.validate_binding(binding, args.base_token)
    e.require(args.engine, "explicit verified engine required")
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
