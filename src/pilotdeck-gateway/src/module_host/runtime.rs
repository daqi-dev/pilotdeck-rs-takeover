//! The host capability provider `pilotdeck server` resolves for every
//! principal: the baseline's `getPublicHostCapabilities`
//! (createLocalGateway.ts:1524-1556), i.e. `createRuntimeHostCapabilityProvider`
//! (publicHostRuntimeAdapter.ts:94-167) over the fallback project's runtime.
//!
//! Operations, in the adapter's `METHOD_PORTS` order: `list_general_skills`
//! (absent while `modules.skills.enabled === false`), `list_model_catalog`,
//! `model_prepare`, `model_stream` (activeRuntimeModelPorts.ts), `file_parse`
//! (activeRuntimeTextParsing.ts) and `read_sop_authority`
//! (nativeSopAuthority.ts).
//!
//! Known gaps, reported rather than imitated:
//! - `file_parse` of a `.pdf`: the baseline extracts text with mupdf, which
//!   has no Rust counterpart here; a PDF is answered with the extractor's own
//!   unsupported-type error after the same input checks.
//! - `read_sop_authority`: the StaffDeck admission authentication
//!   (`authenticateNativeStaffDeckAdmission`) and the SOP state read are
//!   remote calls this host does not make, so a request that passes the
//!   owner/project/session checks ends in `SOP_AUTHORITY_ADMISSION_MISMATCH`,
//!   which is also the baseline's outcome whenever StaffDeck is not
//!   configured or not reachable.
//! - `STAFFDECK_PROJECT_BINDING_ENABLED=true`: the per-principal project
//!   binding (`projectForStaffDeckPrincipal`) is not resolved; the fallback
//!   project serves every principal.
use super::{
    HostBody, HostEvent, HostFailure, HostResponse, PublicHostPrincipal, PublicHostProvider,
    PublicHostResolver, js_truthy, object_value, off_worker, string_value,
};
use crate::{
    config::Env,
    metadata::{Metadata, NativeFiles},
    runtime::{
        manager::{SessionCatalogListInput, SessionCatalogPort, TranscriptSessionCatalog},
        model::{
            http::{ModelStreamError, StreamOptions, StreamOutput},
            provider_registry::ModelInvocationProviderRegistry,
            router::{AbortFlag, RouterModelInvocation},
        },
        project::{
            ProjectResources, ProjectRuntimeRegistry, Resource, RuntimeLease, model::ModelResources,
        },
    },
    skills::{SkillManager, SkillManagerOptions},
};
use base64::Engine as _;
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use tokio::sync::mpsc;

/// What the provider reads: the pilot home (`pilotHome`), the fallback
/// project root (`runtime.projectRoot` for an unbound principal), the bundled
/// skills directory, the process environment and the project runtimes.
pub(crate) struct RuntimeHost {
    pub home: PathBuf,
    pub project: PathBuf,
    pub builtin_skills_root: Option<PathBuf>,
    pub env: Env,
    pub registry: Arc<ProjectRuntimeRegistry>,
}

pub(crate) struct RuntimeHostResolver(pub Arc<RuntimeHost>);

impl PublicHostResolver for RuntimeHostResolver {
    fn resolve(&self, _principal: &PublicHostPrincipal) -> Arc<dyn PublicHostProvider> {
        Arc::clone(&self.0) as Arc<dyn PublicHostProvider>
    }
}

/// A thrown value as the adapter's catch sees it
/// (publicHostRuntimeAdapter.ts:157-164).
enum Thrown {
    /// `ModelRequestError`: 400 `{code, message, details}`.
    Request {
        code: JsString,
        message: JsString,
        details: Option<JsValue>,
    },
    /// `ModelProviderError`: `error.status ?? 502` with the canonical error.
    Provider(JsObject),
    /// Any other error: `status` when numeric, else 502; `code` when a
    /// string, else the error name.
    Coded {
        status: Option<u16>,
        code: JsString,
        message: JsString,
    },
}

fn request_error(code: &str, message: impl Into<JsString>) -> Thrown {
    Thrown::Request {
        code: code.into(),
        message: message.into(),
        details: None,
    }
}

/// `throw new Error(message)` — no status, no code: the name answers.
fn plain_error(message: impl Into<JsString>) -> Thrown {
    Thrown::Coded {
        status: None,
        code: "Error".into(),
        message: message.into(),
    }
}

fn coded(status: u16, code: &str, message: &str) -> Thrown {
    Thrown::Coded {
        status: Some(status),
        code: code.into(),
        message: message.into(),
    }
}

fn json(status: u16, body: JsValue) -> HostResponse {
    HostResponse {
        status,
        headers: Vec::new(),
        body: HostBody::Value(body),
        raw_body: None,
    }
}

impl Thrown {
    fn respond(self) -> HostResponse {
        match self {
            Thrown::Request {
                code,
                message,
                details,
            } => json(
                400,
                object_value(&[
                    ("code", JsValue::String(code)),
                    ("message", JsValue::String(message)),
                    ("details", details.unwrap_or(JsValue::Undefined)),
                ]),
            ),
            Thrown::Provider(canonical) => {
                let status = canonical
                    .get("status")
                    .and_then(JsValue::as_number)
                    .filter(|status| status.fract() == 0.0 && (100.0..600.0).contains(status))
                    .map_or(502, |status| status as u16);
                json(status, JsValue::Object(canonical))
            }
            Thrown::Coded {
                status,
                code,
                message,
            } => json(
                status.unwrap_or(502),
                object_value(&[
                    ("code", JsValue::String(code)),
                    ("message", JsValue::String(message)),
                ]),
            ),
        }
    }
}

fn stream_error(error: ModelStreamError) -> Thrown {
    match error {
        ModelStreamError::Request {
            code,
            message,
            details,
        } => Thrown::Request {
            code,
            message,
            details: details.map(|details| *details),
        },
        ModelStreamError::Provider(canonical) => Thrown::Provider(canonical),
        other => Thrown::Coded {
            status: None,
            code: other.name(),
            message: other.message(),
        },
    }
}

/// `value && typeof value === "object" && !Array.isArray(value)`.
fn record(value: &JsValue) -> Option<&JsObject> {
    match value {
        JsValue::Object(object) => Some(object),
        _ => None,
    }
}

fn field<'a>(object: &'a JsObject, key: &str) -> &'a JsValue {
    static UNDEFINED: JsValue = JsValue::Undefined;
    object.get(key).unwrap_or(&UNDEFINED)
}

fn nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Undefined | JsValue::Null)
}

fn trimmed_empty(text: &JsString) -> bool {
    crate::model_catalog::trim(text).is_empty()
}

/// A non-empty (after trim) string.
fn non_empty_string(value: &JsValue) -> Option<&JsString> {
    value.as_string().filter(|text| !trimmed_empty(text))
}

/// `Number.isSafeInteger(value)` on any JS value.
fn safe_integer(value: &JsValue) -> Option<f64> {
    value
        .as_number()
        .filter(|number| number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0)
}

/// `requiredString` of the adapter: a plain `Error` (502, code `Error`).
fn required_string<'a>(value: &'a JsValue, name: &str) -> Result<&'a JsString, Thrown> {
    non_empty_string(value)
        .ok_or_else(|| plain_error(format!("{name} must be a non-empty string.")))
}

impl RuntimeHost {
    fn lease(&self) -> Result<RuntimeLease, Thrown> {
        self.registry
            .acquire(None)
            .map_err(|error| plain_error(error.to_string()))
    }

    /// `isDisabledModuleBinding(runtime.snapshot.config.modules?.skills)`.
    fn skills_disabled(&self) -> bool {
        let Ok(lease) = self.registry.acquire(None) else {
            return false;
        };
        let Some(resources) = lease.runtime().resources().get::<ProjectResources>() else {
            return false;
        };
        let snapshot = resources.context().snapshot();
        snapshot
            .get("config")
            .and_then(|config| config.get("modules"))
            .and_then(|modules| modules.get("skills"))
            .and_then(|skills| skills.get("enabled"))
            == Some(&JsValue::Bool(false))
    }

    fn metadata(&self) -> Metadata<NativeFiles> {
        Metadata {
            home: self.home.clone(),
            cwd: PathBuf::new(),
            files: NativeFiles,
        }
    }

    /// `sessionModels.modelCatalogList({ includeAuto: false, projectKey })`.
    async fn catalog(&self) -> Result<JsValue, Thrown> {
        let metadata = self.metadata();
        let env = self.env.clone();
        let params = object_value(&[("includeAuto", JsValue::Bool(false))]);
        match off_worker(move || crate::model_catalog::execute(&metadata, &params, &env)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(Thrown::Coded {
                status: None,
                code: if error.code.is_empty() {
                    error.name.into()
                } else {
                    error.code.as_str().into()
                },
                message: error.message,
            }),
            Err(join) => Err(plain_error(join)),
        }
    }

    async fn list_general_skills(&self) -> Result<JsValue, Thrown> {
        let mut options = SkillManagerOptions::new(self.home.to_string_lossy().into_owned());
        options.env = Some(self.env.clone());
        options.home_dir = Some(self.home.to_string_lossy().into_owned());
        options.builtin_skills_root = self
            .builtin_skills_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned());
        let project = self.project.to_string_lossy().into_owned();
        let skill_error = |error: crate::skills::SkillError| Thrown::Coded {
            status: None,
            code: if error.code.is_empty() {
                error.name.into()
            } else {
                error.code
            },
            message: error.message,
        };
        let listed = off_worker(move || {
            let manager = SkillManager::new(options)?;
            manager.list(&object_value(&[(
                "projectKey",
                JsValue::String(project.as_str().into()),
            )]))
        })
        .await
        .map_err(plain_error)?
        .map_err(skill_error)?;
        // `{ ...result, items: result.items.map(item => ({ ...item, id: item.slug })) }`
        let mut result = listed.as_object().cloned().unwrap_or_default();
        let items = match result.get("items") {
            Some(JsValue::Array(items)) => JsValue::Array(
                items
                    .iter()
                    .map(|item| {
                        let mut item = item.as_object().cloned().unwrap_or_default();
                        let slug = item.get("slug").cloned().unwrap_or(JsValue::Undefined);
                        item.insert("id".into(), slug);
                        JsValue::Object(item)
                    })
                    .collect(),
            ),
            other => other.cloned().unwrap_or(JsValue::Undefined),
        };
        result.insert("items".into(), items.clone());
        // `{ data: result.items ?? result.data ?? [], next_cursor:
        // result.nextCursor ?? result.next_cursor ?? null, ...result }`
        let first = |keys: &[&str], default: JsValue| {
            keys.iter()
                .map(|key| field(&result, key))
                .find(|value| !nullish(value))
                .cloned()
                .unwrap_or(default)
        };
        let mut body = JsObject::new();
        body.insert(
            "data".into(),
            first(&["items", "data"], JsValue::Array(vec![])),
        );
        body.insert(
            "next_cursor".into(),
            first(&["nextCursor", "next_cursor"], JsValue::Null),
        );
        for (key, value) in result.entries() {
            body.insert(key.clone(), value.clone());
        }
        Ok(JsValue::Object(body))
    }

    async fn list_model_catalog(&self) -> Result<JsValue, Thrown> {
        let result = self.catalog().await?;
        let default = result
            .get("defaultSelection")
            .cloned()
            .unwrap_or(JsValue::Undefined);
        let items = match result.get("items") {
            Some(JsValue::Array(items)) => items
                .iter()
                .map(|item| {
                    let mut entry = item.as_object().cloned().unwrap_or_default();
                    let get = |key: &str| item.get(key).cloned().unwrap_or(JsValue::Undefined);
                    let is_default = !nullish(&default)
                        && default.get("provider") == item.get("provider")
                        && default.get("model") == item.get("model");
                    entry.insert("name".into(), get("displayName"));
                    entry.insert("enabled".into(), get("available"));
                    entry.insert("is_default".into(), JsValue::Bool(is_default));
                    JsValue::Object(entry)
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(object_value(&[
            ("data", JsValue::Array(items)),
            ("defaultSelection", default),
        ]))
    }

    /// `prepare` of activeRuntimeModelPorts.ts:89-140.
    async fn prepare(&self, input: &JsObject) -> Result<Prepared, Thrown> {
        let request = record(field(input, "request"))
            .ok_or_else(|| request_error("invalid_request", "request must be an object"))?;
        let budget = match field(input, "budget") {
            JsValue::Undefined => JsObject::new(),
            value => record(value)
                .cloned()
                .ok_or_else(|| request_error("invalid_request", "request must be an object"))?,
        };
        let requested = match field(input, "modelId") {
            value if nullish(value) => field(request, "model"),
            value => value,
        };
        let requested = non_empty_string(requested)
            .cloned()
            .ok_or_else(|| request_error("invalid_request", "modelId is required"))?;
        let request_id = non_empty_string(field(input, "requestId"))
            .cloned()
            .ok_or_else(|| request_error("invalid_request", "requestId is required"))?;
        let catalog = self.catalog().await?;
        let matches: Vec<&JsValue> = match catalog.get("items") {
            Some(JsValue::Array(items)) => items
                .iter()
                .filter(|item| {
                    item.get("id").and_then(JsValue::as_string) == Some(&requested)
                        && item.get("available").is_some_and(js_truthy)
                })
                .collect(),
            _ => Vec::new(),
        };
        if matches.len() != 1 {
            return Err(request_error(
                if matches.is_empty() {
                    "model_not_available"
                } else {
                    "model_id_ambiguous"
                },
                format!(
                    "Selected model {} is not a single available catalog entry",
                    String::from_utf16_lossy(requested.units())
                ),
            ));
        }
        let selected = matches[0];
        let text = |key: &str| {
            selected
                .get(key)
                .and_then(JsValue::as_string)
                .cloned()
                .unwrap_or_default()
        };
        let (selected_id, provider, model) = (text("id"), text("provider"), text("model"));
        let request_model = field(request, "model");
        if !request_model.is_undefined()
            && request_model.as_string() != Some(&model)
            && request_model.as_string() != Some(&selected_id)
        {
            return Err(request_error(
                "model_selection_mismatch",
                "request model differs from selected model",
            ));
        }
        let request_provider = field(request, "provider");
        if !request_provider.is_undefined() && request_provider.as_string() != Some(&provider) {
            return Err(request_error(
                "model_selection_mismatch",
                "provider differs from selected model",
            ));
        }
        let positive = |key: &str| -> Result<Option<f64>, Thrown> {
            let value = field(&budget, key);
            if value.is_undefined() {
                return Ok(None);
            }
            match safe_integer(value) {
                Some(number) if number > 0.0 => Ok(Some(number)),
                _ => Err(request_error(
                    "invalid_budget",
                    format!("{key} must be a positive integer"),
                )),
            }
        };
        let max_output_tokens = positive("maxOutputTokens")?;
        let timeout_ms = positive("timeoutMs")?;
        if positive("maxInputTokens")?.is_some() {
            return Err(request_error(
                "unsupported_budget",
                "maxInputTokens needs measured prompt tokens",
            ));
        }
        let lease = self.lease()?;
        let (registry, model_config) = model_resources(&lease)?;
        let capabilities = registry
            .get_capabilities(&provider, &model)
            .map_err(|error| Thrown::Request {
                code: error.code.unwrap_or_default(),
                message: error.message,
                details: None,
            })?;
        let request_max = field(request, "maxOutputTokens").as_number();
        let output = max_output_tokens.or(request_max);
        if let Some(output) = output {
            let cap = capabilities
                .get("maxOutputTokens")
                .and_then(JsValue::as_number);
            let safe = safe_integer(&JsValue::Number(output)).is_some();
            if !safe || output <= 0.0 || cap.is_some_and(|cap| output > cap) {
                return Err(request_error(
                    "invalid_budget",
                    "maxOutputTokens exceeds selected model limit",
                ));
            }
        }
        if let (Some(request_max), Some(budget_max)) = (request_max, max_output_tokens)
            && request_max > budget_max
        {
            return Err(request_error(
                "invalid_budget",
                "request maxOutputTokens exceeds budget",
            ));
        }
        let mut canonical = request.clone();
        canonical.insert("provider".into(), JsValue::String(provider.clone()));
        canonical.insert("model".into(), JsValue::String(model.clone()));
        if let Some(output) = output {
            canonical.insert("maxOutputTokens".into(), JsValue::Number(output));
        }
        validate_public_messages(field(&canonical, "messages"))?;
        let canonical = JsValue::Object(canonical);
        crate::runtime::model::providers::validate_model_request(&canonical, &model_config)
            .map_err(stream_error)?;
        Ok(Prepared {
            body: object_value(&[
                ("requestId", JsValue::String(request_id)),
                (
                    "selection",
                    object_value(&[
                        ("requestedModelId", JsValue::String(requested)),
                        ("selectedModelId", JsValue::String(model)),
                        ("providerId", JsValue::String(provider)),
                    ]),
                ),
                ("request", canonical),
            ]),
            timeout_ms,
            registry,
            lease,
        })
    }

    async fn model_stream(&self, input: &JsObject) -> Result<HostResponse, Thrown> {
        let prepared = self.prepare(input).await?;
        let selection = prepared
            .body
            .get("selection")
            .cloned()
            .unwrap_or(JsValue::Undefined);
        let header = |key: &str| {
            selection
                .get(key)
                .and_then(JsValue::as_string)
                .map(|text| String::from_utf16_lossy(text.units()))
                .unwrap_or_default()
        };
        let headers = vec![
            ("content-type".to_owned(), "application/x-ndjson".to_owned()),
            ("x-pilotdeck-model-id".to_owned(), header("selectedModelId")),
            ("x-pilotdeck-provider-id".to_owned(), header("providerId")),
        ];
        // `{ ...prepared.request, stream: true }`
        let mut request = prepared
            .body
            .get("request")
            .and_then(JsValue::as_object)
            .cloned()
            .unwrap_or_default();
        request.insert("stream".into(), JsValue::Bool(true));
        let request = JsValue::Object(request);
        let (sender, receiver) = mpsc::channel(64);
        let Prepared {
            timeout_ms,
            registry,
            lease,
            ..
        } = prepared;
        tokio::spawn(pump_stream(registry, lease, request, timeout_ms, sender));
        Ok(HostResponse {
            status: 200,
            headers,
            body: HostBody::Stream(receiver),
            raw_body: None,
        })
    }

    /// `createActiveRuntimeTextParsingPort` behind the adapter's base64 and
    /// filename checks (publicHostRuntimeAdapter.ts:131-138).
    fn file_parse(&self, input: &JsObject) -> Result<JsValue, Thrown> {
        let encoded = required_string(field(input, "content_base64"), "content_base64")?;
        let encoded = String::from_utf16_lossy(encoded.units());
        if !canonical_base64(&encoded) {
            return Err(coded(
                400,
                "PUBLIC_HOST_INPUT_INVALID",
                "content_base64 must be canonical base64.",
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .unwrap_or_default();
        let filename = required_string(field(input, "filename"), "filename")?.clone();
        let extension = node_extname(&String::from_utf16_lossy(filename.units())).to_lowercase();
        let parse_error = |status, code, message| coded(status, code, message);
        if ![".txt", ".md", ".markdown", ".pdf"].contains(&extension.as_str()) {
            return Err(parse_error(
                415,
                "PUBLIC_FILE_TYPE_UNSUPPORTED",
                "This PD text extractor accepts PDF, .txt and Markdown",
            ));
        }
        let declared = field(input, "media_type");
        if !nullish(declared) && declared.as_string().is_none() {
            return Err(parse_error(
                400,
                "PUBLIC_FILE_INPUT_INVALID",
                "mediaType must be a string",
            ));
        }
        const LIMIT: f64 = 20.0 * 1024.0 * 1024.0;
        let limit = match field(input, "max_bytes") {
            JsValue::Undefined => Some(LIMIT),
            value => safe_integer(value).filter(|limit| *limit > 0.0 && *limit <= LIMIT),
        };
        let Some(limit) = limit else {
            return Err(parse_error(
                400,
                "PUBLIC_FILE_INPUT_INVALID",
                "maxBytes must be within 20 MiB",
            ));
        };
        if bytes.len() as f64 > limit {
            return Err(parse_error(
                413,
                "PUBLIC_FILE_TOO_LARGE",
                "File exceeds selected extraction limit",
            ));
        }
        if extension == ".pdf" {
            // No PDF text extractor exists in this host (the baseline uses
            // mupdf); answer with the extractor's own unsupported-type error.
            return Err(parse_error(
                415,
                "PUBLIC_FILE_TYPE_UNSUPPORTED",
                "This PD text extractor accepts PDF, .txt and Markdown",
            ));
        }
        // `new TextDecoder("utf-8", { fatal: true }).decode(bytes)`: a leading
        // BOM is consumed.
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Err(parse_error(
                422,
                "PUBLIC_FILE_ENCODING_UNSUPPORTED",
                "File is not valid UTF-8",
            ));
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let media_type = if nullish(declared) {
            string_value("text/plain")
        } else {
            declared.clone()
        };
        Ok(object_value(&[
            ("filename", JsValue::String(filename)),
            ("text", string_value(text)),
            ("mediaType", media_type),
            (
                "metadata",
                object_value(&[
                    ("fileType", string_value(&extension[1..])),
                    ("bytes", JsValue::Number(bytes.len() as f64)),
                ]),
            ),
        ]))
    }

    /// `createNativeSopAuthorityReader` (nativeSopAuthority.ts:6-34).
    async fn read_sop_authority(
        &self,
        input: &JsObject,
        pilot_deck_user_id: &JsString,
    ) -> Result<JsValue, Thrown> {
        let authority = |code: &str| coded(403, code, code);
        const KEYS: [&str; 5] = [
            "sessionKey",
            "projectKey",
            "expectedRevision",
            "requestId",
            "admissionCredentialId",
        ];
        let unknown_key = input
            .own_entries()
            .iter()
            .any(|(key, _)| !KEYS.iter().any(|name| key.equals_str(name)));
        let strings_ok = [
            "sessionKey",
            "projectKey",
            "requestId",
            "admissionCredentialId",
        ]
        .iter()
        .all(|key| non_empty_string(field(input, key)).is_some());
        let revision_ok = field(input, "expectedRevision")
            .as_number()
            .is_some_and(|revision| {
                revision.fract() == 0.0 && revision.is_finite() && revision >= 1.0
            });
        if unknown_key || !strings_ok || !revision_ok {
            return Err(authority("SOP_AUTHORITY_INPUT_INVALID"));
        }
        let session_key = field(input, "sessionKey")
            .as_string()
            .cloned()
            .unwrap_or_default();
        let project_key = field(input, "projectKey")
            .as_string()
            .cloned()
            .unwrap_or_default();
        let home = self.home.clone();
        let env = self.env.clone();
        let owner = off_worker(move || installation_owner(&env, &home))
            .await
            .map_err(plain_error)?;
        if owner.as_ref() != Some(pilot_deck_user_id) {
            return Err(authority("SOP_AUTHORITY_SESSION_DENIED"));
        }
        let metadata = self.metadata();
        let home = self.home.clone();
        let key = project_key.clone();
        let projects = off_worker(move || project_keys(&metadata, &home))
            .await
            .map_err(plain_error)?
            .map_err(|error| plain_error(error.to_string()))?;
        if projects.iter().filter(|project| **project == key).count() != 1 {
            return Err(authority("SOP_AUTHORITY_SESSION_DENIED"));
        }
        let home = self.home.clone();
        let sessions = off_worker(move || {
            let catalog = TranscriptSessionCatalog::new(Arc::new(
                crate::storage::AsyncTranscriptAdapter::new(
                    Arc::new(crate::storage::JsonlTranscriptStore::new()),
                    Arc::new(|key| {
                        crate::session_chat_dir(
                            &key.pilot_home,
                            &key.project_root,
                            &key.project_root.to_string_lossy(),
                        )
                    }),
                ),
            ));
            let mut list = SessionCatalogListInput::new(
                PathBuf::from(String::from_utf16_lossy(project_key.units())),
                home,
            );
            list.include_internal = true;
            catalog.list(&list)
        })
        .await
        .map_err(plain_error)?
        .map_err(|error| Thrown::Coded {
            status: None,
            code: error.code().unwrap_or(error.name()).into(),
            message: error.message().clone(),
        })?;
        if !sessions.iter().any(|session| {
            session.get("sessionId").and_then(JsValue::as_string) == Some(&session_key)
        }) {
            return Err(authority("SOP_AUTHORITY_SESSION_DENIED"));
        }
        // The admission is re-authenticated against StaffDeck before it is
        // trusted; this host does not make that call (module docs).
        Err(authority("SOP_AUTHORITY_ADMISSION_MISMATCH"))
    }
}

struct Prepared {
    body: JsValue,
    timeout_ms: Option<f64>,
    registry: Arc<ModelInvocationProviderRegistry>,
    lease: RuntimeLease,
}

/// The project generation's `runtime.model` registry and
/// `runtime.snapshot.config.model`.
fn model_resources(
    lease: &RuntimeLease,
) -> Result<(Arc<ModelInvocationProviderRegistry>, JsValue), Thrown> {
    let resource = lease
        .runtime()
        .resources()
        .get::<ProjectResources>()
        .and_then(|resources| resources.context().resource(Resource::Model))
        .ok_or_else(|| plain_error("Native model resource is missing."))?;
    let model = resource
        .as_any()
        .downcast_ref::<ModelResources>()
        .ok_or_else(|| plain_error("Native model resource has the wrong type."))?;
    Ok((model.providers.clone(), model.config.clone()))
}

/// Drive `runtime.model.stream(...)` into the transport's event channel. A
/// stream error after the headers is the transport's destroyed response; a
/// gone consumer aborts the provider request like the TS controller signal.
async fn pump_stream(
    registry: Arc<ModelInvocationProviderRegistry>,
    lease: RuntimeLease,
    request: JsValue,
    timeout_ms: Option<f64>,
    sender: mpsc::Sender<Result<HostEvent, String>>,
) {
    let abort = AbortFlag::new();
    let options = StreamOptions {
        abort: Some(abort.clone()),
        stream_timeout_ms: timeout_ms,
        ..StreamOptions::default()
    };
    let (events, mut received) = mpsc::unbounded_channel();
    let producer = tokio::spawn(async move {
        let _lease = lease;
        let mut sink = move |output: StreamOutput| {
            if let StreamOutput::Event(event) = output {
                let _ = events.send(event);
            }
        };
        registry.stream(&request, &options, &mut sink).await
    });
    while let Some(event) = received.recv().await {
        if sender.send(Ok(HostEvent::Value(event))).await.is_err() {
            abort.abort();
        }
    }
    let failure = match producer.await {
        Ok(Ok(())) => return,
        Ok(Err(error)) => String::from_utf16_lossy(error.message().units()),
        Err(join) => join.to_string(),
    };
    let _ = sender.send(Err(failure)).await;
}

/// `validatePublicMessages` (activeRuntimeModelPorts.ts:51-81).
fn validate_public_messages(value: &JsValue) -> Result<(), Thrown> {
    let invalid = |message: String| request_error("invalid_request", message);
    let messages = match value {
        JsValue::Array(messages) if !messages.is_empty() => messages,
        _ => return Err(invalid("messages must be a non-empty array".into())),
    };
    const TYPES: [&str; 9] = [
        "text",
        "thinking",
        "image",
        "pdf",
        "audio",
        "tool_call",
        "tool_result",
        "tool_result_reference",
        "media_reference",
    ];
    for (index, message) in messages.iter().enumerate() {
        let Some(message) = record(message) else {
            return Err(invalid(format!("messages[{index}] must be an object")));
        };
        let role = field(message, "role");
        if !role
            .as_string()
            .is_some_and(|role| role.equals_str("user") || role.equals_str("assistant"))
        {
            return Err(invalid(format!(
                "messages[{index}].role must be user or assistant"
            )));
        }
        let content = match field(message, "content") {
            JsValue::Array(content) if !content.is_empty() => content,
            _ => {
                return Err(invalid(format!(
                    "messages[{index}].content must be a non-empty canonical block array"
                )));
            }
        };
        for (block_index, block) in content.iter().enumerate() {
            let known = record(block)
                .and_then(|block| field(block, "type").as_string().cloned())
                .is_some_and(|kind| TYPES.iter().any(|name| kind.equals_str(name)));
            if !known {
                return Err(invalid(format!(
                    "messages[{index}].content[{block_index}] must be a canonical content block"
                )));
            }
        }
    }
    Ok(())
}

/// `/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/`.
fn canonical_base64(text: &str) -> bool {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return false;
    }
    let alphabet = |byte: &u8| byte.is_ascii_alphanumeric() || *byte == b'+' || *byte == b'/';
    let body_end = bytes.len().saturating_sub(4);
    if !bytes[..body_end].iter().all(alphabet) {
        return false;
    }
    match &bytes[body_end..] {
        [] => true,
        [a, b, b'=', b'='] => alphabet(a) && alphabet(b),
        [a, b, c, b'='] => alphabet(a) && alphabet(b) && alphabet(c),
        tail => tail.iter().all(alphabet),
    }
}

/// Node's POSIX `path.extname`.
fn node_extname(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut start_dot: Option<usize> = None;
    let mut start_part = 0;
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    let mut pre_dot_state = 0;
    for index in (0..bytes.len()).rev() {
        let byte = bytes[index];
        if byte == b'/' {
            if !matched_slash {
                start_part = index + 1;
                break;
            }
            continue;
        }
        if end.is_none() {
            matched_slash = false;
            end = Some(index + 1);
        }
        if byte == b'.' {
            if start_dot.is_none() {
                start_dot = Some(index);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    match (start_dot, end) {
        (Some(dot), Some(end))
            if !(pre_dot_state == 0
                || (pre_dot_state == 1 && dot == end - 1 && dot == start_part + 1)) =>
        {
            &path[dot..end]
        }
        _ => "",
    }
}

/// `readNativeInstallationOwner(env.DATABASE_PATH || pilotHome/auth.db)`:
/// the single active user's id, read-only; any failure is "no owner".
fn installation_owner(env: &Env, home: &std::path::Path) -> Option<JsString> {
    use rusqlite::{OpenFlags, types::ValueRef};
    let path = env
        .get("DATABASE_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("auth.db"));
    let connection = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let mut statement = connection
        .prepare("SELECT id FROM users WHERE is_active = 1")
        .ok()?;
    let mut rows = statement
        .query_map([], |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Null => "null".to_owned(),
                ValueRef::Integer(value) => value.to_string(),
                ValueRef::Real(value) => pilotdeck_protocol::number::number_to_string(value),
                ValueRef::Text(text) | ValueRef::Blob(text) => {
                    String::from_utf8_lossy(text).into_owned()
                }
            })
        })
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (rows.len() == 1).then(|| JsString::from(rows.remove(0).as_str()))
}

/// `dialog.projects.listProjectKeys` (projectRegistry.ts:29-39): the General
/// workspace first, then each registered project once per resolved path.
fn project_keys(
    metadata: &Metadata<NativeFiles>,
    home: &std::path::Path,
) -> std::io::Result<Vec<JsString>> {
    let general = home.to_string_lossy().into_owned();
    let mut resolved = vec![general.clone()];
    let mut keys = vec![JsString::from(general.as_str())];
    let listed = metadata.list_projects()?;
    if let Some(JsValue::Array(projects)) = listed.get("projects") {
        for project in projects {
            let Some(key) = project.get("projectKey").and_then(JsValue::as_string) else {
                continue;
            };
            let path = crate::metadata::resolve(
                &std::env::current_dir().unwrap_or_default(),
                &String::from_utf16_lossy(key.units()),
            )
            .to_string_lossy()
            .into_owned();
            if !resolved.contains(&path) {
                resolved.push(path);
                keys.push(key.clone());
            }
        }
    }
    Ok(keys)
}

impl PublicHostProvider for RuntimeHost {
    fn operations(&self) -> Vec<String> {
        let mut operations = Vec::new();
        if !self.skills_disabled() {
            operations.push("list_general_skills");
        }
        operations.extend([
            "list_model_catalog",
            "model_prepare",
            "model_stream",
            "file_parse",
            "read_sop_authority",
        ]);
        operations.into_iter().map(String::from).collect()
    }

    fn call(
        &self,
        operation: &str,
        input: &JsValue,
        principal: &PublicHostPrincipal,
    ) -> Pin<Box<dyn Future<Output = Result<HostResponse, HostFailure>> + Send + '_>> {
        let operation = operation.to_owned();
        let input = input.clone();
        let user = principal.pilot_deck_user_id.clone();
        Box::pin(async move {
            let Some(fields) = record(&input) else {
                return Ok(json(
                    400,
                    object_value(&[
                        ("code", string_value("PUBLIC_HOST_INPUT_INVALID")),
                        ("message", string_value("input must be an object.")),
                    ]),
                ));
            };
            let result = match operation.as_str() {
                "list_general_skills" => self.list_general_skills().await,
                "list_model_catalog" => self.list_model_catalog().await,
                "model_prepare" => self.prepare(fields).await.map(|prepared| prepared.body),
                "model_stream" => {
                    return Ok(self
                        .model_stream(fields)
                        .await
                        .unwrap_or_else(Thrown::respond));
                }
                "file_parse" => self.file_parse(fields),
                "read_sop_authority" => self.read_sop_authority(fields, &user).await,
                // Unreachable through the transport, which 409s any
                // operation `operations()` does not list: the adapter's
                // `unavailable(operation, "unknown")`.
                _ => {
                    return Ok(json(
                        503,
                        object_value(&[
                            ("code", string_value("PUBLIC_HOST_PORT_UNAVAILABLE")),
                            ("operation", string_value(&operation)),
                            ("port", string_value("unknown")),
                            (
                                "message",
                                string_value("Selected runtime has no unknown capability."),
                            ),
                        ]),
                    ));
                }
            };
            Ok(match result {
                Ok(body) => json(200, body),
                Err(thrown) => thrown.respond(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extname_follows_node_posix() {
        for (path, ext) in [
            ("a.txt", ".txt"),
            ("dir/x.Txt", ".Txt"),
            (".txt", ""),
            ("..txt", ".txt"),
            ("..", ""),
            ("a.", "."),
            ("noext", ""),
            ("a.b/c", ""),
            ("a/b.md/", ".md"),
            ("x.tar.gz", ".gz"),
        ] {
            assert_eq!(node_extname(path), ext, "{path}");
        }
    }

    #[test]
    fn base64_must_be_canonical() {
        for (text, ok) in [
            ("", true),
            ("YQ==", true),
            ("YWI=", true),
            ("YWJj", true),
            ("YQ", false),
            ("abc", false),
            ("Y===", false),
            ("YW=j", false),
            ("  ", false),
            ("YQ==YQ==", false),
        ] {
            assert_eq!(canonical_base64(text), ok, "{text:?}");
        }
    }

    /// `readNativeInstallationOwner`: exactly one active user is the owner;
    /// none, several, or an unreadable database are no owner.
    #[test]
    fn installation_owner_reads_the_single_active_user() {
        let dir = std::env::temp_dir().join(format!(
            "pilotdeck-module-host-owner-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let env = Env::new();
        assert_eq!(installation_owner(&env, &dir), None);
        let connection = rusqlite::Connection::open(dir.join("auth.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE users (id INTEGER, is_active INTEGER);
                 INSERT INTO users VALUES (7, 1), (8, 0);",
            )
            .unwrap();
        assert_eq!(installation_owner(&env, &dir), Some(JsString::from("7")));
        connection
            .execute_batch("INSERT INTO users VALUES ('x', 1);")
            .unwrap();
        assert_eq!(installation_owner(&env, &dir), None);
        let mut moved = Env::new();
        moved.insert(
            "DATABASE_PATH".into(),
            dir.join("missing.db").to_string_lossy().into_owned(),
        );
        assert_eq!(installation_owner(&moved, &dir), None);
        drop(connection);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
