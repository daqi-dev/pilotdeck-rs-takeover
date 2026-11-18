//! Client-side target profile store and connection bookkeeping
//! (S3 `cli.task.t005`, Phase A data plane).
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `target.{list,get,write,
//! select,remove}` 与 `connection.{open,close}` 条目:Target 的形状
//! (targetId/name/kind(local|https|ssh)/endpoint/expectedInstanceId/
//! credentialRef/revision)与 CAS 语义(expectedRevision)一律照 schema。
//! TS 基线语义参考(cc7c4e6):`src/cli/sdkCli.ts` 的 `--gateway-url` /
//! `--auth-token`(命名目标与凭据分离)、`config.rs::resolve_target` 的
//! D8 规则(远程失败绝不回落本机)。本文件是**数据面**:Phase B 的
//! `local_ops/targets.rs` handler 在此之上组合同样的合同结果;本机解析
//! (`config.rs::resolve_target`)继续读 `config.json`/`credentials.json`,
//! 本文件不改它——两边的最小缝合点见 `TargetStore` 的文档注释。
//!
//! 凭据安全:档案只存 `credentialRef`(指向 `credentials.json` 的键),
//! 从不保存 secret 本身;`Display`/日志输出只含 targetId/kind/endpoint。
//! secret 值由凭据域管理,且 CLI 侧 secureInputFields 已禁止 secret 走
//! argv。

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, ContractError, EffectStatus,
    RetrySpec,
};
use agentcpm_pd_protocol::id::{IdGenerator, NodeId};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{atomic_write_file, ensure_secure_dir, normalize_and_validate_ws_url};
use crate::error::ClientError;

/// Target kind exactly as the contract's `Target.kind` enum shapes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Local,
    Https,
    Ssh,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Https => "https",
            Self::Ssh => "ssh",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, ClientError> {
        match raw {
            "local" => Ok(Self::Local),
            "https" => Ok(Self::Https),
            "ssh" => Ok(Self::Ssh),
            other => Err(ClientError::Contract(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("unsupported target kind `{other}`; expected one of local|https|ssh"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ))),
        }
    }
}

/// One stored target profile — the contract `Target` object plus internal
/// ordering metadata. Serialized camelCase; `credentialRef` is a *reference*
/// into the credentials store, never a secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRecord {
    pub target_id: String,
    pub name: String,
    pub kind: TargetKind,
    pub endpoint: String,
    pub expected_instance_id: String,
    #[serde(default)]
    pub credential_ref: Option<String>,
    pub revision: String,
    /// Internal ordering field; excluded from the contract-shaped output
    /// (`to_contract_json`), which is `additionalProperties: false`.
    #[serde(default)]
    pub updated_at: String,
}

impl TargetRecord {
    /// The contract-shaped `Target` object (exactly the seven schema fields).
    pub fn to_contract_json(&self) -> Value {
        json!({
            "targetId": self.target_id,
            "name": self.name,
            "kind": self.kind.as_str(),
            "endpoint": self.endpoint,
            "expectedInstanceId": self.expected_instance_id,
            "credentialRef": self.credential_ref,
            "revision": self.revision,
        })
    }
}

/// Input of `target.write` (data-plane shape of the contract input).
#[derive(Clone, Debug)]
pub struct TargetWriteInput {
    pub target_id: String,
    /// `None` (or JSON null) means create-only: the write is refused when
    /// the profile already exists (contract acceptance: 无revision只允许创建).
    pub expected_revision: Option<String>,
    pub name: String,
    pub kind: TargetKind,
    pub endpoint: String,
    pub expected_instance_id: String,
    pub credential_ref: Option<String>,
}

/// On-disk `<home>/targets.json`. `revision`/`counter` form the store-level
/// snapshot revision every write bumps (the value `target.list`'s
/// `pageInfo.snapshotRevision` reports); each record additionally pins the
/// store revision it was last written at.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetsFile {
    #[serde(default = "default_file_version")]
    version: u32,
    /// Store-level snapshot revision (`rev-%08d` of `counter`).
    #[serde(default)]
    revision: String,
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    selected_target_id: Option<String>,
    #[serde(default)]
    targets: BTreeMap<String, TargetRecord>,
}

fn default_file_version() -> u32 {
    1
}

impl TargetsFile {
    fn snapshot_revision(&self) -> String {
        if self.revision.is_empty() {
            "rev-000000".to_owned()
        } else {
            self.revision.clone()
        }
    }

    fn next_revision(&mut self) -> String {
        self.counter += 1;
        self.revision = format!("rev-{:08}", self.counter);
        self.revision.clone()
    }
}

/// Local target profile store over `<home>/targets.json` (`0600`, atomic
/// replace) guarded by `<home>/targets.lock` (OS `flock`), so concurrent
/// CLI processes serialize their read-modify-write CAS cycles the same way
/// the idempotency ledger does.
///
/// Phase B wiring note (config.rs 的最小调整点,本文件不改 config.rs):
/// `resolve_target` 目前只读 `config.json` 的 `targets`/`defaultTarget`。
/// 最小缝合是二选一:(a) `resolve_target` 在 `config.json` 查不到命名档案
/// 时回退查 `TargetStore::get`(推荐,一行调用);或 (b) Phase B 的
/// `target.select/write` 同时镜像写 `config.json`。由集成者与 config.rs
/// 拥有者裁决;`credentialRef` 的解析继续走 `credentials.json`
/// (`config.rs::load_credentials`),不经过本文件。
pub struct TargetStore {
    home: PathBuf,
}

impl TargetStore {
    pub fn open(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    pub fn store_path(&self) -> PathBuf {
        self.home.join("targets.json")
    }

    #[allow(clippy::type_complexity)] // page result tuple
    fn lock_path(&self) -> PathBuf {
        self.home.join("targets.lock")
    }

    /// `target.list` data plane. Returns the contract result parts:
    /// (records of this page, snapshotRevision, nextCursor, complete,
    /// selectedTargetId).
    #[allow(clippy::type_complexity)] // page result tuple
    pub fn list(
        &self,
        limit: Option<u64>,
        cursor: Option<&str>,
    ) -> Result<
        (
            Vec<TargetRecord>,
            String,
            Option<String>,
            bool,
            Option<String>,
        ),
        ClientError,
    > {
        let store = self.load_unlocked()?;
        let limit = limit.unwrap_or(100).clamp(1, 500) as usize;
        let offset = match cursor {
            None => 0usize,
            Some(raw) => raw.parse::<usize>().map_err(|_| {
                ClientError::Contract(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!("invalid pagination cursor `{raw}`"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ))
            })?,
        };
        let all: Vec<&TargetRecord> = store.targets.values().collect();
        if offset > all.len() {
            return Err(ClientError::Contract(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "pagination cursor beyond end of collection",
                RetrySpec::after_reconcile(),
                EffectStatus::NotApplied,
            )));
        }
        let end = offset.saturating_add(limit).min(all.len());
        let complete = end >= all.len();
        let next_cursor = if complete {
            None
        } else {
            Some(end.to_string())
        };
        Ok((
            all.into_iter().skip(offset).take(limit).cloned().collect(),
            store.snapshot_revision(),
            next_cursor,
            complete,
            store.selected_target_id,
        ))
    }

    /// `target.get` data plane; `NOT_FOUND` when the profile is absent
    /// (contract: 读取不启动隧道 — this is a pure file read).
    pub fn get(&self, target_id: &str) -> Result<TargetRecord, ClientError> {
        let store = self.load_unlocked()?;
        store
            .targets
            .get(target_id)
            .cloned()
            .ok_or_else(|| target_not_found(target_id))
    }

    /// The currently selected default target id, if any.
    pub fn selected(&self) -> Result<Option<String>, ClientError> {
        Ok(self.load_unlocked()?.selected_target_id)
    }

    /// `target.write` data plane: conditional create/update. Validates the
    /// endpoint shape per kind (D7: non-loopback `ws://` refused for `https`
    /// targets — contract acceptance: 保存不自动信任错误TLS证书或SSH
    /// 主机key), then CAS-checks `expected_revision` against the stored
    /// record. A write never touches the server side or credentials.
    pub fn write(&self, input: &TargetWriteInput) -> Result<TargetRecord, ClientError> {
        validate_write_input(input)?;
        self.with_locked_store(|store| {
            match store.targets.get(&input.target_id) {
                Some(existing) => {
                    let expected = input.expected_revision.as_deref().unwrap_or_default();
                    if input.expected_revision.is_none() {
                        return Err(ClientError::Contract(ContractError::new(
                            CODE_REVISION_CONFLICT,
                            format!(
                                "target `{}` already exists; an update requires its revision (create-only write refused)",
                                input.target_id
                            ),
                            RetrySpec::after_reconcile(),
                            EffectStatus::NotApplied,
                        )));
                    }
                    if existing.revision != expected {
                        return Err(ClientError::Contract(ContractError::new(
                            CODE_REVISION_CONFLICT,
                            format!(
                                "target expectedRevision `{}` does not match `{}`",
                                expected, existing.revision
                            ),
                            RetrySpec::after_reconcile(),
                            EffectStatus::NotApplied,
                        )));
                    }
                }
                None => {
                    if input.expected_revision.is_some() {
                        return Err(ClientError::Contract(ContractError::new(
                            CODE_REVISION_CONFLICT,
                            format!(
                                "target `{}` does not exist; only a null expectedRevision may create it",
                                input.target_id
                            ),
                            RetrySpec::after_reconcile(),
                            EffectStatus::NotApplied,
                        )));
                    }
                }
            }
            let revision = store.next_revision();
            let record = TargetRecord {
                target_id: input.target_id.clone(),
                name: input.name.clone(),
                kind: input.kind,
                endpoint: input.endpoint.clone(),
                expected_instance_id: input.expected_instance_id.clone(),
                credential_ref: input.credential_ref.clone(),
                revision,
                updated_at: Utc::now().to_rfc3339(),
            };
            store.targets.insert(record.target_id.clone(), record.clone());
            Ok((record, true))
        })
    }

    /// `target.select` data plane: sets the default target after a CAS check
    /// against the target's current revision. Returns
    /// (selectedTargetId, revision).
    pub fn select(
        &self,
        target_id: &str,
        expected_revision: &str,
    ) -> Result<(String, String), ClientError> {
        self.with_locked_store(|store| {
            let existing = store
                .targets
                .get(target_id)
                .ok_or_else(|| target_not_found(target_id))?;
            if existing.revision != expected_revision {
                return Err(ClientError::Contract(ContractError::new(
                    CODE_REVISION_CONFLICT,
                    format!(
                        "target expectedRevision `{expected_revision}` does not match `{}`",
                        existing.revision
                    ),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                )));
            }
            // Copy out before the `&mut` calls below: `existing` borrows
            // `store.targets`, while `next_revision()` needs the whole store.
            let revision = existing.revision.clone();
            store.selected_target_id = Some(target_id.to_owned());
            // Selecting a default does not rewrite the profile; the store
            // revision still advances so list snapshots observe the change.
            store.next_revision();
            Ok(((target_id.to_owned(), revision), true))
        })
    }

    /// `target.remove` data plane: removes the profile after a CAS check
    /// against its current revision. Never touches the server side or
    /// credentials (contract: 不删除服务端项目或撤销访问key), and a removed
    /// default clears the selection instead of falling back to another
    /// target (contract: 移除默认后清空而非选另一个). Returns
    /// (removedTargetId, revision-of-the-removed-record).
    pub fn remove(
        &self,
        target_id: &str,
        expected_revision: &str,
    ) -> Result<(String, String), ClientError> {
        self.with_locked_store(|store| {
            let existing = store
                .targets
                .get(target_id)
                .ok_or_else(|| target_not_found(target_id))?;
            if existing.revision != expected_revision {
                return Err(ClientError::Contract(ContractError::new(
                    CODE_REVISION_CONFLICT,
                    format!(
                        "target expectedRevision `{expected_revision}` does not match `{}`",
                        existing.revision
                    ),
                    RetrySpec::after_reconcile(),
                    EffectStatus::NotApplied,
                )));
            }
            let revision = existing.revision.clone();
            store.targets.remove(target_id);
            if store.selected_target_id.as_deref() == Some(target_id) {
                store.selected_target_id = None;
            }
            store.next_revision();
            Ok(((target_id.to_owned(), revision), true))
        })
    }

    // -- internals ---------------------------------------------------------

    fn load_unlocked(&self) -> Result<TargetsFile, ClientError> {
        let path = self.store_path();
        if !path.exists() {
            return Ok(TargetsFile::default());
        }
        let raw = fs::read_to_string(&path)
            .map_err(|e| ClientError::config(format!("failed to read {}: {e}", path.display())))?;
        serde_json::from_str(&raw)
            .map_err(|e| ClientError::config(format!("malformed {}: {e}", path.display())))
    }

    /// Read-modify-write under the sidecar flock; `op` returns
    /// (result, dirty). The lock file lives in the home root like the
    /// idempotency lock.
    fn with_locked_store<T>(
        &self,
        op: impl FnOnce(&mut TargetsFile) -> Result<(T, bool), ClientError>,
    ) -> Result<T, ClientError> {
        ensure_secure_dir(&self.home)?;
        let lock_path = self.lock_path();
        let lock_file: File = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| {
                ClientError::config(format!(
                    "failed to open target store lock {}: {e}",
                    lock_path.display()
                ))
            })?;
        lock_file.lock().map_err(|e| {
            ClientError::config(format!(
                "failed to acquire flock on {}: {e}",
                lock_path.display()
            ))
        })?;

        let mut store = self.load_unlocked()?;
        let (result, dirty) = op(&mut store)?;
        if dirty {
            let bytes = serde_json::to_vec_pretty(&store).map_err(|e| {
                ClientError::config(format!("serialize {}: {e}", self.store_path().display()))
            })?;
            atomic_write_file(&self.store_path(), &bytes, true)?;
        }
        drop(lock_file);
        Ok(result)
    }
}

fn target_not_found(target_id: &str) -> ClientError {
    ClientError::Contract(ContractError::new(
        CODE_NOT_FOUND,
        format!("target `{target_id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    ))
}

/// Endpoint validation per kind. `https` targets carry a `ws://` (loopback
/// only) / `wss://` URL validated with the same D7 rule the resolver uses;
/// `local` targets may carry an empty endpoint (implicit data root) or a
/// data-root path; `ssh` targets carry a non-empty host specification.
fn validate_write_input(input: &TargetWriteInput) -> Result<(), ClientError> {
    let invalid = |message: String| {
        ClientError::Contract(ContractError::new(
            CODE_INVALID_ARGUMENT,
            message,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ))
    };
    if input.target_id.trim().is_empty() {
        return Err(invalid("targetId must not be empty".to_owned()));
    }
    if input.expected_instance_id.trim().is_empty() {
        return Err(invalid("expectedInstanceId must not be empty".to_owned()));
    }
    match input.kind {
        TargetKind::Https => {
            normalize_and_validate_ws_url(&input.endpoint).map_err(|e| {
                ClientError::Contract(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!(
                        "invalid https target endpoint: {}",
                        e.into_contract_error().message
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ))
            })?;
        }
        TargetKind::Ssh => {
            if input.endpoint.trim().is_empty() {
                return Err(invalid("ssh target endpoint must not be empty".to_owned()));
            }
        }
        TargetKind::Local => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Connection bookkeeping (connection.open / connection.close, Phase A)
// ---------------------------------------------------------------------------

/// The remote identity confirmed by a successful `connection.open`
/// (the contract `Identity` object's core fields).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionIdentity {
    pub instance_id: String,
    pub owner_id: String,
    pub product: String,
    pub protocol_version: String,
}

/// Bookkeeping record for one opened connection. Deliberately volatile:
/// closing a connection revokes nothing on the server side (contract:
/// 不撤销key、不取消run), so nothing here is persisted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    pub connection_id: String,
    pub target_id: String,
    pub opened_at: String,
    /// Bound after the handshake confirmed the target identity
    /// (`connection.open` contract: instanceId错误零业务请求 — the
    /// Phase B handler fills this only once `expectedInstanceId` matched).
    #[serde(default)]
    pub identity: Option<ConnectionIdentity>,
}

/// In-process connection registry for `connection.open`/`connection.close`
/// bookkeeping. `connection.open` (Phase B local handler) registers the
/// attempt, drives the real `/v1/ws` handshake against the target's
/// endpoint, and binds the confirmed identity; `connection.close` removes
/// the record. Registry entries live only in this process's memory.
pub struct ConnectionRegistry {
    inner: Mutex<BTreeMap<String, ConnectionEntry>>,
    id_gen: Mutex<IdGenerator>,
}

/// One registry entry: the serializable bookkeeping record plus the live
/// client handle (present once a handshake succeeded), so `connection.close`
/// can detach the transport in the same step that removes the record.
pub struct ConnectionEntry {
    pub record: ConnectionRecord,
    pub client: Option<crate::ws_client::PdWsClient>,
}

impl Default for ConnectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
            id_gen: Mutex::new(IdGenerator::new(NodeId::local())),
        }
    }

    /// Registers a new open attempt and returns its connection id.
    pub fn register_open(&self, target_id: &str) -> String {
        let connection_id = self.id_gen.lock().expect("connection id gen").next_id();
        self.inner.lock().expect("connection registry").insert(
            connection_id.clone(),
            ConnectionEntry {
                record: ConnectionRecord {
                    connection_id: connection_id.clone(),
                    target_id: target_id.to_owned(),
                    opened_at: Utc::now().to_rfc3339(),
                    identity: None,
                },
                client: None,
            },
        );
        connection_id
    }

    /// Registers (or replaces) an established connection under the
    /// server-assigned handshake `connectionId`, binding the confirmed
    /// identity and the live client handle. The contract's `connectionId`
    /// comes from the `/v1/ws` initialize handshake, so bookkeeping and the
    /// wire share one id.
    pub fn register_established(
        &self,
        connection_id: &str,
        target_id: &str,
        identity: ConnectionIdentity,
        client: Option<crate::ws_client::PdWsClient>,
    ) {
        self.inner.lock().expect("connection registry").insert(
            connection_id.to_owned(),
            ConnectionEntry {
                record: ConnectionRecord {
                    connection_id: connection_id.to_owned(),
                    target_id: target_id.to_owned(),
                    opened_at: Utc::now().to_rfc3339(),
                    identity: Some(identity),
                },
                client,
            },
        );
    }

    /// Binds the identity a successful handshake confirmed.
    pub fn bind_identity(
        &self,
        connection_id: &str,
        identity: ConnectionIdentity,
    ) -> Result<(), ClientError> {
        let mut guard = self.inner.lock().expect("connection registry");
        match guard.get_mut(connection_id) {
            Some(entry) => {
                entry.record.identity = Some(identity);
                Ok(())
            }
            None => Err(ClientError::config(format!(
                "unknown connection `{connection_id}`"
            ))),
        }
    }

    /// `connection.close` data plane: removes the record. Returns `true`
    /// when an open connection was closed, `false` when it was already gone
    /// (repeat closes must not affect other connections).
    pub fn close(&self, connection_id: &str) -> bool {
        self.take(connection_id).is_some()
    }

    /// Removes and returns the full entry (record + live client) so the
    /// close handler can detach the transport after the bookkeeping removal.
    pub fn take(&self, connection_id: &str) -> Option<ConnectionEntry> {
        self.inner
            .lock()
            .expect("connection registry")
            .remove(connection_id)
    }

    pub fn get(&self, connection_id: &str) -> Option<ConnectionRecord> {
        self.inner
            .lock()
            .expect("connection registry")
            .get(connection_id)
            .map(|entry| entry.record.clone())
    }

    pub fn list(&self) -> Vec<ConnectionRecord> {
        self.inner
            .lock()
            .expect("connection registry")
            .values()
            .map(|entry| entry.record.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "agentcpm-pd-targets-{tag}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            ensure_secure_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn local_write_input(target_id: &str, instance: &str) -> TargetWriteInput {
        TargetWriteInput {
            target_id: target_id.to_owned(),
            expected_revision: None,
            name: format!("target {target_id}"),
            kind: TargetKind::Local,
            endpoint: String::new(),
            expected_instance_id: instance.to_owned(),
            credential_ref: None,
        }
    }

    fn https_write_input(target_id: &str, endpoint: &str) -> TargetWriteInput {
        TargetWriteInput {
            target_id: target_id.to_owned(),
            expected_revision: None,
            name: format!("remote {target_id}"),
            kind: TargetKind::Https,
            endpoint: endpoint.to_owned(),
            expected_instance_id: "inst-remote-1".to_owned(),
            credential_ref: Some("cred-remote-1".to_owned()),
        }
    }

    #[test]
    fn fresh_store_lists_empty_with_selected_null() {
        let tmp = TempHome::new("empty");
        let store = TargetStore::open(&tmp.0);
        let (records, snapshot, next, complete, selected) = store.list(None, None).unwrap();
        assert!(records.is_empty());
        assert!(snapshot.starts_with("rev-"));
        assert_eq!(next, None);
        assert!(complete);
        assert_eq!(selected, None);
        assert_eq!(store.selected().unwrap(), None);
    }

    #[test]
    fn write_get_select_remove_roundtrip_with_cas() {
        let tmp = TempHome::new("roundtrip");
        let store = TargetStore::open(&tmp.0);

        // Create (no revision).
        let created = store
            .write(&local_write_input("dev-local", "inst-1"))
            .unwrap();
        assert_eq!(created.kind, TargetKind::Local);
        assert_eq!(created.revision, "rev-000001");
        assert_eq!(
            created.to_contract_json()["credentialRef"],
            Value::Null,
            "contract shape keeps credentialRef explicit-null"
        );

        // Create-only write on an existing target is refused.
        let err = store
            .write(&local_write_input("dev-local", "inst-1"))
            .unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_REVISION_CONFLICT);

        // CAS update with the current revision.
        let mut updated_input = local_write_input("dev-local", "inst-1");
        updated_input.name = "renamed".to_owned();
        updated_input.expected_revision = Some(created.revision.clone());
        let updated = store.write(&updated_input).unwrap();
        assert_eq!(updated.name, "renamed");
        assert_ne!(updated.revision, created.revision);

        // Stale revision update is refused.
        let mut stale = local_write_input("dev-local", "inst-1");
        stale.expected_revision = Some(created.revision.clone());
        let err = store.write(&stale).unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_REVISION_CONFLICT);

        // get reflects the update.
        let got = store.get("dev-local").unwrap();
        assert_eq!(got.name, "renamed");
        assert_eq!(got.revision, updated.revision);

        // select requires the current revision; a stale select is refused.
        let err = store.select("dev-local", &created.revision).unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_REVISION_CONFLICT);
        let (selected, rev) = store.select("dev-local", &updated.revision).unwrap();
        assert_eq!(selected, "dev-local");
        assert_eq!(rev, updated.revision);
        assert_eq!(store.selected().unwrap(), Some("dev-local".to_owned()));

        // list reports the selection and the record.
        let (records, _snap, _next, _complete, selected) = store.list(None, None).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(selected, Some("dev-local".to_owned()));

        // remove with stale revision refused; with current revision it
        // clears the default selection instead of picking another target.
        store
            .write(&https_write_input("prod", "wss://remote.example.com/v1/ws"))
            .unwrap();
        let err = store.remove("dev-local", &created.revision).unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_REVISION_CONFLICT);
        let (removed, removed_rev) = store.remove("dev-local", &updated.revision).unwrap();
        assert_eq!(removed, "dev-local");
        assert_eq!(removed_rev, updated.revision);
        assert_eq!(
            store.selected().unwrap(),
            None,
            "removing the default clears the selection, never falls back"
        );
        assert!(store.get("dev-local").is_err());
        assert!(store.get("prod").is_ok(), "other targets survive");
    }

    #[test]
    fn get_unknown_target_is_not_found_and_list_pagination_works() {
        let tmp = TempHome::new("paging");
        let store = TargetStore::open(&tmp.0);

        let err = store.get("missing").unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_NOT_FOUND);

        for i in 0..5 {
            store
                .write(&local_write_input(&format!("t-{i:02}"), "inst-1"))
                .unwrap();
        }
        let (page1, _snap, next, complete, _sel) = store.list(Some(2), None).unwrap();
        assert_eq!(
            page1
                .iter()
                .map(|r| r.target_id.clone())
                .collect::<Vec<_>>(),
            vec!["t-00", "t-01"]
        );
        assert!(!complete);
        let (page2, _snap, next, _complete, _sel) = store.list(Some(2), next.as_deref()).unwrap();
        assert_eq!(page2.len(), 2);
        assert!(next.is_some());
        let (page3, _snap, next, complete, _sel) = store.list(Some(2), next.as_deref()).unwrap();
        assert_eq!(page3.len(), 1);
        assert!(next.is_none());
        assert!(complete);

        let err = store.list(Some(2), Some("999")).unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_INVALID_ARGUMENT);
    }

    #[test]
    fn https_endpoints_follow_the_d7_loopback_rule_and_ssh_requires_endpoint() {
        let tmp = TempHome::new("d7");
        let store = TargetStore::open(&tmp.0);

        // wss anywhere is fine.
        store
            .write(&https_write_input("prod", "wss://remote.example.com/v1/ws"))
            .unwrap();
        // plain ws is loopback-only.
        store
            .write(&https_write_input("loop", "ws://127.0.0.1:8787"))
            .unwrap();
        let err = store
            .write(&https_write_input("bad", "ws://192.168.1.5:9000"))
            .unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_INVALID_ARGUMENT);
        let err = store
            .write(&https_write_input("bad2", "http://remote.example.com"))
            .unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_INVALID_ARGUMENT);

        let mut ssh = https_write_input("ssh-t", "user@remote.example.com");
        ssh.kind = TargetKind::Ssh;
        store.write(&ssh).unwrap();
        let mut ssh_empty = ssh.clone();
        ssh_empty.target_id = "ssh-empty".to_owned();
        ssh_empty.endpoint = String::new();
        let err = store.write(&ssh_empty).unwrap_err();
        assert_eq!(err.into_contract_error().code, CODE_INVALID_ARGUMENT);

        // persisted files never contain secrets; only the credentialRef.
        let raw = fs::read_to_string(store.store_path()).unwrap();
        assert!(raw.contains("credentialRef"));
        assert!(
            !raw.contains("secret"),
            "no secret material in targets.json"
        );
        let meta = fs::metadata(store.store_path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "targets.json is 0600"
            );
        }
    }

    #[test]
    fn connection_registry_bookkeeping_and_repeat_close() {
        let registry = ConnectionRegistry::new();
        let conn = registry.register_open("prod");
        assert!(registry.get(&conn).is_some());
        assert_eq!(registry.get(&conn).unwrap().target_id, "prod");
        assert!(registry.get(&conn).unwrap().identity.is_none());

        registry
            .bind_identity(
                &conn,
                ConnectionIdentity {
                    instance_id: "inst-remote-1".to_owned(),
                    owner_id: "owner-1".to_owned(),
                    product: "agentcpm-pd".to_owned(),
                    protocol_version: "agentcpm-pd.operation/1".to_owned(),
                },
            )
            .unwrap();
        assert_eq!(
            registry.get(&conn).unwrap().identity.unwrap().instance_id,
            "inst-remote-1"
        );

        // A second connection is unaffected by closing the first, and a
        // repeat close reports false.
        let other = registry.register_open("dev-local");
        assert!(registry.close(&conn));
        assert!(!registry.close(&conn), "repeat close is not an error");
        assert!(registry.get(&conn).is_none());
        assert!(registry.get(&other).is_some());
        assert_eq!(registry.list().len(), 1);

        // Binding to an unknown connection is a config error, not a panic.
        assert!(
            registry
                .bind_identity(
                    &conn,
                    ConnectionIdentity {
                        instance_id: "x".to_owned(),
                        owner_id: "x".to_owned(),
                        product: "agentcpm-pd".to_owned(),
                        protocol_version: "1".to_owned(),
                    }
                )
                .is_err()
        );
    }
}
