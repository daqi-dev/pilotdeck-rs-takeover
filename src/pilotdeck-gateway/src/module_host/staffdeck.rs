//! StaffDeck identity joins. Credentials only leave through the configured
//! authority's normal authentication endpoints; transcripts hold identifiers.
use super::{HostFailure, PublicHostPrincipal, off_worker, runtime::RuntimeHost};
use crate::{config::Env, metadata::object, runtime::project::ProjectResources};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use pilotdeck_protocol::{JsString, JsValue};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub(crate) mod sop;

pub(super) fn text(value: &JsValue, key: &str) -> Option<String> {
    value.get(key)?.as_string()?.to_utf8().ok()
}
fn env_text<'a>(env: &'a Env, key: &str) -> &'a str {
    env.get(key).map_or("", String::as_str)
}
fn same(value: &JsValue, key: &str, expected: &str) -> bool {
    value
        .get(key)
        .and_then(JsValue::as_string)
        .is_some_and(|v| v.equals_str(expected))
}
fn host_error(code: &str, message: &str) -> HostFailure {
    HostFailure {
        status: Some(403),
        code: Some(code.into()),
        message: Some(message.into()),
    }
}

/// The read-only binding selector never provisions a target or guesses a
/// workspace when the authenticated tuple has zero or multiple matches.
pub(crate) fn project_for_principal(
    home: &Path,
    env: &Env,
    principal: &PublicHostPrincipal,
) -> Result<Option<PathBuf>, HostFailure> {
    if env_text(env, "STAFFDECK_PROJECT_BINDING_ENABLED") != "true" {
        return Ok(None);
    }
    if !principal
        .tenant_id
        .equals_str(env_text(env, "STAFFDECK_COPY_TENANT_ID"))
        || !principal
            .actor_user_id
            .equals_str(env_text(env, "STAFFDECK_COPY_ACTOR_USER_ID"))
        || !principal
            .pilot_deck_user_id
            .equals_str(env_text(env, "STAFFDECK_COPY_PILOTDECK_USER_ID"))
    {
        return Err(host_error(
            "PROJECT_BINDING_PRINCIPAL_MISMATCH",
            "The StaffDeck identity is not bound.",
        ));
    }
    let rows = bindings(home)?;
    let matches = rows
        .iter()
        .filter(|row| {
            same(row, "status", "bound")
                && row.get("agentId").and_then(JsValue::as_string) == Some(&principal.agent_id)
                && connection_matches(row, env)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(host_error(
            "PROJECT_BINDING_NOT_FOUND",
            "No unique bound project exists.",
        ));
    }
    text(matches[0], "projectRoot")
        .map(PathBuf::from)
        .map(Some)
        .ok_or_else(|| {
            host_error(
                "PROJECT_BINDING_NOT_FOUND",
                "No unique bound project exists.",
            )
        })
}
fn connection_matches(row: &JsValue, env: &Env) -> bool {
    let origin = env_text(env, "STAFFDECK_FORMAL_API_ORIGIN");
    same(row, "tenantId", env_text(env, "STAFFDECK_COPY_TENANT_ID"))
        && same(row, "origin", origin.strip_suffix('/').unwrap_or(origin))
}
fn bindings(home: &Path) -> Result<Vec<JsValue>, HostFailure> {
    let invalid = || HostFailure {
        status: Some(409),
        code: Some("PROJECT_BINDING_STORE_INVALID".into()),
        message: Some("StaffDeck binding storage is invalid.".into()),
    };
    let bytes = match std::fs::read(home.join("staffdeck/project-bindings.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(invalid()),
    };
    let value =
        pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&bytes)).map_err(|_| invalid())?;
    if value.get("version").and_then(JsValue::as_number) != Some(1.0)
        || !value
            .get("installationId")
            .is_some_and(crate::metadata::js_truthy)
    {
        return Err(invalid());
    }
    match value.get("projects") {
        Some(JsValue::Array(rows)) => Ok(rows.clone()),
        _ => Err(invalid()),
    }
}

/// The project binding overrides only the target; deployment credentials stay
/// in the process environment and are never written into a project.
pub(crate) fn project_environment(
    home: &Path,
    env: &Env,
    project: &Path,
) -> Result<Env, HostFailure> {
    if env_text(env, "STAFFDECK_PROJECT_BINDING_ENABLED") != "true" {
        return Ok(env.clone());
    }
    let mut next = env.clone();
    next.remove("STAFFDECK_COPY_TARGET_AGENT_ID");
    let project = project.to_string_lossy();
    let rows = bindings(home)?;
    if let Some(row) = rows.iter().find(|row| same(row, "projectRoot", &project)) {
        let live = same(row, "status", "bound")
            || (same(row, "status", "binding")
                && row
                    .get("runtimeReady")
                    .is_some_and(crate::metadata::js_truthy)
                && binding_lock_live(home));
        if live
            && connection_matches(row, env)
            && let Some(agent) = text(row, "agentId")
        {
            next.insert("STAFFDECK_COPY_TARGET_AGENT_ID".into(), agent);
        }
    }
    Ok(next)
}
fn binding_lock_live(home: &Path) -> bool {
    use crate::metadata::Files;
    let Ok(pid) = std::fs::read_to_string(home.join("staffdeck/project-bindings.json.lock")) else {
        return false;
    };
    pid.trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .is_some_and(|pid| crate::metadata::NativeFiles.process_alive(pid))
}

/// One bounded HTTP exchange, with redirects refused and a bounded body. No
/// transport error contains a URL, bearer or response body at the API edge.
pub(super) async fn get_json(
    origin: &url::Url,
    path: &str,
    bearer: &str,
    timeout: Duration,
) -> Result<(u16, JsValue), ()> {
    let url = origin.join(path).map_err(|_| ())?;
    let request = hyper::Request::builder()
        .method("GET")
        .uri(url.as_str())
        .header("authorization", format!("Bearer {bearer}"))
        .header("accept", "application/json")
        .body(Full::new(Bytes::new()))
        .map_err(|_| ())?;
    let connector = crate::runtime::model::http::https_connector(
        crate::runtime::model::http::webpki_client_config(),
    );
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(connector);
    tokio::time::timeout(timeout, async move {
        let response = client.request(request).await.map_err(|_| ())?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(());
        }
        if !(200..300).contains(&status) {
            return Ok((status, JsValue::Null));
        }
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.map_err(|_| ())?.into_data() {
                if bytes.len().saturating_add(data.len()) > 1024 * 1024 {
                    return Err(());
                }
                bytes.extend_from_slice(&data);
            }
        }
        let value =
            pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&bytes)).map_err(|_| ())?;
        Ok((status, value))
    })
    .await
    .map_err(|_| ())?
}
fn origin(env: &Env) -> Option<url::Url> {
    let parsed = url::Url::parse(env_text(env, "STAFFDECK_FORMAL_API_ORIGIN")).ok()?;
    (matches!(parsed.scheme(), "http" | "https")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.path() == "/")
        .then_some(parsed)
}

pub(crate) fn sop_profile(host: &RuntimeHost, project: Option<&Path>) -> Option<JsValue> {
    let lease = host.registry.acquire(project).ok()?;
    let resources = lease.runtime().resources().get::<ProjectResources>()?;
    resources
        .context()
        .snapshot()
        .get("config")?
        .get("modules")?
        .get("sop")
        .cloned()
        .filter(|value| value.get("enabled") != Some(&JsValue::Bool(false)))
}

/// Authenticate the actor, exact active full-access credential, and target
/// before producing the immutable host-admission identifiers.
pub(crate) async fn create_session_admission(
    host: &RuntimeHost,
    requested: Option<&Path>,
) -> Option<JsValue> {
    let lease = host.registry.acquire(requested).ok()?;
    let project = lease.runtime().project_root().to_path_buf();
    let home = host.home.clone();
    let env = host.env.clone();
    let owner = off_worker(move || super::runtime::installation_owner(&env, &home))
        .await
        .ok()
        .flatten()?;
    let env = project_environment(&host.home, &host.env, &project).ok()?;
    if !owner.equals_str(env_text(&env, "STAFFDECK_COPY_PILOTDECK_USER_ID")) {
        return None;
    }
    let profile = sop_profile(host, Some(&project));
    authenticate_admission(&env, &project, &owner, profile.as_ref()).await
}
async fn authenticate_admission(
    env: &Env,
    project: &Path,
    owner: &JsString,
    profile: Option<&JsValue>,
) -> Option<JsValue> {
    let tenant = env_text(env, "STAFFDECK_COPY_TENANT_ID");
    let agent = env_text(env, "STAFFDECK_COPY_TARGET_AGENT_ID");
    let actor = env_text(env, "STAFFDECK_COPY_ACTOR_USER_ID");
    let credential = env_text(env, "STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID");
    let actor_token = env_text(env, "STAFFDECK_COPY_USER_TOKEN");
    let management_key = profile
        .and_then(|p| text(p, "discoveryApiKey"))
        .unwrap_or_else(|| env_text(env, "STAFFDECK_SOP_MANAGEMENT_API_KEY").into());
    let discovery_agent = profile
        .and_then(|p| text(p, "discoveryAgentId"))
        .unwrap_or_else(|| agent.into());
    if [
        tenant,
        agent,
        actor,
        credential,
        actor_token,
        management_key.as_str(),
    ]
    .iter()
    .any(|v| v.is_empty())
        || discovery_agent != agent
        || !owner.equals_str(env_text(env, "STAFFDECK_COPY_PILOTDECK_USER_ID"))
    {
        return None;
    }
    let origin = origin(env)?;
    let formal_origin = origin.origin().ascii_serialization();
    let endpoint = profile
        .and_then(|p| text(p, "discoveryEndpoint"))
        .unwrap_or_else(|| format!("{formal_origin}/api/v1"));
    if endpoint.strip_suffix('/').unwrap_or(&endpoint) != format!("{formal_origin}/api/v1") {
        return None;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        let get = |path: String, bearer: String| {
            let origin = origin.clone();
            async move {
                let (status, value) = get_json(&origin, &path, &bearer, Duration::from_secs(5))
                    .await
                    .ok()?;
                (200..300).contains(&status).then_some(value)
            }
        };
        let user = get("/api/auth/me".into(), actor_token.into()).await?;
        if !same(&user, "id", actor)
            || !same(&user, "tenant_id", tenant)
            || !same(&user, "source", "web")
            || user.get("disabled") == Some(&JsValue::Bool(true))
        {
            return None;
        }
        let credentials = get("/api/auth/me/api-credentials".into(), actor_token.into()).await?;
        let JsValue::Array(ref rows) = credentials else {
            return None;
        };
        let row = rows.iter().find(|row| same(row, "id", credential))?;
        let prefix = text(row, "key_prefix")?;
        let prefix = prefix.strip_suffix('…')?;
        if prefix.encode_utf16().count() != 20
            || !management_key.starts_with(prefix)
            || !same(row, "user_id", actor)
            || !same(row, "status", "active")
            || !same(row, "access", "user_full_access")
            || row
                .get("revoked_at")
                .is_some_and(crate::metadata::js_truthy)
        {
            return None;
        }
        if let Some(expiry) = row
            .get("expires_at")
            .filter(|v| crate::metadata::js_truthy(v))
        {
            let millis = pilotdeck_transport::system_clock::parse_node22_date(expiry.as_string()?)
                .ok()??;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs_f64()
                * 1000.0;
            if millis <= now {
                return None;
            }
        }
        let target = get(
            format!(
                "/api/v1/agents/{}",
                crate::channel::http_client::encode_uri_component(agent)
            ),
            management_key,
        )
        .await?;
        if !same(&target, "id", agent)
            || !same(&target, "status", "active")
            || target.get("is_overall") != Some(&JsValue::Bool(false))
        {
            return None;
        }
        Some(object([
            ("tenantId", tenant.into()),
            ("agentId", agent.into()),
            ("pilotDeckUserId", JsValue::String(owner.clone())),
            ("projectKey", project.to_string_lossy().as_ref().into()),
            ("actorUserId", actor.into()),
            ("credentialId", credential.into()),
            ("staffDeckOrigin", formal_origin.as_str().into()),
        ]))
    })
    .await
    .ok()
    .flatten()
}

const ADMISSION_FIELDS: [&str; 7] = [
    "tenantId",
    "agentId",
    "pilotDeckUserId",
    "projectKey",
    "actorUserId",
    "credentialId",
    "staffDeckOrigin",
];
/// Transcript records, including snapshots, must agree on every identifier.
pub(super) fn original_admission(entries: &[JsValue], session: &JsString) -> Option<JsValue> {
    let mut admitted: Option<JsValue> = None;
    for entry in entries {
        if !same(entry, "type", "session_metadata") {
            continue;
        }
        let Some(metadata) = entry.get("metadata") else {
            continue;
        };
        let Some(next) = metadata
            .get("staffDeckAdmission")
            .filter(|v| crate::metadata::js_truthy(v))
        else {
            continue;
        };
        if entry.get("sessionId").and_then(JsValue::as_string) != Some(session)
            || (!same(entry, "turnId", "host-admission")
                && !metadata
                    .get("isSnapshot")
                    .is_some_and(crate::metadata::js_truthy))
            || !ADMISSION_FIELDS.iter().all(|key| {
                text(next, key).is_some_and(|v| !crate::metadata::js_trim(&v).is_empty())
            })
            || admitted.as_ref().is_some_and(|previous| {
                ADMISSION_FIELDS
                    .iter()
                    .any(|key| previous.get(key) != next.get(key))
            })
        {
            return None;
        }
        admitted = Some(next.clone());
    }
    admitted
}

pub(crate) async fn session_admission(
    host: &RuntimeHost,
    project: &Path,
    session: &JsString,
) -> Option<JsValue> {
    let chat = crate::session_chat_dir(&host.home, project, &project.to_string_lossy());
    let path = chat.join(format!(
        "{}.jsonl",
        crate::storage::sanitize_session_id(session)
    ));
    let session = session.clone();
    let original = off_worker(move || {
        let read = crate::storage::read_transcript(path).ok()?;
        if !read.diagnostics.is_empty() {
            return None;
        }
        original_admission(&read.entries, &session)
    })
    .await
    .ok()
    .flatten()?;
    let current = create_session_admission(host, Some(project)).await?;
    ADMISSION_FIELDS
        .iter()
        .all(|key| current.get(key) == original.get(key))
        .then_some(original)
}

pub(crate) fn projects(host: &RuntimeHost) -> Result<Vec<PathBuf>, HostFailure> {
    let metadata = crate::metadata::Metadata {
        home: host.home.clone(),
        cwd: host.project.clone(),
        files: crate::metadata::NativeFiles,
    };
    super::runtime::project_keys(&metadata, &host.home)
        .map(|keys| {
            keys.into_iter()
                .map(|key| PathBuf::from(String::from_utf16_lossy(key.units())))
                .collect()
        })
        .map_err(|_| {
            host_error(
                "SOP_APPROVAL_SESSION_FORBIDDEN",
                "SOP_APPROVAL_SESSION_FORBIDDEN",
            )
        })
}

pub(crate) fn clone_host(host: &RuntimeHost) -> RuntimeHost {
    RuntimeHost {
        home: host.home.clone(),
        project: host.project.clone(),
        builtin_skills_root: host.builtin_skills_root.clone(),
        env: host.env.clone(),
        registry: Arc::clone(&host.registry),
    }
}

#[cfg(test)]
mod tests;

use super::approvals::{
    ApprovalAuthenticator, ApprovalBinding, ApprovalBindingResolver, ApprovalFailure,
    ApprovalSession, ApprovalSessionResolver, ApprovalSubject,
};
use std::{future::Future, pin::Pin};
fn approval_error(status: u16, code: &str) -> ApprovalFailure {
    ApprovalFailure {
        status: Some(status),
        code: Some(code.into()),
    }
}
pub(super) struct FixedApprovalAuthenticator(pub Env);
impl ApprovalAuthenticator for FixedApprovalAuthenticator {
    fn authenticate<'a>(
        &'a self,
        bearer: &'a JsString,
    ) -> Pin<Box<dyn Future<Output = Result<ApprovalSubject, ApprovalFailure>> + Send + 'a>> {
        Box::pin(async move {
            let env = &self.0;
            let origin_text = env_text(env, "STAFFDECK_FORMAL_API_ORIGIN");
            let approver = env_text(env, "STAFFDECK_APPROVAL_USER_ID");
            let tenant = env_text(env, "STAFFDECK_COPY_TENANT_ID");
            if origin_text.is_empty() || approver.is_empty() {
                return Err(approval_error(503, "APPROVAL_BINDING_UNAVAILABLE"));
            }
            let origin = url::Url::parse(origin_text).map_err(|_| ApprovalFailure {
                status: None,
                code: None,
            })?;
            if !matches!(origin.scheme(), "http" | "https")
                || !origin.username().is_empty()
                || origin.password().is_some()
                || origin.query().is_some()
                || origin.fragment().is_some()
                || crate::metadata::js_trim(tenant).is_empty()
                || crate::metadata::js_trim(approver).is_empty()
            {
                return Err(approval_error(400, "APPROVAL_CONFIG_INVALID"));
            }
            let bearer = String::from_utf16_lossy(bearer.units());
            if crate::metadata::js_trim(&bearer).is_empty() {
                return Err(approval_error(401, "APPROVAL_AUTH_REQUIRED"));
            }
            let (status, user) =
                get_json(&origin, "/api/auth/me", &bearer, Duration::from_secs(300))
                    .await
                    .map_err(|_| ApprovalFailure {
                        status: None,
                        code: None,
                    })?;
            if !(200..300).contains(&status) {
                return Err(approval_error(status, "APPROVAL_AUTH_REJECTED"));
            }
            if !same(&user, "tenant_id", tenant)
                || !same(&user, "id", approver)
                || !same(&user, "source", "web")
                || user.get("disabled") == Some(&JsValue::Bool(true))
                || !text(&user, "role")
                    .is_some_and(|role| matches!(role.as_str(), "admin" | "member"))
            {
                return Err(approval_error(403, "APPROVAL_SUBJECT_MISMATCH"));
            }
            Ok(ApprovalSubject {
                tenant_id: tenant.into(),
                user_id: approver.into(),
                source: "web".into(),
                role: text(&user, "role").unwrap_or_default().as_str().into(),
                disabled: false,
            })
        })
    }
}
pub(super) struct NativeApprovalAccess(pub Arc<RuntimeHost>);
fn selected_projects(host: &RuntimeHost, requested: Option<&JsValue>) -> Vec<PathBuf> {
    let Ok(projects) = projects(host) else {
        return Vec::new();
    };
    match requested {
        None | Some(JsValue::Undefined) => projects,
        Some(JsValue::String(requested)) => {
            let requested = crate::metadata::resolve(
                &host.project,
                &String::from_utf16_lossy(requested.units()),
            );
            projects
                .into_iter()
                .filter(|project| {
                    crate::metadata::resolve(&host.project, &project.to_string_lossy()) == requested
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// The original session catalog is authoritative even when a sanitized
/// transcript filename could also be reached under a different caller key.
async fn catalog_has_session(home: &Path, project: &Path, session: &JsString) -> bool {
    use crate::runtime::manager::{
        SessionCatalogListInput, SessionCatalogPort, TranscriptSessionCatalog,
    };
    let home = home.to_path_buf();
    let project = project.to_path_buf();
    let session = session.clone();
    off_worker(move || {
        let adapter = crate::storage::AsyncTranscriptAdapter::new(
            Arc::new(crate::storage::JsonlTranscriptStore::new()),
            Arc::new(|key| {
                crate::session_chat_dir(
                    &key.pilot_home,
                    &key.project_root,
                    &key.project_root.to_string_lossy(),
                )
            }),
        );
        let catalog = TranscriptSessionCatalog::new(Arc::new(adapter));
        let mut input = SessionCatalogListInput::new(project, home);
        input.include_internal = true;
        catalog.list(&input).is_ok_and(|rows| {
            rows.iter()
                .any(|row| row.get("sessionId").and_then(JsValue::as_string) == Some(&session))
        })
    })
    .await
    .unwrap_or(false)
}
impl ApprovalSessionResolver for NativeApprovalAccess {
    fn resolve<'a>(
        &'a self,
        session: &'a JsString,
        project: Option<&'a JsValue>,
        binding: &'a ApprovalBinding,
        _subject: &'a ApprovalSubject,
    ) -> Pin<Box<dyn Future<Output = Option<ApprovalSession>> + Send + 'a>> {
        Box::pin(async move {
            let env = self.0.env.clone();
            let home = self.0.home.clone();
            let owner = off_worker(move || super::runtime::installation_owner(&env, &home))
                .await
                .ok()
                .flatten()?;
            if owner != binding.pilot_deck_user_id {
                return None;
            }
            let mut matches = Vec::new();
            for project in selected_projects(&self.0, project) {
                if !catalog_has_session(&self.0.home, &project, session).await {
                    continue;
                }
                if let Some(admission) = session_admission(&self.0, &project, session).await
                    && admission.get("tenantId").and_then(JsValue::as_string)
                        == Some(&binding.tenant_id)
                    && admission.get("agentId").and_then(JsValue::as_string)
                        == Some(&binding.agent_id)
                    && admission
                        .get("pilotDeckUserId")
                        .and_then(JsValue::as_string)
                        == Some(&binding.pilot_deck_user_id)
                    && same(&admission, "projectKey", &project.to_string_lossy())
                {
                    matches.push(project);
                }
            }
            if matches.len() != 1 {
                return None;
            }
            Some(ApprovalSession {
                tenant_id: binding.tenant_id.clone(),
                agent_id: binding.agent_id.clone(),
                pilot_deck_user_id: binding.pilot_deck_user_id.clone(),
                session_key: session.clone(),
                project_key: Some(matches[0].to_string_lossy().as_ref().into()),
            })
        })
    }
}
impl ApprovalBindingResolver for NativeApprovalAccess {
    fn resolve<'a>(
        &'a self,
        input: &'a JsValue,
    ) -> Pin<Box<dyn Future<Output = Result<ApprovalBinding, ApprovalFailure>> + Send + 'a>> {
        Box::pin(async move {
            let failed = || ApprovalFailure {
                status: Some(403),
                code: None,
            };
            let Some(session) = input.get("sessionKey").and_then(JsValue::as_string) else {
                return Err(failed());
            };
            let mut matches = Vec::new();
            for project in selected_projects(&self.0, input.get("projectKey")) {
                if let Some(admission) = session_admission(&self.0, &project, session).await {
                    matches.push(admission);
                }
            }
            if matches.len() != 1 {
                return Err(failed());
            }
            let admission = &matches[0];
            Ok(ApprovalBinding {
                tenant_id: admission
                    .get("tenantId")
                    .and_then(JsValue::as_string)
                    .cloned()
                    .ok_or_else(failed)?,
                agent_id: admission
                    .get("agentId")
                    .and_then(JsValue::as_string)
                    .cloned()
                    .ok_or_else(failed)?,
                pilot_deck_user_id: admission
                    .get("pilotDeckUserId")
                    .and_then(JsValue::as_string)
                    .cloned()
                    .ok_or_else(failed)?,
            })
        })
    }
}

pub(crate) fn sop_error_message(code: &str) -> &str {
    match code {
        "SOP_MODULE_DISABLED" => "The StaffDeck SOP module is not enabled for this project.",
        "SOP_AUTHORITY_STATE_MISSING" => "No admitted SOP state exists.",
        "SOP_AUTHORITY_PIN_INVALID" => "The exact SOP pin is unavailable.",
        "SOP_SESSION_NOT_FOUND" => "The StaffDeck SOP session does not exist.",
        "SOP_NOT_WAITING" => "The StaffDeck SOP session is not waiting for a resumable result.",
        "SOP_WAIT_STALE" => "The StaffDeck SOP wait identifier is no longer active.",
        "SOP_REVISION_REQUIRED" => "Approval requires the original revision.",
        _ => code,
    }
}
