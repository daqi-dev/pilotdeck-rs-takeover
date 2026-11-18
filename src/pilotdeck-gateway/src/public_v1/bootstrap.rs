//! Local instance bootstrap (`instance.initialize`), service inspection/stop,
//! and standalone `server serve` loopback listener (`007 §7–§8, WP4`).

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agentcpm_pd_protocol::contract::OPERATION_VERSION;
use agentcpm_pd_protocol::errors::{
    CODE_ALREADY_EXISTS, CODE_FORBIDDEN, CODE_INSTANCE_MISMATCH, CODE_INVALID_ARGUMENT,
    CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

use crate::acceptor;
use crate::http_proxy;
use crate::public_v1::PublicV1State;
use crate::public_v1::runtime::PublicV1Runtime;
use crate::public_v1::store::{InstanceMeta, V1Store};

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
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode));
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
        false,
    ) {
        cleanup_partial_init(false);
        return Err(error);
    }

    // Generate cryptographically hashed random bearer token without logging or exposing it.
    let mut token_hasher = Sha256::new();
    token_hasher.update(instance_id.as_bytes());
    token_hasher.update(owner_id.as_bytes());
    token_hasher.update(id_gen.next_id().as_bytes());
    token_hasher.update(now.as_bytes());
    token_hasher.update(std::process::id().to_le_bytes());
    let token_hex = format!("{:x}", token_hasher.finalize());
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
    if !is_pid_alive(ready.pid) {
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
    let (file, _) = load_local_instance_and_token(data_root, expected_instance_id)?;
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
    // P2 (wp4 review): never signal a pid from a ready.json that belongs to
    // a different instance — a stale foreign file used to make stop SIGTERM
    // an unrelated (possibly pid-reused) process.
    if ready.instance_id != file.instance_id {
        return Err(ContractError::instance_mismatch(
            &file.instance_id,
            &ready.instance_id,
        ));
    }

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

    if mode == "idle" {
        // Check SQLite for active runs; refuse stopping idle if active runs
        // exist unless cancel/drain. A read failure must not degrade into
        // "no active runs" (P2 wp4 review).
        let active_count = count_non_terminal_runs_readonly(data_root)?;
        if active_count > 0 {
            return Err(ContractError::new(
                CODE_FORBIDDEN,
                format!(
                    "refusing service stop in mode=idle while {active_count} run(s) are active; use --mode cancel or --mode drain"
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }

    let _ = file;
    signal_term(ready.pid)?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(100));
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

    // Timeout MUST NOT report `stopped: true` (`public-contract.json` runtime
    // invariant) and reports the runs that are still not terminal.
    let remaining_run_ids = non_terminal_run_ids_readonly(data_root).unwrap_or_default();
    Ok(json!({
        "stopped": false,
        "remainingRunIds": remaining_run_ids,
        "remainingOperationIds": [],
        "remainingTerminalIds": [],
        "stopState": "timeout",
    }))
}

fn count_non_terminal_runs_readonly(data_root: &Path) -> Result<usize, ContractError> {
    let db_path = data_root.join("v1").join("state.sqlite");
    if !db_path.exists() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("open sqlite: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    // P2 (wp4 review): a failed read must not degrade into "no active runs".
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM runs WHERE terminal = 0", [], |r| {
            r.get(0)
        })
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("count runs: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    Ok(count.max(0) as usize)
}

/// The non-terminal run ids, for `service.stop` timeout receipts (best
/// effort — an unreadable store yields an empty list, not a failed stop).
fn non_terminal_run_ids_readonly(data_root: &Path) -> Result<Vec<String>, ContractError> {
    let db_path = data_root.join("v1").join("state.sqlite");
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("open sqlite: {e}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let mut stmt = conn
        .prepare("SELECT run_id FROM runs WHERE terminal = 0")
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("query runs: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    let ids = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("query runs: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("query runs: {e}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
    Ok(ids)
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
                    let st = state.clone();
                    let rx = stop_rx.clone();
                    conns.spawn(async move {
                        let _ = serve_v1_tcp_socket(stream, st, rx).await;
                    });
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
                if http_proxy::is_upgrade(&request) {
                    match (
                        request.uri().path(),
                        acceptor::websocket_accept(request.headers()),
                        lease,
                    ) {
                        ("/v1/ws", Some(key), Some(lease)) => {
                            // Capture the presented Bearer so the WS session
                            // can re-verify it at every admission boundary
                            // (access keys, Plan A). The authorize check below
                            // still rejects the upgrade when it does not
                            // verify.
                            let bearer = PublicV1State::extract_bearer(request.headers());
                            if !st.authorize_headers(request.headers()) {
                                return Ok(PublicV1State::unauthorized_http_response());
                            }
                            if let Some(tx) = upgrade_sender.lock().expect("v1 upgrade").take() {
                                let _ = tx.send((hyper::upgrade::on(&mut request), lease, bearer));
                            }
                            Ok(acceptor::websocket_response(Default::default(), &key))
                        }
                        _ => Err(std::io::Error::other("invalid /v1/ws upgrade")),
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
