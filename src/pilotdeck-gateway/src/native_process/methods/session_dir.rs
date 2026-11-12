//! S1 session directory / lifecycle wiring.
//!
//! TS references:
//!  - `InProcessGateway.ts:2022-2078` (list/resume/new/close/closeProject/record)
//!  - `InProcessGateway.ts:2211-2239` (rename/tag/delete)
//!  - `createLocalGateway.ts:1116-1152` (rename/tag/delete implementations)
//!  - `SessionRouter.ts:368-420` (list, recordAgentStatusMessage)
//!  - `GatewaySessionCatalog.ts:20-35` (cursor protocol over SessionCatalogPort)
use super::super::requests::Requests;
use crate::{
    metadata::object,
    runtime::{
        connection::{Call, RequestError},
        manager::{
            GatewaySessionCatalogConsumer, SessionCatalogListInput, TranscriptSessionCatalog,
        },
        session::transcript_locks,
    },
    storage::{AsyncTranscriptAdapter, JsonlTranscriptStore, ProjectKey, SessionStorageInput},
};
use pilotdeck_protocol::{JsString, JsValue};
use std::sync::Arc;

pub(super) const METHODS: &[&str] = &[
    "list_sessions",
    "resume_session",
    "new_session",
    "close_session",
    "delete_session",
    "rename_session",
    "tag_session",
    "close_project_sessions",
    "record_agent_status_message",
];
pub(super) const HANDLER: super::Handler =
    super::Handler::AfterAdmission(Requests::session_dir_request);

fn gateway_error(code: &'static str, message: impl Into<String>) -> RequestError {
    RequestError {
        code: code.into(),
        message: message.into().into(),
        details: None,
        validation: None,
    }
}

/// Validates that `sessionKey` is present and non-blank (TS `?.trim()` check).
/// Delegates to the family-shared guard (#1604) so the blank test is JS trim
/// (U+FEFF and the other WhiteSpace code points count as blank) and a
/// non-string produces the bare `input.sessionKey?.trim is not a function`
/// TypeError instead of the `INVALID_SESSION_KEY` domain error this file used
/// to answer. The blank check stays
/// lossy: TS accepts a lone surrogate in the key (`?.trim()` leaves it intact,
/// #1455 probe4), so a `to_utf8` filter here would reject a key TS accepts.
/// Callers that need UTF-8 bytes convert lossily at the point of use, like
/// TS's own path encoding.
fn require_session_key(params: &JsValue) -> Result<JsString, RequestError> {
    super::require_text(
        params,
        "sessionKey",
        "INVALID_SESSION_KEY",
        "sessionKey is required.",
    )
}

/// Build a transcript adapter for the configured project.
fn _make_adapter(requests: &Requests) -> AsyncTranscriptAdapter {
    let home = requests.options.home.clone();
    AsyncTranscriptAdapter::new(
        Arc::new(JsonlTranscriptStore::new()),
        Arc::new(move |key: &ProjectKey| {
            crate::session_chat_dir(
                &home,
                &key.project_root,
                &key.project_root.to_string_lossy(),
            )
        }),
    )
}

impl Requests {
    pub(in crate::native_process) fn session_dir_request(
        &self,
        method: String,
        params: JsValue,
    ) -> Call<'_, Result<JsValue, RequestError>> {
        Box::pin(async move {
            match method.as_str() {
                "list_sessions" => self.session_list(&params).await,
                "resume_session" => self.session_resume(&params),
                "new_session" => self.session_new(&params),
                "close_session" => self.session_close(&params).await,
                "delete_session" => self.session_delete(&params).await,
                "rename_session" => self.session_rename_or_tag(&params, false).await,
                "tag_session" => self.session_rename_or_tag(&params, true).await,
                "close_project_sessions" => self.session_close_project(&params).await,
                "record_agent_status_message" => self.session_record_status(&params).await,
                _ => unreachable!("session_dir_request called for unknown method"),
            }
        })
    }

    /// `InProcessGateway.listSessions` → `SessionRouter.list` → catalog path.
    ///
    /// Cursor is a base-10 integer offset string (`GatewaySessionCatalog.ts:24-25`).
    ///
    /// The catalog reads the storage of `registry.resolve(input.projectKey)`
    /// (ProjectRuntimeRegistry.ts:288-291): runtime routing, so an
    /// unregistered or missing project lists as empty instead of failing.
    async fn session_list(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        let project_root = self.route_project(params)?;
        let limit = params
            .get("limit")
            .and_then(JsValue::as_number)
            .filter(|n| n.is_finite() && *n > 0.0);
        let offset: f64 = params
            .get("cursor")
            .and_then(JsValue::as_string)
            .and_then(|s| s.to_utf8().ok())
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .unwrap_or(0.0);
        let home = self.options.home.clone();
        let input = SessionCatalogListInput {
            project_root: project_root.clone(),
            pilot_home: home.clone(),
            limit,
            offset: Some(offset),
            include_internal: false,
        };
        let sessions = tokio::task::spawn_blocking(move || {
            let adapter = AsyncTranscriptAdapter::new(
                Arc::new(JsonlTranscriptStore::new()),
                Arc::new(move |key: &ProjectKey| {
                    crate::session_chat_dir(
                        &home,
                        &key.project_root,
                        &key.project_root.to_string_lossy(),
                    )
                }),
            );
            let catalog = Arc::new(TranscriptSessionCatalog::new(Arc::new(adapter)));
            GatewaySessionCatalogConsumer::new(catalog).list(&input)
        })
        .await
        .map_err(|e| RequestError::new(e.to_string()))?
        .map_err(|e| RequestError {
            code: e.code().unwrap_or("gateway_request_failed").into(),
            message: e.message().clone(),
            details: None,
            validation: None,
        })?;
        let next_cursor = limit
            .filter(|lim| sessions.len() as f64 >= *lim)
            .map(|lim| (offset + lim).to_string());
        let mut result = pilotdeck_protocol::JsObject::new();
        result.insert("sessions".into(), JsValue::Array(sessions));
        if let Some(cursor) = next_cursor {
            result.insert("nextCursor".into(), JsValue::String(cursor.into()));
        }
        Ok(JsValue::Object(result))
    }

    /// `InProcessGateway.resumeSession` (InProcessGateway.ts:2026-2028) is
    /// `return input`: the params come back as sent, whatever they hold.
    fn session_resume(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        Ok(params.clone())
    }

    /// `InProcessGateway.newSession` (InProcessGateway.ts:2030-2034):
    /// `${input.channelKey}:${projectKey}s_${uuid}` with
    /// `projectKey = input.projectKey ? "project=<key>:" : ""`. Nothing is
    /// validated — a missing `channelKey` interpolates as `undefined` and the
    /// project is neither resolved nor checked.
    fn session_new(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        let project_key = super::input_property(params, "projectKey")?;
        let channel_key = params
            .get("channelKey")
            .cloned()
            .unwrap_or(JsValue::Undefined);
        let interpolate = |value: &JsValue| {
            crate::js_value::to_string_units(value)
                .map_err(|_| RequestError::new("Maximum call stack size exceeded"))
        };
        let mut key = interpolate(&channel_key)?;
        key.push(u16::from(b':'));
        if let Some(project) = project_key.filter(crate::metadata::js_truthy) {
            key.extend("project=".encode_utf16());
            key.extend(interpolate(&project)?);
            key.push(u16::from(b':'));
        }
        key.extend(format!("s_{}", uuid::Uuid::new_v4()).encode_utf16());
        Ok(object([(
            "sessionKey",
            JsValue::String(JsString::from_utf16(key)),
        )]))
    }

    /// `InProcessGateway.closeSession` (InProcessGateway.ts:2049-2054) →
    /// `router.close(input.sessionKey)` + interaction cleanup; the WS layer
    /// answers `{ ok: true }` (GatewayWsConnection.ts:339-340). The key is not
    /// validated: closing a missing or absent key is a no-op that still
    /// answers ok.
    async fn session_close(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        let key = super::input_property(params, "sessionKey")?;
        let reason: JsString = params
            .get("reason")
            .and_then(JsValue::as_string)
            .cloned()
            .unwrap_or_else(|| "session_closed".into());
        // The session maps are keyed by string; any other value names no
        // session, like a `Map.get` miss in the baseline.
        if let Some(JsValue::String(key)) = &key {
            self.owner
                .close(key)
                .await
                .map_err(|e| RequestError::new(e.to_string()))?;
            self.interactions.close_session(key, &reason);
            // `this.permissionModes.clear(input.sessionKey)`.
            self.session_overrides.clear_live_mode(key);
        }
        Ok(object([("ok", JsValue::Bool(true))]))
    }

    /// `createLocalGateway.deleteSession` → close live owner, delete transcript +
    /// file history + tool results + subagents directories, then drop the
    /// session's SDK registry rows (`clearSdkSessionState`,
    /// createLocalGateway.ts:1160).
    async fn session_delete(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        // createLocalGateway.ts:1143-1144: the dialog registry answers first.
        let project_root = self.dialog_project(params)?;
        let key = require_session_key(params)?;
        // Close the live session first so the writer drains before we delete.
        self.owner
            .close(&key)
            .await
            .map_err(|e| RequestError::new(e.to_string()))?;
        let home = self.options.home.clone();
        let key_clone = key.clone();
        let project_clone = project_root.clone();
        tokio::task::spawn_blocking(move || {
            let home2 = home.clone();
            let adapter = AsyncTranscriptAdapter::new(
                Arc::new(JsonlTranscriptStore::new()),
                Arc::new(move |k: &ProjectKey| {
                    crate::session_chat_dir(
                        &home2,
                        &k.project_root,
                        &k.project_root.to_string_lossy(),
                    )
                }),
            );
            let storage = adapter
                .create_session_storage(SessionStorageInput {
                    project_root: project_clone.clone(),
                    pilot_home: home.clone(),
                    session_id: key_clone.clone(),
                    now: None,
                })
                .map_err(|e| RequestError::new(e.to_string()))?;
            // Verify the transcript exists before deleting.
            let exists = storage
                .transcript_exists()
                .transpose()
                .map_err(|e| RequestError::new(e.to_string()))?
                .unwrap_or(false);
            if !exists {
                let msg = format!(
                    "Session not found: {}",
                    String::from_utf16_lossy(key_clone.units())
                );
                return Err(gateway_error("SESSION_NOT_FOUND", msg));
            }
            // createLocalGateway.ts:1150-1152: `deleteSessionTranscripts`
            // when the store has it, else `deleteTranscript`. The native
            // JSONL store only has the latter; skipping it left the
            // transcript on disk behind an ok answer.
            if let Some(result) = storage
                .delete_session_transcripts()
                .or_else(|| storage.delete_transcript())
            {
                result.map_err(|e| RequestError::new(e.to_string()))?;
            }
            // Remove file history and tool results directories.
            for dir in [
                &storage.file_history_dir,
                &storage.tool_results_dir,
                &storage.subagents_dir,
            ] {
                let _ = std::fs::remove_dir_all(dir);
            }
            Ok(())
        })
        .await
        .map_err(|e| RequestError::new(e.to_string()))??;
        // `registry.clearSdkSessionState(input.sessionKey)` — after every
        // storage delete succeeded, exactly like the baseline. The native
        // split (#2392): the MCP fields (server table, permission-mode
        // overrides, generated permission rules, the shared session→project
        // entry) live in `SdkMcpConfig`; `sdkThinkingOverrides` and
        // `sdkSessionConfigs.outputStyle` (the TS clear deletes the whole
        // config row, ProjectRuntimeRegistry.ts:776-786) live in
        // `session_overrides`. The registry half of the TS clear (task-budget
        // ledger, plugins, handler registrations) has no production instance
        // in this process yet — see `clear_session_state`'s contract
        // (runtime/sdk_session/registry.rs): it must be called **before**
        // the MCP clear, because the MCP clear drops the shared projects
        // entry its budget step reads. When that half is wired (#1839), it
        // belongs above this line, never below it.
        // `if (projectRoot) this.clearTaskBudgetLedger(projectRoot,
        // sessionKey)`: the session's SDK project (from its config or its
        // MCP servers) gets a `cleared` ledger record. The store is
        // append-only and this clear does not load it.
        let sdk_project = self
            .session_overrides
            .sdk_config_project(&key)
            .map(|root| JsString::from(root.to_string_lossy().as_ref()))
            .or_else(|| {
                self.mcp
                    .config()
                    .session_project(&JsValue::String(key.clone()))
            });
        if let Some(root) = sdk_project {
            let _ = crate::storage::TaskBudgetStore::new(&self.options.home, None)
                .clear_session(&root, &key);
        }
        self.mcp
            .config()
            .clear_sdk_session_state(&JsValue::String(key.clone()));
        self.session_overrides.clear_thinking(&key);
        self.session_overrides.clear_sdk_config(&key);
        // GatewayWsConnection.ts:342: `.then(() => ({ ok: true }))`.
        Ok(object([("ok", JsValue::Bool(true))]))
    }

    /// `createLocalGateway.renameSession` / `tagSession` → busy check + metadata record.
    /// `is_tag` selects between title (rename) and tag metadata fields.
    async fn session_rename_or_tag(
        &self,
        params: &JsValue,
        is_tag: bool,
    ) -> Result<JsValue, RequestError> {
        // createLocalGateway.ts:1117-1121 / 1130-1134: project, key, busy.
        let project_root = self.dialog_project(params)?;
        let key = require_session_key(params)?;
        if self.owner.has_active_turn(&key) {
            let verb = if is_tag { "tag" } else { "rename" };
            return Err(gateway_error(
                "SESSION_BUSY",
                format!("Cannot {verb} an active session."),
            ));
        }
        // #1457 fix-r1: same single-writer fence the config family uses; two
        // fresh writers that both restore the tail and append `sequence + 1`
        // leave the transcript unloadable.
        let fence_path =
            transcript_locks::session_transcript_path(&self.options.home, &project_root, &key);
        let write_fence = transcript_locks::acquire(&fence_path).await?;
        // #1457 fix-r2: a loaded session owns the transcript writer (and the
        // writer lease) with an in-memory sequence, so the metadata entry goes
        // *through* its recorder. Closing it first retired an idle session the
        // user did not ask to close and could abort a turn admitted between the
        // busy check and the close.
        // #1457 fix-r3: with no loaded session the cold append takes the same
        // writer lease a session holds, so it cannot interleave with the
        // factory's restore→publish window; a busy lease means a session owns
        // the transcript after all (use its writer or report busy).
        let recorders = self.recorders.clone();
        let home = self.options.home.clone();
        let value_field: JsValue = params.get("value").cloned().unwrap_or(JsValue::Undefined);
        let value_str: Option<String> = match &value_field {
            JsValue::Null | JsValue::Undefined => None,
            other => other
                .as_string()
                .and_then(|s| s.to_utf8().ok())
                .or_else(|| {
                    // TS: `String(input.value)` — convert number/bool to string
                    match other {
                        JsValue::Number(n) => Some(n.to_string()),
                        JsValue::Bool(b) => Some(b.to_string()),
                        _ => None,
                    }
                }),
        };
        let key_clone = key.clone();
        tokio::task::spawn_blocking(move || {
            // Held inside the closure: a cancelled request drops the future but
            // not the blocking append, so the fence has to travel with the write.
            let _write = write_fence;
            let updated_at = pilotdeck_transport::system_clock::format_epoch_iso(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64,
            );
            let mut meta = pilotdeck_protocol::JsObject::new();
            let field_name = if is_tag { "tag" } else { "title" };
            meta.insert(
                field_name.into(),
                value_str
                    .map(|s| JsValue::String(s.into()))
                    .unwrap_or(JsValue::Undefined),
            );
            meta.insert("updatedAt".into(), JsValue::String(updated_at.into()));
            let turn_id = JsString::from(if is_tag { "sdk-tag" } else { "sdk-rename" });
            let entry = JsValue::Object(meta);
            // The shared three-segment protocol (#1874): this closure is both
            // the first lookup and the busy-lease re-query, so the one checked
            // append below covers both paths.
            let live = || {
                let Some((owner, recorder)) =
                    super::super::assembly::live_recorder(&recorders, &key_clone)
                else {
                    return Ok(None);
                };
                recorder
                    .writer()
                    .record_session_metadata_checked(&key_clone, &turn_id, entry.clone())
                    .map_err(|e| RequestError::new(e.to_string()))?;
                Ok(Some(owner))
            };
            transcript_locks::append_metadata_with_lease(
                &home,
                &project_root,
                &key_clone,
                live,
                |storage| {
                    storage
                        .transcript
                        .record_session_metadata(&key_clone, &turn_id, entry.clone())
                        .map_err(|e| RequestError::new(e.to_string()))
                },
            )?;
            Ok(())
        })
        .await
        .map_err(|e| RequestError::new(e.to_string()))??;
        Ok(object([("updated", JsValue::Bool(true))]))
    }

    /// `InProcessGateway.closeProjectSessions` → resume or close all project sessions.
    async fn session_close_project(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        // #1604: `!input.projectKey?.trim()` — same guard family as
        // `require_session_key`, so a non-string projectKey throws the bare
        // optional-chain TypeError instead of folding into the domain error.
        // The missing/blank branch keeps the plain `Error` shape TS produces
        // (`gateway_request_failed` + "projectKey is required.").
        let project_key = super::require_text(
            params,
            "projectKey",
            "gateway_request_failed",
            "projectKey is required.",
        )?;
        // InProcessGateway.ts:2036-2047: the `?.trim()` shape above is the
        // only gate; any project's sessions can be closed or resumed. The raw
        // UTF-16 key reaches the owner, so a session created under the same
        // spelling closes like any other project (#2083).
        if params.get("resume") == Some(&JsValue::Bool(true)) {
            self.owner.resume_project(&project_key);
            return Ok(object([("sessionKeys", JsValue::Array(vec![]))]));
        }
        let keys = self
            .owner
            .close_project(&project_key)
            .await
            .map_err(|e| RequestError::new(e.to_string()))?;
        for key in &keys {
            self.interactions
                .close_session(key, &"project_closed".into());
            self.session_overrides.clear_live_mode(key);
        }
        let session_keys: Vec<JsValue> = keys.into_iter().map(JsValue::String).collect();
        Ok(object([("sessionKeys", JsValue::Array(session_keys))]))
    }

    /// `InProcessGateway.recordAgentStatusMessage` → live owner first, cold fallback.
    ///
    /// Live path: look up the session's recorder in the shared recorders map.
    /// Cold path: append via a fresh transcript writer (like `options.recordAgentStatusMessage`).
    async fn session_record_status(&self, params: &JsValue) -> Result<JsValue, RequestError> {
        // GatewaySessionHistoryBundle.ts:205-213: the session id goes through
        // the path sanitizer (`sessionId.replace(...)`) before anything else,
        // so a non-string key fails there with the boundary's TypeError.
        let session_key = match params.get("sessionKey") {
            Some(JsValue::String(key)) => key.clone(),
            None | Some(JsValue::Undefined) => {
                return Err(RequestError::new(
                    "Cannot read properties of undefined (reading 'replace')",
                ));
            }
            Some(JsValue::Null) => {
                return Err(RequestError::new(
                    "Cannot read properties of null (reading 'replace')",
                ));
            }
            Some(_) => return Err(RequestError::new("sessionId.replace is not a function")),
        };
        let status = params.get("status").cloned().unwrap_or(JsValue::Undefined);
        // JsonlTranscriptWriter reads `status.event` first: a nullish status
        // throws there, after the cold storage was restored and before it is
        // disposed (so the checkpoint is still flushed). `turnId` is not read
        // before that point.
        let missing_status = match status {
            JsValue::Undefined => Some("Cannot read properties of undefined (reading 'event')"),
            JsValue::Null => Some("Cannot read properties of null (reading 'event')"),
            _ => None,
        };
        let turn_id = match (missing_status, params.get("turnId")) {
            (Some(_), _) => JsString::from(""),
            (None, Some(JsValue::String(turn))) => turn.clone(),
            // A non-string turnId is written verbatim by TS; the native
            // transcript writer only carries string turn ids.
            (None, _) => return Err(RequestError::new("turnId must be a string.")),
        };

        // #1457 fix-r2: take the fence before deciding who writes. The lookup
        // and the append then see one state — either a live writer exists and
        // the entry goes through it, or it does not and this path is the only
        // writer for the tail it restores. Looking the recorder up first left a
        // window where a concurrent close retired it between lookup and append,
        // and the retired writer's no-op append reported `recorded: true`.
        // GatewaySessionHistoryBundle.ts:250: `projectKey ? projectKey :
        // fallbackProjectRoot`, no registry lookup.
        let project_root = self.options.project_root_for(
            params
                .get("projectKey")
                .and_then(JsValue::as_string)
                .filter(|key| !key.is_empty()),
        );
        let fence_path = transcript_locks::session_transcript_path(
            &self.options.home,
            &project_root,
            &session_key,
        );
        let write_fence = transcript_locks::acquire(&fence_path).await?;
        let recorders = self.recorders.clone();
        let home = self.options.home.clone();
        // The shared three-segment protocol (#1874), with #1873's error shape:
        // the live closure below is both the first lookup and the busy-lease
        // re-query, so its one checked append — a writer that closed under us
        // is an error, not a `recorded: true` with nothing on disk — covers
        // both paths; a contended lease reports the retryable busy answer
        // like the rename and config paths; storage failures propagate instead
        // of collapsing into `recorded: false` (TS reserves that for "not live
        // and no cold provider", which never holds in the native process).
        let recorded = tokio::task::spawn_blocking(move || -> Result<bool, RequestError> {
            // The fence must outlive a cancelled request, so it moves into the
            // blocking closure instead of dropping with this future.
            let _write = write_fence;
            let live = || {
                let Some((owner, recorder)) =
                    super::super::assembly::live_recorder(&recorders, &session_key)
                else {
                    return Ok(None);
                };
                if let Some(message) = missing_status {
                    return Err(RequestError::new(message));
                }
                recorder
                    .writer()
                    .record_agent_status_message_checked(&session_key, &turn_id, &status)
                    .map_err(|e| RequestError::new(e.to_string()))?;
                Ok(Some(owner))
            };
            transcript_locks::append_metadata_with_lease(
                &home,
                &project_root,
                &session_key,
                live,
                |storage| {
                    let recorded = match missing_status {
                        Some(message) => Err(RequestError::new(message)),
                        None => storage
                            .transcript
                            .record_agent_status_message(&session_key, &turn_id, &status)
                            .map_err(|e| RequestError::new(e.to_string())),
                    };
                    // GatewaySessionHistoryBundle.ts:229-235: the cold storage
                    // is disposed whether or not the append threw, and dispose
                    // flushes the projection checkpoint.
                    let saved = crate::runtime::session::factory::save_cold_projection_checkpoint(
                        storage,
                        &session_key,
                    )
                    .map_err(|e| RequestError::new(e.message));
                    recorded.and(saved)
                },
            )?;
            Ok(true)
        })
        .await
        .map_err(|e| RequestError::new(e.to_string()))??;
        Ok(object([("recorded", JsValue::Bool(recorded))]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::recorder::Recorder;
    use crate::{negative_assert, negative_assert_eq};

    /// #1457 fix-r2: a loaded session owns the transcript writer, so the rename
    /// append goes through its recorder instead of a second writer restored
    /// from the tail. The request spells the key as an alias (`a8/session` and
    /// `a8-session` are one file after `sanitize_session_id`), which is exactly
    /// the case a raw-key lookup missed: the recorder would keep a stale
    /// sequence and its next append would duplicate `sequence + 1`.
    #[tokio::test]
    async fn rename_appends_through_the_loaded_writer() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let open = |session_id: &str| {
            adapter
                .create_session_storage(SessionStorageInput {
                    project_root: project.clone(),
                    pilot_home: requests.options.home.clone(),
                    session_id: session_id.into(),
                    now: None,
                })
                .expect("#1457 the transcript storage opens")
        };
        let loaded = open("a8/session");
        loaded
            .restore()
            .expect("#1457 the seeded transcript restores");
        let writer = Arc::new(loaded.transcript);
        let recorder = Arc::new(Recorder::from_writer(writer.clone()));
        requests
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("a8/session".into(), Arc::downgrade(&recorder));

        let updated = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "rename_session".into(),
            crate::metadata::object([
                ("sessionKey", "a8-session".into()),
                ("value", "renamed-through-the-recorder".into()),
            ]),
        )
        .await
        .expect("#1457 the rename resolves over a loaded session");
        negative_assert_eq!(
            updated
                .get("updated")
                .and_then(pilotdeck_protocol::JsValue::as_bool),
            Some(true),
            "#1457 the rename reports the metadata write"
        );

        // The loaded writer still holds the only sequence: this append continues
        // the chain the rename extended. A second writer on the same tail would
        // have duplicated `sequence + 1` and this call would be rejected.
        writer
            .record_agent_status_message(
                &"a8/session".into(),
                &"after-rename".into(),
                &JsValue::Undefined,
            )
            .expect("#1457 the loaded writer keeps numbering the chain");
        let reread = open("a8-session");
        reread.restore().expect("#1457 the transcript still loads");
        let text = std::fs::read_to_string(&reread.transcript_path)
            .expect("#1457 the written transcript is readable");
        negative_assert_eq!(
            text.contains("renamed-through-the-recorder"),
            true,
            "#1457 the rename entry reached the loaded session's transcript"
        );
        negative_assert_eq!(
            text.contains("after-rename"),
            true,
            "#1457 the loaded writer kept appending after the rename"
        );
    }

    /// #1457 fix-r3: `record_agent_status_message` has the same live branch as
    /// rename. Two things have to hold at once: the entry goes through the
    /// loaded writer (so the chain keeps one sequence), and the request reports
    /// `recorded: true` only because the writer actually took it.
    #[tokio::test]
    async fn status_appends_through_the_loaded_writer() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let open = |session_id: &str| {
            adapter
                .create_session_storage(SessionStorageInput {
                    project_root: project.clone(),
                    pilot_home: requests.options.home.clone(),
                    session_id: session_id.into(),
                    now: None,
                })
                .expect("#1457 the transcript storage opens")
        };
        let loaded = open("c4/session");
        loaded
            .restore()
            .expect("#1457 the seeded transcript restores");
        let writer = Arc::new(loaded.transcript);
        let recorder = Arc::new(Recorder::from_writer(writer.clone()));
        requests
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("c4/session".into(), Arc::downgrade(&recorder));

        let recorded = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "record_agent_status_message".into(),
            crate::metadata::object([
                ("sessionKey", "c4-session".into()),
                ("turnId", "status-through-the-recorder".into()),
                // A nullish status throws on `status.event` in TS before any
                // write; the writer path needs a status object.
                (
                    "status",
                    crate::metadata::object([("event", "note".into())]),
                ),
            ]),
        )
        .await
        .expect("#1457 the status write resolves over a loaded session");
        negative_assert_eq!(
            recorded
                .get("recorded")
                .and_then(pilotdeck_protocol::JsValue::as_bool),
            Some(true),
            "#1457 the status write reports the metadata write"
        );

        writer
            .record_agent_status_message(
                &"c4/session".into(),
                &"after-status".into(),
                &JsValue::Undefined,
            )
            .expect("#1457 the loaded writer keeps numbering the chain");
        let reread = open("c4-session");
        reread.restore().expect("#1457 the transcript still loads");
        let text = std::fs::read_to_string(&reread.transcript_path)
            .expect("#1457 the written transcript is readable");
        negative_assert_eq!(
            text.contains("status-through-the-recorder"),
            true,
            "#1457 the status entry reached the loaded session's transcript"
        );
    }

    /// #1457 fix-r3: a cold metadata append must not run while a session holds
    /// the writer lease. The lease is the same object the factory takes before
    /// it restores the tail and publishes the recorder, so holding it here is
    /// exactly the hydrate window; the request has to report the retryable busy
    /// error instead of forking the sequence chain.
    #[tokio::test]
    async fn cold_metadata_write_reports_busy_while_a_session_holds_the_lease() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let storage = adapter
            .create_session_storage(SessionStorageInput {
                project_root: project.clone(),
                pilot_home: requests.options.home.clone(),
                session_id: "d2/session".into(),
                now: None,
            })
            .expect("#1457 the transcript storage opens");
        // Stand in for `factory`: the lease is taken before restore and held
        // while the recorder is not yet in the map.
        let lease = transcript_locks::cold_write_lease(&storage.transcript_path)
            .expect("#1873 the transcript lease probe itself must not fail")
            .expect("#1457 the transcript lease is free before the session loads");

        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "rename_session".into(),
            crate::metadata::object([
                ("sessionKey", "d2-session".into()),
                ("value", "must-not-fork".into()),
            ]),
        )
        .await
        .expect_err("#1457 a cold append must not run beside a session that owns the writer");
        negative_assert_eq!(
            error.code.as_str(),
            "SESSION_WRITER_BUSY",
            "#1457 the racing cold append reports the retryable busy error"
        );
        drop(lease);
        negative_assert_eq!(
            std::fs::read_to_string(&storage.transcript_path)
                .map(|text| text.contains("must-not-fork"))
                .unwrap_or(false),
            false,
            "#1457 the refused append wrote nothing"
        );
    }

    /// #1873: the status cold path answers the same retryable busy error the
    /// rename and config paths give when a session holds the writer lease.
    /// The pre-fix shape returned `recorded: false` — TS's "not live and no
    /// cold provider" answer, which never holds in the native process.
    #[tokio::test]
    async fn status_cold_write_reports_busy_while_a_session_holds_the_lease() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let storage = adapter
            .create_session_storage(SessionStorageInput {
                project_root: project.clone(),
                pilot_home: requests.options.home.clone(),
                session_id: "d3/session".into(),
                now: None,
            })
            .expect("#1873 the transcript storage opens");
        // Stand in for `factory`: the lease is taken before restore and held
        // while the recorder is not yet in the map.
        let lease = transcript_locks::cold_write_lease(&storage.transcript_path)
            .expect("#1873 the transcript lease probe itself must not fail")
            .expect("#1873 the transcript lease is free before the session loads");
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "record_agent_status_message".into(),
            crate::metadata::object([
                ("sessionKey", "d3-session".into()),
                ("turnId", "must-not-fork".into()),
                ("status", JsValue::Undefined),
            ]),
        )
        .await
        .expect_err(
            "#1873 a cold status append must not run beside a session that owns the writer",
        );
        negative_assert_eq!(
            error.code.as_str(),
            "SESSION_WRITER_BUSY",
            "#1873 the racing cold status append reports the retryable busy error"
        );
        drop(lease);
        negative_assert_eq!(
            std::fs::read_to_string(&storage.transcript_path)
                .map(|text| text.contains("must-not-fork"))
                .unwrap_or(false),
            false,
            "#1873 the refused status append wrote nothing"
        );
    }

    /// #1873 P2: a `<transcript>.lock` that cannot be opened is a storage
    /// failure. The pre-fix `try_writer_lease` folded every error into the
    /// busy answer, so a broken chat directory masqueraded as a session that
    /// would eventually drain; the request must fail with the underlying
    /// error, never the retryable busy code.
    #[tokio::test]
    async fn status_cold_write_surfaces_a_broken_lock_file_as_an_error() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let storage = adapter
            .create_session_storage(SessionStorageInput {
                project_root: project.clone(),
                pilot_home: requests.options.home.clone(),
                session_id: "d5/session".into(),
                now: None,
            })
            .expect("#1873 the transcript storage opens");
        // The fence inode lives outside the session directory
        // (`storage::ledger_lock_path`); a directory there cannot be opened.
        let lock = crate::storage::ledger_lock_path(
            &crate::storage::canonical_transcript_path(&storage.transcript_path)
                .expect("#1873 canonical path"),
        )
        .expect("#1873 the ledger lock directory");
        std::fs::create_dir(&lock).expect("#1873 the broken ledger lock is in place");
        let _remove_lock = RemoveDir(lock.clone());
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "record_agent_status_message".into(),
            crate::metadata::object([
                ("sessionKey", "d5-session".into()),
                ("turnId", "broken-lock".into()),
                ("status", JsValue::Undefined),
            ]),
        )
        .await
        .expect_err("#1873 the status append surfaces the storage failure");
        negative_assert!(
            error.code.as_str() != "SESSION_WRITER_BUSY",
            "#1873 a broken lock file is a storage error, not the retryable busy answer"
        );
        negative_assert!(
            String::from_utf16_lossy(error.message.units())
                .contains("Could not open session ledger lock"),
            "#1873 the storage error names the ledger lock, got {}",
            String::from_utf16_lossy(error.message.units())
        );
    }

    /// #1873: the rename cold path answers the same storage-error separation
    /// the status caller got — a broken ledger lock is never the retryable
    /// busy answer for a rename either. The `#1873` `try_writer_lease`
    /// `Result` split reaches this caller through the shared
    /// `append_metadata_with_lease` protocol.
    #[tokio::test]
    async fn rename_cold_write_surfaces_a_broken_lock_file_as_an_error() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let storage = adapter
            .create_session_storage(SessionStorageInput {
                project_root: project.clone(),
                pilot_home: requests.options.home.clone(),
                session_id: "d6/session".into(),
                now: None,
            })
            .expect("#1873 the rename transcript storage opens");
        let lock = crate::storage::ledger_lock_path(
            &crate::storage::canonical_transcript_path(&storage.transcript_path)
                .expect("#1873 canonical path"),
        )
        .expect("#1873 the ledger lock directory");
        std::fs::create_dir(&lock).expect("#1873 the rename broken ledger lock is in place");
        let _remove_lock = RemoveDir(lock.clone());
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "rename_session".into(),
            crate::metadata::object([
                ("sessionKey", "d6-session".into()),
                ("value", "broken-lock-rename".into()),
            ]),
        )
        .await
        .expect_err("#1873 the rename append surfaces the storage failure");
        negative_assert!(
            error.code.as_str() != "SESSION_WRITER_BUSY",
            "#1873 the rename storage failure is never the retryable busy code"
        );
        negative_assert!(
            String::from_utf16_lossy(error.message.units())
                .contains("Could not open session ledger lock"),
            "#1873 the rename storage error names the ledger lock, got {}",
            String::from_utf16_lossy(error.message.units())
        );
    }

    /// #1874: a recorder whose writer already closed must surface the close
    /// as an error, not answer `updated: true` with nothing on disk. The live
    /// closure's checked append is what makes the closed writer visible; the
    /// unchecked form the cold path uses would silently no-op.
    #[tokio::test]
    async fn rename_reports_a_closed_writer_as_an_error() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let storage = adapter
            .create_session_storage(SessionStorageInput {
                project_root: project,
                pilot_home: requests.options.home.clone(),
                session_id: "d7/session".into(),
                now: None,
            })
            .expect("#1874 the transcript storage opens");
        let recorder = Arc::new(Recorder::from_writer(Arc::new(storage.transcript)));
        requests
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("d7/session".into(), Arc::downgrade(&recorder));
        // A close that raced the request — the exact window fix-r2's checked
        // append exists for.
        recorder.writer().close();
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "rename_session".into(),
            crate::metadata::object([
                ("sessionKey", "d7-session".into()),
                ("value", "must-not-vanish".into()),
            ]),
        )
        .await
        .expect_err("#1874 a closed session writer is an error, not a silent rename");
        negative_assert!(
            String::from_utf16_lossy(error.message.units()).contains("the session writer closed"),
            "#1874 the rename error names the closed session writer, got {}",
            String::from_utf16_lossy(error.message.units())
        );
    }

    /// #1874: the status twin of the closed-writer gate — a closed writer
    /// must be an error, not a `recorded: true` with nothing on disk.
    #[tokio::test]
    async fn status_reports_a_closed_writer_as_an_error() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let home = requests.options.home.clone();
        let project = requests.options.project.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |k: &ProjectKey| {
                crate::session_chat_dir(&home, &k.project_root, &k.project_root.to_string_lossy())
            }),
        );
        let storage = adapter
            .create_session_storage(SessionStorageInput {
                project_root: project,
                pilot_home: requests.options.home.clone(),
                session_id: "d8/session".into(),
                now: None,
            })
            .expect("#1874 the transcript storage opens");
        let recorder = Arc::new(Recorder::from_writer(Arc::new(storage.transcript)));
        requests
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("d8/session".into(), Arc::downgrade(&recorder));
        // A close that raced the request — the exact window fix-r2's checked
        // append exists for.
        recorder.writer().close();
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "record_agent_status_message".into(),
            crate::metadata::object([
                ("sessionKey", "d8-session".into()),
                ("turnId", "must-not-vanish".into()),
                (
                    "status",
                    crate::metadata::object([("event", "note".into())]),
                ),
            ]),
        )
        .await
        .expect_err("#1874 a closed session writer is an error, not a recorded status");
        negative_assert!(
            String::from_utf16_lossy(error.message.units()).contains("the session writer closed"),
            "#1874 the status error names the closed session writer, got {}",
            String::from_utf16_lossy(error.message.units())
        );
    }

    // #1455 r1: the same gate serves resume / close / delete / rename / tag;    // a guard-level case pins the predicate once for all five callers instead
    // of relying on the single resume-level request test.
    #[test]
    fn require_session_key_keeps_lone_surrogates_and_rejects_blank() {
        let surrogate = JsString::from_utf16(vec![0xd800]);
        let accepted = object([("sessionKey", JsValue::String(surrogate))]);
        negative_assert_eq!(
            require_session_key(&accepted).is_ok(),
            true,
            "#1455 require_session_key keeps the lone-surrogate key for every caller"
        );
        let blank = object([("sessionKey", "  ".into())]);
        let err = require_session_key(&blank)
            .expect_err("#1455 require_session_key rejects the blank key for every caller");
        negative_assert_eq!(
            err.code.as_str(),
            "INVALID_SESSION_KEY",
            "#1455 require_session_key rejects the blank key for every caller"
        );
    }

    // #1455 G7: the shared `?.trim()` gate accepts a lone-surrogate key the
    // way TS does (probe4: `{"label":"thinking-lone-surrogate-key",
    // "value":{"applied":"callback"}}`); the previous `to_utf8` filter
    // rejected it with INVALID_SESSION_KEY, diverging from TS.
    #[tokio::test]
    async fn resume_session_accepts_lone_surrogate_session_key() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        let key = JsString::from_utf16(vec![0xd800]);
        let params = object([("sessionKey", JsValue::String(key.clone()))]);
        let result = r.session_dir_request("resume_session".into(), params).await;
        let value = result.expect("#1455 resume_session accepts the surrogate key");
        let echoed = value
            .get("sessionKey")
            .and_then(JsValue::as_string)
            .cloned()
            .expect("#1455 resume_session echoes the sessionKey");
        negative_assert_eq!(
            echoed.units(),
            key.units(),
            "#1455 resume_session passes the lone-surrogate sessionKey through the trim gate"
        );
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    // Gate order of the session_dir family against the baseline
    // (`scripts/bbx/scenarios/project-keys.json` and
    // `project-session-lifecycle.json`; createLocalGateway.ts:1116-1160,
    // InProcessGateway.ts:2026-2054).
    //
    // - `resume_session` is `return input` and `close_session` is
    //   `router.close(input.sessionKey)`: neither validates the key or looks
    //   at the project, so a missing or blank key and an unknown project are
    //   all answered like a valid request.
    // - `delete_session` / `rename_session` / `tag_session` ask the dialog
    //   registry first (`projectKey ?? fallbackProjectRoot`), then check the
    //   key: an unregistered project is `PROJECT_NOT_FOUND` even when the key
    //   is blank, and under a resolvable project a missing or blank key is
    //   `INVALID_SESSION_KEY`.
    const BLANK_KEY_MESSAGE: &str = "sessionKey is required.";
    const UNREGISTERED_PROJECT: &str = "/nonexistent/pdrs-2090";

    async fn call(r: &Requests, method: &str, params: JsValue) -> Result<JsValue, RequestError> {
        crate::runtime::turn::gateway::GatewayRequests::request(r, method.into(), params).await
    }

    fn gate_cases() -> Vec<(&'static str, JsValue)> {
        vec![
            ("missing key", object([])),
            ("blank key", object([("sessionKey", "   ".into())])),
            (
                "blank key + null project",
                object([("sessionKey", "   ".into()), ("projectKey", JsValue::Null)]),
            ),
            (
                "blank key + unregistered project",
                object([
                    ("sessionKey", "   ".into()),
                    ("projectKey", UNREGISTERED_PROJECT.into()),
                ]),
            ),
            (
                "valid key + unregistered project",
                object([
                    ("sessionKey", "pdrs-2090-control".into()),
                    ("projectKey", UNREGISTERED_PROJECT.into()),
                ]),
            ),
            (
                "valid key + null project",
                object([
                    ("sessionKey", "pdrs-2090-control".into()),
                    ("projectKey", JsValue::Null),
                ]),
            ),
        ]
    }

    #[tokio::test]
    async fn resume_session_returns_its_input_unvalidated() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#2090 fixture");
        for (label, params) in gate_cases() {
            let value = call(&r, "resume_session", params.clone())
                .await
                .unwrap_or_else(|error| panic!("resume_session answers {label}: {error:?}"));
            negative_assert_eq!(value, params, "resume_session echoes its input for {label}");
        }
        r.owner.shutdown().await.expect("#2090 shutdown");
        r.project.dispose().await.expect("#2090 dispose");
    }

    #[tokio::test]
    async fn close_session_answers_ok_without_validating() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#2090 fixture");
        for (label, params) in gate_cases() {
            let value = call(&r, "close_session", params)
                .await
                .unwrap_or_else(|error| panic!("close_session answers {label}: {error:?}"));
            negative_assert_eq!(
                value,
                object([("ok", JsValue::Bool(true))]),
                "close_session answers ok for {label}"
            );
        }
        r.owner.shutdown().await.expect("#2090 shutdown");
        r.project.dispose().await.expect("#2090 dispose");
    }

    async fn project_then_key_gate_order(method: &str) {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#2090 fixture");
        for (label, params) in gate_cases() {
            let result = call(&r, method, params).await;
            if label.contains("unregistered project") {
                let error = result.expect_err("an unregistered project is refused first");
                negative_assert_eq!(
                    error.code.as_str(),
                    "PROJECT_NOT_FOUND",
                    "{method} asks the dialog registry before the sessionKey gate ({label})"
                );
                negative_assert_eq!(
                    key_message(&error),
                    format!("Unknown projectKey: {UNREGISTERED_PROJECT}"),
                    "{method} echoes the key as sent ({label})"
                );
            } else if label.starts_with("valid key") {
                // A null projectKey is the default project (`??`).
                match method {
                    "delete_session" => negative_assert_eq!(
                        result
                            .expect_err("no such session in the default project")
                            .code
                            .as_str(),
                        "SESSION_NOT_FOUND",
                        "delete_session looks in the default project ({label})"
                    ),
                    _ => negative_assert_eq!(
                        result
                            .expect("rename/tag write to the default project")
                            .get("updated")
                            .and_then(JsValue::as_bool),
                        Some(true),
                        "{method} reports updated:true ({label})"
                    ),
                }
            } else {
                let error = result.expect_err("a missing or blank sessionKey is rejected");
                negative_assert_eq!(
                    error.code.as_str(),
                    "INVALID_SESSION_KEY",
                    "{method} answers the sessionKey gate under a resolvable project ({label})"
                );
                negative_assert_eq!(key_message(&error), BLANK_KEY_MESSAGE, "{method} {label}");
            }
        }
        r.owner.shutdown().await.expect("#2090 shutdown");
        r.project.dispose().await.expect("#2090 dispose");
    }

    #[tokio::test]
    async fn delete_session_blank_key_gate_order() {
        project_then_key_gate_order("delete_session").await;
    }

    /// The session keys one `snapshot_state` table holds, as UTF-8 strings.
    fn snapshot_table_keys(
        config: &crate::runtime::mcp::sdk::SdkMcpConfig,
        field: &str,
    ) -> Vec<String> {
        let rows = config
            .snapshot_state()
            .get(field)
            .cloned()
            .unwrap_or(JsValue::Undefined);
        let JsValue::Array(ref rows) = rows else {
            return Vec::new();
        };
        rows.iter()
            .map(|row| match row {
                JsValue::Array(parts) => parts
                    .first()
                    .and_then(JsValue::as_string)
                    .map(|key| String::from_utf16_lossy(key.units()))
                    .unwrap_or_default(),
                _ => String::new(),
            })
            .collect()
    }

    /// Seed the `clearSdkSessionState` halves this process owns for one key:
    /// the MCP permission tables (an override row plus its generated rule,
    /// exactly what `setMcpPermissionModeOverrideForSdk` leaves), the thinking
    /// override, and the output-style selection (`sdkSessionConfigs`'s
    /// `outputStyle` row, the other `session_overrides` half the TS clear
    /// drops with the config row).
    async fn seed_sdk_session_state(requests: &Requests, session: &str) {
        let input = object([
            ("sessionKey", session.into()),
            ("serverName", "pdrs-2392-server".into()),
            ("mode", "auto".into()),
        ]);
        requests
            .mcp
            .config()
            .set_mcp_permission_mode_override(&input)
            .await
            .expect("#2392 the seeded permission override commits");
        // The lease-acquired sync shape (the wiring layer calls this with
        // `lease.rules().ask`, ProjectRuntimeRegistry.ts:305-308): generates
        // the session's permission-rule row for the override just seeded.
        requests
            .mcp
            .config()
            .sync_permission_rules(&session.into(), &mut Vec::new());
        requests
            .session_overrides
            .set_thinking(&session.into(), object([("enabled", JsValue::Bool(true))]));
        requests
            .session_overrides
            .set_output_style(&session.into(), "pdrs-2392-style".into());
        requests
            .session_overrides
            .set_permission_mode(&session.into(), "acceptEdits".into());
    }

    /// #2392: `deleteSession` ends with `clearSdkSessionState`
    /// (createLocalGateway.ts:1160), so the deleted session's registry rows
    /// must not outlive its files — the MCP tables, the thinking override,
    /// and the output-style selection (the config row the TS clear drops
    /// carries it, ProjectRuntimeRegistry.ts:779). The permission-mode
    /// selection is the counter-scope: the TS clear never touches
    /// `sessionOverrides`, so the native delete must keep it.
    #[tokio::test]
    async fn session_delete_clears_the_sdk_session_state() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let session: JsString = "a8-session".into();
        seed_sdk_session_state(&requests, "a8-session").await;
        let config = requests.mcp.config();
        negative_assert!(
            snapshot_table_keys(config, "overrides").contains(&"a8-session".to_string()),
            "#2392 delete starts with a seeded MCP permission override"
        );
        negative_assert!(
            snapshot_table_keys(config, "rules").contains(&"a8-session".to_string()),
            "#2392 delete starts with a seeded generated permission rule"
        );
        negative_assert!(
            requests.session_overrides.thinking(&session).is_some(),
            "#2392 delete starts with a seeded thinking override"
        );
        negative_assert!(
            requests.session_overrides.output_style(&session).is_some(),
            "#2392 delete starts with a seeded output-style selection"
        );

        let result = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "delete_session".into(),
            object([("sessionKey", "a8-session".into())]),
        )
        .await;
        negative_assert_eq!(
            result.ok(),
            Some(object([("ok", JsValue::Bool(true))])),
            "#2392 delete_session answers ok after deleting the transcript"
        );
        negative_assert!(
            !snapshot_table_keys(config, "overrides").contains(&"a8-session".to_string()),
            "#2392 delete drops the session's MCP permission overrides"
        );
        negative_assert!(
            !snapshot_table_keys(config, "rules").contains(&"a8-session".to_string()),
            "#2392 delete drops the session's generated permission rules"
        );
        negative_assert!(
            requests.session_overrides.thinking(&session).is_none(),
            "#2392 delete drops the session's thinking override"
        );
        negative_assert!(
            requests.session_overrides.output_style(&session).is_none(),
            "#2392 delete drops the session's output-style selection"
        );
        negative_assert_eq!(
            requests
                .session_overrides
                .permission_mode(&session)
                .map(|mode| String::from_utf16_lossy(mode.units())),
            Some("acceptEdits".to_string()),
            "#2392 delete keeps the permission-mode selection, which the TS clear does not touch"
        );

        requests.owner.shutdown().await.expect("#2392 shutdown");
        requests.project.dispose().await.expect("#2392 dispose");
    }

    /// #2392: `clearSdkSessionState` runs only after every storage delete
    /// succeeded (createLocalGateway.ts:1150-1160); `SESSION_NOT_FOUND`
    /// leaves before any clear, so the seeded rows survive the refusal.
    #[tokio::test]
    async fn session_delete_not_found_keeps_the_sdk_session_state() {
        let (_scratch, requests) = crate::native_process::management_tests::setup();
        let session: JsString = "pdrs-2392-ghost".into();
        seed_sdk_session_state(&requests, "pdrs-2392-ghost").await;
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &requests,
            "delete_session".into(),
            object([("sessionKey", "pdrs-2392-ghost".into())]),
        )
        .await
        .expect_err("#2392 delete_session refuses the session without a transcript");
        negative_assert_eq!(
            error.code.as_str(),
            "SESSION_NOT_FOUND",
            "#2392 the missing transcript answers SESSION_NOT_FOUND"
        );
        let config = requests.mcp.config();
        negative_assert!(
            snapshot_table_keys(config, "overrides").contains(&"pdrs-2392-ghost".to_string()),
            "#2392 the refusal keeps the session's MCP permission overrides"
        );
        negative_assert!(
            snapshot_table_keys(config, "rules").contains(&"pdrs-2392-ghost".to_string()),
            "#2392 the refusal keeps the session's generated permission rules"
        );
        negative_assert!(
            requests.session_overrides.thinking(&session).is_some(),
            "#2392 the refusal keeps the session's thinking override"
        );
        negative_assert!(
            requests.session_overrides.output_style(&session).is_some(),
            "#2392 the refusal keeps the session's output-style selection"
        );

        requests.owner.shutdown().await.expect("#2392 shutdown");
        requests.project.dispose().await.expect("#2392 dispose");
    }

    #[tokio::test]
    async fn rename_session_blank_key_gate_order() {
        project_then_key_gate_order("rename_session").await;
    }

    #[tokio::test]
    async fn tag_session_blank_key_gate_order() {
        project_then_key_gate_order("tag_session").await;
    }

    // #1604: the sessionKey gate is the family-shared `?.trim()` guard, so
    // every method that goes through `require_session_key` answers a
    // non-string the way the TS optional chain does, and treats JS WhiteSpace
    // (U+FEFF included) as blank. `session_record_status` /
    // `session_new` keep their own `must be a string.` gate on purpose (TS has
    // no `?.trim()` there — A7.8-INTERACTION-NONSTRING-OWNER-001-R1).
    fn require_key(value: JsValue) -> Result<JsString, RequestError> {
        require_session_key(&object([("sessionKey", value)]))
    }

    fn key_message(error: &RequestError) -> String {
        error
            .message
            .to_utf8()
            .expect("#1604 the gate's message is UTF-8")
    }

    #[test]
    fn a_non_string_session_key_throws_the_optional_chain_type_error() {
        let error = require_key(JsValue::Number(7.0))
            .expect_err("#1604 a number is not a trimmable string");
        assert_eq!("gateway_request_failed", error.code);
        assert_eq!(
            "input.sessionKey?.trim is not a function",
            key_message(&error)
        );
    }

    // #1604: with a null params object TS throws reading `sessionKey` off
    // null, so the guard must not fold that into the domain error either.
    #[test]
    fn a_null_params_object_reports_the_optional_chain_read_error() {
        let error =
            require_session_key(&JsValue::Null).expect_err("#1604 null params cannot be read");
        assert_eq!("gateway_request_failed", error.code);
        assert_eq!(
            "Cannot read properties of null (reading 'sessionKey')",
            key_message(&error)
        );
    }

    #[test]
    fn a_bom_only_session_key_is_blank() {
        let error = require_key(JsValue::from("\u{feff}"))
            .expect_err("#1604 a BOM-only key is blank after JS trim");
        assert_eq!("INVALID_SESSION_KEY", error.code);
        assert_eq!("sessionKey is required.", key_message(&error));
    }

    #[test]
    fn a_non_blank_session_key_passes_the_gate() {
        let key = require_key(JsValue::from("a8/session"))
            .expect("#1604 a non-blank string passes the gate");
        assert_eq!("a8/session", key.to_utf8().expect("#1604 key is UTF-8"));
    }

    // #1604: `close_project_sessions` gates `projectKey` with the same
    // `?.trim()` shape (A7-W5-NATIVE-SESSION-CONFIG-CAPABILITY-001-R2 lists
    // it as this issue's second item), so a non-string must surface the
    // optional-chain TypeError instead of the `projectKey is required.`
    // domain error. The gate runs before any owner call, so the fixture is
    // only there to reach the request entry point.
    #[tokio::test]
    async fn close_project_sessions_non_string_project_key_throws_the_type_error() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        let params = object([("projectKey", JsValue::Number(7.0))]);
        let error = r
            .session_dir_request("close_project_sessions".into(), params)
            .await
            .expect_err("#1604 a number is not a trimmable projectKey");
        assert_eq!("gateway_request_failed", error.code);
        assert_eq!(
            "input.projectKey?.trim is not a function",
            key_message(&error)
        );
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    // #1604: the other half of the same gate. Missing and blank (JS trim,
    // U+FEFF included) keys keep the plain `Error` shape TS produces —
    // `gateway_request_failed` and `projectKey is required.` — and neither
    // reaches an owner call.
    #[tokio::test]
    async fn close_project_sessions_blank_project_key_is_the_domain_error() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        for (label, value) in [
            ("blank", JsValue::from("  ")),
            ("bom-only", JsValue::from("\u{feff}")),
        ] {
            let error = r
                .session_dir_request(
                    "close_project_sessions".into(),
                    object([("projectKey", value)]),
                )
                .await
                .expect_err("#1604 a projectKey that cannot name a directory is refused");
            assert_eq!(
                "gateway_request_failed", error.code,
                "#1604 the {label} projectKey keeps the domain code"
            );
            assert_eq!(
                "projectKey is required.",
                key_message(&error),
                "#1604 the {label} projectKey keeps the domain message"
            );
        }
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    // #2083: a lone-surrogate `projectKey` is a non-blank key, and the TS
    // baseline runs it to completion (`closeProjectSessions` answers
    // `{sessionKeys: []}` on both branches — real-entry probe
    // `_tmp/20261107-issue-2083/worker-r1/probe.out`). The `?.trim()` shape is
    // the method's only gate (InProcessGateway.ts:2036-2047), so the key
    // reaches the owner and names a project with no sessions.
    #[tokio::test]
    async fn close_project_sessions_surrogate_project_key_runs_to_completion() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        for (label, resume) in [("close", false), ("resume", true)] {
            let value = crate::runtime::turn::gateway::GatewayRequests::request(
                &r,
                "close_project_sessions".into(),
                object([
                    (
                        "projectKey",
                        JsValue::String(JsString::from_utf16(vec![0xd800])),
                    ),
                    ("resume", JsValue::Bool(resume)),
                ]),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("#2083 the surrogate projectKey runs to completion ({label}): {error:?}")
            });
            assert_eq!(
                value,
                object([("sessionKeys", JsValue::Array(vec![]))]),
                "#2083 the surrogate project has no sessions ({label} branch)"
            );
        }
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    // #2564: request-level `projectKey: null` for the three family members
    // whose subject key is the project itself. The admission gate reads the
    // key nullish (#2272), so a null reaches each method's own read, and the
    // TS baselines differ per method: `closeProjectSessions` gates with
    // `!input.projectKey?.trim()` (InProcessGateway.ts:2037) so a null is the
    // missing-key domain error; `listSessions` resolves
    // `input.projectKey ?? fallbackProjectRoot` (ProjectRuntimeRegistry.ts:1542)
    // so it lists the configured project; `newSession` reads
    // `input.projectKey ? \`project=${…}:\` : ""` (InProcessGateway.ts:2029)
    // so the null omits the prefix. Node v22.22.0 end-to-end probe:
    // `_tmp/20261026-issue-2562/ts-probe/probe-submit.out`.
    #[tokio::test]
    async fn close_project_sessions_null_project_key_is_the_domain_error() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        let error = crate::runtime::turn::gateway::GatewayRequests::request(
            &r,
            "close_project_sessions".into(),
            object([("projectKey", JsValue::Null)]),
        )
        .await
        .expect_err("#2564 a null projectKey is refused by the close_project_sessions gate");
        negative_assert_eq!(
            error.code.as_str(),
            "gateway_request_failed",
            "#2564 a null projectKey keeps the domain code"
        );
        negative_assert_eq!(
            key_message(&error),
            "projectKey is required.",
            "#2564 a null projectKey keeps the domain message"
        );
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    #[tokio::test]
    async fn list_sessions_null_project_key_lists_the_configured_project() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        let value = crate::runtime::turn::gateway::GatewayRequests::request(
            &r,
            "list_sessions".into(),
            object([("projectKey", JsValue::Null)]),
        )
        .await
        .expect("#2564 a null projectKey lists the configured project");
        negative_assert!(
            matches!(value.get("sessions"), Some(JsValue::Array(_))),
            "#2564 a null projectKey falls back to the configured project's catalog"
        );
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    #[tokio::test]
    async fn new_session_null_project_key_omits_the_project_prefix() {
        let (_s, r) = std::panic::catch_unwind(crate::native_process::management_tests::setup)
            .expect("#1310 fixture");
        let key_of = |params| {
            crate::runtime::turn::gateway::GatewayRequests::request(
                &r,
                "new_session".into(),
                params,
            )
        };
        let null_key = key_of(object([
            ("channelKey", "pdrs-2564".into()),
            ("projectKey", JsValue::Null),
        ]))
        .await
        .expect("#2564 a null projectKey generates a session key")
        .get("sessionKey")
        .and_then(JsValue::as_string)
        .cloned()
        .expect("#2564 new_session answers a sessionKey");
        negative_assert!(
            String::from_utf16_lossy(null_key.units()).starts_with("pdrs-2564:s_"),
            "#2564 a null projectKey omits the project= prefix"
        );
        // Control: a served string is truthy on both sides, so the prefix
        // appears and the null above is proven to be the nullish read.
        let served_key = key_of(object([
            ("channelKey", "pdrs-2564".into()),
            (
                "projectKey",
                JsValue::String(r.options.project.to_string_lossy().into_owned().into()),
            ),
        ]))
        .await
        .expect("#2564 the configured project string generates a session key")
        .get("sessionKey")
        .and_then(JsValue::as_string)
        .cloned()
        .expect("#2564 new_session answers a sessionKey for the served project");
        negative_assert!(
            String::from_utf16_lossy(served_key.units()).starts_with("pdrs-2564:project="),
            "#2564 a served project string keeps the project= prefix"
        );
        r.owner.shutdown().await.expect("#1310 shutdown");
        r.project.dispose().await.expect("#1310 dispose");
    }

    /// Removes a broken ledger lock planted outside the scratch directory.
    struct RemoveDir(std::path::PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }
}
