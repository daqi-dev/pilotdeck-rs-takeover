#!/usr/bin/env python3
"""Explicit root-only repair of verified empty NEW fact cells; no creates or old-journal edits."""
import argparse
import copy
import json
from pathlib import Path
import extension as e
import root_apply as a


def row_sha(record):
    return e.sha(e.canonical(record).encode("utf-8"))


def make_plan(original, target, remote, baseline):
    e.require(remote.get("schema") == "iface-table-remote/v1" and remote.get("complete") is True, "complete repair readback required")
    e.require(original["source"] == target["source"] and original["sourceSha256"] == target["sourceSha256"], "repair cannot change full ledger")
    old_runs = {r["id"] for r in baseline["runs"]}
    descriptor = target.get("factReferenceSource", {})
    policy_path = Path(descriptor.get("path", ""))
    e.require(e.sha(policy_path.read_bytes()) == descriptor.get("sha256"), "repair fact policy drift")
    protected = e.unique(e.load(policy_path).get("protectedRunFacts"))
    e.require(set(protected) == old_runs and all(e.sha(e.canonical(r).encode()) == protected[r["id"]]["canonicalFactSha256"] for r in baseline["runs"]), "repair baseline must exactly match protected policy facts")
    old_rows = {kind: {r[table["key"]]: r for r in table["rows"]} for kind, table in original["tables"].items()}
    target_rows = {kind: {r[table["key"]]: r for r in table["rows"]} for kind, table in target["tables"].items()}
    changed = {}
    for kind in e.TABLES:
        e.require(old_rows[kind].keys() == target_rows[kind].keys(), "repair cannot add/delete rows")
        for ident, before in old_rows[kind].items():
            after = target_rows[kind][ident]
            fields = {k for k in before if before[k] != after[k]}
            if fields:
                e.require(kind == "runs" and ident not in old_runs and fields == {"事实JSON", "行SHA256"}, "repair may only change new-run fact encoding/hash")
                envelope = json.loads(after["事实JSON"])
                e.require(envelope.get("schema") == "iface-fact-reference/v1" and envelope.get("id") == ident, "repair needs explicit fact envelope")
                raw = before["事实JSON"].encode("utf-8")
                e.require(envelope.get("canonicalFactSha256") == e.sha(raw) and envelope.get("canonicalFactUtf8Bytes") == len(raw), "envelope must preserve exact full fact")
                changed[ident] = after
    e.require(bool(changed), "empty repair scope")
    identities, guards, updates, classification = set(), [], {}, []
    for kind, table in remote["tables"].items():
        key = original["tables"][kind]["key"]
        seen = set()
        for record in table["records"]:
            ident = record["fields"].get(key)
            e.require(ident in old_rows[kind] and ident not in seen and record["id"] not in identities, "repair remote identity mismatch")
            seen.add(ident); identities.add(record["id"])
            before = old_rows[kind][ident]
            differences = {k for k, value in before.items() if (record["fields"].get(k) or "") != value}
            if ident in changed:
                e.require(differences == {"事实JSON"} and (record["fields"].get("事实JSON") or "") == "", "repair before-image must be exact original row with only empty fact cell")
                updates[record["id"]] = {"事实JSON": changed[ident]["事实JSON"], "行SHA256": changed[ident]["行SHA256"]}
                guards.append({"id": ident, "recordId": record["id"], "beforeRecordSha256": row_sha(record), "beforeFactSha256": e.sha(b""), "originalFullFactSha256": e.sha(before["事实JSON"].encode()), "targetFactSha256": e.sha(changed[ident]["事实JSON"].encode()), "targetRowSha256": changed[ident]["行SHA256"]})
            else:
                e.require(not differences, "unexpected non-repair text difference: " + str(ident))
            if kind == "runs" and ident not in old_runs:
                classification.append({"id": ident, "recordId": record["id"], "originalCreateReadback": "EXACT_ALL_TEXT_FIELDS" if not differences else "FACT_CELL_EMPTY_OTHER_TEXT_EXACT", "originalFactSha256": e.sha(before["事实JSON"].encode()), "observedFactSha256": e.sha((record["fields"].get("事实JSON") or "").encode())})
        e.require(seen == set(old_rows[kind]), "repair requires every desired record visible")
    e.require(len(guards) == len(changed), "missing repair record")
    # This validates all schema/identity/immutability guards against a synthetic before-image
    # in which ONLY the explicitly approved repaired encoding is supplied; the live snapshot
    # remains pinned byte-for-byte below. No general immutable check is bypassed in plan/apply.
    repaired = copy.deepcopy(remote)
    for record in repaired["tables"]["runs"]["records"]:
        if record["id"] in updates: record["fields"].update(updates[record["id"]])
    e.require(not any(t["operations"] for t in e.plan(target, repaired)["tables"]), "targeted repair must restore exact desired text")
    operations = e.batch_operations("+record-batch-update", remote["tables"]["runs"]["tableId"], updates)
    return {"schema": "iface-empty-new-fact-repair/v1", "source": target["source"], "fullLedgerSha256": target["sourceSha256"], "originalProjectionSha256": e.sha(e.canonical(original).encode()), "targetProjectionSha256": e.sha(e.canonical(target).encode()), "remoteSnapshotSha256": e.sha(e.canonical(remote).encode()), "protectedSnapshots": remote["protectedSnapshots"], "factReferenceSource": target["factReferenceSource"], "protectedRunIds": sorted(old_runs), "guards": guards, "operations": operations, "originalCreatedRunClassification": classification, "originalJournalPolicy": "UNTOUCHED; original pending operations require separate explicit root append-only resolution; not automatically visible/successful"}


def repair(client, binding, original, target, baseline, approved, out, repair_journal, execute=False):
    a.verify_projection(original)
    a.verify_projection(target)
    current = a.snapshot(client, binding)
    e.require(make_plan(original, target, current, baseline) == approved, "repair source/remote/approved-plan drift")
    for operation in approved["operations"]: e.validate_operation_payload(operation)
    if not execute: return {"status": "DRY_RUN", "writes": 0, "repairPlan": approved, "originalJournalTouched": False}
    out, repair_journal = Path(out), Path(repair_journal)
    e.require(not out.exists() and not repair_journal.exists(), "repair attempt exists; inspect/read back, never blindly retry under new receipt")
    journal = {"schema": "iface-fact-repair-journal/v1", "repairPlanSha256": e.sha(e.canonical(approved).encode()), "pendingOperations": []}
    receipt = {"schema": "iface-fact-repair-receipt/v1", "status": "STARTED", "repairPlan": approved, "operations": [], "originalJournalTouched": False, "originalPendingResolved": False}
    expected = copy.deepcopy(current)
    a.atomic_save(repair_journal, journal); a.atomic_save(out, receipt)
    try:
        for operation in approved["operations"]:
            a.verify_projection(target)
            live = a.snapshot(client, binding)
            e.require(e.sha(e.canonical(live).encode()) == e.sha(e.canonical(expected).encode()), "repair before-image/schema/protected snapshot drift")
            e.validate_operation_payload(operation)
            journal["pendingOperations"].append(copy.deepcopy(operation)); a.atomic_save(repair_journal, journal)
            receipt["operations"].append({"operation": operation, "status": "UNKNOWN_UNTIL_READBACK"}); a.atomic_save(out, receipt)
            client.call(operation["shortcut"], operation["tableId"], operation["body"])
            receipt["operations"][-1]["status"] = "ACK_PENDING_READBACK"; a.atomic_save(out, receipt)
            for record in expected["tables"]["runs"]["records"]:
                if record["id"] in operation["body"]["update_records"]: record["fields"].update(operation["body"]["update_records"][record["id"]])
            observed = a.snapshot(client, binding)
            e.require(e.sha(e.canonical(observed).encode()) == e.sha(e.canonical(expected).encode()), "targeted repair readback mismatch; original pending remains unresolved")
            receipt["operations"][-1]["status"] = "VERIFIED_READBACK"; a.atomic_save(out, receipt)
        final = a.snapshot(client, binding)
        e.require(not any(t["operations"] for t in e.plan(target, final)["tables"]), "repaired text differs")
        a.verify_projection(target)
        receipt.update(status="PASS_TARGETED_REPAIR_READBACK", remoteSha256=e.sha(e.canonical(final).encode()), protectedAfter=final["protectedSnapshots"], remainingLinks=len(e.link_operations(target, final)), productValidationPerformed=False)
        journal["status"] = receipt["status"]
        # Keep operation history even after verified repair; this is NOT the original write journal.
        journal["verifiedOperations"] = copy.deepcopy(journal["pendingOperations"]); journal["pendingOperations"] = []
        a.atomic_save(repair_journal, journal); a.atomic_save(out, receipt)
        return receipt
    except Exception as error:
        receipt.update(status="STOP_REPAIR_UNKNOWN_OR_PARTIAL_REQUIRES_READBACK", errorType=type(error).__name__)
        journal["status"] = receipt["status"]; a.atomic_save(repair_journal, journal); a.atomic_save(out, receipt)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("plan", "apply"))
    for arg in ("original-projection", "projection", "baseline", "out"): parser.add_argument("--" + arg, required=True)
    for arg in ("engine", "base-token", "tables", "plan", "remote"): parser.add_argument("--" + arg)
    parser.add_argument("--repair-journal", help="must be the binding-scoped .fact-repair-journal.json path")
    parser.add_argument("--execute", action="store_true"); args = parser.parse_args()
    original, target, baseline = e.load(args.original_projection), e.load(args.projection), e.load(args.baseline)
    if args.action == "plan":
        e.require(args.remote and not args.execute, "repair plan needs read-only complete remote file")
        a.verify_projection(original); a.verify_projection(target)
        result = make_plan(original, target, e.load(args.remote), baseline)
        a.atomic_save(args.out, result)
        print(e.canonical({"status": "PLAN_ONLY", "writes": 0, "repairRecords": len(result["guards"])})); return
    e.require(args.engine and args.base_token and args.tables and args.plan, "repair apply requires explicit engine/binding/approved plan")
    binding = e.load(args.tables); e.validate_binding(binding, args.base_token)
    repair_journal = Path(args.tables).with_suffix(".fact-repair-journal.json")
    e.require(not args.repair_journal or Path(args.repair_journal).resolve() == repair_journal.resolve(), "repair journal must remain binding-scoped")
    # An explicit shared binding write lock prevents overlap with the normal root writer.
    client = a.load_client(args.engine, args.base_token, binding)
    with a.exclusive_lock(Path(args.tables).with_suffix(".write.lock")):
        result = repair(client, binding, original, target, baseline, e.load(args.plan), args.out, repair_journal, args.execute)
        if not args.execute: a.atomic_save(args.out, result)
    print(e.canonical({"status": result["status"], "originalJournalTouched": False, "originalPendingResolved": False}))


if __name__ == "__main__": main()
