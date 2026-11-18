//! Adapters for the original driver fixtures. Fixture credential strings are
//! public oracle data; this host has no environment, filesystem or socket access.
use super::{StreamOptions, StreamOutput};
use crate::ModelStreamError;
use crate::driver_transport::{DriverBodyFailure, HostOperationError, LegacyTransport};
use crate::headers::{
    DriverFailure, HeaderMaterializeError, HeaderMaterializeResult, HeaderProjection,
    HeaderValidateError, ModelHeaderService, PreparedModelHeaders, RequestMode,
};
use crate::services::ModelHostServices;
use crate::transport::{BodyFailure, ModelBody, ModelHttpTransport};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::Response;
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_protocol::{JsString, JsValue};
use std::convert::Infallible;

pub(super) struct OracleHost;
struct OracleHeaders(Vec<(JsString, JsValue)>);

impl ModelHeaderService for OracleHost {
    type Rejection = Infallible;
    type Preparation = ();

    fn materialize(
        &self,
        provider: &JsValue,
        body: &JsValue,
        mode: RequestMode,
    ) -> HeaderMaterializeResult<Infallible, ()> {
        let headers = pilotdeck_model_core::codec::providers::request_headers_with_credentials(
            provider,
            body,
            mode == RequestMode::Stream,
        )
        .map_err(|error| HeaderMaterializeError::Original(super::providers::codec_error(error)))?;
        Ok(Box::new(OracleHeaders(headers)))
    }
}
impl PreparedModelHeaders for OracleHeaders {
    type Rejection = Infallible;
    type Preparation = ();
    fn validate_url(&self, url: &str) -> Result<HeaderProjection, HeaderValidateError<Infallible>> {
        crate::headers::build_http_request(url, &self.0)
            .map(|(url, headers)| HeaderProjection {
                preparation: (),
                url,
                headers,
            })
            .map_err(HeaderValidateError::Original)
    }
}
impl ModelHostServices for OracleHost {
    fn dump_request_if_enabled(
        &self,
        _: &JsValue,
        _: &JsValue,
    ) -> Result<(), HostOperationError<JsError, Infallible>> {
        Ok(())
    }
    fn parse_retry_after_date(&self, _: &JsString) -> Result<Option<f64>, JsString> {
        Ok(None)
    }
    fn format_epoch_iso(&self, _: i64) -> String {
        panic!("audit storage belongs to the retained Gateway tests")
    }
    fn streaming_duration_limit(&self) -> Option<String> {
        None
    }
    fn diagnostic(&self, _: &str) {}
}

fn legacy_response(
    response: Response<(crate::driver_transport::DriverBody<Infallible>, bool)>,
) -> Response<(ModelBody, bool)> {
    response.map(|(body, before)| (body.map_err(legacy_fixture_body_error).boxed(), before))
}

pub(super) async fn stream_model(
    transport: &dyn ModelHttpTransport,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
    sink: &mut (dyn FnMut(StreamOutput) + Send),
) -> Result<(), ModelStreamError> {
    super::stream_model(
        &LegacyTransport(transport),
        &OracleHost,
        request,
        config,
        options,
        sink,
    )
    .await
    .map_err(DriverFailure::into_model)
}
pub(super) async fn complete_model(
    transport: &dyn ModelHttpTransport,
    request: &JsValue,
    config: &JsValue,
    options: &StreamOptions,
) -> Result<JsValue, ModelStreamError> {
    super::complete_model(
        &LegacyTransport(transport),
        &OracleHost,
        request,
        config,
        options,
    )
    .await
    .map_err(DriverFailure::into_model)
}
pub(super) async fn send_provider_request(
    transport: &dyn ModelHttpTransport,
    provider: &JsValue,
    body: &JsValue,
    options: &StreamOptions,
    stream: bool,
) -> Result<Response<(ModelBody, bool)>, ModelStreamError> {
    super::send_provider_request(
        &LegacyTransport(transport),
        &OracleHost,
        provider,
        body,
        options,
        stream,
    )
    .await
    .map(legacy_response)
    .map_err(DriverFailure::into_model)
}
pub(super) async fn send_google_request(
    transport: &dyn ModelHttpTransport,
    provider: &JsValue,
    model: &JsValue,
    body: &JsValue,
    options: &StreamOptions,
) -> Result<Response<(ModelBody, bool)>, (ModelStreamError, bool)> {
    super::google::send_google_request(
        &LegacyTransport(transport),
        &OracleHost,
        provider,
        model,
        body,
        options,
    )
    .await
    .map(legacy_response)
    .map_err(|(error, before)| (error.into_model(), before))
}
pub(super) async fn google_response_error(
    response: Response<(ModelBody, bool)>,
    options: &StreamOptions,
    provider: &JsValue,
) -> Result<ModelStreamError, ModelStreamError> {
    let response = response.map(|(body, before)| {
        (
            body.map_err(|error| HostOperationError::Original(error.into()))
                .boxed(),
            before,
        )
    });
    super::google::google_response_error::<Infallible>(response, options, provider)
        .await
        .map_err(DriverFailure::into_model)
}
pub(super) async fn response_text(
    body: &mut ModelBody,
    options: &StreamOptions,
    continuation: bool,
) -> Result<String, ModelStreamError> {
    super::response_text::<Infallible, _>(
        &mut (&mut *body).map_err(|error| HostOperationError::Original(error.into())),
        options,
        continuation,
    )
    .await
    .map_err(DriverFailure::into_model)
}
pub(super) async fn read_frame(
    body: &mut ModelBody,
    idle_ms: f64,
    options: &StreamOptions,
    continuation: &mut bool,
) -> Result<Option<Bytes>, ModelStreamError> {
    super::read_frame::<Infallible, _>(
        &mut (&mut *body).map_err(|error| HostOperationError::Original(error.into())),
        idle_ms,
        options,
        continuation,
    )
    .await
    .map_err(DriverFailure::into_model)
}
pub(super) async fn read_shape_probe(
    body: &mut ModelBody,
    options: &StreamOptions,
) -> super::ShapeProbe<Infallible> {
    super::read_shape_probe::<Infallible, _>(
        &mut (&mut *body).map_err(|error| HostOperationError::Original(error.into())),
        options,
    )
    .await
}
pub(super) fn replay_body(bytes: Bytes, tail: Option<BodyFailure>) -> ModelBody {
    super::replay_body::<Infallible>(bytes, tail.map(Into::into))
        .map_err(legacy_fixture_body_error)
        .boxed()
}

fn legacy_fixture_body_error(
    error: HostOperationError<DriverBodyFailure, Infallible>,
) -> BodyFailure {
    match error.into_original() {
        DriverBodyFailure::Terminated => BodyFailure::Terminated,
        DriverBodyFailure::StringTooLong => BodyFailure::StringTooLong,
        DriverBodyFailure::Decode(std::borrow::Cow::Borrowed(reason)) => {
            BodyFailure::Decode(reason)
        }
        DriverBodyFailure::Decode(std::borrow::Cow::Owned(_)) => {
            panic!("legacy fixture unexpectedly received a managed owned decode cause")
        }
    }
}
