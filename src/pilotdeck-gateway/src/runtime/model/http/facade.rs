//! Trusted Gateway services around the single shared async driver.
//!
//! The ambient entry is local to the existing Gateway product. It never grants
//! managed ifACE authority and cannot be selected as a managed transport fallback.
#[cfg(test)]
use super::ModelBody;
use super::{
    JsError, ModelHttpTransport, ModelStreamError, StreamOptions, StreamOutput,
    dump_request_to_file, errors, providers,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Request};
use pilotdeck_model_core::codec::projection::{field, put};
use pilotdeck_model_runtime::{
    driver_transport::{DriverHttpTransport, DriverTransportFuture, HostOperationError},
    headers::{
        DriverFailure, HeaderMaterializeError, HeaderMaterializeResult, HeaderProjection,
        HeaderValidateError, ModelHeaderService, PreparedModelHeaders, RequestMode,
        build_http_request,
    },
    services::{InvocationAuditSink, ModelHostServices},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::{convert::Infallible, sync::Arc};

/// A fresh candidate's full headers stay in the trusted facade. The ordinary
/// driver receives only empty values with the same names and positions. This is
/// not a Kernel capability: only this module can construct and consume it.
#[derive(Clone)]
struct AmbientPreparation(Arc<HeaderMap>);
struct AmbientTransport<'a>(&'a dyn ModelHttpTransport);
impl DriverHttpTransport for AmbientTransport<'_> {
    type Rejection = Infallible;
    type Preparation = AmbientPreparation;

    fn send(
        &self,
        mut request: Request<Full<Bytes>>,
        preparation: Self::Preparation,
    ) -> DriverTransportFuture<Self::Rejection> {
        // Follow may have permanently removed credentials/content headers. The
        // wire-header step may have replaced host, connection or sec-fetch-mode.
        // Restore only still-present, unmodified projected slots; never reinsert.
        for (name, value) in request.headers_mut() {
            if value.as_bytes().is_empty()
                && let Some(original) = preparation.0.get(name)
            {
                *value = original.clone();
            }
        }
        self.0.send(request).map(|result| {
            result
                .map(|response| {
                    response.map(|body| body.map_err(HostOperationError::Original).boxed())
                })
                .map_err(HostOperationError::Original)
        })
    }
}

struct GatewayServices {
    provider: JsValue,
}
struct GatewayHeaders(Vec<(JsString, JsValue)>);
impl ModelHeaderService for GatewayServices {
    type Rejection = Infallible;
    type Preparation = AmbientPreparation;

    fn materialize(
        &self,
        _provider_view: &JsValue,
        body: &JsValue,
        mode: RequestMode,
    ) -> HeaderMaterializeResult<Self::Rejection, Self::Preparation> {
        let headers = pilotdeck_model_core::codec::providers::request_headers_with_credentials(
            &self.provider,
            body,
            mode == RequestMode::Stream,
        )
        .map_err(providers::codec_error)
        .map_err(HeaderMaterializeError::Original)?;
        Ok(Box::new(GatewayHeaders(headers)))
    }
}
impl PreparedModelHeaders for GatewayHeaders {
    type Rejection = Infallible;
    type Preparation = AmbientPreparation;

    fn validate_url(
        &self,
        url: &str,
    ) -> Result<HeaderProjection<Self::Preparation>, HeaderValidateError<Self::Rejection>> {
        // Retain the original URL -> whole-record ByteString -> Headers order.
        let (url, full) =
            build_http_request(url, &self.0).map_err(HeaderValidateError::Original)?;
        let mut headers = full.clone();
        for value in headers.values_mut() {
            *value = hyper::header::HeaderValue::from_static("");
        }
        Ok(HeaderProjection {
            preparation: AmbientPreparation(Arc::new(full)),
            url,
            headers,
        })
    }
}
impl ModelHostServices for GatewayServices {
    fn dump_request_if_enabled(
        &self,
        body: &JsValue,
        model: &JsValue,
    ) -> Result<(), HostOperationError<JsError, Infallible>> {
        if std::env::var("PILOTDECK_DUMP_REQUEST").as_deref() == Ok("1") {
            dump_request_to_file(body, model)
                .map_err(|failure| HostOperationError::Original(failure.as_js_error()))?;
        }
        Ok(())
    }
    fn parse_retry_after_date(&self, value: &JsString) -> Result<Option<f64>, JsString> {
        errors::admitted_node22_parse(value)
    }
    fn format_epoch_iso(&self, epoch_ms: i64) -> String {
        pilotdeck_transport::system_clock::format_epoch_iso(epoch_ms)
    }
    fn streaming_duration_limit(&self) -> Option<String> {
        std::env::var("LITELLM_MAX_STREAMING_DURATION_SECONDS").ok()
    }
    fn diagnostic(&self, message: &str) {
        eprintln!("{message}");
    }
}
impl InvocationAuditSink for crate::storage::invocation_log::JsonlInvocationLogSink {
    fn stage(&self, record: &JsObject) -> std::io::Result<()> {
        Self::stage(self, record)
    }
    fn append(&self, record: &JsObject) -> std::io::Result<()> {
        Self::append(self, record)
    }
}

fn provider_view(provider: &JsValue) -> JsValue {
    let JsValue::Object(provider) = provider else {
        return provider.clone();
    };
    let mut view = JsObject::new();
    for (key, value) in provider.own_entries() {
        if !key.equals_str("apiKey") && !key.equals_str("headers") {
            view.insert(key.clone(), value.clone());
        }
    }
    JsValue::Object(view)
}
fn config_view(config: &JsValue) -> JsValue {
    let JsValue::Object(providers) = field(config, "providers") else {
        return config.clone();
    };
    let mut values = JsObject::new();
    for (key, provider) in providers.own_entries() {
        values.insert(key.clone(), provider_view(provider));
    }
    let mut view = config.clone();
    put(&mut view, "providers", JsValue::Object(values));
    view
}
fn services(
    request: &JsValue,
    config: &JsValue,
    stream: bool,
) -> Result<GatewayServices, ModelStreamError> {
    // One additional pure validation selects the trusted provider before making
    // a credential-free config. It precedes all sinks, clocks and I/O, just as
    // the shared driver's initial validation does; all later validation sites
    // are unchanged. A failed validation therefore runs once, a successful
    // entry runs twice plus the original per-attempt validations.
    let mut current = request.clone();
    put(&mut current, "stream", JsValue::Bool(stream));
    let (provider, _) = providers::validate_model_request(&current, config)?;
    Ok(GatewayServices { provider })
}
pub async fn stream_model(
    transport: &dyn ModelHttpTransport,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
) -> Result<(), ModelStreamError> {
    let host = services(request, config, true)?;
    pilotdeck_model_runtime::http::stream_model(
        &AmbientTransport(transport),
        &host,
        request,
        &config_view(config),
        options,
        sink,
    )
    .await
    .map_err(DriverFailure::into_model)
}
pub async fn complete_model(
    transport: &dyn ModelHttpTransport,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
) -> Result<JsValue, ModelStreamError> {
    let host = services(request, config, false)?;
    pilotdeck_model_runtime::http::complete_model(
        &AmbientTransport(transport),
        &host,
        request,
        &config_view(config),
        options,
    )
    .await
    .map_err(DriverFailure::into_model)
}

#[cfg(test)]
pub(super) async fn response_text(
    body: &mut ModelBody,
    options: &StreamOptions,
    continuation: bool,
) -> Result<String, ModelStreamError> {
    pilotdeck_model_runtime::http::response_text::<Infallible, _>(
        &mut (&mut *body).map_err(HostOperationError::Original),
        options,
        continuation,
    )
    .await
    .map_err(DriverFailure::into_model)
}
#[cfg(test)]
pub(super) async fn read_frame(
    body: &mut ModelBody,
    idle_ms: f64,
    options: &StreamOptions,
    continuation: &mut bool,
) -> Result<Option<Bytes>, ModelStreamError> {
    pilotdeck_model_runtime::http::read_frame::<Infallible, _>(
        &mut (&mut *body).map_err(HostOperationError::Original),
        idle_ms,
        options,
        continuation,
    )
    .await
    .map_err(DriverFailure::into_model)
}
