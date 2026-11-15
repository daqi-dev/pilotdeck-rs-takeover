#!/usr/bin/env python3
"""ifACE 接入账本的离线校验、投影和写入计划；本模块不访问网络。"""
import argparse
import hashlib
import json
import re
from datetime import datetime
from pathlib import Path

PROJECT = "pilotdeck-iface"
# Keep the existing row cap and reserve 1 MiB below the observed 5 MiB API request limit.
BATCH_MAX_RECORDS = 200
BATCH_MAX_BODY_BYTES = 4 * 1024 * 1024
# Conservative local transport policy, not a claim about an official service limit.
CELL_MAX_TEXT_BYTES = 50000
TABLES = {
    "contracts": ("ifACE 接入合同与能力映射", "合同ID"),
    "tasks": ("ifACE 接入交付任务", "任务ID"),
    "runs": ("ifACE 接入验收记录", "验收ID"),
}
AUDIT_LAYERS = ("interfaceDefined", "implemented", "productionWired", "externallyInjectable", "jointlyVerified")
LAYER_LABELS = ("接口定义", "能力实现", "正式入口接线", "公开注入", "联合验收")
STATUSES = {None: "未核实", "planned": "待实现", "in_progress": "进行中", "needs_verification": "待验证", "needs_integration": "待集成", "delivered": "已交付", "blocked": "阻塞"}
COMMON = ["标题", "状态", "阶段", "责任归属", "下一步", "合同关联", "任务关联", "旧后端任务", "验收用例", "首次失败", "证据", "术语说明", "事实JSON", "行SHA256"]
FEATURE_FIELDS = ["功能ID", "功能关联JSON", "关联变更ID"]
CLASSIFICATION_FIELDS = ["变更性质", "分类依据JSON", "本地提交", "目标远端", "目标分支", "远端可见", "目标主线合入", "分支观测JSON"]
CHANGE_NATURES = ("ifACE公共接口", "Kernel实现", "PilotDeck能力适配", "产品组装与入口兼容", "测试与维护工具")
TRACKING = ("upstreamDecisions", "kernelChanges", "productChanges")
REPOSITORY_BRANCHES = {"agent-kernel": "master", "PilotDeck": "main"}
FIELDS = {
    "contracts": COMMON + ["规范与代码依据", "PilotDeck责任", "Kernel责任", "组装程序责任", "状态所有者", *LAYER_LABELS],
    "tasks": COMMON + ["交付物", "验收标准", "前置任务", "验收记录", "交付版本", "记录类型", "来源仓库", "来源版本", "来源路径", "审阅层级", "处置结论", "处置理由", "实现状态", "实现提交", "实测状态", "合入状态"],
    "runs": COMMON + ["用例ID", "批次ID", "验证层", "执行方式", "实际版本", "运行入口", "平台", "配置SHA256", "开始时间", "结束时间", "继承范围"],
}
LEGACY_FIELDS = {kind: list(fields) for kind, fields in FIELDS.items()}
for _kind in FIELDS:
    FIELDS[_kind] += FEATURE_FIELDS + (CLASSIFICATION_FIELDS if _kind == "tasks" else [])
LINKS = {
    "contracts": {"接入任务": ("tasks", "taskIds")},
    "tasks": {"接入合同": ("contracts", "contractIds"), "前置接入任务": ("tasks", "dependsOn"), "关联交付任务": ("tasks", "taskIds"), "验收执行": ("runs", "runIds")},
    "runs": {"接入合同": ("contracts", "contractIds"), "接入任务": ("tasks", "taskIds")},
}
LEGACY_LINKS = {kind: dict(fields) for kind, fields in LINKS.items()}
for _kind in LINKS:
    LINKS[_kind]["关联接入变更"] = ("tasks", "__featureChangeIds")


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def load(path):
    return json.loads(Path(path).read_text())


def save(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def body_utf8_bytes(body):
    """Exact bytes serialized by the verified Lark engine's canonical(body)."""
    return len(canonical(body).encode("utf-8"))


def validate_text_cells(record, identity="unknown"):
    for field, value in record.items():
        if isinstance(value, str):
            require(len(value.encode("utf-8")) <= CELL_MAX_TEXT_BYTES, "text cell exceeds local UTF-8 budget: " + str(identity) + " field=" + field)


def validate_operation_payload(operation):
    shortcut, body = operation.get("shortcut"), operation.get("body")
    require(shortcut in ("+record-batch-create", "+record-batch-update"), "unsupported payload operation")
    field = "create_records" if shortcut == "+record-batch-create" else "update_records"
    require(isinstance(body, dict) and set(body) == {field}, "invalid operation payload")
    records = body[field]
    require(isinstance(records, list if field == "create_records" else dict), "invalid payload records")
    require(0 < len(records) <= BATCH_MAX_RECORDS, "payload record count exceeds budget")
    for identity, record in enumerate(records) if isinstance(records, list) else records.items():
        validate_text_cells(record, identity)
    size = body_utf8_bytes(body)
    require(size <= BATCH_MAX_BODY_BYTES, "operation payload exceeds UTF-8 byte budget: " + str(size))
    return size


def batch_operations(shortcut, table_id, records):
    """Preserve every full row/value; reject a too-large singleton before returning."""
    create = shortcut == "+record-batch-create"
    require(create or shortcut == "+record-batch-update", "unsupported payload operation")
    require(isinstance(records, list if create else dict), "invalid batch records")
    field = "create_records" if create else "update_records"
    items = records if create else list(records.items())
    operations, batch = [], []

    def operation(values):
        return {"shortcut": shortcut, "tableId": table_id, "body": {field: list(values) if create else dict(values)}}

    for item in items:
        singleton = operation([item])
        size = body_utf8_bytes(singleton["body"])
        if size > BATCH_MAX_BODY_BYTES:
            identity = next((item[k] for k in ("合同ID", "任务ID", "验收ID") if k in item), "unknown") if create else item[0]
            raise ValueError("single record payload exceeds UTF-8 byte budget: " + str(identity) + " bytes=" + str(size))
        candidate = batch + [item]
        if batch and (len(candidate) > BATCH_MAX_RECORDS or body_utf8_bytes(operation(candidate)["body"]) > BATCH_MAX_BODY_BYTES):
            operations.append(operation(batch))
            batch = [item]
        else:
            batch = candidate
    if batch:
        operations.append(operation(batch))
    for value in operations:
        validate_operation_payload(value)
    return operations


def text(value):
    return isinstance(value, str) and bool(value.strip())


def digest(value, length):
    return isinstance(value, str) and re.fullmatch("[0-9a-f]{" + str(length) + "}", value) is not None


def timestamp(value):
    require(text(value), "missing timestamp")
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(parsed.tzinfo is not None, "timestamp requires timezone")
    return parsed


def unique(rows, key="id"):
    require(isinstance(rows, list), "records must be arrays")
    result = {}
    for row in rows:
        require(isinstance(row, dict), "record must be an object")
        value = row.get(key)
        require(text(value) and value not in result, "missing/duplicate " + key)
        result[value] = row
    return result


def refs(row, key, known):
    values = row.get(key, [])
    require(isinstance(values, list) and all(text(v) for v in values), "invalid " + key)
    require(len(values) == len(set(values)) and set(values) <= set(known), "unknown/duplicate " + key)
    return values


def evidence(items, required=False, root=None):
    require(isinstance(items, list) and (bool(items) or not required), "evidence array required")
    for item in items:
        require(isinstance(item, dict) and text(item.get("path")) and digest(item.get("sha256"), 64), "evidence needs path and SHA256")
        if root is not None:
            path = Path(item["path"])
            if not path.is_absolute():
                path = Path(root) / path
            require(sha(path.read_bytes()) == item["sha256"], "evidence hash mismatch: " + item["path"])


def versions(items, allow_unconfirmed=False):
    require(isinstance(items, list) and bool(items), "actual versions required")
    seen = set()
    for item in items:
        require(isinstance(item, dict) and text(item.get("component")) and item["component"] not in seen, "invalid/duplicate version component")
        seen.add(item["component"])
        require(text(item.get("scope")), "version identity scope required")
        if item.get("identityStatus") == "UNCONFIRMED":
            require(allow_unconfirmed and text(item.get("reason")) and not item.get("commit") and not item.get("artifactSha256"), "unconfirmed identity cannot claim a version or close delivery")
            continue
        if not allow_unconfirmed:
            require(item.get("identityStatus") not in ("CURRENT_FILE_ONLY_NOT_BOUND_TO_RUN", "SOURCE_COMPARISON_ONLY"), "unbound identity cannot close delivery")
        require(digest(item.get("commit"), 40) or digest(item.get("artifactSha256"), 64), "exact commit or artifact SHA required")
        if "commit" in item:
            require(digest(item["commit"], 40), "invalid commit")
        if "artifactSha256" in item:
            require(digest(item["artifactSha256"], 64) and text(item.get("artifactKind")), "artifact kind required")


def metadata_source(source, ledger, descriptor=None):
    declared = ledger.get("featureMetadata")
    require(declared is None or isinstance(declared, dict), "invalid feature metadata descriptor")
    if declared and descriptor:
        def resolved(value):
            path = Path(value["path"])
            return str((source.parent.parent / path).resolve() if not path.is_absolute() else path.resolve()), value["sha256"]
        require(resolved(declared) == resolved(descriptor), "feature metadata descriptor drift/disagreement")
    descriptor = descriptor or declared
    require(isinstance(descriptor, dict) and text(descriptor.get("path")) and digest(descriptor.get("sha256"), 64), "explicit SHA-bound feature metadata required for current projection")
    path = Path(descriptor["path"])
    if not path.is_absolute():
        path = source.parent.parent / path
    raw = path.read_bytes()
    require(sha(raw) == descriptor["sha256"], "feature metadata drift")
    document = json.loads(raw)
    return document, {"path": str(path.resolve()), "sha256": descriptor["sha256"]}


def branch_integration(item, record, root=None):
    require(isinstance(item, dict), "branch integration required")
    repo = item.get("repository")
    require(repo == record["sourceRepository"] and repo in REPOSITORY_BRANCHES, "branch repository mismatch")
    remote_url = lambda value: isinstance(value, str) and (re.fullmatch(r"(?:https?|ssh|git)://[^\s/]+/[^\s]+", value) is not None or re.fullmatch(r"[^\s@]+@[^\s:]+:[^\s]+", value) is not None)
    require(item.get("targetBranch") == REPOSITORY_BRANCHES[repo] and remote_url(item.get("remote")) and remote_url(item.get("targetRemote")) and text(item.get("remoteName")), "wrong target branch/actual remote URL")
    local = item.get("localCommit")
    require(local is None or digest(local, 40), "invalid local commit")
    for key, statuses in (("remoteVisibility", ("UNKNOWN", "NOT_VISIBLE", "VISIBLE")), ("targetIntegration", ("UNKNOWN", "NOT_MERGED", "MERGED", "NOT_APPLICABLE"))):
        observation = item.get(key)
        require(isinstance(observation, dict) and observation.get("status") in statuses, "invalid branch observation")
        status = observation["status"]
        if status in ("UNKNOWN", "NOT_APPLICABLE"):
            require(text(observation.get("reason")) and not any(k in observation for k in ("observedCommit", "ancestryEvidence")), "unknown branch status cannot claim observed commits")
            evidence(observation.get("evidence", []), root=root)
        else:
            require(digest(local, 40), "observed branch claim requires explicit local commit")
            timestamp(observation.get("observedAt"))
            require(digest(observation.get("observedCommit"), 40), "observed branch commit required")
            evidence(observation.get("evidence", []), required=True, root=root)
            if key == "targetIntegration":
                ancestry = observation.get("ancestryEvidence", {})
                require(ancestry.get("candidateCommit") == local and ancestry.get("targetCommit") == observation["observedCommit"] and type(ancestry.get("isAncestor")) is bool and ancestry["isAncestor"] == (status == "MERGED"), "target merge requires exact candidate/target ancestry proof")
            else:
                require(observation["observedCommit"] == local, "visibility must observe the explicit candidate")
                scope = observation.get("referenceScope")
                require(scope in ("EXPLICIT_REFS", "ALL_REMOTE_REFS") and isinstance(observation.get("refNames"), list) and all(text(ref) and ref.startswith("refs/") for ref in observation["refNames"]), "remote visibility requires explicit ref scope")
                if status == "NOT_VISIBLE":
                    require(scope == "ALL_REMOTE_REFS" and observation.get("allRefsComplete") is True, "NOT_VISIBLE requires complete remote refs; target non-ancestry is insufficient")
                else:
                    require(bool(observation["refNames"]), "visible claim requires observed remote refs")
    return item


def version_identity(item):
    return item["component"], item.get("commit"), item.get("artifactSha256")


def check_feature_metadata(ledger, document, evidence_root=None):
    require(document.get("schema") == "iface-feature-metadata/v1" and document.get("projectId") == PROJECT and not document.get("exampleOnly"), "wrong/example feature metadata")
    if document.get("sourceLedger"):
        evidence([document["sourceLedger"]], required=True, root=evidence_root)
    features = unique(document.get("features"))
    for ident, feature in features.items():
        require(ident.startswith("iface.feature.") and all(text(feature.get(k)) for k in ("title", "requirement")), "feature identity/requirement required")
        require(isinstance(feature.get("boundaries"), list) and bool(feature["boundaries"]) and all(text(v) for v in feature["boundaries"]), "feature boundaries required")
        evidence(feature.get("evidence", []), required=True, root=evidence_root)
    collections = {kind: unique(ledger.get(kind, [])) for kind in (*TABLES, *TRACKING)}
    classification = {}
    expected = {(kind, ident) for kind in ("tasks", *TRACKING) for ident in collections[kind]}
    require(isinstance(document.get("records"), list), "classification records required")
    for record in document["records"]:
        identity = record.get("collection"), record.get("recordId")
        require(identity in expected and identity not in classification, "unknown/duplicate classification record")
        require(record.get("recordSha256") == sha(canonical(collections[identity[0]][identity[1]]).encode()), "classification source record hash mismatch")
        nature = record.get("changeNatures")
        require(isinstance(nature, list) and bool(nature) and len(nature) == len(set(nature)) and set(nature) <= set(CHANGE_NATURES), "nonempty valid change natures required")
        basis = record.get("classificationBasis")
        require(text(basis) or isinstance(basis, list) and bool(basis) and all(text(v) for v in basis), "classification basis required")
        evidence(record.get("evidence", []), root=evidence_root)
        if identity[0] in TRACKING:
            branch_integration(record.get("branchIntegration"), collections[identity[0]][identity[1]], evidence_root)
        else:
            require("branchIntegration" not in record, "delivery task cannot invent branch integration")
        classification[identity] = record
    require(set(classification) == expected, "classification coverage must exactly match delivery and tracking records")
    associations = unique(document.get("associations"))
    change_ids = set(collections["kernelChanges"]) | set(collections["productChanges"])
    for ident, association in associations.items():
        require(ident.startswith("iface.association.") and association.get("featureId") in features, "unknown feature/association identity")
        kind, rid = association.get("collection"), association.get("recordId")
        require(kind in collections and rid in collections[kind], "association record not found")
        require(association.get("recordSha256") == sha(canonical(collections[kind][rid]).encode()), "association source record hash mismatch")
        refs(association, "relatedChangeIds", change_ids)
        require(bool(association.get("relatedChangeIds")), "specific associated integration change required")
        proof = association.get("proofKind")
        require(proof in ("source_review", "executed", "build", "management", "evidence_review", "requirement", "not_applicable"), "invalid feature proof kind")
        require(text(association.get("proofScope")) and text(association.get("reviewer")), "explicit feature proof scope/reviewer required")
        timestamp(association.get("reviewedAt"))
        versions(association.get("appliesToVersions"), allow_unconfirmed=proof in ("executed", "build", "management", "evidence_review"))
        evidence(association.get("evidence", []), required=True, root=evidence_root)
        run_ids = refs(association, "runIds", collections["runs"])
        run_hashes = association.get("runFactSha256", {})
        require(isinstance(run_hashes, dict) and set(run_hashes) == set(run_ids) and all(run_hashes[run_id] == sha(canonical(collections["runs"][run_id]).encode()) for run_id in run_ids), "association referenced run fact hash mismatch")
        if proof in ("executed", "build", "management", "evidence_review"):
            require(bool(run_ids), "run proof requires explicit original run IDs")
            if kind == "runs":
                require(run_ids == [rid], "run association must bind only its own original execution")
            for run_id in run_ids:
                run = collections["runs"][run_id]
                require(run["executionKind"] == proof, "feature proof kind differs from original run")
                actual = {canonical(v) for v in run.get("actualVersions", [])}
                require({canonical(v) for v in association["appliesToVersions"]} <= actual, "feature applicability cannot retag an original run version")
                if any(v.get("identityStatus") == "UNCONFIRMED" for v in association["appliesToVersions"]):
                    require(association.get("observedOriginalIdentityOnly") is True, "unconfirmed feature identity must remain original-only")
        else:
            require(not run_ids and kind != "runs", "source/requirement applicability must not impersonate runtime proof")
    return features, classification, associations


def relation_values(row, source_field):
    if source_field == "__featureChangeIds":
        return sorted({ident for item in json.loads(row["功能关联JSON"]) for ident in item["relatedChangeIds"]})
    return json.loads(row["事实JSON"]).get(source_field, [])


def check(ledger, evidence_root=None):
    require(ledger.get("schema") == "iface-integration/v1" and ledger.get("projectId") == PROJECT, "wrong ledger identity")
    require(ledger.get("legacyBackendDenominator") == 127, "legacy denominator must remain 127")
    require(ledger.get("inheritLegacyStatus") is False, "legacy status inheritance prohibited")
    if ledger.get("sourceCatalog"):
        evidence([ledger["sourceCatalog"]], required=True, root=evidence_root)
    inventories = ledger.get("sourceInventories", [])
    require(isinstance(inventories, list), "source inventories must be an array")
    source_ids = set()
    for inventory in inventories:
        identity = (inventory.get("baseToken"), inventory.get("tableId"))
        require(all(text(v) for v in identity) and identity not in source_ids, "source inventory identity repeated/missing")
        source_ids.add(identity)
        require(type(inventory.get("count")) is int and inventory["count"] >= 0 and digest(inventory.get("snapshotSha256"), 64), "invalid source count/hash")
        timestamp(inventory.get("readAt"))
    groups = {kind: unique(ledger.get(kind)) for kind in TABLES}
    all_ids = set()
    for kind, rows in groups.items():
        for ident, row in rows.items():
            require(ident.startswith("iface.") and ident not in all_ids and text(row.get("title")), "invalid global identity/title")
            all_ids.add(ident)
            for field, target in (("contractIds", "contracts"), ("taskIds", "tasks")):
                refs(row, field, groups[target])
            old = row.get("legacyBackendTaskIds", [])
            require(isinstance(old, list) and all(text(v) and v.startswith("backend.") for v in old), "invalid legacy task link")
            require("legacyStatus" not in row, "legacy status must not become integration status")
            evidence(row.get("evidence", []), root=evidence_root)
    tracking = {kind: unique(ledger.get(kind, [])) for kind in ("upstreamDecisions", "kernelChanges", "productChanges")}
    for kind, records in tracking.items():
        for ident, row in records.items():
            require(ident.startswith("iface.") and ident not in all_ids and text(row.get("title")), "invalid tracking identity/title")
            all_ids.add(ident)
            require(all(text(row.get(k)) for k in ("sourceRepository", "sourceVersion", "sourcePath", "reviewDepth", "decision", "reason", "implementationStatus", "testStatus", "mergeStatus")), "tracking facts required")
            require(digest(row["sourceVersion"], 40), "tracking source version must be a pinned commit")
            require(row["implementationStatus"] != "已交付", "tracking is not a delivery claim")
            require(row["mergeStatus"] in ("未推送", "远端main已合入", "不适用", "未核实"), "invalid merge status")
            require(row["testStatus"] in ("未实测", "局部实测", "局部实测（含失败）", "局部实测（构建失败）", "局部运行与静态检查结果并存", "局部运行与构建/检查结果并存", "构建结果PASS与FAIL并存", "静态检查PASS与FAIL并存", "构建失败，测试未运行", "历史局部实测（不同版本）", "不适用", "未核实"), "invalid test status")
            require(not row.get("dependsOn"), "tracking records cannot be delivery prerequisites")
            refs(row, "contractIds", groups["contracts"])
            refs(row, "taskIds", groups["tasks"])
            refs(row, "runIds", groups["runs"])
            if row["testStatus"] in ("局部实测", "历史局部实测（不同版本）"):
                require(bool(row.get("runIds")) and any(groups["runs"][rid].get("executionKind") == "executed" and groups["runs"][rid].get("result") == "PASS" and (set(row.get("taskIds", [])) & set(groups["runs"][rid].get("taskIds", [])) or set(row.get("contractIds", [])) & set(groups["runs"][rid].get("contractIds", []))) for rid in row["runIds"]), "actual test requires passing executed run linked to the same scope")
            if row["testStatus"] == "局部实测（含失败）":
                require(any(groups["runs"][rid].get("executionKind") == "executed" and groups["runs"][rid].get("result") == "FAIL" for rid in row.get("runIds", [])), "failed test requires actual failed run")
            if row["testStatus"] in ("局部实测（构建失败）", "局部运行与静态检查结果并存", "局部运行与构建/检查结果并存"):
                require(any(groups["runs"][rid].get("executionKind") == "executed" and groups["runs"][rid].get("result") == "PASS" for rid in row.get("runIds", [])) and any(groups["runs"][rid].get("executionKind") == "build" and groups["runs"][rid].get("result") == "FAIL" for rid in row.get("runIds", [])), "mixed result requires passing execution and failed build")
            if row["testStatus"] in ("静态检查PASS与FAIL并存", "构建结果PASS与FAIL并存"):
                require(any(groups["runs"][rid].get("executionKind") == "build" and groups["runs"][rid].get("result") == "PASS" for rid in row.get("runIds", [])) and any(groups["runs"][rid].get("executionKind") == "build" and groups["runs"][rid].get("result") == "FAIL" for rid in row.get("runIds", [])), "mixed static checks require passing and failing build records")
            if row["testStatus"] == "构建失败，测试未运行":
                require(any(groups["runs"][rid].get("executionKind") == "build" and groups["runs"][rid].get("result") == "FAIL" for rid in row.get("runIds", [])), "build failure requires actual failed build")
            if row["mergeStatus"] == "远端main已合入":
                require(digest(row.get("mergeCommit"), 40) and bool(row.get("evidence")), "remote merge requires exact commit and evidence")
            if row.get("implementationCommit"):
                require(digest(row["implementationCommit"], 40), "invalid implementation commit")
            evidence(row.get("evidence", []), root=evidence_root)
    for ident, contract in groups["contracts"].items():
        require(text(contract.get("requirement")) and isinstance(contract.get("sourceRefs"), list) and bool(contract["sourceRefs"]), "contract requirement/source required")
        for ref in contract["sourceRefs"]:
            require(all(text(ref.get(k)) for k in ("repository", "path", "anchor")) and digest(ref.get("commit"), 40), "source must have pinned code/document anchor")
        require(set(contract.get("audit", {})) == set(AUDIT_LAYERS), "five audit layers must be separate")
        for layer, item in contract["audit"].items():
            require(item.get("status") in ("UNKNOWN", "PRESENT", "MISSING", "PARTIAL"), "invalid audit status")
            evidence(item.get("evidence", []), required=item["status"] != "UNKNOWN", root=evidence_root)
            if layer == "jointlyVerified" and item["status"] == "PRESENT":
                rr = refs(item, "runIds", groups["runs"])
                require(bool(rr) and all(groups["runs"][rid].get("result") == "PASS" and groups["runs"][rid].get("executionKind") == "executed" and ident in groups["runs"][rid].get("contractIds", []) for rid in rr), "joint verification requires actual passing runs for this contract")
        require(all(text(contract.get("responsibilities", {}).get(k)) for k in ("pilotdeck", "kernel", "composition", "stateOwner")), "responsibility boundaries required")
    for ident, run in groups["runs"].items():
        require(all(text(run.get(k)) for k in ("caseId", "batchId", "scope", "verificationLayer", "platform", "entrypoint")), "run identity/scope required")
        require(run.get("executionKind") in ("executed", "build", "management", "evidence_review", "not_run"), "invalid execution kind")
        require(run.get("result") in ("PASS", "FAIL", "PARTIAL", "INVALID", "NOT_RUN"), "invalid result")
        if run["executionKind"] == "not_run":
            require(run["result"] == "NOT_RUN" and not run.get("actualVersions") and not run.get("finishedAt"), "NOT_RUN cannot claim versions or completion")
        else:
            require(run["result"] != "NOT_RUN", "executed/reviewed record needs actual result")
            versions(run.get("actualVersions"), allow_unconfirmed=True)
            if run.get("timingStatus") == "UNCONFIRMED_LOG_ONLY":
                require(not run.get("startedAt") and not run.get("finishedAt"), "unknown timing must not invent execution timestamps")
                timestamp(run.get("recordedAt"))
            else:
                require(timestamp(run.get("finishedAt")) >= timestamp(run.get("startedAt")), "run ends before start")
            evidence(run.get("evidence", []), required=True, root=evidence_root)
            if run["executionKind"] in ("executed", "build", "management"):
                if run.get("commandStatus") == "UNCONFIRMED":
                    require(not run.get("command") and text(run.get("commandIdentityReason")), "unknown argv must remain explicit")
                else:
                    require(text(run.get("command")), "execution command required")
                if run.get("configurationStatus") == "UNCONFIRMED":
                    require(not run.get("configSha256") and text(run.get("configurationIdentityReason")), "unknown config cannot claim an evidence hash as config")
                else:
                    require(digest(run.get("configSha256"), 64), "execution config identity required")
        inheritance = run.get("inheritance")
        if inheritance:
            require(run["executionKind"] == "evidence_review", "inherited evidence is not a new execution")
            require(all(text(inheritance.get(k)) for k in ("scope", "reason")), "inheritance needs scope/reason")
            versions(inheritance.get("appliesToVersions"))
            evidence(inheritance.get("sourceEvidence", []), required=True, root=evidence_root)
    for ident, task in groups["tasks"].items():
        require(task.get("status") in STATUSES, "invalid task status")
        require(task.get("phase") in ["P" + str(n) for n in range(8)], "invalid phase")
        require(text(task.get("deliverable")) and isinstance(task.get("acceptance"), list) and bool(task["acceptance"]), "task contract required")
        require(task.get("verificationKind") in ("runtime", "review"), "verification kind required")
        required_cases = task.get("acceptanceCaseIds")
        require(isinstance(required_cases, list) and bool(required_cases) and len(set(required_cases)) == len(required_cases) and all(text(v) for v in required_cases), "unique acceptance cases required")
        dependencies = refs(task, "dependsOn", groups["tasks"])
        require(ident not in dependencies, "self dependency")
        runs = refs(task, "runIds", groups["runs"])
        require(all(ident in groups["runs"][rid].get("taskIds", []) for rid in runs), "run does not cover task")
        if task.get("status") == "delivered":
            delivery = task.get("delivery") or {}
            require(isinstance(delivery, dict), "delivery review must be an object")
            accepted = refs(delivery, "runIds", groups["runs"])
            require(bool(accepted) and set(accepted) <= set(runs) and text(delivery.get("reviewer")) and text(delivery.get("scope")), "delivery review/runs required")
            timestamp(delivery.get("reviewedAt"))
            versions(delivery.get("appliesToVersions"))
            covered = set()
            for rid in accepted:
                run = groups["runs"][rid]
                require(run["result"] == "PASS" and run["executionKind"] != "not_run", "delivery requires passing evidence")
                versions(run.get("actualVersions"))
                require(run.get("configurationStatus") != "UNCONFIRMED" and run.get("commandStatus") != "UNCONFIRMED", "unconfirmed execution identity cannot close delivery")
                if task["verificationKind"] == "runtime":
                    require(run["executionKind"] == "executed", "runtime delivery cannot inherit review-only evidence")
                applicable = (run.get("inheritance") or {}).get("appliesToVersions", run.get("actualVersions"))
                require(canonical(applicable) == canonical(delivery["appliesToVersions"]), "delivery version differs from tested/applicable version")
                covered.add(run["caseId"])
            require(set(required_cases) <= covered, "delivery acceptance coverage incomplete")
            require(all(groups["tasks"][dep].get("status") == "delivered" for dep in dependencies), "delivery dependencies not closed")
        else:
            require(not task.get("delivery"), "non-delivered task cannot retain delivery claim")
    visiting, done = set(), set()
    def visit(ident):
        require(ident not in visiting, "dependency cycle")
        if ident in done:
            return
        visiting.add(ident)
        for dependency in groups["tasks"][ident].get("dependsOn", []):
            visit(dependency)
        visiting.remove(ident)
        done.add(ident)
    for ident in groups["tasks"]:
        visit(ident)
    return {"ok": True, "counts": {kind: len(rows) for kind, rows in groups.items()}, "trackingCounts": {kind: len(rows) for kind, rows in tracking.items()}, "upstreamCountsBySourceVersion": {version: sum(row["sourceVersion"] == version for row in tracking["upstreamDecisions"].values()) for version in sorted({row["sourceVersion"] for row in tracking["upstreamDecisions"].values()})}, "taskStatuses": {label: sum(t.get("status") == value for t in groups["tasks"].values()) for value, label in STATUSES.items()}, "legacyBackendDenominator": 127, "productPassInferredFromManagement": False}


def check_transition(before, after):
    check(before)
    check(after)
    for kind in (*TABLES, "upstreamDecisions", "kernelChanges", "productChanges"):
        old, new = unique(before.get(kind, [])), unique(after.get(kind, []))
        require(set(old) <= set(new), "records may be archived, never removed: " + kind)
        if kind == "runs":
            for ident, record in old.items():
                require(record == new[ident], "historical execution is immutable: " + ident)


def readable(value):
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    if isinstance(value, list) and all(isinstance(v, str) for v in value):
        return "\n".join(value)
    return canonical(value)


def run_identity_text(fact, field):
    """Render nullable facts into TEXT columns without inventing identity/time."""
    value = fact.get(field)
    if value is not None:
        return readable(value)
    if fact.get("executionKind") == "not_run":
        return "NOT_RUN（未执行）"
    if field == "configSha256":
        if fact.get("configurationStatus") == "UNCONFIRMED":
            return "UNCONFIRMED（原配置未封存）"
        if fact.get("executionKind") == "evidence_review":
            return "不适用（证据审阅）"
        return ""
    if fact.get("timingStatus") == "UNCONFIRMED_LOG_ONLY":
        return "UNCONFIRMED（原执行时间未保存）"
    return ""


def fact_reference_policy(source, ledger, descriptor):
    """Explicit immutable-fact transport references; the full ledger remains unchanged."""
    require(isinstance(descriptor, dict), "fact reference source required")
    manifest_path = Path(descriptor.get("path", ""))
    if not manifest_path.is_absolute():
        manifest_path = source.parent.parent / manifest_path
    require(sha(manifest_path.read_bytes()) == descriptor.get("sha256"), "fact reference source drift")
    manifest = load(manifest_path)
    require(manifest.get("schema") == "iface-fact-reference-policy/v1", "wrong fact reference policy")
    location = manifest.get("factSource", {})
    require(location.get("collection") == "runs" and text(location.get("path")), "invalid fact reference collection")
    reference_path = Path(location["path"])
    if not reference_path.is_absolute():
        reference_path = source.parent.parent / reference_path
    require(reference_path.resolve() == source, "fact reference must resolve to the current complete ledger")
    runs = unique(ledger["runs"])
    protected_facts = unique(manifest.get("protectedRunFacts"))
    require(bool(protected_facts), "protected historical run facts required")
    for ident, protected_fact in protected_facts.items():
        require(ident in runs and sha(canonical(runs[ident]).encode("utf-8")) == protected_fact.get("canonicalFactSha256"), "protected historical run changed")
    protected = set(protected_facts)
    entries = unique(manifest.get("entries"))
    for ident, entry in entries.items():
        require(ident in runs and ident not in protected, "fact reference may not change a protected historical run")
        raw = canonical(runs[ident]).encode("utf-8")
        require(sha(raw) == entry.get("canonicalFactSha256") and len(raw) == entry.get("canonicalFactUtf8Bytes"), "referenced full fact changed: " + ident)
        require(len(raw) > CELL_MAX_TEXT_BYTES, "fact reference is limited to oversized full facts")
    return manifest, {"path": str(manifest_path.resolve()), "sha256": descriptor["sha256"]}


def project(path, fact_references=None, feature_metadata=None):
    source = Path(path).resolve()
    raw = source.read_bytes()
    ledger = json.loads(raw)
    summary = check(ledger)
    metadata, metadata_descriptor = metadata_source(source, ledger, feature_metadata)
    _, classification, associations = check_feature_metadata(ledger, metadata)
    out = {"schema": "iface-table-projection/v1", "projectId": PROJECT, "source": str(source), "sourceSha256": sha(raw), "legacyBackendDenominatorChanged": False, "summary": summary, "tables": {}}
    out["featureMetadataSource"] = metadata_descriptor
    out["additionalSources"] = [metadata_descriptor]
    reference_entries, reference_location = {}, None
    if fact_references:
        policy, descriptor = fact_reference_policy(source, ledger, fact_references)
        reference_entries = unique(policy["entries"])
        reference_location = policy["factSource"]
        out["factReferenceSource"] = descriptor
        out["additionalSources"].append(descriptor)
    if ledger.get("sourceCatalog"):
        catalog = ledger["sourceCatalog"]
        catalog_path = Path(catalog["path"])
        if not catalog_path.is_absolute():
            catalog_path = source.parent.parent / catalog_path
        require(sha(catalog_path.read_bytes()) == catalog["sha256"], "source catalog drift")
        out.setdefault("additionalSources", []).append({"path": str(catalog_path.resolve()), "sha256": catalog["sha256"]})
    for extra in ledger.get("trackingSources", []):
        extra_path = Path(extra["path"])
        if not extra_path.is_absolute():
            extra_path = source.parent.parent / extra_path
        require(sha(extra_path.read_bytes()) == extra["sha256"], "tracking source drift")
        out.setdefault("additionalSources", []).append({"path": str(extra_path.resolve()), "sha256": extra["sha256"]})
    display = {}
    terminology = ledger.get("terminologySource")
    if terminology:
        terminology_path = Path(terminology["path"])
        if not terminology_path.is_absolute():
            terminology_path = source.parent.parent / terminology_path
        require(sha(terminology_path.read_bytes()) == terminology["sha256"], "terminology source drift")
        out.setdefault("additionalSources", []).append({"path": str(terminology_path.resolve()), "sha256": terminology["sha256"]})
        for entry in load(terminology_path)["records"]:
            ident = (entry["kind"], entry["id"])
            require(ident not in display, "duplicate terminology record")
            display[ident] = entry
    overrides = {}
    override_source = ledger.get("displayOverrides")
    if override_source:
        override_path = Path(override_source["path"])
        if not override_path.is_absolute():
            override_path = source.parent.parent / override_path
        require(sha(override_path.read_bytes()) == override_source["sha256"], "display override source drift")
        out.setdefault("additionalSources", []).append({"path": str(override_path.resolve()), "sha256": override_source["sha256"]})
        for entry in load(override_path)["overrides"]:
            identity = (entry["kind"], entry["id"], entry["field"])
            require(identity not in overrides and entry["field"] in ("标题", "术语说明", "验证层", "处置理由", "实现状态") and text(entry["value"]), "invalid display override")
            overrides[identity] = entry
    applied_overrides = set()
    for kind, (name, key) in TABLES.items():
        rows = []
        facts = ledger[kind] + (ledger.get("upstreamDecisions", []) + ledger.get("kernelChanges", []) + ledger.get("productChanges", []) if kind == "tasks" else [])
        upstream_ids = {r["id"] for r in ledger.get("upstreamDecisions", [])}
        kernel_ids = {r["id"] for r in ledger.get("kernelChanges", [])}
        product_ids = {r["id"] for r in ledger.get("productChanges", [])}
        for fact in facts:
            projected_fact = canonical(fact)
            if kind == "runs" and fact["id"] in reference_entries:
                entry = reference_entries[fact["id"]]
                projected_fact = canonical({"schema": "iface-fact-reference/v1", "id": fact["id"], "encoding": "canonical-full-fact-in-source-ledger", "source": {**reference_location, "id": fact["id"]}, "canonicalFactSha256": entry["canonicalFactSha256"], "canonicalFactUtf8Bytes": entry["canonicalFactUtf8Bytes"], "contractIds": fact.get("contractIds", []), "taskIds": fact.get("taskIds", [])})
            tracking_row = kind == "tasks" and fact["id"] in upstream_ids | kernel_ids | product_ids
            row = {field: "" for field in [key] + FIELDS[kind]}
            failures = sorted((r for r in ledger["runs"] if r["result"] == "FAIL" and (fact["id"] in r.get("taskIds", []) or fact["id"] in r.get("contractIds", []))), key=lambda r: (timestamp(r.get("startedAt") or r.get("failureObservedAt") or r["recordedAt"]), r["id"]))
            row.update({key: fact["id"], "标题": fact["title"], "状态": (fact["implementationStatus"] if tracking_row else STATUSES[fact.get("status")]) if kind == "tasks" else fact.get("result", "未核实"), "阶段": fact.get("phase", ""), "责任归属": readable(fact.get("owner")), "下一步": readable(fact.get("nextAction")), "合同关联": readable(fact.get("contractIds", [])), "任务关联": readable(fact.get("taskIds", [])), "旧后端任务": readable(fact.get("legacyBackendTaskIds", [])), "验收用例": readable(fact.get("acceptanceCaseIds", [])), "首次失败": failures[0]["id"] if failures else "", "证据": readable(fact.get("evidence", [])), "事实JSON": projected_fact})
            suggestion = display.get((kind, fact["id"]))
            if suggestion:
                require(suggestion.get("originalTitle") == fact["title"], "terminology title source drift")
                if kind == "runs" and suggestion.get("sourceRecordSha256"):
                    require(suggestion["sourceRecordSha256"] == sha(canonical(fact).encode()), "historical run differs from terminology source")
                row["标题"] = suggestion["suggestedDisplayTitle"]
                row["术语说明"] = suggestion["suggestedDisplayNote"]
                if kind == "runs" and suggestion.get("suggestedDisplayLayer"):
                    row["验证层"] = suggestion["suggestedDisplayLayer"]
            if kind == "contracts":
                for field, role in (("PilotDeck责任", "pilotdeck"), ("Kernel责任", "kernel"), ("组装程序责任", "composition"), ("状态所有者", "stateOwner")):
                    row[field] = fact["responsibilities"][role]
                row["规范与代码依据"] = readable(fact["sourceRefs"])
                for layer, label in zip(AUDIT_LAYERS, LAYER_LABELS):
                    row[label] = fact["audit"][layer]["status"]
                row["状态"] = " / ".join(label + ":" + fact["audit"][layer]["status"] for layer, label in zip(AUDIT_LAYERS, LAYER_LABELS))
            elif kind == "tasks":
                if tracking_row:
                    row.update({"记录类型": "上游增量处置" if fact["id"] in upstream_ids else "接入变更", "来源仓库": fact["sourceRepository"], "来源版本": fact["sourceVersion"], "来源路径": fact["sourcePath"], "审阅层级": fact["reviewDepth"], "处置结论": fact["decision"], "处置理由": fact["reason"], "实现状态": fact["implementationStatus"], "实现提交": fact.get("implementationCommit", ""), "实测状态": fact["testStatus"], "合入状态": fact["mergeStatus"], "验收记录": readable(fact.get("runIds", []))})
                else:
                    row.update({"记录类型": "交付任务", "交付物": fact["deliverable"], "验收标准": readable(fact["acceptance"]), "前置任务": readable(fact.get("dependsOn", [])), "验收记录": readable(fact.get("runIds", [])), "交付版本": readable((fact.get("delivery") or {}).get("appliesToVersions"))})
            else:
                row.update({"用例ID": fact["caseId"], "批次ID": fact["batchId"], "验证层": suggestion.get("suggestedDisplayLayer", fact["verificationLayer"]) if suggestion else fact["verificationLayer"], "执行方式": fact["executionKind"], "实际版本": readable(fact.get("actualVersions", [])), "运行入口": fact["entrypoint"], "平台": fact["platform"], "配置SHA256": run_identity_text(fact, "configSha256"), "开始时间": run_identity_text(fact, "startedAt"), "结束时间": run_identity_text(fact, "finishedAt"), "继承范围": readable(fact.get("inheritance"))})
            collection = ("upstreamDecisions" if fact["id"] in upstream_ids else "kernelChanges" if fact["id"] in kernel_ids else "productChanges" if fact["id"] in product_ids else kind)
            associated = [item for item in associations.values() if (item["collection"], item["recordId"]) == (collection, fact["id"])]
            row.update({"功能ID": readable(sorted({item["featureId"] for item in associated})), "功能关联JSON": canonical(associated), "关联变更ID": readable(sorted({value for item in associated for value in item["relatedChangeIds"]}))})
            if kind == "tasks":
                classified = classification[(collection, fact["id"])]
                row["变更性质"] = readable(classified["changeNatures"])
                row["分类依据JSON"] = canonical(classified)
                if tracking_row:
                    branch = classified["branchIntegration"]
                    labels = {"UNKNOWN": "未核实", "NOT_VISIBLE": "观测未见", "VISIBLE": "远端可见", "NOT_MERGED": "未合入", "MERGED": "已合入", "NOT_APPLICABLE": "不适用"}
                    row.update({"本地提交": branch["localCommit"] or "未核实", "目标远端": branch["targetRemote"], "目标分支": branch["targetBranch"], "远端可见": labels[branch["remoteVisibility"]["status"]], "目标主线合入": labels[branch["targetIntegration"]["status"]], "分支观测JSON": canonical(branch)})
                    row["合入状态"] = labels[branch["targetIntegration"]["status"]] + "（" + branch["remote"] + "/" + branch["targetBranch"] + "）"
            for identity, entry in overrides.items():
                if identity[:2] == (kind, fact["id"]):
                    if entry["field"] == "实现状态":
                        require(kind == "tasks" and tracking_row, "implementation display override is limited to tracking records")
                    require(entry["field"] in row and row[entry["field"]] == entry["expected"], "display override baseline drift")
                    row[entry["field"]] = entry["value"]
                    if entry["field"] == "实现状态":
                        row["状态"] = entry["value"]
                    applied_overrides.add(identity)
            row.pop("行SHA256")
            row["行SHA256"] = sha(canonical(row).encode())
            rows.append(row)
        out["tables"][kind] = {"name": name, "key": key, "rows": rows}
    require(applied_overrides == set(overrides), "display override record missing")
    require(sha(source.read_bytes()) == out["sourceSha256"], "source changed while projecting")
    for extra in out.get("additionalSources", []):
        require(sha(Path(extra["path"]).read_bytes()) == extra["sha256"], "additional source changed while projecting")
    return out


def validate_binding(binding, base_token):
    require(binding.get("schema") == "iface-table-binding/v1" and binding.get("projectId") == PROJECT, "wrong binding identity")
    require(binding.get("baseToken") == base_token and text(base_token), "binding/base mismatch")
    require(binding.get("schemaConfirmedByRoot") is True, "root must confirm actual schemas")
    protected = binding.get("protectedTableIds", [])
    require(isinstance(protected, list) and len(protected) == 6 and len(set(protected)) == 6 and all(text(v) for v in protected), "protect the six distinct existing tables")
    require(set(binding.get("tables", {})) == set(TABLES), "exactly three managed tables required")
    ids = []
    for kind, (name, key) in TABLES.items():
        table = binding["tables"][kind]
        require(table.get("name") == name and text(table.get("tableId")), "unconfirmed table identity")
        require(set(table.get("fields", [])) == {key, *FIELDS[kind], *LINKS[kind]}, "schema mapping mismatch")
        require(set(table.get("linkFields", {})) == set(LINKS[kind]), "required actual link fields missing")
        for label, (target_kind, source_field) in LINKS[kind].items():
            expected = {"link_table": binding["tables"][target_kind]["tableId"], "bidirectional": False, "targetKind": target_kind, "sourceField": source_field}
            require(table["linkFields"][label] == expected, "link target outside managed tables or wrong relation")
        ids.append(table["tableId"])
    require(len(set(ids)) == 3 and not set(ids) & set(protected), "duplicate/protected table target")


def plan(projection, remote):
    require(projection.get("schema") == "iface-table-projection/v1" and projection.get("projectId") == PROJECT, "wrong projection")
    require(remote.get("schema") == "iface-table-remote/v1" and remote.get("complete") is True and remote.get("projectId") == PROJECT, "complete explicit remote snapshot required")
    require(set(remote.get("tables", {})) == set(TABLES) and set(projection.get("tables", {})) == set(TABLES), "three tables required")
    managed_ids = [t.get("tableId") for t in remote["tables"].values()]
    protected = remote.get("protectedTableIds", [])
    require(len(set(managed_ids)) == 3 and all(text(v) for v in managed_ids) and len(set(protected)) == 6 and len(protected) == 6 and not set(managed_ids) & set(protected), "unsafe table boundary")
    result = []
    for kind, (name, key) in TABLES.items():
        target = remote["tables"][kind]
        fields = {key, *FIELDS[kind]}
        require(target.get("name") == name and set(target.get("fields", [])) == fields | set(LINKS[kind]), "remote schema mismatch")
        require(set(target.get("linkFields", {})) == set(LINKS[kind]), "remote link schema missing")
        for label, (target_kind, source_field) in LINKS[kind].items():
            require(target["linkFields"][label] == {"link_table": remote["tables"][target_kind]["tableId"], "bidirectional": False, "targetKind": target_kind, "sourceField": source_field}, "wrong link destination")
        desired = unique(projection["tables"][kind]["rows"], key)
        existing = {}
        record_ids = set()
        for record in target.get("records", []):
            rid, values = record.get("id"), record.get("fields", {})
            stable = values.get(key)
            require(text(rid) and rid not in record_ids and text(stable) and stable not in existing, "remote missing/duplicate identity")
            require(stable in desired, "unknown remote stable ID: " + stable)
            require(set(values) <= fields | set(LINKS[kind]) and all(isinstance(v, (str, type(None))) for k, v in values.items() if k in fields), "unknown/non-text remote field")
            for label in LINKS[kind]:
                link_ids(values.get(label))
            existing[stable] = record
            record_ids.add(rid)
        creates, updates = [], {}
        for stable, values in desired.items():
            require(set(values) == fields and all(isinstance(v, str) for v in values.values()), "invalid desired row")
            if stable not in existing:
                creates.append(values)
                continue
            old = {k: v or "" for k, v in existing[stable]["fields"].items() if k in fields}
            if kind == "runs":
                require(old.get("事实JSON") == values["事实JSON"], "historical execution is immutable: " + stable)
            if old.get("首次失败"):
                require(old["首次失败"] == values["首次失败"], "first failure must remain: " + stable)
            delta = {k: v for k, v in values.items() if old.get(k, "") != v}
            if delta:
                updates[existing[stable]["id"]] = delta
        operations = batch_operations("+record-batch-create", target["tableId"], creates)
        operations += batch_operations("+record-batch-update", target["tableId"], updates)
        result.append({"kind": kind, "tableId": target["tableId"], "create": len(creates), "update": len(updates), "operations": operations})
    relationships = {kind: {row[TABLES[kind][1]]: {label: relation_values(row, field) for label, (_, field) in LINKS[kind].items()} for row in projection["tables"][kind]["rows"]} for kind in TABLES}
    return {"schema": "iface-table-plan/v1", "projectId": PROJECT, "sourceSha256": projection["sourceSha256"], "remoteSha256": sha(canonical(remote).encode()), "noDeletes": True, "rootOnlyWrites": True, "tables": result, "relationships": relationships, "linkWritePolicy": "CREATE_RECORDS_THEN_RESOLVE_REAL_IDS_AND_VERIFY"}


def link_ids(value):
    if value in (None, ""):
        return []
    require(isinstance(value, list), "link value must be an actual relation array")
    result = []
    for item in value:
        require(isinstance(item, dict) and text(item.get("id")), "link entry needs actual record ID")
        result.append(item["id"])
    require(len(set(result)) == len(result), "duplicate link IDs")
    return sorted(result)


def link_operations(projection, remote):
    """Only after all desired records are visible; never invent service record IDs."""
    plan(projection, remote)
    identities = {kind: {r["fields"][TABLES[kind][1]]: r for r in remote["tables"][kind]["records"]} for kind in TABLES}
    operations = []
    for kind, table in projection["tables"].items():
        updates = {}
        for row in table["rows"]:
            stable = row[TABLES[kind][1]]
            require(stable in identities[kind], "source record not visible before linking")
            record = identities[kind][stable]
            fact = json.loads(row["事实JSON"])
            for label, (target_kind, source_field) in LINKS[kind].items():
                linked = []
                for target_id in relation_values(row, source_field):
                    require(target_id in identities[target_kind], "target record not visible before linking")
                    linked.append(identities[target_kind][target_id]["id"])
                linked.sort()
                if link_ids(record["fields"].get(label)) != linked:
                    updates.setdefault(record["id"], {})[label] = [{"id": rid} for rid in linked]
        operations += batch_operations("+record-batch-update", remote["tables"][kind]["tableId"], updates)
    return operations


def schema():
    return {"schema": "iface-table-schema/v1", "projectId": PROJECT, "tables": [{"kind": kind, "name": name, "primaryField": key, "fields": [{"name": field, "type": "TEXT"} for field in [key] + FIELDS[kind]] + [{"name": label, "type": "LINK", "targetKind": target, "sourceField": source, "bidirectional": False} for label, (target, source) in LINKS[kind].items()]} for kind, (name, key) in TABLES.items()], "note": "先建三个表并取得实际table IDs，再建立当前schema中的单向LINK字段。文本稳定ID仅辅助查阅；root_apply创建记录后解析真实record IDs并写关系。"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    item = sub.add_parser("check")
    item.add_argument("--ledger", required=True)
    item.add_argument("--previous")
    item.add_argument("--feature-metadata", help="independent SHA-bound metadata document")
    item.add_argument("--verify-evidence-root")
    item.add_argument("--out")
    item = sub.add_parser("schema")
    item.add_argument("--out", required=True)
    item = sub.add_parser("project")
    item.add_argument("--ledger", required=True)
    item.add_argument("--feature-metadata", help="SHA-bound independent feature/classification/applicability metadata")
    item.add_argument("--fact-references", help="explicit SHA-bound full-fact transport policy; never modifies the ledger")
    item.add_argument("--out", required=True)
    item = sub.add_parser("plan")
    item.add_argument("--projection", required=True)
    item.add_argument("--remote", required=True)
    item.add_argument("--out", required=True)
    args = parser.parse_args()
    if args.command == "check":
        ledger = load(args.ledger)
        result = check(ledger, args.verify_evidence_root)
        if ledger.get("featureMetadata") or args.feature_metadata:
            explicit = {"path": str(Path(args.feature_metadata).resolve()), "sha256": sha(Path(args.feature_metadata).read_bytes())} if args.feature_metadata else None
            metadata, _ = metadata_source(Path(args.ledger).resolve(), ledger, explicit)
            check_feature_metadata(ledger, metadata, args.verify_evidence_root)
        if args.previous:
            check_transition(load(args.previous), ledger)
    elif args.command == "schema":
        result = schema()
    elif args.command == "project":
        descriptor = {"path": str(Path(args.fact_references).resolve()), "sha256": sha(Path(args.fact_references).read_bytes())} if args.fact_references else None
        metadata_descriptor = {"path": str(Path(args.feature_metadata).resolve()), "sha256": sha(Path(args.feature_metadata).read_bytes())} if args.feature_metadata else None
        result = project(args.ledger, descriptor, metadata_descriptor)
    else:
        result = plan(load(args.projection), load(args.remote))
    if args.out:
        save(args.out, result)
    else:
        print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
