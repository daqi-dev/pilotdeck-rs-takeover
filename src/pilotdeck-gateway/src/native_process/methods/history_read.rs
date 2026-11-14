//! The durable history reader behind `read_session_messages` and
//! `read_subagent_messages`.
//!
//! Baseline: `GatewaySessionHistoryBundle.readSessionMessages` /
//! `.readSubagentMessages` (src/cli/GatewaySessionHistoryBundle.ts:122-150)
//! calling `readWebSessionMessages` / `readSubagentWebMessages`
//! (src/web/server/readSessionMessages.ts:69-166, :262-346) with the native
//! JSONL storage provider. This module resolves paths and reads files; the
//! projection onto web messages is `crate::storage::web_*_messages`.
//!
//! Neither method resolves `projectKey` through the dialog registry: the raw
//! key (or the fallback project root) names the storage directory, so an
//! unregistered or missing project reads as an empty history.
use super::super::requests::Requests;
use crate::{
    runtime::{
        connection::RequestError,
        manager::{
            GatewaySessionCatalogConsumer, SessionCatalogListInput, TranscriptSessionCatalog,
        },
        turn::read::{SessionHistoryReader, TurnReadError},
    },
    storage::{
        AsyncTranscriptAdapter, JsonlTranscriptStore, ProjectKey, WebReadOptions,
        is_background_task_input, read_transcript, sanitize_session_id, session_info_from_entries,
        subagent_entries, web_session_messages, web_subagent_messages,
    },
};
use pilotdeck_protocol::{JsString, JsValue};
use std::{
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

pub(super) fn request_error(error: TurnReadError) -> RequestError {
    RequestError {
        code: error
            .code
            .unwrap_or_else(|| "gateway_request_failed".into()),
        message: error.message,
        details: None,
        validation: None,
    }
}

fn thrown(name: &str, code: Option<&str>, message: impl Into<JsString>) -> TurnReadError {
    TurnReadError {
        code: code.map(str::to_string),
        name: name.to_string(),
        message: message.into(),
    }
}
fn plain(message: impl Into<JsString>) -> TurnReadError {
    thrown("Error", None, message)
}

/// A V8 TypeError from reading a hand-edited transcript shape.
fn type_error(message: String) -> TurnReadError {
    thrown("TypeError", None, message)
}

static UNDEFINED: JsValue = JsValue::Undefined;
fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    value.get(key).unwrap_or(&UNDEFINED)
}
fn nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Undefined | JsValue::Null)
}

/// `sanitizeSessionIdForPath(value)` (ProjectSessionStorage.ts:239-245) for an
/// untyped wire value: anything but a string fails on `.replace`.
fn sanitize(value: &JsValue) -> Result<String, TurnReadError> {
    match value {
        JsValue::String(id) => Ok(sanitize_session_id(id)),
        JsValue::Undefined => Err(thrown(
            "TypeError",
            None,
            "Cannot read properties of undefined (reading 'replace')",
        )),
        JsValue::Null => Err(thrown(
            "TypeError",
            None,
            "Cannot read properties of null (reading 'replace')",
        )),
        _ => Err(thrown(
            "TypeError",
            None,
            "sessionId.replace is not a function",
        )),
    }
}

/// `path.resolve(base, relative)` for an absolute `base`.
fn lexical_resolve(base: &Path, relative: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in base.components().chain(Path::new(relative).components()) {
        match component {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// `resolveRelativeTranscriptPath` (readSessionMessages.ts:372-385).
fn resolve_relative_transcript(
    path: &JsValue,
    base: &Path,
    allowed_root: &Path,
) -> Result<PathBuf, TurnReadError> {
    let text = path
        .as_string()
        .map(|path| String::from_utf16_lossy(path.units()))
        .unwrap_or_default();
    if text.is_empty() || text.starts_with('/') {
        return Err(plain("relativeTranscriptPath must be a relative path."));
    }
    let candidate = lexical_resolve(base, &text);
    let inside = candidate
        .strip_prefix(lexical_resolve(allowed_root, ""))
        .is_ok_and(|rest| {
            let rest = rest.to_string_lossy();
            !rest.is_empty() && !rest.starts_with("..")
        });
    if !inside || !candidate.to_string_lossy().ends_with(".jsonl") {
        return Err(plain(
            "relativeTranscriptPath points outside the project transcript directory.",
        ));
    }
    Ok(candidate)
}

fn now_iso() -> String {
    pilotdeck_transport::system_clock::format_epoch_iso(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    )
}

/// The storage directory and the root spelling a request reads from.
struct Location {
    /// `input.projectKey ?? options.projectRoot`, as given.
    root: PathBuf,
    chat_dir: PathBuf,
    /// `resolve(root) !== resolve(pilotHome)`.
    outside_home: bool,
}

#[derive(Clone)]
pub(super) struct HistoryReader {
    requests: Requests,
}

impl HistoryReader {
    pub(super) fn new(requests: &Requests) -> Self {
        Self {
            requests: requests.clone(),
        }
    }

    fn locate(&self, input: &JsValue) -> Result<Location, TurnReadError> {
        let options = &self.requests.options;
        let key = super::super::projects::requested_key(input).map_err(|other| {
            match crate::metadata::argument_received(other) {
                crate::metadata::ArgumentReceived::Text(received) => thrown(
                    "TypeError",
                    Some("ERR_INVALID_ARG_TYPE"),
                    crate::metadata::invalid_arg_type_message("paths[0]", &received),
                ),
                crate::metadata::ArgumentReceived::Throws { message, .. } => plain(message),
            }
        })?;
        let root = match key {
            Some(key) => PathBuf::from(String::from_utf16_lossy(key.units())),
            None => options.project.clone(),
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Ok(Location {
            chat_dir: crate::session_chat_dir(&options.home, &cwd, &root.to_string_lossy()),
            outside_home: options.project_root_for(key) != options.home,
            root,
        })
    }

    /// `maxContextTokens` / `maxOutputTokens` of the default runtime's agent
    /// config (createLocalGateway.ts:905-906), as configured.
    fn agent_limits(&self) -> (Option<f64>, Option<f64>) {
        let Ok(runtime) = self.requests.project.resolve(None) else {
            return (None, None);
        };
        let Some(resources) = runtime
            .resources()
            .get::<crate::runtime::project::ProjectResources>()
        else {
            return (None, None);
        };
        let snapshot = resources.context().snapshot();
        let agent = field(field(&snapshot, "config"), "agent");
        (
            field(agent, "maxContextTokens").as_number(),
            field(agent, "maxOutputTokens").as_number(),
        )
    }

    /// `locateSession` (readSessionMessages.ts:406-438): the catalog row for
    /// this session, else what the entries themselves say. A catalog failure
    /// falls back the same way because a storage provider is configured.
    fn locate_session(
        &self,
        location: &Location,
        key: &JsValue,
        entries: &[JsValue],
    ) -> Result<JsValue, TurnReadError> {
        let home = self.requests.options.home.clone();
        let chat_dir = location.chat_dir.clone();
        let adapter = AsyncTranscriptAdapter::new(
            Arc::new(JsonlTranscriptStore::new()),
            Arc::new(move |_: &ProjectKey| chat_dir.clone()),
        );
        let catalog = Arc::new(TranscriptSessionCatalog::new(Arc::new(adapter)));
        let listed = GatewaySessionCatalogConsumer::new(catalog).list(&SessionCatalogListInput {
            project_root: location.root.clone(),
            pilot_home: home,
            limit: None,
            offset: None,
            include_internal: false,
        });
        let safe: Option<JsValue> = key
            .as_string()
            .map(|key| sanitize_session_id(key).as_str().into());
        let found = listed.ok().and_then(|sessions| {
            sessions.into_iter().find(|session| {
                let id = field(session, "sessionId");
                id == key || Some(id) == safe.as_ref()
            })
        });
        match found {
            Some(session) => Ok(session),
            None => session_info_from_entries(
                key,
                entries,
                &location.root.to_string_lossy().as_ref().into(),
            )
            .map_err(type_error),
        }
    }

    fn read_session(&self, input: &JsValue) -> Result<JsValue, TurnReadError> {
        let location = self.locate(input)?;
        let session_key = field(input, "sessionKey");
        let background = is_background_task_input(input);
        let path = if background {
            resolve_relative_transcript(
                field(input, "relativeTranscriptPath"),
                &location.chat_dir,
                &location.chat_dir,
            )?
        } else {
            location
                .chat_dir
                .join(format!("{}.jsonl", sanitize(session_key)?))
        };
        let entries = read_transcript(&path)
            .map_err(|error| plain(error.to_string()))?
            .entries;
        let info = if background {
            JsValue::Undefined
        } else {
            self.locate_session(&location, session_key, &entries)?
        };
        let (max_context_tokens, max_output_tokens) = self.agent_limits();
        web_session_messages(
            input,
            &entries,
            &info,
            &WebReadOptions {
                max_context_tokens,
                max_output_tokens,
                now_iso: now_iso(),
                show_file_artifacts: location.outside_home,
            },
        )
        .map_err(type_error)
    }

    fn read_sidechain(&self, input: &JsValue) -> Result<JsValue, TurnReadError> {
        let location = self.locate(input)?;
        let background = is_background_task_input(input);
        let session_file = |id: &JsValue| -> Result<PathBuf, TurnReadError> {
            Ok(location.chat_dir.join(format!("{}.jsonl", sanitize(id)?)))
        };
        let parent_path = if background {
            resolve_relative_transcript(
                field(input, "relativeTranscriptPath"),
                &location.chat_dir,
                &location.chat_dir,
            )?
        } else {
            session_file(field(input, "sessionKey"))?
        };
        let parent_id = match field(input, "parentSessionId") {
            id if nullish(id) => field(input, "sessionKey"),
            id => id,
        };
        let read = |path: &Path| -> Result<Vec<JsValue>, TurnReadError> {
            Ok(read_transcript(path)
                .map_err(|error| plain(error.to_string()))?
                .entries)
        };
        let parent_entries = if background {
            read(&parent_path)?
        } else {
            read(&session_file(parent_id)?)?
        };
        let subagent_id = field(input, "subagentId");
        let (started, _) = subagent_entries(&parent_entries);
        let Some(reference) = started
            .iter()
            .find(|entry| field(entry, "subagentId") == subagent_id)
        else {
            return Ok(web_subagent_messages(input, &[], &now_iso()));
        };
        let relative = field(reference, "transcriptRelativePath");
        let has = |value: &JsValue| value.as_string().is_some_and(|text| !text.is_empty());
        let entries = if !background && has(field(reference, "subagentSessionId")) {
            read(
                &location
                    .chat_dir
                    .join(sanitize(parent_id)?)
                    .join("subagents")
                    .join(format!("{}.jsonl", sanitize(subagent_id)?)),
            )?
        } else if has(relative) {
            read(&resolve_relative_transcript(
                relative,
                parent_path.parent().unwrap_or(Path::new("/")),
                &location.chat_dir,
            )?)?
        } else {
            Vec::new()
        };
        Ok(web_subagent_messages(input, &entries, &now_iso()))
    }

    pub(super) async fn read_subagent(self, input: JsValue) -> Result<JsValue, TurnReadError> {
        tokio::task::spawn_blocking(move || self.read_sidechain(&input))
            .await
            .map_err(|error| plain(error.to_string()))?
    }
}

impl SessionHistoryReader for HistoryReader {
    fn read(
        &self,
        input: &JsValue,
    ) -> Pin<Box<dyn Future<Output = Result<JsValue, TurnReadError>> + Send>> {
        let reader = self.clone();
        let input = input.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || reader.read_session(&input))
                .await
                .map_err(|error| plain(error.to_string()))?
        })
    }
}
