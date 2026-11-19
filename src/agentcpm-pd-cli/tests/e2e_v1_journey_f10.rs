//! S5 故障旅程 F10（两段式 plan 过期）端到端：`_tmp/20261119-s5/journeys-execution-plan.md`
//! §3 F10 的序列与判据，经真实 `agentcpm-pd` 二进制、隔离 `AGENTCPM_PD_HOME`、
//! `common_v1` 挂具的 envelope schema 校验。
//!
//! **语义缩窄登记（按任务书决议）**：`PLAN_TTL_MINUTES=10` 是编译常量
//! （memory.rs:89 / snapshots.rs:70，git.rs:97 的 PLAN_TTL_SECS=600 同族），
//! 本构建无注入缝（W0 未交付 env 缝），真等 10 分钟不可接受。因此
//! PLAN_EXPIRED 分支**不经 CLI 触发**（其存在与判定顺序已在 gateway 域源码
//! 核实：memory.rs:1963-1973、snapshots.rs:986-996——`expiresAt < now` 时报
//! `PLAN_EXPIRED`、`EffectStatus::NotApplied`、`RetrySpec::never`）。本文件把
//! 旅程缩窄到 CLI 可达面：
//!
//! 1. **TTL 合同锚定**：`memory plan-clear` / `memory plan-import` 输出的
//!    `plan.expiresAt` 距生成时刻 ≈ 10 分钟（±60s 窗口）——把
//!    PLAN_TTL_MINUTES=10 作为对外可观察合同钉住（若产品改动该常量，本测试
//!    失败）。
//! 2. **第二跳可达门**（复验顺序：存在 → 类型 → 未消费 → 过期 → revision，
//!    memory.rs:1945-2004）：缺失 plan → NOT_FOUND；用 import 计划充当
//!    clear（类型不符）→ INVALID_ARGUMENT；过期 planRevision →
//!    REVISION_CONFLICT（无副作用）；snapshots.restore 的同族门（缺失 →
//!    NOT_FOUND、跨类型 → INVALID_ARGUMENT）。
//! 3. **重新 plan → commit 成功**：plan-clear → clear 走通，记忆计数 2→0 而
//!    项目/会话/工作区文件仍在（文件树/记忆计数前后对照）。
//! 4. **同幂等键重放不扩大**：clear 之后新种一条记忆，同 `--request-key` 重放
//!    原 clear——回放原回执（deduplicated + 同 operationId），新记忆存活、
//!    原 operation 的 deletedIds 不扩大。
//!
//! 快照侧（`snapshots plan-restore`）受 J4 已登记的捕获 seam 缺口阻塞
//! （`v1_file_snapshots` 无产品写入方，见 `_tmp/20261119-s5/w1a-report.md`
//! §3.1）：无法经 CLI 播种真实快照计划，本文件只验收其第二跳错误门与诚实
//! 空集，PLAN_EXPIRED 与真实 restore 主干待 seam 接线后补（J4 预留）。
//! git 分支的两段式（PLAN_TTL_SECS=600 同样 10 分钟）已由 J4 三处故障注入
//! 覆盖（stale revision / 计划后状态变动 / 重放），本文件不重复。

// The suite spawns the embedded gateway through the CLI feature; without it
// there is nothing to test.
#![cfg(feature = "embedded-gateway")]

mod common_v1;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use common_v1::{CliOutcome, TestHome, await_cli, expect_ok, init_home, init_home_instance};

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("base64")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 失败断言：非零退出码 + `outcome=failed` + `error.code`。
fn expect_failure(method: &str, out: &CliOutcome) -> String {
    assert_ne!(out.code, 0, "`{method}` must not exit 0: {}", out.stderr);
    let lines: Vec<&str> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "`{method}` stdout: {}", out.stdout);
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("`{method}` stdout not JSON: {e}"));
    assert_eq!(value["outcome"], "failed", "`{method}`: {value}");
    value["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// 解析 chrono `to_rfc3339()` 输出（`YYYY-MM-DDTHH:MM:SS(.frac)+00:00`）为
/// epoch 秒（civil 日期换用 Howard Hinnant days_from_civil；亚秒与偏移截断
/// ——服务端一律 UTC）。
fn rfc3339_epoch_seconds(value: &str) -> i64 {
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
    let (date, rest) = value
        .split_once('T')
        .unwrap_or_else(|| panic!("plan expiresAt is not RFC3339 (no T): {value}"));
    let seconds = rest
        .split(['.', '+', '-'])
        .next()
        .unwrap_or(rest)
        .trim_end_matches('Z');
    let ymd: Vec<i64> = date
        .split('-')
        .map(|p| {
            p.parse::<i64>()
                .unwrap_or_else(|e| panic!("bad year part {p}: {e}"))
        })
        .collect();
    let hms: Vec<i64> = seconds
        .split(':')
        .map(|p| {
            p.parse::<i64>()
                .unwrap_or_else(|e| panic!("bad time part {p}: {e}"))
        })
        .collect();
    assert_eq!(ymd.len(), 3, "malformed date: {value}");
    assert_eq!(hms.len(), 3, "malformed time: {value}");
    days_from_civil(ymd[0], ymd[1], ymd[2]) * 86_400 + hms[0] * 3_600 + hms[1] * 60 + hms[2]
}

/// TTL 合同锚定：`expiresAt` 必须落在生成时刻之后约 10 分钟（±60s 容差；
/// 同机时钟，进程间差远小于该窗）。这把 `PLAN_TTL_MINUTES=10` 钉为对外合同。
fn assert_plan_ttl_is_ten_minutes(expires_at: &str, what: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64;
    let delta = rfc3339_epoch_seconds(expires_at) - now;
    assert!(
        (540..=660).contains(&delta),
        "{what}: plan expiresAt `{expires_at}` is {delta}s from now; \
         expected the PLAN_TTL_MINUTES=10 window (540..=660s)"
    );
}

/// 播种一个带真实授权根的项目 + 工作区 + 会话（t013/t018 e2e 同法），并在
/// 工作区放一个标记文件（clear 前后文件树对照的锚点）。
async fn seed_workspace(tag: &str) -> (TestHome, PathBuf, String, String) {
    let home = TestHome::new(tag);
    init_home(home.path());
    let instance_id = init_home_instance(home.path());

    let workspace_rel = format!("{tag}-ws");
    let out = await_cli(
        home.path(),
        &[
            "directories",
            "create",
            "--output",
            "json",
            "--path",
            &json!({
                "authorizedRootId": "default",
                "relativePath": {"encoding": "utf8", "value": workspace_rel},
            })
            .to_string(),
            "--request-key",
            &format!("dirs-{tag}"),
        ],
    );
    expect_ok("directories.create", &out);
    let workspace = home.path().join("workspaces").join(&workspace_rel);
    assert!(
        workspace.is_dir(),
        "workspace seeded: {}",
        workspace.display()
    );

    let project_id = format!("p-{tag}");
    let out = await_cli(
        home.path(),
        &[
            "projects",
            "register",
            "--output",
            "json",
            "--project-id",
            &project_id,
            "--name",
            "S5 Journey F10",
            "--environment-id",
            "local",
            "--root",
            &format!(
                "{{\"instanceId\":\"{instance_id}\",\"rootId\":\"default\",\"relativePath\":\"{workspace_rel}\"}}"
            ),
        ],
    );
    expect_ok("project.register", &out);
    let session_id = format!("s-{tag}");
    let out = await_cli(
        home.path(),
        &[
            "sessions",
            "create",
            "--output",
            "json",
            "--session-id",
            &session_id,
            "--project-id",
            &project_id,
        ],
    );
    expect_ok("session.create", &out);

    let marker = workspace.join("marker.txt");
    std::fs::write(&marker, "f10 workspace marker\n").expect("write marker");
    (home, workspace, project_id, session_id)
}

/// 上传一段 JSON 字节为不可变产物，返回 artifactId（J5 同法）。
async fn upload_bytes(
    home: &Path,
    project_id: &str,
    display: &str,
    bytes: &[u8],
    key: &str,
) -> String {
    let out = await_cli(
        home,
        &[
            "uploads",
            "create",
            "--output",
            "json",
            "--project-id",
            project_id,
            "--display-name",
            display,
            "--media-type",
            "application/json",
            "--byte-length",
            &bytes.len().to_string(),
            "--sha256",
            &sha256_hex(bytes),
            "--request-key",
            key,
        ],
    );
    let created = expect_ok("uploads.create", &out);
    let upload_id = created["upload"]["uploadId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let out = await_cli(
        home,
        &[
            "uploads",
            "part",
            "--output",
            "json",
            "--upload-id",
            &upload_id,
            "--part-number",
            "0",
            "--bytes-base64",
            &b64_encode(bytes),
            "--sha256",
            &sha256_hex(bytes),
        ],
    );
    expect_ok("uploads.part", &out);
    let out = await_cli(
        home,
        &[
            "uploads",
            "complete",
            "--output",
            "json",
            "--upload-id",
            &upload_id,
            "--expected-sha256",
            &sha256_hex(bytes),
            "--request-key",
            &format!("{key}-complete"),
        ],
    );
    expect_ok("uploads.complete", &out);

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            home,
            &[
                "uploads",
                "get",
                "--output",
                "json",
                "--upload-id",
                &upload_id,
            ],
        );
        let got = expect_ok("uploads.get", &out);
        if got["upload"]["state"] == json!("complete") {
            return got["upload"]["artifactId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        assert!(Instant::now() < deadline, "upload never completed: {got}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 轮询 `operations get` 至终态，返回 operation JSON（含 result）。
async fn await_operation(home: &Path, operation_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let out = await_cli(
            home,
            &[
                "operations",
                "get",
                "--output",
                "json",
                "--operation-id",
                operation_id,
            ],
        );
        let got = expect_ok("operations.get", &out);
        let state = got["operation"]["state"].as_str().unwrap_or_default();
        if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "interrupted" | "result_unknown"
        ) {
            return got["operation"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "operation `{operation_id}` never reached a terminal state"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 当前 scope 的记忆条目数（`memory list`）。
async fn memory_count(home: &Path, scope: &Value) -> usize {
    let out = await_cli(
        home,
        &[
            "memory",
            "list",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let listed = expect_ok("memory.list", &out);
    listed["entries"].as_array().map(Vec::len).unwrap_or(0)
}

/// 上传一个 bundle 产物并返回 artifactId。
async fn upload_bundle(
    home: &Path,
    project_id: &str,
    scope: &Value,
    names: &[&str],
    tag: &str,
) -> String {
    let entries: Vec<Value> = names
        .iter()
        .map(|name| {
            json!({
                "kind": "project",
                "name": name,
                "description": format!("f10 entry {name}"),
                "scope": scope,
                "fields": {"notes": ["f10"]},
            })
        })
        .collect();
    let bundle = json!({
        "bundleFormatVersion": "agentcpm-pd.memory-bundle.v1",
        "createdAt": "2026-11-19T00:00:00Z",
        "scope": scope,
        "entries": entries,
        "summary": {"entryCount": names.len(), "projectIds": [project_id]},
    });
    let bundle_bytes = bundle.to_string().into_bytes();
    upload_bytes(
        home,
        project_id,
        &format!("{tag}-bundle.json"),
        &bundle_bytes,
        &format!("{tag}-bundle"),
    )
    .await
}

/// 上传 bundle → `memory plan-import` → `memory import`，返回 plan_import 的
/// 计划输出（供调用方锚定 TTL / planId）。
async fn seed_memories(home: &Path, scope: &Value, names: &[&str], tag: &str) -> Value {
    let project_id = scope["projectId"].as_str().unwrap_or_default();
    let artifact_id = upload_bundle(home, project_id, scope, names, tag).await;
    let out = await_cli(
        home,
        &[
            "memory",
            "plan-import",
            "--output",
            "json",
            "--artifact-id",
            &artifact_id,
            "--target-scope",
            &scope.to_string(),
            "--mode",
            "merge",
        ],
    );
    let plan = expect_ok("memory.plan_import", &out);
    let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
    let out = await_cli(
        home,
        &[
            "memory",
            "import",
            "--output",
            "json",
            "--plan-id",
            plan["plan"]["planId"].as_str().unwrap(),
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            &format!("{tag}-import"),
        ],
    );
    let receipt = expect_ok("memory.import", &out);
    let operation =
        await_operation(home, receipt["operationId"].as_str().unwrap_or_default()).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(
        operation["result"]["importedIds"].as_array().unwrap().len(),
        names.len(),
        "{operation}"
    );
    plan
}

/// F10 旅程一：TTL 合同锚定 + 第二跳可达门（memory 与 snapshots 双侧）。
/// PLAN_EXPIRED 本身不经 CLI 触发（文件头登记的缩窄）；本测试把 10 分钟窗口
/// 钉为可观察合同，并验收复验顺序中先于过期判定的全部可达门。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f10_plan_ttl_window_and_second_hop_gates() {
    let (home, _workspace, project_id, _session_id) = seed_workspace("f10-gates").await;
    let root = home.path();
    let scope = json!({"kind": "project", "projectId": project_id});

    // 播种两条记忆（bundle 导入路径的 plan TTL 与 clear 同族，一并锚定）。
    let import_plan = seed_memories(root, &scope, &["alpha", "beta"], "f10g-seed").await;
    assert_eq!(import_plan["entryCount"], json!(2));
    let import_plan_id = import_plan["plan"]["planId"].as_str().unwrap().to_owned();
    let import_plan_rev = import_plan["plan"]["planRevision"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_plan_ttl_is_ten_minutes(
        import_plan["plan"]["expiresAt"].as_str().unwrap(),
        "memory.plan_import",
    );
    assert_eq!(memory_count(root, &scope).await, 2);

    // 留一个未消费的 import 计划（用于跨类型门）。
    let open_import_artifact =
        upload_bundle(root, &project_id, &scope, &["unused"], "f10g-open").await;
    let out = await_cli(
        root,
        &[
            "memory",
            "plan-import",
            "--output",
            "json",
            "--artifact-id",
            &open_import_artifact,
            "--target-scope",
            &scope.to_string(),
            "--mode",
            "merge",
        ],
    );
    let open_import_plan = expect_ok("memory.plan_import", &out);
    let open_import_plan_id = open_import_plan["plan"]["planId"]
        .as_str()
        .unwrap()
        .to_owned();

    // ---- 第一跳：plan-clear（只读预览 + TTL 锚定）。
    let out = await_cli(
        root,
        &[
            "memory",
            "plan-clear",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let plan = expect_ok("memory.plan_clear", &out);
    assert_eq!(plan["memoryCount"], json!(2));
    assert_eq!(plan["traceCount"], json!(0));
    assert_eq!(plan["plan"]["destructive"], json!(true));
    assert_eq!(
        plan["plan"]["affectedResources"],
        json!([format!("memory:project:{project_id}")]),
        "{}",
        plan
    );
    assert!(
        plan["plan"]["conflicts"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{plan}"
    );
    let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
    let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();
    assert!(!plan_id.is_empty() && !plan_rev.is_empty(), "{plan}");
    assert_plan_ttl_is_ten_minutes(
        plan["plan"]["expiresAt"].as_str().unwrap(),
        "memory.plan_clear",
    );

    // 只读性：plan 不动记忆面。
    assert_eq!(memory_count(root, &scope).await, 2);

    // ---- 第二跳可达门（复验顺序：存在 → 类型 → 未消费 → [过期] → revision）。
    // 缺失 plan → NOT_FOUND。
    let out = await_cli(
        root,
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            "f10-missing-plan",
            "--expected-plan-revision",
            "whatever",
            "--request-key",
            "f10g-missing",
        ],
    );
    assert_eq!(expect_failure("memory.clear", &out), "NOT_FOUND");

    // 跨类型：用 import 计划充当 clear → INVALID_ARGUMENT（类型门先于过期与
    // revision 判定；已消费的 import 计划同样先撞类型门）。每发一个独立幂等键
    // （同键异输入会先撞 IDEMPOTENCY_CONFLICT，掩盖待验门）。
    for (index, (wrong_id, what)) in [
        (&open_import_plan_id, "an open import plan"),
        (&import_plan_id, "a consumed import plan"),
    ]
    .into_iter()
    .enumerate()
    {
        let out = await_cli(
            root,
            &[
                "memory",
                "clear",
                "--output",
                "json",
                "--plan-id",
                wrong_id,
                "--expected-plan-revision",
                "whatever",
                "--request-key",
                &format!("f10g-wrong-kind-{index}"),
            ],
        );
        assert_eq!(
            expect_failure("memory.clear", &out),
            "INVALID_ARGUMENT",
            "using {what} for memory.clear must be a kind error"
        );
    }

    // 过期 planRevision → REVISION_CONFLICT，无副作用。
    let out = await_cli(
        root,
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            "f10-wrong-plan-revision",
            "--request-key",
            "f10g-stale",
        ],
    );
    assert_eq!(expect_failure("memory.clear", &out), "REVISION_CONFLICT");
    assert_eq!(
        memory_count(root, &scope).await,
        2,
        "a refused second hop must not clear anything"
    );

    // ---- snapshots 侧的第二跳门（捕获 seam 缺口下可达的面）。
    // 诚实空集：本构建无快照写入方（J4 已登记）。
    let out = await_cli(
        root,
        &[
            "snapshots",
            "list",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let snapshots = expect_ok("snapshots.list", &out);
    assert_eq!(snapshots["snapshots"].as_array().unwrap().len(), 0);

    // 缺失计划 → NOT_FOUND。
    let out = await_cli(
        root,
        &[
            "snapshots",
            "restore",
            "--output",
            "json",
            "--plan-id",
            "f10-missing-snapshot-plan",
            "--expected-plan-revision",
            "whatever",
            "--request-key",
            "f10g-snap-missing",
        ],
    );
    assert_eq!(expect_failure("snapshots.restore", &out), "NOT_FOUND");

    // 跨类型：memory 的 clear/import 计划不是 restore 计划 → INVALID_ARGUMENT
    // （复验顺序与 memory 侧同族：类型门先于过期判定）。每发一个独立幂等键。
    for (index, (wrong_id, what)) in [
        (&plan_id, "a memory clear plan"),
        (&open_import_plan_id, "a memory import plan"),
    ]
    .into_iter()
    .enumerate()
    {
        let out = await_cli(
            root,
            &[
                "snapshots",
                "restore",
                "--output",
                "json",
                "--plan-id",
                wrong_id,
                "--expected-plan-revision",
                "whatever",
                "--request-key",
                &format!("f10g-snap-wrong-kind-{index}"),
            ],
        );
        assert_eq!(
            expect_failure("snapshots.restore", &out),
            "INVALID_ARGUMENT",
            "using {what} for snapshots.restore must be a kind error"
        );
    }

    // 原始 clear 计划仍开放：两个门都没消费它（副作用为零）。
    assert_eq!(memory_count(root, &scope).await, 2);
}

/// F10 旅程二：重新 plan → commit 成功；clear 后新种记忆 + 同幂等键重放原
/// clear——回放原回执且不扩大到后续新记忆（deletedIds 不增长、新条目存活）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f10_replan_commit_and_same_key_replay_does_not_extend() {
    let (home, workspace, project_id, session_id) = seed_workspace("f10-clear").await;
    let root = home.path();
    let scope = json!({"kind": "project", "projectId": project_id});

    seed_memories(root, &scope, &["alpha", "beta"], "f10c-seed").await;
    assert_eq!(memory_count(root, &scope).await, 2);

    // ---- 重新 plan → commit 成功（第一跳的计数面 + 第二跳终态）。
    let out = await_cli(
        root,
        &[
            "memory",
            "plan-clear",
            "--output",
            "json",
            "--scope",
            &scope.to_string(),
        ],
    );
    let plan = expect_ok("memory.plan_clear", &out);
    assert_eq!(plan["memoryCount"], json!(2));
    assert_plan_ttl_is_ten_minutes(
        plan["plan"]["expiresAt"].as_str().unwrap(),
        "memory.plan_clear",
    );
    let plan_id = plan["plan"]["planId"].as_str().unwrap().to_owned();
    let plan_rev = plan["plan"]["planRevision"].as_str().unwrap().to_owned();

    let out = await_cli(
        root,
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            "f10c-clear",
        ],
    );
    let receipt = expect_ok("memory.clear", &out);
    let operation_id = receipt["operationId"].as_str().unwrap().to_owned();
    let operation = await_operation(root, &operation_id).await;
    assert_eq!(operation["state"], json!("succeeded"), "{operation}");
    assert_eq!(
        operation["result"]["outcome"],
        json!("complete"),
        "{operation}"
    );
    assert_eq!(
        operation["result"]["deletedIds"].as_array().unwrap().len(),
        2,
        "{operation}"
    );
    assert!(
        operation["result"]["recoveryArtifactId"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "recovery material must be preserved: {operation}"
    );

    // 文件树/记忆计数前后对照：记忆 2→0，项目/会话/工作区标记仍在。
    assert_eq!(memory_count(root, &scope).await, 0);
    let out = await_cli(
        root,
        &[
            "projects",
            "get",
            "--output",
            "json",
            "--project-id",
            &project_id,
        ],
    );
    let project = expect_ok("project.get", &out);
    assert_eq!(
        project["project"]["projectId"].as_str(),
        Some(project_id.as_str())
    );
    let out = await_cli(
        root,
        &[
            "sessions",
            "get",
            "--output",
            "json",
            "--session-id",
            &session_id,
        ],
    );
    let session = expect_ok("session.get", &out);
    assert_eq!(
        session["session"]["sessionId"].as_str(),
        Some(session_id.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("marker.txt")).unwrap(),
        "f10 workspace marker\n",
        "clearing memory must not touch workspace files"
    );
    assert!(root.join("v1").join("state.sqlite").is_file());

    // ---- clear 之后新种一条记忆。
    seed_memories(root, &scope, &["gamma-new"], "f10c-new").await;
    assert_eq!(memory_count(root, &scope).await, 1);
    let listed = {
        let out = await_cli(
            root,
            &[
                "memory",
                "list",
                "--output",
                "json",
                "--scope",
                &scope.to_string(),
            ],
        );
        expect_ok("memory.list", &out)
    };
    let new_memory_id = listed["entries"][0]["memoryId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(!new_memory_id.is_empty(), "{listed}");

    // ---- 同幂等键重放原 clear：回放原回执（deduplicated + 同 operationId），
    // 不消费已消费计划之外的新东西，也不扩大删除面。
    let replay_out = await_cli(
        root,
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            "f10c-clear",
        ],
    );
    let replay = expect_ok("memory.clear", &replay_out);
    assert_eq!(replay["deduplicated"], json!(true), "{replay}");
    assert_eq!(
        replay["operationId"].as_str(),
        Some(operation_id.as_str()),
        "the replay must resolve to the original acceptance, not a new operation"
    );

    // 不扩大：新记忆存活、原 operation 的删除面不变。
    assert_eq!(
        memory_count(root, &scope).await,
        1,
        "the same-key replay must not extend the clear to newer memories"
    );
    let operation_after = await_operation(root, &operation_id).await;
    let deleted: Vec<&str> = operation_after["result"]["deletedIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        deleted.len(),
        2,
        "the original operation's deletion surface must not grow: {operation_after}"
    );
    assert!(
        !deleted.contains(&new_memory_id.as_str()),
        "the newer memory must never appear in the original clear's deletion set"
    );
    assert_eq!(
        memory_count(root, &scope).await,
        1,
        "the newer memory must still be listed after the replay"
    );

    // 一次性计划的第二道保险：同计划换新键 → NOT_FOUND（计划已消费；J5 已
    // 覆盖该门，这里只确认与幂等回放叠加时状态一致）。
    let out = await_cli(
        root,
        &[
            "memory",
            "clear",
            "--output",
            "json",
            "--plan-id",
            &plan_id,
            "--expected-plan-revision",
            &plan_rev,
            "--request-key",
            "f10c-clear-new-key",
        ],
    );
    assert_eq!(expect_failure("memory.clear", &out), "NOT_FOUND");
    assert_eq!(memory_count(root, &scope).await, 1);
}
