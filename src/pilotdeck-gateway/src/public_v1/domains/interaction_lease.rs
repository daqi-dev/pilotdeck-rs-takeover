//! `interaction.claim` / `interaction.release` — the cross-connection dialog
//! edit lease (t011, capability `cli.capability.a016`).
//!
//! Scope is lease coordination only. The interaction record itself —
//! listing, lookup, response, and the run wake — is S2's
//! `domains/interaction.rs` / `V1Store::respond_interaction`, deliberately
//! untouched. A lease never grants response authority: `interaction.respond`
//! and `approval.respond` stay authorized by their own permissions, and a
//! client that wants its response bound to its lease passes `leaseId` to
//! `interaction.respond` (that optional input is currently ignored by the S2
//! handler — registered as a t011 report item, not changed here).
//!
//! Semantics (contract `state/cli-v1-operations.json`, interaction.claim /
//! interaction.release; TS baseline `user_dialog_claim` / `user_dialog_release`
//! as semantic reference only — at cc7c4e6 those gateway methods are
//! `CAPABILITY_UNAVAILABLE` stubs, so the operations contract is the behavior
//! authority):
//!
//! * The lease binds to the interaction, not to a connection: it survives
//!   disconnects and CLI process exits (007 §7.4 — interactions bind to the
//!   principal and the run, never to a connection). A Gateway restart still
//!   expires every pending interaction (007 §7.5), at which point claims
//!   answer `EXPIRED`.
//! * `interaction.claim` is a conditional write against the interaction's
//!   `expectedRevision` and bumps it: a claim by another owner while a live
//!   lease exists answers `LEASE_CONFLICT` (不同 owner 竞争拒绝); re-claiming
//!   with the same `leaseId` renews (new ttl counted from now); an expired
//!   lease frees the interaction for the next claim. `ttlMs` above the
//!   5-minute ceiling is rejected as `INVALID_ARGUMENT` (超长 ttl 明确拒绝,
//!   ceiling = the TS baseline's `MAX_USER_DIALOG_LEASE_MS`); the schema's
//!   own floor (`minimum: 1`) is honored as-is, so sub-second leases are
//!   legal. Claiming an `expired` interaction answers `EXPIRED`; an
//!   `answered`/`cancelled` one answers `REVISION_CONFLICT`, mirroring
//!   `respond_interaction`'s family behavior for non-pending state.
//! * `interaction.release` never answers or rejects the dialog (不代表回答或
//!   拒绝) and ignores the interaction's state: it only drops the lease. A
//!   stale `leaseId` cannot release a new owner's lease (`LEASE_CONFLICT`,
//!   旧 lease 不能释放新 owner); with no live lease it reports
//!   `released: false` and leaves the revision alone. A stale
//!   `expectedRevision` is `REVISION_CONFLICT` first — the write stays
//!   conditional (`retry: conditionalWrite`).
//! * `leaseOwner` in the `Interaction` object (interaction.get/list) mirrors
//!   the live lease's id through the S2-owned `interactions.lease_owner`
//!   column. Expiry is lazy, like the TS bus: between expiry and the next
//!   lease operation the mirror may still show the dead holder — a new
//!   claim overwrites it, and releasing an expired lease clears it.
//!
//! The authoritative expiry lives in the `interaction_leases` table
//! (registered below through the domain DDL hook). Every claim/release runs
//! inside one SQLite transaction on the store's single-writer connection, so
//! lease transitions serialize with `respond_interaction` exactly like every
//! other store write. Per the contracts (`events: []`) neither operation
//! appends events.

use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

/// Error codes this domain introduces beyond the shared registry set (both
/// are listed in the interaction.claim / interaction.release `errors`
/// arrays; the shared constants live in `agentcpm_pd_protocol::errors`).
const CODE_LEASE_CONFLICT: &str = "LEASE_CONFLICT";
const CODE_EXPIRED: &str = "EXPIRED";

/// Default and ceiling for `ttlMs`. The default matches the operation
/// schema's; the 5-minute ceiling is the TS baseline's
/// `MAX_USER_DIALOG_LEASE_MS` and satisfies the claim acceptance
/// "超长ttl明确拒绝". The schema floor (`minimum: 1`) is deliberately not
/// raised to the TS baseline's 1-second minimum: the contract's validated
/// schema says 1.
const DEFAULT_TTL_MS: u64 = 30_000;
const MAX_TTL_MS: u64 = 300_000;

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.schema(
        "interaction_leases",
        "CREATE TABLE IF NOT EXISTS interaction_leases (
            interaction_id TEXT PRIMARY KEY,
            lease_id TEXT NOT NULL,
            lease_expires_ms INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    );
    reg.handler("interaction.claim", |rt, input, _key, rev| {
        Box::pin(interaction_claim(rt, input, rev))
    });
    reg.handler("interaction.release", |rt, input, _key, rev| {
        Box::pin(interaction_release(rt, input, rev))
    });
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn run_blocking<T, F>(task: F) -> Result<T, ContractError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ContractError> + Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("interaction lease worker failed: {e}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::Unknown,
        )
    })?
}

/// Derives an opaque, non-secret owner tag stored in `interactions.lease_owner`
/// so public read methods (`interaction.get`/`list`) and conflict errors never
/// reveal the fencing `leaseId` credential (#9).
fn public_lease_owner(lease_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(lease_id.as_bytes());
    let digest = hasher.finalize();
    format!(
        "lease-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7]
    )
}

async fn interaction_claim(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    // Envelope-level revision wins when both are present, mirroring the S2
    // interaction.respond / approval.respond handlers.
    let rev = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    // Schema validation guarantees an integer >= 1 when present; a value no
    // u64 can represent (e.g. 1e20, accepted by the schema's f64 check) is
    // rejected here instead of silently falling back to the default.
    let ttl_ms = match input.get("ttlMs") {
        None => DEFAULT_TTL_MS,
        Some(v) => v.as_u64().ok_or_else(|| {
            lease_err(
                CODE_INVALID_ARGUMENT,
                format!("ttlMs must be an integer between 1 and {MAX_TTL_MS}"),
            )
        })?,
    };
    let interaction_id = input["interactionId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let lease_id = input["leaseId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let claim = run_blocking(move || {
        store.claim_interaction_lease(&interaction_id, &rev, &lease_id, ttl_ms)
    })
    .await?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "leaseId": claim.lease_id,
            "expiresAt": claim.expires_at,
            "revision": claim.revision,
        }),
    ))
}

async fn interaction_release(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    expected_revision: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let rev = expected_revision
        .as_deref()
        .or_else(|| input.get("expectedRevision").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned();
    let interaction_id = input["interactionId"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let lease_id = input["leaseId"].as_str().unwrap_or_default().to_owned();
    let store = rt.store().clone();
    let release =
        run_blocking(move || store.release_interaction_lease(&interaction_id, &rev, &lease_id))
            .await?;
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "released": release.released,
            "revision": release.revision,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Store methods (own-file `impl V1Store` block per the S3 substrate rule)
// ---------------------------------------------------------------------------

/// A successful `interaction.claim` (fresh or renewed).
#[derive(Clone, Debug)]
pub(crate) struct InteractionLeaseClaim {
    pub lease_id: String,
    pub expires_at: String,
    pub revision: String,
}

/// A `interaction.release` outcome: `released` is true only when a live,
/// matching lease was actually dropped; the revision is the interaction's
/// current one (bumped only by an actual release).
#[derive(Clone, Debug)]
pub(crate) struct InteractionLeaseRelease {
    pub released: bool,
    pub revision: String,
}

impl V1Store {
    /// Claims or renews the dialog edit lease on one interaction.
    ///
    /// Check order (every rejection leaves zero side effects): resolve
    /// (`NOT_FOUND`) -> ttl ceiling (`INVALID_ARGUMENT`) -> state
    /// (`EXPIRED` / `REVISION_CONFLICT` for non-pending) -> `expectedRevision`
    /// (`REVISION_CONFLICT`) -> live foreign lease (`LEASE_CONFLICT`).
    /// A matching live lease renews; an expired lease is overwritten.
    pub(crate) fn claim_interaction_lease(
        &self,
        interaction_id: &str,
        expected_revision: &str,
        lease_id: &str,
        ttl_ms: u64,
    ) -> Result<InteractionLeaseClaim, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let target = fetch_interaction_target(&tx, interaction_id)?.ok_or_else(|| {
            lease_err(
                CODE_NOT_FOUND,
                format!("interaction `{interaction_id}` not found"),
            )
        })?;

        if ttl_ms > MAX_TTL_MS {
            return Err(lease_err(
                CODE_INVALID_ARGUMENT,
                format!("ttlMs must be at most {MAX_TTL_MS} (5 minutes), got {ttl_ms}"),
            ));
        }
        if target.state == "expired" {
            return Err(lease_err(
                CODE_EXPIRED,
                format!("interaction `{interaction_id}` is expired"),
            ));
        }
        if target.state != "pending" {
            return Err(reconcile_err(
                CODE_REVISION_CONFLICT,
                format!(
                    "interaction `{interaction_id}` is already `{}`",
                    target.state
                ),
            ));
        }
        if target.revision != expected_revision {
            return Err(reconcile_err(
                CODE_REVISION_CONFLICT,
                format!(
                    "interaction expectedRevision `{expected_revision}` does not match `{}`",
                    target.revision
                ),
            ));
        }

        let now = now_ms();
        if let Some(lease) = fetch_interaction_lease(&tx, interaction_id)?
            && lease.lease_expires_ms > now
            && lease.lease_id != lease_id
        {
            return Err(reconcile_err(
                CODE_LEASE_CONFLICT,
                format!(
                    "interaction `{interaction_id}` is currently leased ({}) until {}",
                    public_lease_owner(&lease.lease_id),
                    ms_to_rfc3339(lease.lease_expires_ms)
                ),
            ));
        }

        let expires_ms = now + ttl_ms as i64;
        let new_rev = next_rev(&tx)?;
        let now_str = now_rfc3339();
        let owner_tag = public_lease_owner(lease_id);
        tx.execute(
            "INSERT INTO interaction_leases (interaction_id, lease_id, lease_expires_ms, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(interaction_id) DO UPDATE SET
                lease_id = excluded.lease_id,
                lease_expires_ms = excluded.lease_expires_ms,
                updated_at = excluded.updated_at",
            params![interaction_id, lease_id, expires_ms, now_str],
        )
        .map_err(map_sql)?;
        // Store an opaque non-secret owner tag in `interactions.lease_owner`
        // so `interaction.get`/`list` never expose the fencing `leaseId`.
        tx.execute(
            "UPDATE interactions SET lease_owner = ?1, revision = ?2 WHERE interaction_id = ?3",
            params![owner_tag, new_rev, interaction_id],
        )
        .map_err(map_sql)?;

        let claim = InteractionLeaseClaim {
            lease_id: lease_id.to_owned(),
            expires_at: ms_to_rfc3339(expires_ms),
            revision: new_rev,
        };
        tx.commit().map_err(map_sql)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        Ok(claim)
    }

    /// Releases the dialog edit lease on one interaction.
    ///
    /// Check order: resolve (`NOT_FOUND`) -> `expectedRevision`
    /// (`REVISION_CONFLICT`) -> expired interaction (cleanup lease + mirror,
    /// `released: false`) -> lease check (`LEASE_CONFLICT` on foreign live
    /// lease; `released: true` on matching live lease).
    pub(crate) fn release_interaction_lease(
        &self,
        interaction_id: &str,
        expected_revision: &str,
        lease_id: &str,
    ) -> Result<InteractionLeaseRelease, ContractError> {
        let mut conn = self.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql)?;
        let target = fetch_interaction_target(&tx, interaction_id)?.ok_or_else(|| {
            lease_err(
                CODE_NOT_FOUND,
                format!("interaction `{interaction_id}` not found"),
            )
        })?;

        if target.revision != expected_revision {
            return Err(reconcile_err(
                CODE_REVISION_CONFLICT,
                format!(
                    "interaction expectedRevision `{expected_revision}` does not match `{}`",
                    target.revision
                ),
            ));
        }

        if target.state == "expired" {
            tx.execute(
                "DELETE FROM interaction_leases WHERE interaction_id = ?1",
                params![interaction_id],
            )
            .map_err(map_sql)?;
            tx.execute(
                "UPDATE interactions SET lease_owner = NULL WHERE interaction_id = ?1",
                params![interaction_id],
            )
            .map_err(map_sql)?;
            tx.commit().map_err(map_sql)?;
            drop(conn);
            self.state_notifier().notify_waiters();
            return Ok(InteractionLeaseRelease {
                released: false,
                revision: target.revision,
            });
        }

        let now = now_ms();
        let Some(lease) = fetch_interaction_lease(&tx, interaction_id)? else {
            // Never claimed (or already released): nothing to drop.
            return Ok(InteractionLeaseRelease {
                released: false,
                revision: target.revision,
            });
        };
        if lease.lease_expires_ms <= now {
            // Lazily expired: drop the dead row and correct the stale mirror,
            // then report the true state. The interaction's revision does not
            // move — no contract-visible state changed.
            tx.execute(
                "DELETE FROM interaction_leases WHERE interaction_id = ?1",
                params![interaction_id],
            )
            .map_err(map_sql)?;
            tx.execute(
                "UPDATE interactions SET lease_owner = NULL WHERE interaction_id = ?1",
                params![interaction_id],
            )
            .map_err(map_sql)?;
            tx.commit().map_err(map_sql)?;
            drop(conn);
            self.state_notifier().notify_waiters();
            return Ok(InteractionLeaseRelease {
                released: false,
                revision: target.revision,
            });
        }
        if lease.lease_id != lease_id {
            return Err(reconcile_err(
                CODE_LEASE_CONFLICT,
                format!(
                    "leaseId does not match the active lease on interaction `{interaction_id}`"
                ),
            ));
        }

        let new_rev = next_rev(&tx)?;
        tx.execute(
            "DELETE FROM interaction_leases WHERE interaction_id = ?1",
            params![interaction_id],
        )
        .map_err(map_sql)?;
        tx.execute(
            "UPDATE interactions SET lease_owner = NULL, revision = ?1 WHERE interaction_id = ?2",
            params![new_rev, interaction_id],
        )
        .map_err(map_sql)?;
        tx.commit().map_err(map_sql)?;
        drop(conn);
        self.state_notifier().notify_waiters();
        Ok(InteractionLeaseRelease {
            released: true,
            revision: new_rev,
        })
    }
}

/// Coordination hook for `store.rs::respond_interaction` (#9): validates any
/// live lease against `supplied_lease_id` (`LEASE_CONFLICT` if missing or
/// mismatched), then deletes the lease row and clears `interactions.lease_owner`
/// when the interaction transitions out of `pending`.
pub(crate) fn validate_and_consume_lease_in_tx(
    tx: &Connection,
    interaction_id: &str,
    supplied_lease_id: Option<&str>,
) -> Result<(), ContractError> {
    let now = now_ms();
    if let Some(lease) = fetch_interaction_lease(tx, interaction_id)?
        && lease.lease_expires_ms > now
        && supplied_lease_id != Some(lease.lease_id.as_str())
    {
        return Err(reconcile_err(
            CODE_LEASE_CONFLICT,
            format!(
                "interaction `{interaction_id}` holds an active edit lease; provide a matching leaseId or release it first"
            ),
        ));
    }
    tx.execute(
        "DELETE FROM interaction_leases WHERE interaction_id = ?1",
        params![interaction_id],
    )
    .map_err(map_sql)?;
    tx.execute(
        "UPDATE interactions SET lease_owner = NULL WHERE interaction_id = ?1",
        params![interaction_id],
    )
    .map_err(map_sql)?;
    Ok(())
}

/// Coordination hook for `store.rs` restart/expiry sweep (#9): prunes expired
/// or non-pending rows from `interaction_leases` and clears stale
/// `interactions.lease_owner` mirrors.
pub(crate) fn cleanup_expired_leases_in_tx(tx: &Connection) -> Result<(), ContractError> {
    let now = now_ms();
    tx.execute(
        "DELETE FROM interaction_leases
         WHERE lease_expires_ms <= ?1
            OR interaction_id IN (SELECT interaction_id FROM interactions WHERE state != 'pending')",
        params![now],
    )
    .map_err(map_sql)?;
    tx.execute(
        "UPDATE interactions
         SET lease_owner = NULL
         WHERE lease_owner IS NOT NULL
           AND interaction_id NOT IN (SELECT interaction_id FROM interaction_leases)",
        [],
    )
    .map_err(map_sql)?;
    Ok(())
}

/// Read-side coordination hook for `store.rs` (`interaction.get`/`list`) (#9):
/// projects `leaseOwner` as `None` whenever the lease row is missing or expired,
/// and never returns a raw `leaseId` fencing credential.
pub(crate) fn effective_lease_owner(
    conn: &Connection,
    interaction_id: &str,
    stored_owner: Option<String>,
) -> Option<String> {
    let now = now_ms();
    if let Ok(Some(lease)) = fetch_interaction_lease(conn, interaction_id) {
        if lease.lease_expires_ms > now {
            Some(public_lease_owner(&lease.lease_id))
        } else {
            None
        }
    } else {
        stored_owner.filter(|s| s.starts_with("lease-"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The interaction fields the lease transitions condition on.
struct InteractionLeaseTarget {
    revision: String,
    state: String,
}

/// The current lease row, if any (expiry is judged by the caller).
struct InteractionLeaseRow {
    lease_id: String,
    lease_expires_ms: i64,
}

fn fetch_interaction_target(
    conn: &Connection,
    interaction_id: &str,
) -> Result<Option<InteractionLeaseTarget>, ContractError> {
    conn.query_row(
        "SELECT revision, state FROM interactions WHERE interaction_id = ?1",
        params![interaction_id],
        |r| {
            Ok(InteractionLeaseTarget {
                revision: r.get(0)?,
                state: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(map_sql)
}

fn fetch_interaction_lease(
    conn: &Connection,
    interaction_id: &str,
) -> Result<Option<InteractionLeaseRow>, ContractError> {
    conn.query_row(
        "SELECT lease_id, lease_expires_ms FROM interaction_leases WHERE interaction_id = ?1",
        params![interaction_id],
        |r| {
            Ok(InteractionLeaseRow {
                lease_id: r.get(0)?,
                lease_expires_ms: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(map_sql)
}

/// Terminal-condition error: nothing was applied, do not retry.
fn lease_err(code: &str, message: impl Into<String>) -> ContractError {
    ContractError::new(code, message, RetrySpec::never(), EffectStatus::NotApplied)
}

/// Conditional-write conflict: nothing was applied, re-read and reconcile
/// before retrying (the family's REVISION_CONFLICT advice).
fn reconcile_err(code: &str, message: impl Into<String>) -> ContractError {
    ContractError::new(
        code,
        message,
        RetrySpec::after_reconcile(),
        EffectStatus::NotApplied,
    )
}

fn map_sql(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

/// The next opaque revision from the store's global counter (same source
/// `store.rs`'s private `next_rev_in_tx` and every other domain-local copy
/// use, so lease writes keep the store-wide revision monotonic).
fn next_rev(conn: &Connection) -> Result<String, ContractError> {
    let cur: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_sql)?
        .unwrap_or(1);
    let next = cur + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(map_sql)?;
    Ok(format!("rev-{next:08}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(now_rfc3339)
}

// ---------------------------------------------------------------------------
// Tests (contract positives/negatives against a real store + dispatch)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::store::{InstanceMeta, SubmitDisposition};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use std::path::PathBuf;

    struct Fixture {
        _tmp: TempDir,
        home: PathBuf,
        rt: Arc<PublicV1Runtime>,
        instance_id: String,
    }

    async fn fixture(tag: &str) -> Fixture {
        let tmp = TempDir::new(tag);
        let home = tmp.0.join("pd-home");
        initialize_instance(&home, None, Some("local"), Some(true)).unwrap();
        let (file, _token) = load_local_instance_and_token(&home, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(&home, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        let instance_id = rt.store().meta().instance_id.clone();
        Fixture {
            _tmp: tmp,
            home,
            rt,
            instance_id,
        }
    }

    /// Registers a project/session, submits a (never-started) run, and parks
    /// one pending question interaction on it — the same store-direct seeding
    /// shape the regression tests use. Returns (interaction id, revision).
    fn seed_pending_question(fx: &Fixture, tag: &str) -> (String, String) {
        let store = fx.rt.store();
        let (project_id, session_id) = (format!("proj-{tag}"), format!("sess-{tag}"));
        store
            .register_project(&json!({
                "instanceId": fx.instance_id,
                "projectId": project_id,
                "name": "Interaction lease test",
                "environmentId": "local",
                "root": {"instanceId": fx.instance_id, "rootId": "root-1", "relativePath": "ws"},
            }))
            .unwrap();
        store
            .create_session(&json!({
                "instanceId": fx.instance_id,
                "sessionId": session_id,
                "projectId": project_id,
            }))
            .unwrap();
        let run_id = match store
            .submit_run(
                &json!({
                    "instanceId": fx.instance_id,
                    "sessionId": session_id,
                    "input": [{"kind": "text", "text": "[pd-driver:sleep=600000]"}],
                }),
                Some(&format!("key-{tag}")),
            )
            .unwrap()
        {
            SubmitDisposition::NewRun { run_id, .. } => run_id,
            other => panic!("expected a new run, got {other:?}"),
        };
        let ia = store
            .create_interaction(
                &run_id,
                "question",
                "lease test question",
                vec![],
                Some("agentcpm-pd.question.response/1".to_owned()),
                json!({"type": "object"}),
            )
            .unwrap();
        (ia.interaction_id.clone(), ia.revision.clone())
    }

    async fn claim(
        fx: &Fixture,
        interaction_id: &str,
        expected_revision: &str,
        lease_id: &str,
        ttl_ms: Option<u64>,
    ) -> Result<Value, ContractError> {
        let mut input = json!({
            "interactionId": interaction_id,
            "expectedRevision": expected_revision,
            "leaseId": lease_id,
        });
        if let Some(ttl) = ttl_ms {
            input["ttlMs"] = json!(ttl);
        }
        match fx.rt.dispatch("interaction.claim", input, None, None).await {
            Ok((_, result)) => Ok(result),
            Err(DispatchError::Application(err)) => Err(err),
            other => panic!("unexpected interaction.claim outcome: {other:?}"),
        }
    }

    async fn release(
        fx: &Fixture,
        interaction_id: &str,
        expected_revision: &str,
        lease_id: &str,
    ) -> Result<Value, ContractError> {
        match fx
            .rt
            .dispatch(
                "interaction.release",
                json!({
                    "interactionId": interaction_id,
                    "expectedRevision": expected_revision,
                    "leaseId": lease_id,
                }),
                None,
                None,
            )
            .await
        {
            Ok((_, result)) => Ok(result),
            Err(DispatchError::Application(err)) => Err(err),
            other => panic!("unexpected interaction.release outcome: {other:?}"),
        }
    }

    async fn get_interaction(fx: &Fixture, interaction_id: &str) -> Value {
        let (_, result) = fx
            .rt
            .dispatch(
                "interaction.get",
                json!({"interactionId": interaction_id}),
                None,
                None,
            )
            .await
            .expect("interaction.get");
        result["interaction"].clone()
    }

    #[tokio::test]
    async fn claim_renews_and_releases_with_contract_shaped_results() {
        let fx = fixture("ia-lease-core").await;
        let (iid, rev) = seed_pending_question(&fx, "core");
        let registry = OperationRegistry::global();

        // Fresh claim: contract-shaped result, revision bumped.
        let first = claim(&fx, &iid, &rev, "lease-a", None)
            .await
            .expect("first claim");
        registry
            .validate_result("interaction.claim", &first)
            .expect("claim result matches the operation schema");
        assert_eq!(first["leaseId"], "lease-a");
        assert!(first["expiresAt"].as_str().is_some());
        let rev1 = first["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev1, rev);

        // The S2 read path reports an opaque non-secret owner tag instead of
        // leaking the fencing `leaseId` credential (#9).
        let got = get_interaction(&fx, &iid).await;
        assert_ne!(got["leaseOwner"], "lease-a");
        assert_eq!(got["leaseOwner"], public_lease_owner("lease-a"));
        assert_eq!(got["state"], "pending");

        // Same lease id renews: new revision, lease kept.
        let renewed = claim(&fx, &iid, &rev1, "lease-a", Some(60_000))
            .await
            .expect("renew");
        registry
            .validate_result("interaction.claim", &renewed)
            .expect("renew result matches the operation schema");
        let rev2 = renewed["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev2, rev1);
        assert_eq!(
            get_interaction(&fx, &iid).await["leaseOwner"],
            public_lease_owner("lease-a")
        );

        // A competing owner is rejected with zero side effects and without
        // leaking the active `leaseId` inside the error message.
        let err = claim(&fx, &iid, &rev2, "lease-b", None)
            .await
            .expect_err("competing claim");
        assert_eq!(err.code, "LEASE_CONFLICT");
        assert!(!err.message.contains("lease-a"));
        assert_eq!(err.effect, EffectStatus::NotApplied);
        let still = get_interaction(&fx, &iid).await;
        assert_eq!(still["leaseOwner"], public_lease_owner("lease-a"));
        assert_eq!(still["revision"].as_str(), Some(rev2.as_str()));

        // Release drops the lease and only the lease: the dialog stays pending.
        let released = release(&fx, &iid, &rev2, "lease-a").await.expect("release");
        registry
            .validate_result("interaction.release", &released)
            .expect("release result matches the operation schema");
        assert_eq!(released["released"], true);
        let rev3 = released["revision"].as_str().unwrap().to_owned();
        assert_ne!(rev3, rev2);
        let freed = get_interaction(&fx, &iid).await;
        assert_eq!(freed["leaseOwner"], Value::Null);
        assert_eq!(freed["state"], "pending");

        // The dialog is claimable again by the next owner.
        let again = claim(&fx, &iid, &rev3, "lease-c", None)
            .await
            .expect("re-claim");
        assert_eq!(again["leaseId"], "lease-c");
    }

    #[tokio::test]
    async fn overlong_ttl_is_explicitly_rejected() {
        let fx = fixture("ia-lease-ttl").await;
        let (iid, rev) = seed_pending_question(&fx, "ttl");

        // Above the 5-minute ceiling: rejected before any write.
        let err = claim(&fx, &iid, &rev, "lease-a", Some(MAX_TTL_MS + 1))
            .await
            .expect_err("overlong ttl");
        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert_eq!(err.effect, EffectStatus::NotApplied);
        assert_eq!(get_interaction(&fx, &iid).await["leaseOwner"], Value::Null);

        // The ceiling itself is accepted.
        let boundary = claim(&fx, &iid, &rev, "lease-a", Some(MAX_TTL_MS))
            .await
            .expect("boundary ttl");
        let rev1 = boundary["revision"].as_str().unwrap().to_owned();

        // A ttl the schema accepts (finite f64, fract 0) but no u64 can
        // represent is rejected, never silently defaulted.
        let mut input = json!({
            "interactionId": iid,
            "expectedRevision": rev1,
            "leaseId": "lease-b",
            "ttlMs": 1e20,
        });
        match fx
            .rt
            .dispatch("interaction.claim", input.clone(), None, None)
            .await
        {
            Err(DispatchError::Application(err)) => {
                assert_eq!(err.code, "INVALID_ARGUMENT");
            }
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }
        input["ttlMs"] = json!(300_001u64);
        match fx.rt.dispatch("interaction.claim", input, None, None).await {
            Err(DispatchError::Application(err)) => {
                assert_eq!(err.code, "INVALID_ARGUMENT");
            }
            other => panic!("expected INVALID_ARGUMENT, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claim_requires_pending_state_and_fresh_revision() {
        let fx = fixture("ia-lease-state").await;
        let (iid, rev) = seed_pending_question(&fx, "state");

        // Stale expectedRevision.
        let err = claim(&fx, &iid, "rev-bogus", "lease-a", None)
            .await
            .expect_err("stale revision");
        assert_eq!(err.code, "REVISION_CONFLICT");

        // Unknown interaction: the family's NOT_FOUND (same as respond).
        let err = claim(&fx, "ia-missing", &rev, "lease-a", None)
            .await
            .expect_err("missing interaction");
        assert_eq!(err.code, "NOT_FOUND");
        let err = release(&fx, "ia-missing", &rev, "lease-a")
            .await
            .expect_err("missing interaction");
        assert_eq!(err.code, "NOT_FOUND");

        // Answered dialog: claim is rejected like respond's family behavior.
        let (_, answered) = fx
            .rt
            .dispatch(
                "interaction.respond",
                json!({
                    "interactionId": iid,
                    "expectedRevision": rev,
                    "answer": {"text": "no"},
                }),
                None,
                None,
            )
            .await
            .expect("interaction.respond");
        assert_eq!(answered["interaction"]["state"], "answered");
        let err = claim(&fx, &iid, &rev, "lease-a", None)
            .await
            .expect_err("claim on answered");
        assert_eq!(err.code, "REVISION_CONFLICT");
    }

    #[tokio::test]
    async fn lease_neither_grants_nor_blocks_respond() {
        let fx = fixture("ia-lease-respond").await;
        let (iid, rev) = seed_pending_question(&fx, "respond");
        let first = claim(&fx, &iid, &rev, "lease-a", None)
            .await
            .expect("claim");
        let rev1 = first["revision"].as_str().unwrap().to_owned();

        // A response prepared before the claim is rejected: the claim bumped
        // the revision (c016's claim/respond competition).
        match fx
            .rt
            .dispatch(
                "interaction.respond",
                json!({
                    "interactionId": iid,
                    "expectedRevision": rev,
                    "answer": {"text": "stale"},
                }),
                None,
                None,
            )
            .await
        {
            Err(DispatchError::Application(err)) => assert_eq!(err.code, "REVISION_CONFLICT"),
            other => panic!("expected REVISION_CONFLICT, got {other:?}"),
        }

        // A client that does NOT hold the lease can still respond once it has
        // the fresh revision: the lease grants no respond right (租约不授予
        // 审批权).
        let (_, answered) = fx
            .rt
            .dispatch(
                "interaction.respond",
                json!({
                    "interactionId": iid,
                    "expectedRevision": rev1,
                    "answer": {"text": "fresh"},
                }),
                None,
                None,
            )
            .await
            .expect("respond while leased");
        assert_eq!(answered["interaction"]["state"], "answered");
        let rev_after_answer = answered["interaction"]["revision"]
            .as_str()
            .unwrap()
            .to_owned();

        // Releasing the still-live lease after the answer drops only the
        // lease (不代表回答或拒绝): the dialog stays answered.
        let released = release(&fx, &iid, &rev_after_answer, "lease-a")
            .await
            .expect("release after answer");
        assert_eq!(released["released"], true);
        let got = get_interaction(&fx, &iid).await;
        assert_eq!(got["state"], "answered");
        assert_eq!(got["leaseOwner"], Value::Null);
    }

    #[tokio::test]
    async fn expired_lease_frees_the_dialog_and_old_leases_do_not_release_new_owners() {
        let fx = fixture("ia-lease-expiry").await;
        let (iid, rev) = seed_pending_question(&fx, "expiry");

        let first = claim(&fx, &iid, &rev, "lease-a", Some(200))
            .await
            .expect("short claim");
        let rev1 = first["revision"].as_str().unwrap().to_owned();

        // Wait past the ttl — waiting FOR expiry, not racing a positive read.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The expired lease no longer blocks a competing owner.
        let second = claim(&fx, &iid, &rev1, "lease-b", None)
            .await
            .expect("claim after expiry");
        let rev2 = second["revision"].as_str().unwrap().to_owned();
        assert_eq!(
            get_interaction(&fx, &iid).await["leaseOwner"],
            public_lease_owner("lease-b")
        );

        // The old lease cannot release the new owner (旧 lease 不能释放新
        // owner).
        let err = release(&fx, &iid, &rev2, "lease-a")
            .await
            .expect_err("old lease release");
        assert_eq!(err.code, "LEASE_CONFLICT");

        // The matching lease releases fine, and the dialog is claimable again.
        let released = release(&fx, &iid, &rev2, "lease-b")
            .await
            .expect("matching release");
        assert_eq!(released["released"], true);
        let rev3 = released["revision"].as_str().unwrap().to_owned();
        claim(&fx, &iid, &rev3, "lease-c", None)
            .await
            .expect("re-claim after release");
    }

    #[tokio::test]
    async fn release_without_a_live_lease_is_false_and_leaves_the_revision_alone() {
        let fx = fixture("ia-lease-release-false").await;
        let (iid, rev) = seed_pending_question(&fx, "release-false");

        // Never claimed: released=false, revision unchanged.
        let released = release(&fx, &iid, &rev, "lease-a")
            .await
            .expect("release never claimed");
        assert_eq!(released["released"], false);
        assert_eq!(released["revision"].as_str(), Some(rev.as_str()));

        // An expired lease is lazily dropped: released=false AND the stale
        // leaseOwner mirror is corrected.
        let first = claim(&fx, &iid, &rev, "lease-a", Some(200))
            .await
            .expect("short claim");
        let rev1 = first["revision"].as_str().unwrap().to_owned();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let released = release(&fx, &iid, &rev1, "lease-a")
            .await
            .expect("release expired lease");
        assert_eq!(released["released"], false);
        assert_eq!(released["revision"].as_str(), Some(rev1.as_str()));
        assert_eq!(get_interaction(&fx, &iid).await["leaseOwner"], Value::Null);

        // A stale revision is rejected even when there is nothing to release
        // (conditionalWrite discipline).
        let err = release(&fx, &iid, "rev-bogus", "lease-a")
            .await
            .expect_err("stale revision");
        assert_eq!(err.code, "REVISION_CONFLICT");
    }

    #[tokio::test]
    async fn gateway_restart_expires_the_dialog_so_claim_answers_expired() {
        let fx = fixture("ia-lease-restart").await;
        let (iid, rev) = seed_pending_question(&fx, "restart");
        claim(&fx, &iid, &rev, "lease-a", None)
            .await
            .expect("claim before restart");

        // The lease survives its claiming process/connection by design, but a
        // Gateway restart expires every pending interaction (007 §7.5):
        // claims then answer EXPIRED regardless of the lease table.
        let home = fx.home.clone();
        let mut meta = fx.rt.store().meta().clone();
        meta.generation = "gen-test-2".to_owned();
        let Fixture { _tmp, rt, .. } = fx;
        drop(rt);
        let store = V1Store::open_and_recover(&home, meta).unwrap();
        let rt2 = PublicV1Runtime::new(store);
        let got = {
            let (_, result) = rt2
                .dispatch("interaction.get", json!({"interactionId": iid}), None, None)
                .await
                .expect("interaction.get after restart");
            result["interaction"].clone()
        };
        assert_eq!(got["state"], "expired");
        let rev_after_restart = got["revision"].as_str().unwrap().to_owned();
        match rt2
            .dispatch(
                "interaction.claim",
                json!({
                    "interactionId": iid,
                    "expectedRevision": rev_after_restart,
                    "leaseId": "lease-b",
                }),
                None,
                None,
            )
            .await
        {
            Err(DispatchError::Application(err)) => assert_eq!(err.code, "EXPIRED"),
            other => panic!("expected EXPIRED, got {other:?}"),
        }

        // Releasing an interaction already in `expired` state cleans up the
        // residual lease + lease_owner mirror and reports `released: false`
        // regardless of leaseId (no false LEASE_CONFLICT or released: true).
        let (_, rel_expired) = rt2
            .dispatch(
                "interaction.release",
                json!({
                    "interactionId": iid,
                    "expectedRevision": rev_after_restart,
                    "leaseId": "lease-foreign",
                }),
                None,
                None,
            )
            .await
            .expect("release on expired interaction");
        assert_eq!(rel_expired["released"], false);
        assert_eq!(rel_expired["revision"], rev_after_restart);
        let (_, got_after) = rt2
            .dispatch("interaction.get", json!({"interactionId": iid}), None, None)
            .await
            .unwrap();
        assert_eq!(got_after["interaction"]["leaseOwner"], Value::Null);
        // `_tmp` stays bound until scope end, keeping the isolated home alive.
    }

    #[tokio::test]
    async fn coordination_hooks_enforce_fencing_and_clean_expired_leases() {
        let fx = fixture("ia-lease-hooks").await;
        let (iid, rev) = seed_pending_question(&fx, "hooks");

        let claimed = claim(&fx, &iid, &rev, "secret-lease-token", Some(60_000))
            .await
            .expect("claim");
        let _rev1 = claimed["revision"].as_str().unwrap().to_owned();

        // Wrong or omitted leaseId is rejected by validate_and_consume_lease_in_tx.
        {
            let mut conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            let tx = conn.transaction().unwrap();
            let err_none = validate_and_consume_lease_in_tx(&tx, &iid, None).unwrap_err();
            assert_eq!(err_none.code, CODE_LEASE_CONFLICT);
            let err_wrong =
                validate_and_consume_lease_in_tx(&tx, &iid, Some("other-token")).unwrap_err();
            assert_eq!(err_wrong.code, CODE_LEASE_CONFLICT);
            assert!(
                effective_lease_owner(&tx, &iid, None)
                    == Some(public_lease_owner("secret-lease-token"))
            );

            // Matching leaseId consumes the lease and clears lease_owner.
            validate_and_consume_lease_in_tx(&tx, &iid, Some("secret-lease-token")).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(get_interaction(&fx, &iid).await["leaseOwner"], Value::Null);

        // cleanup_expired_leases_in_tx clears expired leases cleanly.
        let (iid2, rev2) = seed_pending_question(&fx, "hooks-exp");
        claim(&fx, &iid2, &rev2, "exp-secret", Some(200))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        {
            let mut conn = fx.rt.store().conn.lock().expect("sqlite mutex");
            let tx = conn.transaction().unwrap();
            assert_eq!(
                effective_lease_owner(&tx, &iid2, Some("lease-stale".to_owned())),
                None
            );
            cleanup_expired_leases_in_tx(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(get_interaction(&fx, &iid2).await["leaseOwner"], Value::Null);
    }
}
