use super::super::requests::Requests;
use crate::{
    metadata::{Metadata, NativeFiles, object},
    runtime::connection::{Call, RequestError},
};
use pilotdeck_protocol::JsValue;

pub(super) const METHODS: &[&str] = crate::dispatch::LOCAL_METHODS;
pub(super) const HANDLER: super::Handler =
    super::Handler::AfterAdmission(Requests::metadata_request);

fn field(value: &JsValue, name: &str) -> JsValue {
    value.get(name).cloned().unwrap_or(JsValue::Undefined)
}
impl Requests {
    pub(in crate::native_process) fn metadata_request(
        &self,
        method: String,
        params: JsValue,
    ) -> Call<'_, Result<JsValue, RequestError>> {
        Box::pin(async move {
            let metadata = Metadata {
                home: self.options.home.clone(),
                cwd: std::path::PathBuf::new(),
                files: NativeFiles,
            };
            if crate::native_archive::ARCHIVE_METHODS.contains(&method.as_str())
                && let Some(policy) = &self.native_archive_policy
            {
                policy
                    .authorize(
                        &metadata,
                        &method,
                        &params,
                        self.session_storage_adapter().as_deref(),
                    )
                    .await?;
            }
            let request = object([
                ("type", "request".into()),
                ("id", "native".into()),
                ("method", JsValue::String(method.clone().into())),
                ("params", params),
            ]);
            let selected = self.session_storage_adapter();
            let route = if crate::native_archive::ARCHIVE_METHODS.contains(&method.as_str())
                && selected.is_some()
            {
                crate::native_archive::response_with_storage(
                    &request,
                    &metadata,
                    selected.as_deref(),
                )
                .map(crate::dispatch::Route::LocalResponse)
                .unwrap_or(crate::dispatch::Route::Forward)
            } else {
                crate::dispatch::route_with_environment(&request, Some(&metadata), Some(&self.env))
            };
            match route {
                crate::dispatch::Route::LocalResponse(bytes) => {
                    let response = crate::dispatch::parse(&bytes)
                        .ok_or_else(|| RequestError::new("Native method returned invalid JSON."))?;
                    if response.get("ok") == Some(&JsValue::Bool(true)) {
                        Ok(field(&response, "result"))
                    } else {
                        let error = field(&response, "error");
                        Err(RequestError {
                            code: error
                                .get("code")
                                .and_then(JsValue::as_string)
                                .and_then(|s| s.to_utf8().ok())
                                .unwrap_or_else(|| "gateway_request_failed".into()),
                            message: error
                                .get("message")
                                .and_then(JsValue::as_string)
                                .cloned()
                                .unwrap_or_else(|| "Native request failed.".into()),
                            details: error.get("details").cloned().map(Box::new),
                            validation: None,
                        })
                    }
                }
                crate::dispatch::Route::Forward => unavailable(method),
            }
        })
    }
}

pub(super) fn unavailable(method: String) -> Result<JsValue, RequestError> {
    Err(RequestError {
        code: "CAPABILITY_UNAVAILABLE".into(),
        message: format!("Native gateway method {method} is not configured.").into(),
        details: None,
        validation: None,
    })
}
