//! Local instance bootstrap (`instance.initialize`), service inspection/stop,
//! and standalone `server serve` loopback listener (`007 §7–§8, WP4`).

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::OPERATION_VERSION;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_INSTANCE_MISMATCH, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND,
    CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId, generate_csprng_token_hex};
use chrono::Utc;
use rusqlite::{OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, oneshot, watch};

use crate::acceptor;
use crate::http_proxy;
use crate::public_v1::PublicV1State;
use crate::public_v1::runtime::PublicV1Runtime;
use crate::public_v1::store::{InstanceMeta, V1Store};

const MAX_V1_CONNECTIONS: usize = 64;
const CODE_ACTIVE_RESOURCES: &str = "ACTIVE_RESOURCES";
const CODE_STOP_FAILED: &str = "STOP_FAILED";

static BOOT_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalInstanceFile {
    pub instance_id: String,
    pub owner_id: String,
    pub product: String,
    pub protocol_version: String,
    pub node_id: String,
    pub initialized_at: String,
    pub initialization_revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyFile {
    pub pid: u32,
    pub instance_id: String,
    pub generation: String,
    pub protocol_version: String,
    pub ws_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    pub started_at: String,
    pub server_revision: String,
}

pub fn ensure_secure_dir(path: &Path) -> Result<(), ContractError> {
    fs::create_dir_all(path).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("failed to create dir {}: {e}", path.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("failed to chmod 0700 {}: {e}", path.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    }
    Ok(())
}

pub fn atomic_write_file(path: &Path, bytes: &[u8], secret: bool) -> Result<(), ContractError> {
    let parent = path.parent().ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("path {} has no parent", path.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    ensure_secure_dir(parent)?;

    let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".tmp.{}.{}.{}", std::process::id(), seq, "boot"));
    // P3 (wp4 review): a failed write must not leave the (possibly secret,
    // 0600) temp copy behind; the guard removes it on every error exit.
    struct TmpCleanup<'a>(&'a Path);
    impl Drop for TmpCleanup<'_> {
        fn drop(&mut self) {
            let _ = fs::remove_file(self.0);
        }
    }
    let _tmp_guard = TmpCleanup(&tmp);
    let mode = if secret { 0o600 } else { 0o644 };
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let mut f = opts.open(&tmp).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("create temp file {}: {e}", tmp.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    f.write_all(bytes).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("write temp file {}: {e}", tmp.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    f.sync_all().map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("sync temp file {}: {e}", tmp.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    drop(f);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("chmod temp file {}: {e}", tmp.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    }

    fs::rename(&tmp, path).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("rename {}: {e}", tmp.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    Ok(())
}

/// Executes `instance.initialize` creating `<home>/local.json`, `<home>/local.token` (`0600`),
/// and `<home>/v1/state.sqlite` WAL (`0700` dirs). Rejects non-empty/already-initialized roots.
pub fn initialize_instance(
    data_root: &Path,
    explicit_instance_id: Option<&str>,
    node_id_str: Option<&str>,
    expected_absent: Option<bool>,
) -> Result<Value, ContractError> {
    if expected_absent == Some(false) {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "expectedAbsent must be true",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let local_json = data_root.join("local.json");
    let local_token = data_root.join("local.token");
    let sqlite_path = data_root.join("v1").join("state.sqlite");
    if local_json.exists() || local_token.exists() || sqlite_path.exists() {
        return Err(ContractError::new(
            CODE_ALREADY_EXISTS,
            format!("dataRoot `{}` is already initialized", data_root.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    let node_str = node_id_str.unwrap_or("local");
    let node = NodeId::new(node_str).map_err(|msg| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            msg,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let id_gen = IdGenerator::new(node);
    let instance_id = match explicit_instance_id {
        Some(s) if !s.trim().is_empty() => s.trim().to_owned(),
        _ => id_gen.next_id(),
    };
    let owner_id = id_gen.next_id();
    // P3 (wp4 review): slice the *generated* id's own length, not the
    // caller-supplied instanceId's, so the revision prefix stays 16 chars.
    let rev_source = id_gen.next_id();
    let init_rev = format!("init-{}", &rev_source[..16.min(rev_source.len())]);
    let now = Utc::now().to_rfc3339();

    ensure_secure_dir(data_root)?;

    let meta_file = LocalInstanceFile {
        instance_id: instance_id.clone(),
        owner_id: owner_id.clone(),
        product: "agentcpm-pd".to_owned(),
        protocol_version: OPERATION_VERSION.to_owned(),
        node_id: node_str.to_owned(),
        initialized_at: now.clone(),
        initialization_revision: init_rev.clone(),
    };

    let meta = InstanceMeta {
        instance_id: instance_id.clone(),
        owner_id: owner_id.clone(),
        product: "agentcpm-pd".to_owned(),
        protocol_version: OPERATION_VERSION.to_owned(),
        node_id: node_str.to_owned(),
        generation: format!("gen-{}", id_gen.next_id()),
        server_revision: "srv-init".to_owned(),
        initialization_revision: init_rev.clone(),
    };
    // P2 (wp4 review): write order is store -> local.json -> token (the
    // secret last), and a failure after the first artifact removes what was
    // written — a half-initialized root used to fail every retry with
    // ALREADY_EXISTS (e.g. a token without local.json).
    let cleanup_partial_init = |also_local_json: bool| {
        if also_local_json {
            let _ = fs::remove_file(&local_json);
        }
        let _ = fs::remove_file(data_root.join("v1").join("state.sqlite"));
        let _ = fs::remove_file(data_root.join("v1").join("state.sqlite-wal"));
        let _ = fs::remove_file(data_root.join("v1").join("state.sqlite-shm"));
    };
    let store = V1Store::open_and_recover(data_root, meta).inspect_err(|_| {
        cleanup_partial_init(false);
    })?;
    drop(store);

    if let Err(error) = atomic_write_file(
        &local_json,
        &serde_json::to_vec_pretty(&meta_file).unwrap_or_default(),
        true,
    ) {
        cleanup_partial_init(false);
        return Err(error);
    }

    // Generate cryptographically random 256-bit OS CSPRNG bearer token without logging or exposing it.
    let token_hex = generate_csprng_token_hex().map_err(|e| {
        cleanup_partial_init(true);
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("generate CSPRNG local.token: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if let Err(error) = atomic_write_file(&local_token, token_hex.as_bytes(), true) {
        cleanup_partial_init(true);
        return Err(error);
    }

    Ok(json!({
        "identity": {
            "instanceId": instance_id,
            "ownerId": owner_id,
            "product": "agentcpm-pd",
            "protocolVersion": OPERATION_VERSION,
            "nodeId": node_str,
        },
        "dataRoot": data_root.display().to_string(),
        "initializationRevision": init_rev,
        "configured": true,
    }))
}

pub fn load_local_instance_and_token(
    data_root: &Path,
    expected_instance_id: Option<&str>,
) -> Result<(LocalInstanceFile, String), ContractError> {
    let local_json = data_root.join("local.json");
    let local_token = data_root.join("local.token");
    if !local_json.exists() || !local_token.exists() {
        return Err(ContractError::new(
            CODE_NOT_FOUND,
            format!(
                "dataRoot `{}` is not initialized (run `agentcpm-pd instance init`)",
                data_root.display()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let meta_raw = fs::read_to_string(&local_json).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("read {}: {e}", local_json.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let file: LocalInstanceFile = serde_json::from_str(&meta_raw).map_err(|e| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("malformed {}: {e}", local_json.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if let Some(expected) = expected_instance_id
        && file.instance_id != expected
    {
        return Err(ContractError::instance_mismatch(
            expected,
            &file.instance_id,
        ));
    }

    let token = fs::read_to_string(&local_token)
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("read {}: {e}", local_token.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?
        .trim()
        .to_owned();
    if token.is_empty() {
        return Err(ContractError::new(
            CODE_UNAVAILABLE,
            "local.token is empty",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok((file, token))
}

pub fn inspect_service_status(
    data_root: &Path,
    expected_instance_id: Option<&str>,
) -> Result<Value, ContractError> {
    let (file, _) = load_local_instance_and_token(data_root, expected_instance_id)?;
    let ready_path = data_root.join("run").join("ready.json");
    if !ready_path.exists() {
        return Ok(json!({
            "pid": Value::Null,
            "identity": Value::Null,
            "generation": Value::Null,
            "serviceState": "stopped",
        }));
    }

    let ready_raw = fs::read_to_string(&ready_path).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("read {}: {e}", ready_path.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let ready: ReadyFile = serde_json::from_str(&ready_raw).map_err(|_| {
        ContractError::new(
            CODE_UNAVAILABLE,
            "malformed ready.json",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if !is_pid_alive(ready.pid) || !is_gateway_lock_held(data_root) {
        return Ok(json!({
            "pid": Value::Null,
            "identity": Value::Null,
            "generation": Value::Null,
            "serviceState": "stopped",
        }));
    }
    if ready.instance_id != file.instance_id {
        return Err(ContractError::new(
            CODE_INSTANCE_MISMATCH,
            format!(
                "running gateway instance `{}` does not match `{}`",
                ready.instance_id, file.instance_id
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }

    Ok(json!({
        "pid": ready.pid,
        "identity": {
            "instanceId": file.instance_id,
            "ownerId": file.owner_id,
            "product": file.product,
            "protocolVersion": file.protocol_version,
        },
        "generation": ready.generation,
        "serviceState": "ready",
    }))
}

pub fn stop_local_service(
    data_root: &Path,
    expected_instance_id: Option<&str>,
    mode: &str,
    timeout_ms: u64,
) -> Result<Value, ContractError> {
    if !matches!(mode, "idle" | "drain" | "cancel") {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            format!("unsupported service.stop mode `{mode}` (expected idle, drain, or cancel)"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if timeout_ms == 0 {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "timeoutMs must be >= 1",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms.max(100)))
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(24 * 60 * 60));

    let (file, token) = load_local_instance_and_token(data_root, expected_instance_id)?;
    let ready_path = data_root.join("run").join("ready.json");
    if !ready_path.exists() {
        return Ok(json!({
            "stopped": true,
            "remainingRunIds": [],
            "remainingOperationIds": [],
            "remainingTerminalIds": [],
            "stopState": "stopped",
        }));
    }
    let ready_raw = fs::read_to_string(&ready_path).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("read ready.json: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let ready: ReadyFile = serde_json::from_str(&ready_raw).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("malformed ready.json: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if ready.instance_id != file.instance_id {
        return Err(ContractError::instance_mismatch(
            &file.instance_id,
            &ready.instance_id,
        ));
    }

    // Verify BOTH PID liveness and `gateway.instance.lock` ownership so a stale
    // ready.json after SIGKILL never signals an OS-reused unrelated PID.
    if !is_pid_alive(ready.pid) || !is_gateway_lock_held(data_root) {
        let _ = fs::remove_file(&ready_path);
        return Ok(json!({
            "stopped": true,
            "remainingRunIds": [],
            "remainingOperationIds": [],
            "remainingTerminalIds": [],
            "stopState": "stopped",
        }));
    }

    if mode == "idle" {
        let active = inspect_active_resources_readonly(data_root, &ready.generation)?;
        if !active.is_empty() {
            return Err(ContractError::new(
                CODE_ACTIVE_RESOURCES,
                format!(
                    "refusing service stop in mode=idle while {} run(s), {} operation(s), and {} terminal(s) are active; use --mode cancel or --mode drain",
                    active.run_ids.len(),
                    active.operation_ids.len(),
                    active.terminal_ids.len(),
                ),
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            ));
        }
        signal_term(ready.pid)?;
        while Instant::now() < deadline {
            if !is_pid_alive(ready.pid) {
                let _ = fs::remove_file(&ready_path);
                return Ok(json!({
                    "stopped": true,
                    "remainingRunIds": [],
                    "remainingOperationIds": [],
                    "remainingTerminalIds": [],
                    "stopState": "stopped",
                }));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let active =
            inspect_active_resources_readonly(data_root, &ready.generation).unwrap_or_default();
        return Ok(json!({
            "stopped": false,
            "remainingRunIds": active.run_ids,
            "remainingOperationIds": active.operation_ids,
            "remainingTerminalIds": active.terminal_ids,
            "stopState": "timeout",
        }));
    }

    // `drain` or `cancel`: instruct the running gateway over its authenticated
    // internal control endpoint before signaling exit.
    send_internal_stop_command(&ready, &token, mode, deadline)?;

    let mut term_sent = false;
    while Instant::now() < deadline {
        if !is_pid_alive(ready.pid) {
            let _ = fs::remove_file(&ready_path);
            return Ok(json!({
                "stopped": true,
                "remainingRunIds": [],
                "remainingOperationIds": [],
                "remainingTerminalIds": [],
                "stopState": "stopped",
            }));
        }
        if !term_sent {
            let active = inspect_active_resources_readonly(data_root, &ready.generation)?;
            if active.is_empty() {
                signal_term(ready.pid)?;
                term_sent = true;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let active =
        inspect_active_resources_readonly(data_root, &ready.generation).unwrap_or_default();
    Ok(json!({
        "stopped": false,
        "remainingRunIds": active.run_ids,
        "remainingOperationIds": active.operation_ids,
        "remainingTerminalIds": active.terminal_ids,
        "stopState": "timeout",
    }))
}

#[derive(Default)]
struct ActiveResources {
    run_ids: Vec<String>,
    operation_ids: Vec<String>,
    terminal_ids: Vec<String>,
}

impl ActiveResources {
    fn is_empty(&self) -> bool {
        self.run_ids.is_empty() && self.operation_ids.is_empty() && self.terminal_ids.is_empty()
    }
}

fn inspect_active_resources_readonly(
    data_root: &Path,
    generation: &str,
) -> Result<ActiveResources, ContractError> {
    let db_path = data_root.join("v1").join("state.sqlite");
    if !db_path.exists() {
        return Ok(ActiveResources::default());
    }
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("open sqlite readonly: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    conn.busy_timeout(Duration::from_millis(5000))
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite busy_timeout: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;

    let table_exists = |name: &str| -> Result<bool, ContractError> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("query sqlite_master `{name}`: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })
    };

    let run_ids = if table_exists("runs")? {
        let mut stmt = conn
            .prepare("SELECT run_id FROM runs WHERE terminal = 0 ORDER BY rowid ASC")
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("query active runs: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
        stmt.query_map([], |r| r.get(0))
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("query active runs: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("collect active runs: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
    } else {
        Vec::new()
    };

    let operation_ids = if table_exists("operations")? {
        let mut stmt = conn
            .prepare("SELECT operation_id FROM operations WHERE terminal = 0 ORDER BY rowid ASC")
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("query active operations: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
        stmt.query_map([], |r| r.get(0))
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("query active operations: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("collect active operations: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
    } else {
        Vec::new()
    };

    let terminal_ids = if table_exists("terminals")? {
        let mut stmt = conn
            .prepare(
                "SELECT terminal_id FROM terminals
                 WHERE state NOT IN ('exited', 'failed', 'lost') AND server_epoch = ?1
                 ORDER BY rowid ASC",
            )
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("query active terminals: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
        stmt.query_map(params![generation], |r| r.get(0))
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("query active terminals: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("collect active terminals: {e}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?
    } else {
        Vec::new()
    };

    Ok(ActiveResources {
        run_ids,
        operation_ids,
        terminal_ids,
    })
}

fn is_gateway_lock_held(data_root: &Path) -> bool {
    let lock_path = gateway_instance_lock_path(data_root);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
        return false;
    };
    file.try_lock().is_err()
}

fn send_internal_stop_command(
    ready: &ReadyFile,
    token: &str,
    mode: &str,
    deadline: Instant,
) -> Result<(), ContractError> {
    let raw_url = ready.http_url.as_deref().unwrap_or(&ready.ws_url);
    let stripped = raw_url
        .strip_prefix("http://")
        .or_else(|| raw_url.strip_prefix("https://"))
        .or_else(|| raw_url.strip_prefix("ws://"))
        .or_else(|| raw_url.strip_prefix("wss://"))
        .unwrap_or(raw_url);
    let authority = stripped.split(['/', '?']).next().unwrap_or("");
    let normalized = if let Some(port) = authority.strip_prefix("0.0.0.0:") {
        format!("127.0.0.1:{port}")
    } else if let Some(port) = authority.strip_prefix("[::]:") {
        format!("[::1]:{port}")
    } else {
        authority.to_owned()
    };
    let addr: SocketAddr = normalized.parse().map_err(|e| {
        ContractError::new(
            CODE_STOP_FAILED,
            format!("invalid gateway endpoint `{authority}` in ready.json: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let io_timeout = deadline
        .saturating_duration_since(Instant::now())
        .clamp(Duration::from_millis(200), Duration::from_secs(5));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, io_timeout).map_err(|e| {
        ContractError::new(
            CODE_STOP_FAILED,
            format!("connect gateway control endpoint {addr}: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let _ = stream.set_read_timeout(Some(io_timeout));
    let _ = stream.set_write_timeout(Some(io_timeout));
    let req = format!(
        "POST /v1/internal/stop?mode={mode} HTTP/1.1\r\nHost: {normalized}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).map_err(|e| {
        ContractError::new(
            CODE_STOP_FAILED,
            format!("send stop control request: {e}"),
            RetrySpec::never(),
            EffectStatus::Unknown,
        )
    })?;
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).map_err(|e| {
        ContractError::new(
            CODE_STOP_FAILED,
            format!("read stop control response: {e}"),
            RetrySpec::never(),
            EffectStatus::Unknown,
        )
    })?;
    let status_line = String::from_utf8_lossy(&buf[..n])
        .lines()
        .next()
        .unwrap_or("")
        .to_owned();
    if !status_line.contains(" 200 ") {
        return Err(ContractError::new(
            CODE_STOP_FAILED,
            format!("gateway control endpoint rejected {mode}: {status_line}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    Ok(())
}

/// The gateway's per-instance lock file: exactly one serving gateway per
/// `dataRoot`, held for the process lifetime.
///
/// Lock separation contract (P0, wp4fix review): the CLI client's auto-start
/// election lock is `<home>/run/daemon.lock` — the client holds it
/// exclusively from *before* spawning `server serve` until `ready.json`
/// appears (`agentcpm-pd-client` `ensure_local_gateway_running`). This
/// gateway lock therefore MUST live on a different path: sharing `daemon.lock`
/// made every auto-started gateway fail its `try_lock` against the parent
/// client and exit before writing `ready.json`, so all CLI auto-starts timed
/// out. The paths are intentionally distinct; do not alias them.
pub fn gateway_instance_lock_path(data_root: &Path) -> PathBuf {
    data_root.join("run").join("gateway.instance.lock")
}

/// Acquires the single-instance gateway lock. Must be taken BEFORE
/// `V1Store::open_and_recover`: crash recovery is destructive (running runs
/// are interrupted, the queue is deleted, pending interactions expire), so a
/// second gateway that recovered first would corrupt the live instance even
/// though it then fails on the lock (P1-A, wp4fix review).
fn acquire_gateway_instance_lock(data_root: &Path) -> Result<std::fs::File, ContractError> {
    let run_dir = data_root.join("run");
    ensure_secure_dir(&run_dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(gateway_instance_lock_path(data_root))
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("open gateway instance lock: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    lock.try_lock().map_err(|_| {
        ContractError::new(
            CODE_ALREADY_EXISTS,
            "another gateway already owns this dataRoot (gateway.instance.lock is held)".to_owned(),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    Ok(lock)
}

/// Runs the standalone foreground `public_v1` Gateway (`/v1/ws` + `/health`) on loopback.
pub async fn serve_v1_gateway(
    data_root: PathBuf,
    expected_instance_id: Option<String>,
    listen_addr: SocketAddr,
    stop: impl Future<Output = ()> + Send,
) -> Result<ReadyFile, ContractError> {
    // t021 (009 §7): a restart-handoff successor waits for its predecessor
    // pid to exit before touching anything — the single-instance lock below
    // is only free once the old gateway is gone. A no-op without the
    // `AGENTCPM_PD_GATEWAY_RESTART_HANDOFF` env.
    crate::public_v1::domains::installation::wait_for_predecessor_exit().await?;

    let (file, token) = load_local_instance_and_token(&data_root, expected_instance_id.as_deref())?;

    // P1-A (wp4fix review): claim the single-instance lock before anything
    // touches the store — a second gateway must fail fast without running
    // crash recovery over the live instance's state.
    let _instance_lock = acquire_gateway_instance_lock(&data_root)?;

    let node = NodeId::new(file.node_id.clone()).unwrap_or_else(|_| NodeId::local());
    let gen_id = IdGenerator::new(node).next_id();
    let generation = format!("gen-{gen_id}");
    let server_revision = format!("srv-{}", &gen_id[..gen_id.len().min(16)]);

    let meta = InstanceMeta {
        instance_id: file.instance_id.clone(),
        owner_id: file.owner_id.clone(),
        product: file.product.clone(),
        protocol_version: file.protocol_version.clone(),
        node_id: file.node_id.clone(),
        generation: generation.clone(),
        server_revision: server_revision.clone(),
        initialization_revision: file.initialization_revision.clone(),
    };
    let store = V1Store::open_and_recover(&data_root, meta)?;
    let runtime = PublicV1Runtime::new(store);
    // Start the scheduling machinery with the gateway itself: the scheduler
    // thread must live for the process lifetime, not just across a
    // `schedules.*` dispatch, and schedules configured before this gateway
    // started resume firing on the next tick (a failure here does not take
    // the gateway down — the schedules domain also starts lazily on the
    // first `schedules.*` dispatch, so the same error resurfaces there).
    let _ = crate::public_v1::domains::schedules::ensure_started(&runtime);
    // t021 (009 §7): complete a pending `installation.restart` — the
    // successor side of the restart handoff. A no-op without the marker in
    // the data root; failures surface in the operation record, not by
    // taking the fresh gateway down.
    {
        let install_root =
            crate::public_v1::domains::installation::layout::resolve_install_root(None)
                .unwrap_or_default();
        crate::public_v1::domains::installation::recover_pending_restart(&runtime, &install_root);
    }
    let state = Arc::new(PublicV1State::new(runtime, token));

    let listener = TcpListener::bind(listen_addr).await.map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("bind {listen_addr}: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let actual_addr = listener.local_addr().map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("local_addr: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;

    let ws_url = format!("ws://{actual_addr}/v1/ws");
    let http_url = format!("http://{actual_addr}");
    let ready = ReadyFile {
        pid: std::process::id(),
        instance_id: file.instance_id,
        generation,
        protocol_version: file.protocol_version,
        ws_url,
        http_url: Some(http_url),
        started_at: Utc::now().to_rfc3339(),
        server_revision,
    };
    let ready_path = data_root.join("run").join("ready.json");
    atomic_write_file(
        &ready_path,
        &serde_json::to_vec_pretty(&ready).unwrap_or_default(),
        false,
    )?;

    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::pin!(stop);
    let conn_limit = Arc::new(Semaphore::new(MAX_V1_CONNECTIONS));
    let mut conns = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut stop => {
                let _ = stop_tx.send(true);
                break;
            }
            _ = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    match conn_limit.clone().try_acquire_owned() {
                        Ok(permit) => {
                            let st = state.clone();
                            let rx = stop_rx.clone();
                            conns.spawn(async move {
                                let _permit = permit;
                                let _ = serve_v1_tcp_socket(stream, st, rx).await;
                            });
                        }
                        Err(_) => {
                            drop(stream);
                        }
                    }
                } else {
                    // P2 (wp4 review): a persistent accept error (e.g. EMFILE)
                    // must not turn the accept loop into a 100% CPU spin.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    let drain_deadline = tokio::time::timeout(Duration::from_secs(2), async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drain_deadline.is_err() {
        conns.abort_all();
    }

    // Remove ready.json only if it still points to our own PID.
    if let Ok(raw) = fs::read_to_string(&ready_path)
        && let Ok(disk_ready) = serde_json::from_str::<ReadyFile>(&raw)
        && disk_ready.pid == std::process::id()
    {
        let _ = fs::remove_file(&ready_path);
    }

    Ok(ready)
}

async fn serve_v1_tcp_socket(
    socket: tokio::net::TcpStream,
    state: Arc<PublicV1State>,
    stop: watch::Receiver<bool>,
) -> Result<(), crate::ProxyError> {
    // t015 P1 risk fix (same as native_ws.rs `serve_socket`): /v1/ws carries
    // interactive lanes whose round-trip budgets (terminal echo p95 <30ms)
    // cannot sit behind Nagle; loopback small frames hit the 40ms+ delayed-ACK
    // interaction without TCP_NODELAY. Node's HTTP server sets noDelay on
    // accepted sockets by default, so this also matches the TS baseline. Set
    // before the upgrade so the handshake response itself is uncoalesced.
    let _ = socket.set_nodelay(true);
    let (upgrade_tx, upgrade_rx) = oneshot::channel();
    let upgrade_holder = std::sync::Mutex::new(Some(upgrade_tx));
    let upgrade_sender = &upgrade_holder;
    let state_http = state.clone();

    http_proxy::serve_connection_native_with_shutdown(
        socket,
        move |mut request, lease| {
            let st = state_http.clone();
            async move {
                if crate::native_http::over_header_budget(&request) {
                    return Ok(crate::native_http::request_header_fields_too_large());
                }
                if http_proxy::is_upgrade(&request) {
                    match (
                        request.uri().path(),
                        acceptor::websocket_accept(request.headers()),
                        lease,
                    ) {
                        ("/v1/ws", Some(key), Some(lease)) => {
                            let bearer = PublicV1State::extract_bearer(request.headers());
                            if !st.authorize_headers_async(request.headers()).await {
                                return Ok(PublicV1State::unauthorized_http_response());
                            }
                            if let Some(tx) = upgrade_sender.lock().expect("v1 upgrade").take() {
                                let _ = tx.send((hyper::upgrade::on(&mut request), lease, bearer));
                            }
                            Ok(acceptor::websocket_response(Default::default(), &key))
                        }
                        _ => Err(std::io::Error::other("invalid /v1/ws upgrade")),
                    }
                } else if request.uri().path() == "/v1/internal/stop"
                    && request.method() == hyper::Method::POST
                {
                    if !PublicV1State::extract_bearer(request.headers())
                        .is_some_and(|tok| st.is_local_bearer_match(&tok))
                    {
                        return Ok(PublicV1State::unauthorized_http_response());
                    }
                    let mode = request
                        .uri()
                        .query()
                        .and_then(|q| {
                            q.split('&')
                                .filter_map(|kv| kv.split_once('='))
                                .find(|(k, _)| *k == "mode")
                                .map(|(_, v)| v)
                        })
                        .unwrap_or("");
                    match mode {
                        "drain" => {
                            st.runtime.enter_draining();
                            Ok(PublicV1State::json_response(
                                hyper::StatusCode::OK,
                                &json!({"accepted": true, "mode": "drain"}),
                            ))
                        }
                        "cancel" => {
                            st.runtime.cancel_all_for_stop().await;
                            Ok(PublicV1State::json_response(
                                hyper::StatusCode::OK,
                                &json!({"accepted": true, "mode": "cancel"}),
                            ))
                        }
                        _ => Ok(PublicV1State::json_response(
                            hyper::StatusCode::BAD_REQUEST,
                            &json!({"error": "invalid stop mode"}),
                        )),
                    }
                } else if request.uri().path() == "/health" {
                    let body = json!({
                        "ok": true,
                        "product": "agentcpm-pd",
                        "instanceId": st.runtime.store().meta().instance_id,
                    });
                    Ok(PublicV1State::json_response(hyper::StatusCode::OK, &body))
                } else {
                    Ok(PublicV1State::json_response(
                        hyper::StatusCode::NOT_FOUND,
                        &json!({"error": "not found"}),
                    ))
                }
            }
        },
        stop.clone(),
        None,
        None,
    )
    .await?;

    drop(upgrade_holder);
    let Ok((upgraded, lease, bearer)) = upgrade_rx.await else {
        return Ok(());
    };
    let upgraded_stream = http_proxy::upgraded_stream::<tokio::net::TcpStream>(upgraded.await?)?;
    drop(lease);

    let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded_stream,
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        Some(crate::public_v1::ws::v1_ws_config()),
    )
    .await;
    crate::public_v1::ws::serve_v1_ws(ws_stream, state, stop, bearer).await;
    Ok(())
}

pub fn default_loopback_addr() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(raw) = i32::try_from(pid) else {
            return false;
        };
        let Some(p) = rustix::process::Pid::from_raw(raw) else {
            return false;
        };
        match rustix::process::test_kill_process(p) {
            Ok(()) => true,
            Err(err) => err != rustix::io::Errno::SRCH,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn signal_term(pid: u32) -> Result<(), ContractError> {
    #[cfg(unix)]
    {
        let raw = i32::try_from(pid).map_err(|_| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("invalid pid {pid}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let p = rustix::process::Pid::from_raw(raw).ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("invalid non-positive pid {pid}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        rustix::process::kill_process(p, rustix::process::Signal::TERM).map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("failed to send SIGTERM to {pid}: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Err(ContractError::new(
            CODE_UNAVAILABLE,
            "signal termination unsupported on this platform",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn stop_local_service_validates_modes_and_overflow_without_panicking() {
        let tmp = crate::public_v1::test_support::TempDir::new("stop-validate");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();

        let err = stop_local_service(&root_dir, None, "invalid-mode", 1000).unwrap_err();
        assert_eq!(err.code, CODE_INVALID_ARGUMENT);

        let err = stop_local_service(&root_dir, None, "idle", 0).unwrap_err();
        assert_eq!(err.code, CODE_INVALID_ARGUMENT);

        // `timeout_ms == u64::MAX` must not panic in `Instant::checked_add`.
        let res = stop_local_service(&root_dir, None, "idle", u64::MAX).unwrap();
        assert_eq!(res["stopped"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_local_service_checks_idle_active_resources_and_handles_drain_and_cancel() {
        let tmp = crate::public_v1::test_support::TempDir::new("stop-modes");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (file, token) = load_local_instance_and_token(&root_dir, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-stop-1".to_owned(),
            server_revision: "srv-stop-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(&root_dir, meta).unwrap();
        let runtime = PublicV1Runtime::new(store);
        let state = Arc::new(PublicV1State::new(runtime.clone(), token.clone()));

        // Seed a project, session, and active queued run so `mode=idle` refuses stop.
        let store = runtime.store();
        store
            .register_project(&json!({
                "instanceId": file.instance_id,
                "projectId": "proj-stop",
                "name": "Stop",
                "environmentId": "local",
                "root": {"instanceId": file.instance_id, "rootId": "root-1", "relativePath": "ws"}
            }))
            .unwrap();
        store
            .create_session(&json!({
                "instanceId": file.instance_id,
                "sessionId": "sess-stop",
                "projectId": "proj-stop",
            }))
            .unwrap();
        let run_id = match store
            .submit_run(
                &json!({
                    "instanceId": file.instance_id,
                    "sessionId": "sess-stop",
                    "input": [{"kind": "text", "text": "hold"}],
                }),
                Some("stop-key-1"),
            )
            .unwrap()
        {
            crate::public_v1::store::SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("unexpected disposition: {other:?}"),
        };

        // Hold `gateway.instance.lock` as a separate open file description.
        let lock_path = gateway_instance_lock_path(&root_dir);
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock_file.try_lock().unwrap();

        // Bind a real loopback HTTP control server on `serve_v1_tcp_socket`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = watch::channel(false);
        let server_state = state.clone();
        let server_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    accepted = listener.accept() => {
                        if let Ok((sock, _)) = accepted {
                            let st = server_state.clone();
                            let rx = stop_rx.clone();
                            tokio::spawn(async move {
                                let _ = serve_v1_tcp_socket(sock, st, rx).await;
                            });
                        }
                    }
                }
            }
        });

        // 1) Access key (non-local.token) must NOT authorize `/v1/internal/stop`.
        let mut raw_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let bad_req = format!(
            "POST /v1/internal/stop?mode=drain HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer not-the-local-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        raw_stream.write_all(bad_req.as_bytes()).await.unwrap();
        let mut resp_buf = [0u8; 512];
        let n = tokio::time::timeout(Duration::from_secs(5), raw_stream.read(&mut resp_buf))
            .await
            .expect("HTTP 401 timeout")
            .unwrap();
        let resp_line = String::from_utf8_lossy(&resp_buf[..n]);
        assert!(
            resp_line.contains("401"),
            "non-local bearer must receive 401 on /v1/internal/stop, got: {resp_line}"
        );

        // Spawn a harmless dummy child PID so `signal_term` never targets our test runner.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        let ready_path = root_dir.join("run").join("ready.json");
        let ready = ReadyFile {
            pid: child_pid,
            instance_id: file.instance_id.clone(),
            generation: "gen-stop-1".to_owned(),
            protocol_version: file.protocol_version.clone(),
            ws_url: format!("ws://{addr}/v1/ws"),
            http_url: Some(format!("http://{addr}")),
            started_at: Utc::now().to_rfc3339(),
            server_revision: "srv-stop-1".to_owned(),
        };
        atomic_write_file(
            &ready_path,
            &serde_json::to_vec_pretty(&ready).unwrap(),
            false,
        )
        .unwrap();

        // 2) `idle` refuses while the active run exists with `ACTIVE_RESOURCES`.
        let root_clone = root_dir.clone();
        let idle_err = tokio::task::spawn_blocking(move || {
            stop_local_service(&root_clone, None, "idle", 1000)
        })
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(idle_err.code, CODE_ACTIVE_RESOURCES);

        // 3) `drain` hits `/v1/internal/stop?mode=drain` (setting `runtime.is_draining()`),
        //    then waits for active resources up to `timeoutMs` (reporting remainingRunIds on timeout).
        let root_clone = root_dir.clone();
        let drain_res = tokio::task::spawn_blocking(move || {
            stop_local_service(&root_clone, None, "drain", 200)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(runtime.is_draining());
        assert_eq!(drain_res["stopState"], "timeout");
        assert_eq!(drain_res["remainingRunIds"], json!([run_id]));

        // 4) `cancel` cancels active resources via `/v1/internal/stop?mode=cancel`,
        //    signals the process, and reports `stopped: true`.
        let root_clone = root_dir.clone();
        let cancel_res = tokio::task::spawn_blocking(move || {
            stop_local_service(&root_clone, None, "cancel", 5000)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(cancel_res["stopped"], true);
        assert_eq!(cancel_res["stopState"], "stopped");
        assert!(!ready_path.exists());

        let _ = stop_tx.send(true);
        server_task.abort();
        let _ = reaper.join();
    }
}
