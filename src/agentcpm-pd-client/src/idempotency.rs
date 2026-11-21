//! Durable client idempotency ledger (`<home>/client/idempotency.json`, `007 §6.3, §10.2 WP2`).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::Path;

use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, ContractError, EffectStatus, RetrySpec,
};
use agentcpm_pd_protocol::id::IdGenerator;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    atomic_write_file, ensure_secure_dir, idempotency_ledger_path, idempotency_lock_path,
};
use crate::error::ClientError;

const MAX_RETAINED_ENTRIES: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerEntryStatus {
    Pending,
    Confirmed,
    FailedSafe,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdempotencyRecord {
    pub idempotency_key: String,
    pub method_id: String,
    pub input_digest: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: LedgerEntryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdempotencyStoreFile {
    #[serde(default)]
    entries: BTreeMap<String, IdempotencyRecord>,
}

#[derive(Clone, Debug)]
pub struct IdempotencyLedger {
    home: std::path::PathBuf,
}

impl IdempotencyLedger {
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            home: home.as_ref().to_path_buf(),
        }
    }

    /// Reserves or validates an `idempotencyKey` BEFORE dispatch and persists durably under OS lock.
    pub fn prepare_key(
        &self,
        method_id: &str,
        input: &Value,
        explicit_key: Option<&str>,
        request_id: &str,
        id_gen: &IdGenerator,
    ) -> Result<String, ClientError> {
        let digest = compute_input_digest(method_id, input);
        let key = explicit_key
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| id_gen.next_id());

        self.with_locked_store(|store| {
            let now = Utc::now().to_rfc3339();
            if let Some(existing) = store.entries.get_mut(&key) {
                if existing.method_id != method_id || existing.input_digest != digest {
                    return Err(ClientError::Contract(ContractError::new(
                        CODE_IDEMPOTENCY_CONFLICT,
                        format!(
                            "idempotencyKey `{key}` already bound locally to {} with a different payload",
                            existing.method_id
                        ),
                        RetrySpec::never(),
                        EffectStatus::NotApplied,
                    )));
                }
                existing.request_id = Some(request_id.to_owned());
                existing.updated_at = now;
                return Ok((key.clone(), true));
            }

            if store.entries.len() >= MAX_RETAINED_ENTRIES
                && let Some(oldest_key) = store
                    .entries
                    .iter()
                    .filter(|(_, r)| r.status != LedgerEntryStatus::Pending && r.status != LedgerEntryStatus::Unknown)
                    .min_by_key(|(_, r)| r.updated_at.clone())
                    .map(|(k, _)| k.clone())
            {
                store.entries.remove(&oldest_key);
            }

            store.entries.insert(
                key.clone(),
                IdempotencyRecord {
                    idempotency_key: key.clone(),
                    method_id: method_id.to_owned(),
                    input_digest: digest,
                    created_at: now.clone(),
                    updated_at: now,
                    status: LedgerEntryStatus::Pending,
                    request_id: Some(request_id.to_owned()),
                    last_error_code: None,
                },
            );
            Ok((key.clone(), true))
        })
    }

    pub fn update_status(
        &self,
        idempotency_key: &str,
        status: LedgerEntryStatus,
        error_code: Option<&str>,
    ) -> Result<(), ClientError> {
        self.with_locked_store(|store| {
            if let Some(rec) = store.entries.get_mut(idempotency_key) {
                rec.status = status;
                rec.updated_at = Utc::now().to_rfc3339();
                rec.last_error_code = error_code.map(ToOwned::to_owned);
                Ok(((), true))
            } else {
                Ok(((), false))
            }
        })
    }

    pub fn get(&self, idempotency_key: &str) -> Result<Option<IdempotencyRecord>, ClientError> {
        self.with_locked_store(|store| Ok((store.entries.get(idempotency_key).cloned(), false)))
    }

    pub async fn prepare_key_async(
        &self,
        method_id: &str,
        input: &Value,
        explicit_key: Option<&str>,
        request_id: &str,
        id_gen: &std::sync::Arc<IdGenerator>,
    ) -> Result<String, ClientError> {
        let ledger = self.clone();
        let method_id = method_id.to_owned();
        let input = input.clone();
        let explicit_key = explicit_key.map(ToOwned::to_owned);
        let request_id = request_id.to_owned();
        let id_gen = id_gen.clone();
        tokio::task::spawn_blocking(move || {
            ledger.prepare_key(
                &method_id,
                &input,
                explicit_key.as_deref(),
                &request_id,
                &id_gen,
            )
        })
        .await
        .map_err(|e| ClientError::config(format!("idempotency ledger join error: {e}")))?
    }

    pub async fn update_status_async(
        &self,
        idempotency_key: &str,
        status: LedgerEntryStatus,
        error_code: Option<&str>,
    ) -> Result<(), ClientError> {
        let ledger = self.clone();
        let key = idempotency_key.to_owned();
        let error_code = error_code.map(ToOwned::to_owned);
        tokio::task::spawn_blocking(move || {
            ledger.update_status(&key, status, error_code.as_deref())
        })
        .await
        .map_err(|e| ClientError::config(format!("idempotency ledger join error: {e}")))?
    }

    pub async fn get_async(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<IdempotencyRecord>, ClientError> {
        let ledger = self.clone();
        let key = idempotency_key.to_owned();
        tokio::task::spawn_blocking(move || ledger.get(&key))
            .await
            .map_err(|e| ClientError::config(format!("idempotency ledger join error: {e}")))?
    }

    fn with_locked_store<T>(
        &self,
        op: impl FnOnce(&mut IdempotencyStoreFile) -> Result<(T, bool), ClientError>,
    ) -> Result<T, ClientError> {
        let ledger_path = idempotency_ledger_path(&self.home);
        let lock_path = idempotency_lock_path(&self.home);
        if let Some(parent) = ledger_path.parent() {
            ensure_secure_dir(parent)?;
        }

        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| {
                ClientError::config(format!(
                    "failed to open idempotency lock {}: {e}",
                    lock_path.display()
                ))
            })?;
        lock_file.lock().map_err(|e| {
            ClientError::config(format!(
                "failed to acquire flock on {}: {e}",
                lock_path.display()
            ))
        })?;

        let mut store = if ledger_path.exists() {
            let raw = fs::read_to_string(&ledger_path).map_err(|e| {
                ClientError::config(format!("failed to read {}: {e}", ledger_path.display()))
            })?;
            serde_json::from_str::<IdempotencyStoreFile>(&raw).map_err(|e| {
                ClientError::config(format!("malformed {}: {e}", ledger_path.display()))
            })?
        } else {
            IdempotencyStoreFile::default()
        };

        let (result, dirty) = op(&mut store)?;
        if dirty {
            let bytes = serde_json::to_vec_pretty(&store)
                .map_err(|e| ClientError::config(format!("serialize idempotency ledger: {e}")))?;
            atomic_write_file(&ledger_path, &bytes, true)?;
        }
        drop(lock_file);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentcpm_pd_protocol::id::NodeId;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "current_thread")]
    async fn async_ledger_operations_run_via_spawn_blocking() {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agentcpm-pd-idem-async-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let ledger = IdempotencyLedger::new(&dir);
        let id_gen = Arc::new(IdGenerator::new(NodeId::local()));

        let key = ledger
            .prepare_key_async(
                "run.submit",
                &json!({"sessionId": "s1"}),
                Some("k-async-1"),
                "req-1",
                &id_gen,
            )
            .await
            .unwrap();
        assert_eq!(key, "k-async-1");
        ledger
            .update_status_async(&key, LedgerEntryStatus::Confirmed, None)
            .await
            .unwrap();
        let rec = ledger.get_async(&key).await.unwrap().unwrap();
        assert_eq!(rec.status, LedgerEntryStatus::Confirmed);
        let _ = fs::remove_dir_all(&dir);
    }
}
