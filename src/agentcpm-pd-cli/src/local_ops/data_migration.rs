//! `data.inspect` / `data.list_backup_entries` / `data.read_backup_artifact`
//! / `migration.plan` — the CLI-local data-migration operations (t020, S4)。
//!
//! 合同正本是 `state/cli-v1-operations.json` 对应四条（全部
//! `execution=local`）；设计正本是 `docs/plan/008_t020-legacy-data-migration.md`。
//! 数据面全部来自 `pilotdeck-gateway::public_v1::domains::data_backup`（扫
//! 描/备份核心）与 `migration`（目标格式版本）——CLI 侧只做输入解析、
//! home 解析与合同 envelope 的组装（lifecycle.rs 的 embedded-gateway 先
//! 例；无该 feature 时如实 `CAPABILITY_UNAVAILABLE`）。
//!
//! 红线：**不默认读真实 `~/.pilotdeck`**。`--data-root` / `--source-root`
//! 由调用方显式给出（tilde/env 展开后使用）；备份读取类操作只经
//! `AGENTCPM_PD_HOME` 解析的 home 里的备份索引。
//!
//! 语义要点：
//! - `data.inspect`：readOnly 扫描（identity/formatVersion/activeWriter/
//!   issues/summary），零写入（g010「检查不产生迁移写入」）。
//! - `data.list_backup_entries`：分页不自动解压执行内容（只读 manifest +
//!   entries.json）；游标越界拒绝。
//! - `data.read_backup_artifact`：校验 hash 后写入新目标；不覆盖已有文
//!   件（`DESTINATION_EXISTS`）；相对路径拒绝（`PATH_OUTSIDE_SCOPE`——内
//!   容寻址存储使 entryId 不可能逃逸，唯一逃逸面是调用方自己的
//!   destination，相对路径落点不受本操作控制）。
//! - `migration.plan`：preview 零写（不写源根/目标根）；源/目标相同或嵌
//!   套 → `INVALID_DESTINATION`；planId/planHash 为计划内容的规范化
//!   sha256（内容寻址，同一输入同一计划，008 §4 第 4 步与 T4）。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(feature = "embedded-gateway")]
use pilotdeck_gateway::public_v1::domains::{data_backup, migration};

use agentcpm_pd_client::config::{expand_tilde_with_env, resolve_pd_home};
use agentcpm_pd_client::error::ClientError;

use crate::commands::{local_request_id, local_response, write_envelope_or_fail, write_error};
use crate::local_ops::LocalOpRegistry;
use crate::{GlobalOptions, OutputFormat, usage_contract_error};

pub(super) fn register(reg: &mut LocalOpRegistry) {
    reg.op("data.inspect", |globals, input, format, out| {
        Box::pin(op_data_inspect(globals, input, format, out))
    });
    reg.op("data.list_backup_entries", |globals, input, format, out| {
        Box::pin(op_list_backup_entries(globals, input, format, out))
    });
    reg.op(
        "data.read_backup_artifact",
        |globals, input, format, out| {
            Box::pin(op_read_backup_artifact(globals, input, format, out))
        },
    );
    reg.op("migration.plan", |globals, input, format, out| {
        Box::pin(op_migration_plan(globals, input, format, out))
    });
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-gateway")]
fn finish(out: &mut dyn Write, format: OutputFormat, result: Result<Value, ClientError>) -> i32 {
    match result {
        Ok(value) => {
            write_envelope_or_fail(out, format, &local_response(local_request_id(), value))
        }
        Err(err) => write_error(out, format, &err),
    }
}

#[cfg(feature = "embedded-gateway")]
/// Validates the input against the embedded operation schema (local ops skip
/// the gateway dispatch gate — `local_ops/targets.rs` precedent).
fn validate_input(method_id: &str, input: &Value) -> Result<(), ClientError> {
    agentcpm_pd_protocol::registry::OperationRegistry::global()
        .validate_input(method_id, input)
        .map(|_| ())
        .map_err(|errs| {
            ClientError::Contract(
                agentcpm_pd_protocol::registry::validation_errors_to_contract_error(errs),
            )
        })
}

#[cfg(feature = "embedded-gateway")]
fn contract(code: &str, message: impl Into<String>) -> ClientError {
    ClientError::Contract(agentcpm_pd_protocol::errors::ContractError::new(
        code,
        message,
        agentcpm_pd_protocol::errors::RetrySpec::never(),
        agentcpm_pd_protocol::errors::EffectStatus::NotApplied,
    ))
}

#[cfg(feature = "embedded-gateway")]
fn required_str(input: &Value, field: &str) -> Result<String, ClientError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ClientError::Contract(usage_contract_error(&format!("{field} is required"))))
}

/// 读一个扫描到的源文件：缺失/不可读按 config 错误上报（镜像 local_ops 先例）。
#[cfg(feature = "embedded-gateway")]
fn read_scan_file(source: &std::path::Path, rel_path: &str) -> Result<Vec<u8>, ClientError> {
    std::fs::read(source.join(rel_path))
        .map_err(|err| ClientError::config(format!("read `{rel_path}`: {err}")))
}

// ---------------------------------------------------------------------------
// data.inspect
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-gateway")]
async fn op_data_inspect(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let result = validate_input("data.inspect", input).and_then(|_| {
        let raw = required_str(input, "dataRoot")?;
        let root = expand_tilde_with_env(&raw)?;
        let depth = match input.get("depth").and_then(Value::as_str) {
            Some("full") => data_backup::ScanDepth::Full,
            _ => data_backup::ScanDepth::Metadata,
        };
        let scan = data_backup::scan_source_root(&root, depth)
            .map_err(|error| ClientError::Contract(error))?;
        Ok(data_backup::inspect_result_json(&scan))
    });
    finish(out, format, result)
}

#[cfg(not(feature = "embedded-gateway"))]
async fn op_data_inspect(
    _globals: &GlobalOptions,
    _input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let err = crate::capability_unavailable(
        "data.inspect",
        "local data operations require the `embedded-gateway` feature; rebuild with `--features embedded-gateway`",
    );
    write_error(out, format, &err)
}

// ---------------------------------------------------------------------------
// data.list_backup_entries
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-gateway")]
async fn op_list_backup_entries(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let result = validate_input("data.list_backup_entries", input).and_then(|_| {
        let artifact_id = required_str(input, "artifactId")?;
        let home = resolve_pd_home(None)?;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let cursor_raw = input
            .get("cursor")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        list_backup_entries(&home, &artifact_id, cursor_raw.as_deref(), limit)
    });
    finish(out, format, result)
}

#[cfg(not(feature = "embedded-gateway"))]
async fn op_list_backup_entries(
    _globals: &GlobalOptions,
    _input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let err = crate::capability_unavailable(
        "data.list_backup_entries",
        "local data operations require the `embedded-gateway` feature; rebuild with `--features embedded-gateway`",
    );
    write_error(out, format, &err)
}

#[cfg(feature = "embedded-gateway")]
/// 分页清单：只读 manifest.json + entries.json（不触碰 data/ 内容文件——
/// 「分页不自动解压执行内容」）；游标越界拒绝。
fn list_backup_entries(
    home: &Path,
    artifact_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Value, ClientError> {
    let (_record, manifest, entries) =
        data_backup::load_registered_backup(home, artifact_id).map_err(ClientError::Contract)?;
    let total = entries.len();
    let mut offset = 0usize;
    if let Some(cursor) = cursor {
        let bad = || {
            contract(
                "INVALID_ARGUMENT",
                format!("invalid data.list_backup_entries cursor `{cursor}`"),
            )
        };
        let rest = cursor.strip_prefix("bkpage-").ok_or_else(bad)?;
        let (count, off) = rest.rsplit_once('-').ok_or_else(bad)?;
        let count: usize = count.parse().map_err(|_| bad())?;
        let parsed: usize = off.parse().map_err(|_| bad())?;
        if count != total {
            return Err(contract(
                "INVALID_ARGUMENT",
                "the backup entry list changed during pagination; restart from the first page",
            ));
        }
        if parsed > total {
            return Err(contract(
                "INVALID_ARGUMENT",
                format!("cursor offset {parsed} is beyond the {total} backup entries"),
            ));
        }
        offset = parsed;
    }
    let end = (offset + limit).min(total);
    let items: Vec<Value> = entries[offset..end]
        .iter()
        .map(|entry| {
            json!({
                "entryId": entry.entry_id,
                "kind": entry.kind,
                "bytes": entry.bytes,
                "checksum": entry.checksum,
            })
        })
        .collect();
    // 备份不可变：manifest.checksum 即清单的快照版本。
    Ok(json!({
        "manifest": manifest,
        "items": items,
        "nextCursor": if end < total { json!(format!("bkpage-{total}-{end}")) } else { Value::Null },
        "snapshotRevision": manifest["checksum"],
    }))
}

// ---------------------------------------------------------------------------
// data.read_backup_artifact
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-gateway")]
async fn op_read_backup_artifact(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let result = validate_input("data.read_backup_artifact", input).and_then(|_| {
        let artifact_id = required_str(input, "artifactId")?;
        let entry_id = required_str(input, "entryId")?;
        let raw_destination = required_str(input, "destination")?;
        let home = resolve_pd_home(None)?;
        read_backup_artifact(&home, &artifact_id, &entry_id, &raw_destination)
    });
    finish(out, format, result)
}

#[cfg(not(feature = "embedded-gateway"))]
async fn op_read_backup_artifact(
    _globals: &GlobalOptions,
    _input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let err = crate::capability_unavailable(
        "data.read_backup_artifact",
        "local data operations require the `embedded-gateway` feature; rebuild with `--features embedded-gateway`",
    );
    write_error(out, format, &err)
}

#[cfg(feature = "embedded-gateway")]
/// 导出指定条目：校验 hash 后写入新目标；不覆盖已有文件；相对路径拒绝。
fn read_backup_artifact(
    home: &Path,
    artifact_id: &str,
    entry_id: &str,
    raw_destination: &str,
) -> Result<Value, ClientError> {
    let (record, _manifest, entries) =
        data_backup::load_registered_backup(home, artifact_id).map_err(ClientError::Contract)?;
    let entry = entries
        .iter()
        .find(|entry| entry.entry_id == entry_id)
        .ok_or_else(|| {
            contract(
                "NOT_FOUND",
                format!("backup artifact `{artifact_id}` has no entry `{entry_id}`"),
            )
        })?;
    let destination = expand_tilde_with_env(raw_destination)?;
    if !destination.is_absolute() {
        return Err(contract(
            "PATH_OUTSIDE_SCOPE",
            format!(
                "destination `{raw_destination}` is relative; this operation only writes explicit absolute destinations"
            ),
        ));
    }
    if destination.exists() {
        return Err(contract(
            "DESTINATION_EXISTS",
            format!("destination `{}` already exists", destination.display()),
        ));
    }
    // 校验 hash 后写入（backup_entry_bytes 内部复算条目 sha256）。
    let backup_at = PathBuf::from(&record.destination);
    let bytes =
        data_backup::backup_entry_bytes(&backup_at, entry).map_err(ClientError::Contract)?;
    let parent = destination
        .parent()
        .ok_or_else(|| usage_contract_error("destination has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|err| ClientError::config(format!("mkdir parent: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                contract(
                    "DESTINATION_EXISTS",
                    format!("destination `{}` already exists", destination.display()),
                )
            } else {
                ClientError::config(format!("write {}: {err}", destination.display()))
            }
        })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|err| ClientError::config(format!("write destination: {err}")))?;
    let canonical = destination.canonicalize().unwrap_or(destination.clone());
    Ok(json!({
        "destination": canonical.to_string_lossy(),
        "bytes": bytes.len(),
        "checksum": entry.checksum,
    }))
}

// ---------------------------------------------------------------------------
// migration.plan
// ---------------------------------------------------------------------------

#[cfg(feature = "embedded-gateway")]
async fn op_migration_plan(
    _globals: &GlobalOptions,
    input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let result = validate_input("migration.plan", input).and_then(|_| {
        let raw_source = required_str(input, "sourceRoot")?;
        let raw_target = required_str(input, "targetRoot")?;
        let target_format_version = required_str(input, "targetFormatVersion")?;
        let source = expand_tilde_with_env(&raw_source)?;
        let target = expand_tilde_with_env(&raw_target)?;
        plan_migration(&source, &target, &target_format_version)
    });
    finish(out, format, result)
}

#[cfg(not(feature = "embedded-gateway"))]
async fn op_migration_plan(
    _globals: &GlobalOptions,
    _input: &Value,
    format: OutputFormat,
    out: &mut dyn Write,
) -> i32 {
    let err = crate::capability_unavailable(
        "migration.plan",
        "local data operations require the `embedded-gateway` feature; rebuild with `--features embedded-gateway`",
    );
    write_error(out, format, &err)
}

#[cfg(feature = "embedded-gateway")]
/// 源/目标相同或互相嵌套 → true（008 acceptance「源/目标根不能相同或嵌
/// 套到互相覆盖」）。
fn roots_conflict(source: &Path, target: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let a = canonical(source);
    let b = canonical(target);
    a == b || a.starts_with(&b) || b.starts_with(&a)
}

#[cfg(feature = "embedded-gateway")]
/// 计算变更清单 + 计划哈希（preview 零写；与 stage 转换同源的计数）。
fn plan_migration(
    source: &Path,
    target: &Path,
    target_format_version: &str,
) -> Result<Value, ClientError> {
    if roots_conflict(source, target) {
        return Err(contract(
            "INVALID_DESTINATION",
            format!(
                "source root `{}` and target root `{}` are the same or nested",
                source.display(),
                target.display()
            ),
        ));
    }
    if target_format_version != migration::TARGET_FORMAT_VERSION {
        return Err(contract(
            "FORMAT_UNSUPPORTED",
            format!(
                "target format `{target_format_version}` is not supported; this build migrates into `{}`",
                migration::TARGET_FORMAT_VERSION
            ),
        ));
    }
    let scan = data_backup::scan_source_root(source, data_backup::ScanDepth::Full)
        .map_err(ClientError::Contract)?;
    if let Some(writer) = &scan.active_writer {
        return Err(contract(
            "WRITER_ACTIVE",
            format!(
                "the source data root still has an active writer ({writer}); a plan would be immediately stale"
            ),
        ));
    }

    // 计数（与 stage 转换同源：project_transcript / registry / tasks）。
    let mut sessions = 0u64;
    let mut history_entries = 0u64;
    let mut run_records = 0u64;
    let mut schedules = 0u64;
    for file in &scan.files {
        match file.kind {
            "session-transcript" | "subagent-transcript" => {
                let raw = read_scan_file(source, &file.rel_path)?;
                let projection = data_backup::project_transcript(&raw);
                sessions += 1;
                history_entries += projection.messages.len() as u64;
            }
            "run-registry" => {
                let raw = read_scan_file(source, &file.rel_path)?;
                if let Ok(parsed) = serde_json::from_slice::<Value>(&raw) {
                    if let Some(runs) = parsed["runs"].as_object() {
                        run_records += runs.len() as u64;
                    }
                }
            }
            "cron-task" => {
                let raw = read_scan_file(source, &file.rel_path)?;
                if let Ok(parsed) = serde_json::from_slice::<Value>(&raw) {
                    if let Some(tasks) = parsed["tasks"].as_array() {
                        schedules += tasks.len() as u64;
                    }
                }
            }
            _ => {}
        }
    }
    let count_kind = |kind: &str| scan.files.iter().filter(|file| file.kind == kind).count() as u64;
    // 项目：projects/<p>/.cwd 条目为权威计数（缺失 .cwd 的目录也会转换，
    // 计入 .cwd 存在的口径；stage 同源——stage 按 projects/ 目录计数）。
    let mut project_dirs: std::collections::BTreeMap<String, ()> =
        std::collections::BTreeMap::new();
    for file in &scan.files {
        if let Some(rest) = file.rel_path.strip_prefix("projects/") {
            if let Some(project) = rest.split('/').next() {
                project_dirs.insert(project.to_owned(), ());
            }
        }
    }
    let projects = project_dirs.len() as u64;

    let mut changes: Vec<Value> = Vec::new();
    let mut push = |kind: &str, count: u64, action: &str| {
        if count > 0 {
            changes.push(json!({"kind": kind, "count": count, "action": action}));
        }
    };
    push("project", projects, "convert");
    push("session", sessions, "convert");
    push("history-entry", history_entries, "convert");
    push("run-record", run_records, "convert");
    push("schedule", schedules, "convert");
    push(
        "skill",
        count_kind("skill"),
        "manual-via-skills.plan_import",
    );
    push(
        "memory",
        count_kind("memory"),
        "manual-via-memory.plan_import",
    );
    push(
        "credential",
        count_kind("credential"),
        "manual-via-channels.configure",
    );
    push(
        "config-key",
        count_kind("config"),
        "manual-via-configuration.update",
    );
    push(
        "always-on",
        count_kind("always-on"),
        "unsupported_projection",
    );
    push("auxiliary-file", count_kind("runtime"), "skip");

    // unsupported：与 stage/verify 共享同一函数（同输入同输出）。
    let items: Vec<(String, String)> = scan
        .files
        .iter()
        .map(|file| (file.rel_path.clone(), file.kind.to_owned()))
        .collect();
    let unsupported = data_backup::unsupported_projection(source, &items);

    let target_identity = data_backup::target_identity(target);
    // planHash：身份字段 + changes + unsupported 的规范化序列化 sha256
    // （serde_json 的 BTreeMap 键序 → 确定性；008 §11 / T4）。
    let canonical = json!({
        "sourceIdentity": scan.identity,
        "sourceRevision": scan.revision,
        "targetIdentity": target_identity,
        "targetFormatVersion": migration::TARGET_FORMAT_VERSION,
        "requiredBackup": true,
        "changes": changes,
        "unsupportedEntries": unsupported,
    });
    let serialized = serde_json::to_string(&canonical)
        .map_err(|err| ClientError::config(format!("serialize plan: {err}")))?;
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    Ok(json!({
        "planId": format!("plan-{}", &digest[..32]),
        "planHash": digest,
        "sourceIdentity": scan.identity,
        "sourceRevision": scan.revision,
        "targetIdentity": target_identity,
        "requiredBackup": true,
        "changes": changes,
        "unsupportedEntries": unsupported,
    }))
}

// ---------------------------------------------------------------------------
// Tests（直接测共享核心的 local 侧组装；二进制级往返见 e2e_v1_data_migration.rs）
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "embedded-gateway"))]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-localops-datamig-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_legacy_root(tmp: &Path) -> PathBuf {
        let root = tmp.join("legacy");
        std::fs::create_dir_all(root.join("projects").join("p1").join("chats")).unwrap();
        std::fs::create_dir_all(root.join("runs")).unwrap();
        std::fs::write(root.join("projects").join("p1").join(".cwd"), "/tmp/p1").unwrap();
        std::fs::write(
            root.join("projects").join("p1").join("chats").join("s.jsonl"),
            concat!(
                r#"{"type":"session_metadata","sessionId":"tui:project=/tmp/p1:default","turnId":"t0","sequence":1,"createdAt":"2026-11-01T00:00:00Z","title":"T","tag":"old"}"#,
                "\n",
                r#"{"type":"accepted_input","sessionId":"tui:project=/tmp/p1:default","turnId":"t1","sequence":2,"createdAt":"2026-11-01T00:00:01Z","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
                "\n",
                r#"{"type":"assistant_message","sessionId":"tui:project=/tmp/p1:default","turnId":"t1","sequence":3,"createdAt":"2026-11-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
                "\n",
                r#"{"type":"model_stream_event","sessionId":"tui:project=/tmp/p1:default","turnId":"t1","sequence":4,"createdAt":"2026-11-01T00:00:03Z","delta":"x"}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("runs").join("registry.json"),
            r#"{"runs":{"r-1":{"sessionKey":"tui:project=/tmp/p1:default","state":"completed"}}}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn plan_lists_changes_and_unsupported() {
        let tmp = temp_dir("plan");
        let root = seed_legacy_root(&tmp);
        let target = tmp.join("target-home");
        std::fs::create_dir_all(&target).unwrap();
        let plan = plan_migration(&root, &target, migration::TARGET_FORMAT_VERSION).unwrap();
        assert!(
            plan["planId"]
                .as_str()
                .is_some_and(|id| id.starts_with("plan-"))
        );
        let find = |kind: &str| -> Value {
            plan["changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|change| change["kind"] == json!(kind))
                .cloned()
                .unwrap()
        };
        assert_eq!(find("project")["count"], json!(1));
        assert_eq!(find("session")["count"], json!(1));
        assert_eq!(find("history-entry")["count"], json!(2));
        assert_eq!(find("run-record")["count"], json!(1));
        assert_eq!(find("project")["action"], json!("convert"));
        let unsupported = plan["unsupportedEntries"].as_array().unwrap();
        assert!(
            unsupported
                .iter()
                .any(|entry| entry.as_str().unwrap().contains("model_stream_event")),
            "unsupported lists the unconvertible transcript type: {unsupported:?}"
        );
        // 同一输入 → 同一 planId/planHash（确定性）。
        let again = plan_migration(&root, &target, migration::TARGET_FORMAT_VERSION).unwrap();
        assert_eq!(plan["planId"], again["planId"]);
        assert_eq!(plan["planHash"], again["planHash"]);

        // 相同根 → INVALID_DESTINATION；嵌套 → INVALID_DESTINATION。
        let err = plan_migration(&root, &root, migration::TARGET_FORMAT_VERSION).unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "INVALID_DESTINATION"));
        let err = plan_migration(&root, &tmp, migration::TARGET_FORMAT_VERSION).unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "INVALID_DESTINATION"));
        // 不支持的目标格式 → FORMAT_UNSUPPORTED。
        let err = plan_migration(&root, &target, "agentcpm-pd.v9/9").unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "FORMAT_UNSUPPORTED"));
    }

    #[test]
    fn list_and_read_backup_entries_page_and_verify() {
        let tmp = temp_dir("list-read");
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let root = seed_legacy_root(&tmp);
        // 写一个备份（直接用共享核心；不经 gateway 域）。
        let scan = data_backup::scan_source_root(&root, data_backup::ScanDepth::Full).unwrap();
        let destination = tmp.join("backup-1");
        data_backup::write_backup(&root, &scan, "art-1", &destination, false).unwrap();
        data_backup::register_backup(
            &home,
            &data_backup::BackupRecord {
                artifact_id: "art-1".to_owned(),
                data_root: root.to_string_lossy().into_owned(),
                destination: destination.to_string_lossy().into_owned(),
                format_version: data_backup::BACKUP_FORMAT_VERSION.to_owned(),
                created_at: "2026-11-19T00:00:00Z".to_owned(),
                source_identity: scan.identity.clone(),
                source_revision: scan.revision.clone(),
                checksum: String::new(),
                includes_secrets: false,
                entry_count: 0,
            },
        )
        .unwrap();
        // 用真实 checksum/entry_count 重登记（写备份时才算得出来）。
        let (manifest, entries) = data_backup::load_backup_at(&destination).unwrap();
        data_backup::register_backup(
            &home,
            &data_backup::BackupRecord {
                artifact_id: "art-1".to_owned(),
                data_root: root.to_string_lossy().into_owned(),
                destination: destination.to_string_lossy().into_owned(),
                format_version: data_backup::BACKUP_FORMAT_VERSION.to_owned(),
                created_at: "2026-11-19T00:00:00Z".to_owned(),
                source_identity: scan.identity.clone(),
                source_revision: scan.revision.clone(),
                checksum: manifest["checksum"].as_str().unwrap().to_owned(),
                includes_secrets: false,
                entry_count: entries.len() as u64,
            },
        )
        .unwrap();

        // 分页：limit=1 两页。
        let page = list_backup_entries(&home, "art-1", None, 1).unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert!(page["nextCursor"].as_str().is_some());
        let cursor = page["nextCursor"].as_str().unwrap().to_owned();
        let page2 = list_backup_entries(&home, "art-1", Some(&cursor), 200).unwrap();
        assert!(page2["nextCursor"].is_null());
        assert_eq!(
            page2["manifest"]["entryCount"].as_u64().unwrap(),
            entries.len() as u64
        );
        // 越界游标拒绝。
        let err =
            list_backup_entries(&home, "art-1", Some("bkpage-999999-999999"), 50).unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "INVALID_ARGUMENT"));

        // 未知 artifact → NOT_FOUND。
        let err = list_backup_entries(&home, "missing", None, 50).unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "NOT_FOUND"));

        // 导出条目：hash 校验后写新目标；已有文件拒绝。
        let entry = &entries[0];
        let out_path = tmp.join("exported").join("entry.jsonl");
        let result = read_backup_artifact(
            &home,
            "art-1",
            &entry.entry_id,
            out_path.to_string_lossy().as_ref(),
        )
        .unwrap();
        assert!(result["checksum"].as_str().is_some());
        assert!(out_path.exists());
        let err = read_backup_artifact(
            &home,
            "art-1",
            &entry.entry_id,
            out_path.to_string_lossy().as_ref(),
        )
        .unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "DESTINATION_EXISTS"));
        // 相对路径 → PATH_OUTSIDE_SCOPE。
        let err = read_backup_artifact(&home, "art-1", &entry.entry_id, "relative-out.jsonl")
            .unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "PATH_OUTSIDE_SCOPE"));
        // 未知条目 → NOT_FOUND。
        let err = read_backup_artifact(
            &home,
            "art-1",
            "no-such-entry",
            tmp.join("x").to_string_lossy().as_ref(),
        )
        .unwrap_err();
        assert!(matches!(&err, ClientError::Contract(c) if c.code == "NOT_FOUND"));
    }
}
