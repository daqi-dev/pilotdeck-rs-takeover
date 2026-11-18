//! `directories.*` and the v1 authorized-root registry (`t012`).
//!
//! Contract: `directories.list` / `directories.create` address the server-side
//! filesystem exclusively through `ServerPath { authorizedRootId,
//! relativePath: BytePath }`. The authorized-root table is the server's
//! authorization declaration for that address space (the `workspace.browse` /
//! `workspace.create` permissions): a request naming an unregistered root is
//! refused with `FORBIDDEN`, and every resolved path is confined to its root
//! both lexically (component validation) and physically (`canonicalize`
//! containment, so symlink escapes are rejected with `PATH_ESCAPE`).
//!
//! A single bootstrap default root (`default` -> `<home>/workspaces`) is
//! registered lazily on first resolution, mirroring the TS baseline's
//! `WORKSPACES_ROOT` (ui/server/routes/projects.js:56) inside the isolated
//! v1 home. Later tasks (`workspaces.prepare` for clone, `files`/`uploads`)
//! register additional roots in the same table.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_FORBIDDEN, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError,
    EffectStatus, RetrySpec,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::public_v1::domains::DomainRegistry;
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

/// The root id of the lazily-registered default workspace root.
pub(crate) const DEFAULT_ROOT_ID: &str = "default";

// Error codes this domain returns that the protocol crate has no constant
// for yet (they are all declared by the `directories.*` contracts).
pub(crate) const CODE_PATH_ESCAPE: &str = "PATH_ESCAPE";
pub(crate) const CODE_NOT_A_DIRECTORY: &str = "NOT_A_DIRECTORY";
pub(crate) const CODE_PATH_NOT_ALLOWED: &str = "PATH_NOT_ALLOWED";
pub(crate) const CODE_PATH_EXISTS: &str = "PATH_EXISTS";
pub(crate) const CODE_CREATE_FAILED: &str = "CREATE_FAILED";
pub(crate) const CODE_ACTIVE_RESOURCES: &str = "ACTIVE_RESOURCES";

pub(super) fn register(reg: &mut DomainRegistry) {
    // Final t012 shape of the authorized-root registry. It is a superset of
    // the interim draft `files.rs` (t013) registered while this owner hadn't
    // landed (`instance_id` column): whichever `CREATE TABLE IF NOT EXISTS`
    // runs first on a fresh database, the resolver's INSERT below only uses
    // the columns both shapes share, and the `source` provenance column is
    // backfilled with an ignorable UPDATE.
    reg.schema(
        "authorized_roots",
        "CREATE TABLE IF NOT EXISTS authorized_roots (
            root_id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            absolute_path TEXT NOT NULL,
            source TEXT,
            created_at TEXT NOT NULL
        );",
    );
    reg.handler("directories.list", |rt, input, _key, _rev| {
        Box::pin(directories_list(rt, input))
    });
    reg.handler("directories.create", |rt, input, key, _rev| {
        Box::pin(directories_create(rt, input, key))
    });
}

// ---------------------------------------------------------------------------
// ServerPath / BytePath decoding and root confinement
// ---------------------------------------------------------------------------

/// A decoded `workspace__ServerPath`: the resolved root and the validated
/// relative components.
pub(crate) struct RootedPath {
    pub root_id: String,
    pub root: PathBuf,
    pub relative: PathBuf,
}

/// Decodes a `workspace__BytePath` (`{encoding: utf8|base64, value}`) into a
/// `PathBuf`. base64 carries raw path bytes; on non-unix platforms, where
/// paths are not byte strings, base64 is refused.
pub(crate) fn decode_byte_path(value: &Value) -> Result<PathBuf, ContractError> {
    let err = |msg: String| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            msg,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let encoding = value
        .get("encoding")
        .and_then(Value::as_str)
        .ok_or_else(|| err("relativePath.encoding is required".to_owned()))?;
    let raw = value
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| err("relativePath.value is required".to_owned()))?;
    match encoding {
        "utf8" => Ok(PathBuf::from(raw)),
        "base64" => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(raw)
                .map_err(|e| err(format!("relativePath.value is not valid base64: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
            }
            #[cfg(not(unix))]
            {
                let _ = bytes;
                Err(err(
                    "base64 relativePath is not supported on this platform".to_owned()
                ))
            }
        }
        other => Err(err(format!(
            "unsupported relativePath.encoding `{other}` (utf8 or base64)"
        ))),
    }
}

/// Encodes a root-relative path back into a `workspace__BytePath` value:
/// utf8 when the path is valid UTF-8, base64 of the raw bytes otherwise.
fn encode_byte_path(relative: &Path) -> Value {
    match relative.to_str() {
        Some(text) => json!({"encoding": "utf8", "value": text}),
        None => {
            #[cfg(unix)]
            {
                use base64::Engine;
                use std::os::unix::ffi::OsStrExt;
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(relative.as_os_str().as_bytes());
                json!({"encoding": "base64", "value": encoded})
            }
            #[cfg(not(unix))]
            {
                json!({"encoding": "utf8", "value": relative.to_string_lossy()})
            }
        }
    }
}

/// Splits a relative path into components, refusing anything that could
/// escape the root: absolute paths, `..`, and explicit `.` segments. An
/// empty relative path (the root itself) yields an empty vector.
pub(crate) fn relative_components(
    relative: &Path,
) -> Result<Vec<std::ffi::OsString>, ContractError> {
    let mut out = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => out.push(name.to_os_string()),
            Component::CurDir | Component::ParentDir | Component::RootDir => {
                return Err(ContractError::new(
                    CODE_PATH_ESCAPE,
                    format!(
                        "relativePath `{}` must stay inside the authorized root \
                         (no absolute paths or `..`)",
                        relative.display()
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            // The variant exists on every platform (only constructed on
            // Windows), so it must be covered unconditionally to stay
            // exhaustive on unix targets too.
            Component::Prefix(_) => {
                return Err(ContractError::new(
                    CODE_PATH_ESCAPE,
                    "relativePath must not carry a drive prefix",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
        }
    }
    Ok(out)
}

fn path_escape(msg: String) -> ContractError {
    ContractError::new(
        CODE_PATH_ESCAPE,
        msg,
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

// ---------------------------------------------------------------------------
// Shared SQLite helpers for the t012 domain files
//
// The S2 store module keeps its transaction helpers private to
// `public_v1::store`; the sibling domain files re-implement the three
// primitives they need (revision bump, event append, error mapping) so
// `store.rs` itself stays untouched.
// ---------------------------------------------------------------------------

pub(crate) fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

pub(crate) fn domain_not_found(kind: &str, id: &str) -> ContractError {
    ContractError::new(
        CODE_NOT_FOUND,
        format!("{kind} `{id}` not found"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

/// Allocates the next global revision (`meta.rev_counter`), mirroring the S2
/// `next_rev_in_tx` inside an open transaction.
pub(crate) fn bump_rev_in_tx(conn: &Connection) -> Result<String, ContractError> {
    let cur: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let next = cur.and_then(|v| v.parse::<i64>().ok()).unwrap_or(1) + 1;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('rev_counter', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![next.to_string()],
    )
    .map_err(sql_err)?;
    Ok(format!("rev-{next:08}"))
}

/// Appends a domain event row (`project.changed`, `session.changed`, …) to
/// the durable events table, mirroring the S2 `append_event_in_tx`
/// placeholder-cursor dance.
pub(crate) fn append_domain_event_in_tx(
    conn: &Connection,
    instance_id: &str,
    resource_id: &str,
    session_id: &str,
    event_type: &str,
    payload: &Value,
    occurred_at: &str,
) -> Result<(), ContractError> {
    let placeholder = format!(
        "tmp-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT INTO events (cursor, resource_id, session_id, event_type, schema_id, payload_json, data_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, 'agentcpm-pd.event/1', ?5, ?5, ?6)",
        params![placeholder, resource_id, session_id, event_type, payload_str, occurred_at],
    )
    .map_err(sql_err)?;
    let seq = conn.last_insert_rowid();
    conn.execute(
        "UPDATE events SET cursor = ?1 WHERE seq = ?2",
        params![format!("cur-{seq:012}"), seq],
    )
    .map_err(sql_err)?;
    let _ = instance_id;
    Ok(())
}

/// `canonicalize` with io-error classification: missing targets are
/// `NOT_FOUND`, everything else is a storage `UNAVAILABLE`.
fn canonicalize_or(path: &Path, kind: &str) -> Result<PathBuf, ContractError> {
    std::fs::canonicalize(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ContractError::new(
                CODE_NOT_FOUND,
                format!("{kind} `{}` not found", path.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        } else {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("cannot resolve {}: {e}", path.display()),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::NotApplied,
            )
        }
    })
}

/// Verifies `candidate` (already canonicalized) stays inside `root`
/// (already canonicalized); otherwise the path escaped through a symlink.
fn ensure_contained(
    canonical_root: &Path,
    canonical_candidate: &Path,
) -> Result<(), ContractError> {
    canonical_candidate
        .strip_prefix(canonical_root)
        .map(|_| ())
        .map_err(|_| {
            path_escape(format!(
                "resolved path `{}` escapes authorized root `{}` (symlink?)",
                canonical_candidate.display(),
                canonical_root.display()
            ))
        })
}

impl V1Store {
    /// Resolves `root_id` against the authorized-root registry. The default
    /// root is registered (and its directory created) on first use; unknown
    /// ids answer `FORBIDDEN` — the table is the authorization declaration.
    pub(crate) fn resolve_authorized_root(&self, root_id: &str) -> Result<PathBuf, ContractError> {
        let conn = self.conn.lock().expect("sqlite mutex");
        resolve_authorized_root_conn(&conn, self.home(), root_id)
    }
}

pub(crate) fn resolve_authorized_root_conn(
    conn: &Connection,
    home: &Path,
    root_id: &str,
) -> Result<PathBuf, ContractError> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT absolute_path FROM authorized_roots WHERE root_id = ?1",
            params![root_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite storage error: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            )
        })?;
    if let Some(path) = existing {
        return Ok(PathBuf::from(path));
    }
    if root_id == DEFAULT_ROOT_ID {
        // Lazy bootstrap registration of the default workspace root inside
        // the isolated v1 home (the TS baseline's WORKSPACES_ROOT analog).
        // Only the columns every authorized_roots shape shares are written
        // here; the `source` provenance is backfilled separately below.
        let path = home.join("workspaces");
        std::fs::create_dir_all(&path).map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!(
                    "failed to create default workspace root {}: {e}",
                    path.display()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let instance_id: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'instance_id'",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("sqlite storage error: {e}"),
                    RetrySpec::backoff_read(Some(250)),
                    EffectStatus::Unknown,
                )
            })?
            .unwrap_or_else(|| "local".to_owned());
        conn.execute(
            "INSERT OR IGNORE INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                root_id,
                instance_id,
                path.to_string_lossy(),
                crate::public_v1::store::now_rfc3339()
            ],
        )
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite storage error: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            )
        })?;
        // Provenance backfill: a no-op on databases created from an interim
        // draft shape without the `source` column.
        let _ = conn.execute(
            "UPDATE authorized_roots SET source = 'bootstrap-default' WHERE root_id = ?1",
            params![root_id],
        );
        return Ok(path);
    }
    Err(ContractError::new(
        CODE_FORBIDDEN,
        format!("authorizedRootId `{root_id}` is not authorized on this instance"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    ))
}

/// Decodes and validates a request `path` (`ServerPath`), resolving the root
/// and checking that the *existing* portion of the target stays inside it.
/// `must_exist` additionally requires the full target to exist (listing).
pub(crate) fn decode_server_path(
    store: &V1Store,
    path: &Value,
    must_exist: bool,
) -> Result<RootedPath, ContractError> {
    let root_id = path
        .get("authorizedRootId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                "path.authorizedRootId is required",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?
        .to_owned();
    let relative = decode_byte_path(path.get("relativePath").ok_or_else(|| {
        ContractError::new(
            CODE_INVALID_ARGUMENT,
            "path.relativePath is required",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?)?;
    let components = relative_components(&relative)?;
    let root = store.resolve_authorized_root(&root_id)?;
    let canonical_root = canonicalize_or(&root, "authorized root")?;

    let mut target = canonical_root.clone();
    for component in &components {
        target.push(component);
    }
    if must_exist {
        let canonical_target = canonicalize_or(&target, "directory")?;
        ensure_contained(&canonical_root, &canonical_target)?;
        if !canonical_target.is_dir() {
            return Err(ContractError::new(
                CODE_NOT_A_DIRECTORY,
                format!("`{}` is not a directory", target.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    } else {
        // The target may not exist yet (creation): verify the deepest
        // existing ancestor stays inside the root, so symlinked parents
        // cannot redirect the final component.
        let mut existing = canonical_root.clone();
        for component in &components {
            let candidate = existing.join(component);
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => {
                    existing = canonicalize_or(&candidate, "directory")?;
                    ensure_contained(&canonical_root, &existing)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => {
                    return Err(ContractError::new(
                        CODE_UNAVAILABLE,
                        format!("cannot stat {}: {e}", candidate.display()),
                        RetrySpec::backoff_read(Some(250)),
                        EffectStatus::NotApplied,
                    ));
                }
            }
        }
    }
    Ok(RootedPath {
        root_id,
        root: canonical_root,
        relative,
    })
}

// ---------------------------------------------------------------------------
// directories.list
// ---------------------------------------------------------------------------

async fn directories_list(
    rt: Arc<PublicV1Runtime>,
    input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let path = input["path"].clone();
    let page = input.get("page").cloned();
    let outcome = tokio::task::spawn_blocking(move || list_directories(&store, &path, page))
        .await
        .map_err(|e| {
            DispatchError::Application(ContractError::new(
                CODE_UNAVAILABLE,
                format!("directories.list worker failed: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            ))
        })??;
    Ok((ResponseOutcome::Succeeded, outcome))
}

fn list_directories(
    store: &V1Store,
    path: &Value,
    page: Option<Value>,
) -> Result<Value, ContractError> {
    let rooted = decode_server_path(store, path, true)?;
    let (offset, limit) = parse_dir_page(page)?;
    let target = rooted.root.join(&rooted.relative);
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    let read = std::fs::read_dir(&target).map_err(|e| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("cannot read directory {}: {e}", target.display()),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        )
    })?;
    for entry in read.flatten() {
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        // Hidden directories (dot-prefixed) stay invisible, matching the TS
        // baseline's browseDirectories default (showHidden=false).
        if lossy.starts_with('.') {
            continue;
        }
        let child = target.join(&name);
        let is_dir = match std::fs::metadata(&child) {
            Ok(meta) => meta.is_dir(),
            Err(_) => continue,
        };
        if !is_dir {
            continue;
        }
        // A child resolving outside the root (symlink) refuses the listing
        // instead of leaking an out-of-root path ("符号链接越界拒绝").
        let canonical_child = canonicalize_or(&child, "directory")?;
        ensure_contained(&rooted.root, &canonical_child)?;
        names.push(name);
    }
    names.sort();

    let revision = store_current_revision(store)?;
    let total = names.len();
    if offset > total {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "pagination cursor beyond end of collection",
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        ));
    }
    let end = (offset + limit).min(total);
    let mut items = Vec::with_capacity(end - offset);
    for name in &names[offset..end] {
        let child_rel = rooted.relative.join(name);
        let can_create = std::fs::metadata(rooted.root.join(&child_rel))
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
        items.push(json!({
            "path": {
                "authorizedRootId": rooted.root_id,
                "relativePath": encode_byte_path(&child_rel),
            },
            "displayName": name.to_string_lossy(),
            "canCreate": can_create,
        }));
    }
    let mut result = json!({
        "directories": items,
        "revision": revision,
    });
    if end < total {
        result["nextCursor"] = json!(format!("d{end}"));
    }
    Ok(result)
}

fn parse_dir_page(page: Option<Value>) -> Result<(usize, usize), ContractError> {
    let obj = page.as_ref().and_then(Value::as_object);
    let limit = obj
        .and_then(|p| p.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let offset = match obj.and_then(|p| p.get("cursor")).and_then(Value::as_str) {
        Some(c) => c
            .strip_prefix('d')
            .and_then(|rest| rest.parse::<usize>().ok())
            .ok_or_else(|| {
                ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    format!("invalid pagination cursor `{c}`"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?,
        None => 0,
    };
    Ok((offset, limit))
}

fn store_current_revision(store: &V1Store) -> Result<String, ContractError> {
    let conn = store.conn.lock().expect("sqlite mutex");
    let cur: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'rev_counter'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite storage error: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            )
        })?;
    Ok(format!(
        "rev-{:08}",
        cur.and_then(|v| v.parse::<i64>().ok()).unwrap_or(1)
    ))
}

// ---------------------------------------------------------------------------
// directories.create
// ---------------------------------------------------------------------------

async fn directories_create(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let key = idempotency_key.clone();
    let created =
        tokio::task::spawn_blocking(move || create_directory(&store, &input, key.as_deref()))
            .await
            .map_err(|e| {
                DispatchError::Application(ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("directories.create worker failed: {e}"),
                    RetrySpec::backoff_read(Some(250)),
                    EffectStatus::Unknown,
                ))
            })??;
    Ok((ResponseOutcome::Succeeded, created))
}

fn create_directory(
    store: &V1Store,
    input: &Value,
    idempotency_key: Option<&str>,
) -> Result<Value, ContractError> {
    let path = &input["path"];
    // Idempotent replay (the op's retry class is idempotentSubmit): a known
    // key with the same input returns the originally-confirmed result
    // instead of re-executing; the same key with different input conflicts.
    if let Some(key) = idempotency_key
        && let Some(replay) = replay_idempotent_receipt(store, "directories.create", key, input)?
    {
        return Ok(replay);
    }

    let rooted = decode_server_path(store, path, false)?;
    let components = relative_components(&rooted.relative)?;
    if components.is_empty() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "relativePath must name a directory to create, not the authorized root itself",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    let target = rooted.root.join(&rooted.relative);
    let parent = target
        .parent()
        .ok_or_else(|| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                "relativePath has no parent directory",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?
        .to_path_buf();

    // The parent must exist and be a directory inside the root.
    let canonical_parent = canonicalize_or(&parent, "parent directory")?;
    ensure_contained(&rooted.root, &canonical_parent)?;
    if !canonical_parent.is_dir() {
        return Err(ContractError::new(
            CODE_NOT_A_DIRECTORY,
            format!("`{}` is not a directory", parent.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    // An existing target is a recognizable conflict — never merged or
    // overwritten ("已有目录明确返回冲突").
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_ALREADY_EXISTS,
            format!("directory `{}` already exists", target.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    match std::fs::create_dir(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ContractError::new(
                agentcpm_pd_protocol::errors::CODE_ALREADY_EXISTS,
                format!("directory `{}` already exists", target.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        Err(e) => {
            return Err(ContractError::new(
                CODE_CREATE_FAILED,
                format!("failed to create `{}`: {e}", target.display()),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
    }
    // Post-create confinement check: if a racing parent replacement moved
    // the fresh (still empty) directory out of the root, undo it and refuse.
    let canonical_target = match canonicalize_or(&target, "directory") {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir(&target);
            return Err(e);
        }
    };
    if let Err(escape) = ensure_contained(&rooted.root, &canonical_target) {
        let _ = std::fs::remove_dir(&target);
        return Err(escape);
    }

    let result = json!({
        "path": {
            "authorizedRootId": rooted.root_id,
            "relativePath": encode_byte_path(&rooted.relative),
        },
        "created": true,
    });
    if let Some(key) = idempotency_key {
        record_simple_idempotent_receipt(store, "directories.create", key, input, &result);
    }
    Ok(result)
}

/// Reads a previously-confirmed `directories.create` result for `key`, if
/// this exact operation accepted the same input before. A key reused with
/// different input answers `IDEMPOTENCY_CONFLICT`.
fn replay_idempotent_receipt(
    store: &V1Store,
    method_id: &str,
    key: &str,
    input: &Value,
) -> Result<Option<Value>, ContractError> {
    let digest = agentcpm_pd_protocol::envelope::compute_input_digest(method_id, input);
    let conn = store.conn.lock().expect("sqlite mutex");
    let found: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, request_state, receipt_json FROM idempotency_records
             WHERE idempotency_key = ?1 AND method_id = ?2",
            params![key, method_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("sqlite storage error: {e}"),
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::Unknown,
            )
        })?;
    let Some((stored_digest, state, receipt)) = found else {
        return Ok(None);
    };
    if stored_digest != digest {
        return Err(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_IDEMPOTENCY_CONFLICT,
            format!("idempotencyKey `{key}` already used for {method_id} with different input"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if state == "accepted" {
        // An unparseable receipt falls through to re-execution (which then
        // honestly reports the existing directory) instead of fabricating
        // a result.
        return Ok(receipt.and_then(|s| serde_json::from_str::<Value>(&s).ok()));
    }
    Ok(None)
}

/// Persists a confirmed `directories.create` result so retries with the same
/// key replay it instead of colliding with the created directory.
fn record_simple_idempotent_receipt(
    store: &V1Store,
    method_id: &str,
    key: &str,
    input: &Value,
    result: &Value,
) {
    let conn = store.conn.lock().expect("sqlite mutex");
    let digest = agentcpm_pd_protocol::envelope::compute_input_digest(method_id, input);
    let _ = conn.execute(
        "INSERT OR IGNORE INTO idempotency_records
         (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
          receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
         VALUES (?1, ?2, ?3, 'accepted', 'receiptFound', NULL, ?4, NULL, ?5, ?6, 'doNotReplay')",
        params![
            key,
            method_id,
            digest,
            serde_json::to_string(result).unwrap_or_default(),
            crate::public_v1::store::now_rfc3339(),
            (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339(),
        ],
    );
}

// ---------------------------------------------------------------------------
// Tests (contract anchors: 目录不越根；authorizedRootId 由服务端授权声明)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::PublicV1Runtime;
    use crate::public_v1::store::{InstanceMeta, V1Store};
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;
    use serde_json::json;

    fn open_runtime(root_dir: &std::path::Path) -> (std::sync::Arc<PublicV1Runtime>, String) {
        let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
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
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    fn server_path(root_id: &str, relative: &str) -> Value {
        json!({
            "authorizedRootId": root_id,
            "relativePath": {"encoding": "utf8", "value": relative},
        })
    }

    fn contract_code(error: &crate::public_v1::runtime::DispatchError) -> String {
        match error {
            crate::public_v1::runtime::DispatchError::Application(e) => e.code.clone(),
            crate::public_v1::runtime::DispatchError::Rpc { data, .. } => data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or_default()
                .to_owned(),
        }
    }

    #[tokio::test]
    async fn directories_create_and_list_stay_inside_the_authorized_root() {
        let tmp = TempDir::new("dirs-root");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        // Listing the (lazily registered) default root starts empty.
        let (_, listed) = rt
            .dispatch(
                "directories.list",
                json!({"instanceId": iid, "path": server_path("default", "")}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("directories.list", &listed)
            .unwrap();
        assert_eq!(listed["directories"].as_array().map(Vec::len), Some(0));
        assert!(listed["revision"].as_str().is_some());

        // Create a nested directory (parent chain created explicitly).
        let (_, made) = rt
            .dispatch(
                "directories.create",
                json!({"instanceId": iid, "path": server_path("default", "team-a")}),
                Some("dir-key-1"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("directories.create", &made)
            .unwrap();
        assert_eq!(made["created"], true);
        assert_eq!(
            made["path"]["relativePath"]["value"], "team-a",
            "created paths are root-relative"
        );
        let (_, nested) = rt
            .dispatch(
                "directories.create",
                json!({"instanceId": iid, "path": server_path("default", "team-a/proj-x")}),
                Some("dir-key-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(nested["created"], true);

        // The new directory is browsable with a root-relative ServerPath.
        let (_, listed) = rt
            .dispatch(
                "directories.list",
                json!({"instanceId": iid, "path": server_path("default", "")}),
                None,
                None,
            )
            .await
            .unwrap();
        let dirs = listed["directories"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0]["displayName"], "team-a");
        assert_eq!(dirs[0]["path"]["authorizedRootId"], "default");
        assert_eq!(dirs[0]["path"]["relativePath"]["value"], "team-a");
        assert_eq!(dirs[0]["canCreate"], true);

        // Existing directory with a *different* key: recognizable conflict.
        let conflict = rt
            .dispatch(
                "directories.create",
                json!({"instanceId": iid, "path": server_path("default", "team-a")}),
                Some("dir-key-3"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&conflict), "ALREADY_EXISTS");

        // Same key + same input: idempotent replay of the confirmed result.
        let (_, replay) = rt
            .dispatch(
                "directories.create",
                json!({"instanceId": iid, "path": server_path("default", "team-a/proj-x")}),
                Some("dir-key-2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(replay["created"], true);

        // Same key + different input: conflict, not a silent second create.
        let conflict = rt
            .dispatch(
                "directories.create",
                json!({"instanceId": iid, "path": server_path("default", "team-b")}),
                Some("dir-key-2"),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&conflict), "IDEMPOTENCY_CONFLICT");

        // The authorized root is a real registry row (consumed by t013/clone).
        let roots: Vec<(String, String, Option<String>)> = {
            let conn = rt.store().conn.lock().expect("sqlite mutex");
            let mut stmt = conn
                .prepare("SELECT root_id, absolute_path, source FROM authorized_roots")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(roots.iter().any(|(id, path, source)| id == "default"
            && path.ends_with("workspaces")
            && source.as_deref() == Some("bootstrap-default")));
    }

    #[tokio::test]
    async fn directories_refuse_escapes_and_unknown_roots() {
        let tmp = TempDir::new("dirs-escape");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);

        // Lexical escapes: absolute paths and `..`.
        for bad in ["../outside", "/etc", "a/../../b"] {
            let error = rt
                .dispatch(
                    "directories.create",
                    json!({"instanceId": iid, "path": server_path("default", bad)}),
                    Some("escape-key"),
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&error), "PATH_ESCAPE", "input `{bad}`");
        }

        // An unknown root id is not authorized to browse.
        let error = rt
            .dispatch(
                "directories.list",
                json!({"instanceId": iid, "path": server_path("nope", "")}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "FORBIDDEN");

        // A missing path inside the root answers NOT_FOUND; a file answers
        // NOT_A_DIRECTORY.
        std::fs::write(root_dir.join("workspaces").join("afile.txt"), b"x").unwrap();
        let error = rt
            .dispatch(
                "directories.list",
                json!({"instanceId": iid, "path": server_path("default", "missing-dir")}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_FOUND");
        let error = rt
            .dispatch(
                "directories.list",
                json!({"instanceId": iid, "path": server_path("default", "afile.txt")}),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(contract_code(&error), "NOT_A_DIRECTORY");

        // A symlink resolving outside the root refuses the listing
        // ("符号链接越界拒绝").
        #[cfg(unix)]
        {
            let outside = tmp.0.join("outside-target");
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, root_dir.join("workspaces").join("bad-link"))
                .unwrap();
            let error = rt
                .dispatch(
                    "directories.list",
                    json!({"instanceId": iid, "path": server_path("default", "")}),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(contract_code(&error), "PATH_ESCAPE");
        }
    }

    #[tokio::test]
    async fn directories_list_paginates_and_marks_writable() {
        let tmp = TempDir::new("dirs-page");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, iid) = open_runtime(&root_dir);
        for i in 0..3 {
            std::fs::create_dir_all(root_dir.join("workspaces").join(format!("d{i}"))).unwrap();
        }
        let (_, page_one) = rt
            .dispatch(
                "directories.list",
                json!({
                    "instanceId": iid,
                    "path": server_path("default", ""),
                    "page": {"limit": 2},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("directories.list", &page_one)
            .unwrap();
        assert_eq!(page_one["directories"].as_array().map(Vec::len), Some(2));
        let cursor = page_one["nextCursor"].as_str().unwrap().to_owned();
        let (_, page_two) = rt
            .dispatch(
                "directories.list",
                json!({
                    "instanceId": iid,
                    "path": server_path("default", ""),
                    "page": {"limit": 2, "cursor": cursor},
                }),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(page_two["directories"].as_array().map(Vec::len), Some(1));
        assert!(page_two.get("nextCursor").is_none());
    }
}
