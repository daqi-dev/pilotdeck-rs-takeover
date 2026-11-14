//! Embedding-owned native archive access policy. This is a Rust host port,
//! independent of client request configuration or JavaScript loading.
use crate::{
    ProxyError,
    metadata::{Files, Metadata},
    runtime::connection::RequestError,
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{future::Future, pin::Pin, sync::Arc};

pub type NativeArchiveAuthorizationFuture =
    Pin<Box<dyn Future<Output = Result<(), NativeArchiveAuthorizationError>> + Send + 'static>>;
pub trait NativeArchiveAuthorizer: Send + Sync {
    fn authorize(&self, input: JsValue) -> NativeArchiveAuthorizationFuture;
}
/// Dialog errors retain their code/details; ordinary host errors are denied
/// under ARCHIVE_UNAUTHORIZED. Denied represents a non-Error rejection.
#[derive(Clone, Debug)]
pub enum NativeArchiveAuthorizationError {
    Message(JsString),
    Dialog(RequestError),
    Denied,
}
#[derive(Clone)]
pub struct NativeArchivePolicy {
    authorizer: Option<Arc<dyn NativeArchiveAuthorizer>>,
    retention_ms: Option<f64>,
    now: Arc<dyn Fn() -> f64 + Send + Sync>,
}
impl NativeArchivePolicy {
    pub fn new(
        authorizer: Option<Arc<dyn NativeArchiveAuthorizer>>,
        retention_ms: Option<f64>,
        now: Option<Arc<dyn Fn() -> f64 + Send + Sync>>,
    ) -> Result<Self, ProxyError> {
        if retention_ms.is_some_and(|value| {
            !value.is_finite()
                || value.fract() != 0.0
                || value <= 0.0
                || value > 9_007_199_254_740_991.0
        }) {
            return Err(ProxyError::from(
                "nativeArchiveRetentionMs must be a positive safe integer.",
            ));
        }
        Ok(Self {
            authorizer,
            retention_ms,
            now: now.unwrap_or_else(|| {
                Arc::new(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|value| value.as_secs_f64() * 1000.0)
                        .unwrap_or(0.0)
                        .floor()
                })
            }),
        })
    }
    pub(crate) async fn authorize<F: Files>(
        &self,
        metadata: &Metadata<F>,
        method: &str,
        params: &JsValue,
        adapter: Option<&crate::storage::AsyncTranscriptAdapter>,
    ) -> Result<(), RequestError> {
        let input = super::authorization_input(method, params)?;
        if let Some(authorizer) = &self.authorizer {
            authorizer
                .authorize(input.clone())
                .await
                .map_err(|failure| match failure {
                    NativeArchiveAuthorizationError::Dialog(error) => error,
                    NativeArchiveAuthorizationError::Message(message) => RequestError {
                        code: "ARCHIVE_UNAUTHORIZED".into(),
                        message,
                        details: None,
                        validation: None,
                    },
                    NativeArchiveAuthorizationError::Denied => RequestError {
                        code: "ARCHIVE_UNAUTHORIZED".into(),
                        message: "Native transcript archive access was denied.".into(),
                        details: None,
                        validation: None,
                    },
                })?;
        }
        let Some(retention) = self.retention_ms else {
            return Ok(());
        };
        if !input
            .get("operation")
            .and_then(JsValue::as_string)
            .is_some_and(|operation| operation.equals_str("artifact"))
        {
            return Ok(());
        }
        let session = super::session(params).map_err(super::request_error)?;
        let project = super::project(metadata, params).map_err(super::request_error)?;
        if let Some(adapter) = adapter {
            let storage = super::host_storage(adapter, metadata, &project, session)
                .map_err(super::request_error)?;
            if storage.tool_result_artifact_storage().is_some() {
                return Ok(());
            }
        }
        let name = super::field(params, "artifactName")
            .as_string()
            .expect("validated artifact name");
        let name_text = String::from_utf16_lossy(name.units());
        let directory = super::resolve(
            &project,
            &format!(".pilotdeck/tool-results/{}", super::session_id(session)),
        );
        let path = directory.join(&name_text);
        // Use the same canonical containment and descriptor-relative stat as
        // the archive reader. A failed stat leaves the ordinary provider to
        // report its existing missing/security error, as the baseline catch.
        if let Ok((root, path)) = super::canonical(metadata, method, &project, &path)
            && let Ok((stat, _)) = metadata.files.project_stat(&root, &path)
            && (self.now)() - stat.mtime_ms > retention
        {
            return Err(RequestError {
                code: "ARCHIVE_ARTIFACT_EXPIRED".into(),
                message: format!(
                    "Archive artifact {name_text} is outside the configured retention window."
                )
                .into(),
                details: None,
                validation: None,
            });
        }
        Ok(())
    }
}

pub(super) fn authorization_input(method: &str, params: &JsValue) -> Result<JsValue, RequestError> {
    super::session(params).map_err(super::request_error)?;
    let operation = match method {
        "native_archive_entries" => {
            super::validate_entries(params, true).map_err(super::request_error)?;
            "entries"
        }
        "native_archive_artifact" => {
            super::artifact_name(params, true).map_err(super::request_error)?;
            super::artifact_limit(params).map_err(super::request_error)?;
            "artifact"
        }
        _ => "manifest",
    };
    let mut input = JsObject::new();
    input.insert("operation".into(), operation.into());
    if let Some(params) = params.as_object() {
        for (key, value) in params.iter() {
            input.insert(key.clone(), value.clone());
        }
    }
    // The method is authoritative: caller extras must not turn an artifact
    // request into another operation to bypass authorization or retention.
    input.insert("operation".into(), operation.into());
    Ok(JsValue::Object(input))
}
