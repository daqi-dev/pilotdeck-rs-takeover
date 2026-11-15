//! Causal controls for typed execution denial across the real shared driver.
//! These tests use only in-memory transports; they make no resource-release claim.
use super::{StreamOptions, StreamOutput, complete_model, stream_model};
use crate::driver_transport::{
    DriverBody, DriverHttpTransport, DriverTransportFuture, HostOperationError,
};
use crate::driver_transport::{
    DriverBodyFailure as BodyFailure, DriverNetworkFailure as TransportFailure,
};
use crate::headers::{
    DriverFailure, HeaderMaterializeError, HeaderMaterializeResult, HeaderProjection,
    HeaderValidateError, ModelHeaderService, PreparedModelHeaders, RequestMode,
};
use crate::services::ModelHostServices;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Body, Frame},
};
use pilotdeck_model_core::codec::normalize_error::JsError;
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    Denied,
    Deadline,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopAt {
    None,
    Materialize,
    Validate,
    Dump,
}
struct Host {
    stop: StopAt,
    reason: Rejection,
}
struct Prepared {
    stop: StopAt,
    reason: Rejection,
}
impl ModelHeaderService for Host {
    type Rejection = Rejection;
    type Preparation = u64;
    fn materialize(
        &self,
        _: &JsValue,
        _: &JsValue,
        _: RequestMode,
    ) -> HeaderMaterializeResult<Rejection, u64> {
        if self.stop == StopAt::Materialize {
            return Err(HeaderMaterializeError::Rejected(self.reason));
        }
        Ok(Box::new(Prepared {
            stop: self.stop,
            reason: self.reason,
        }))
    }
}
impl PreparedModelHeaders for Prepared {
    type Rejection = Rejection;
    type Preparation = u64;
    fn validate_url(
        &self,
        url: &str,
    ) -> Result<HeaderProjection<u64>, HeaderValidateError<Rejection>> {
        if self.stop == StopAt::Validate {
            return Err(HeaderValidateError::Rejected(self.reason));
        }
        let (url, headers) =
            crate::headers::build_http_request(url, &[]).map_err(HeaderValidateError::Original)?;
        Ok(HeaderProjection {
            preparation: 41,
            url,
            headers,
        })
    }
}
impl ModelHostServices for Host {
    fn dump_request_if_enabled(
        &self,
        _: &JsValue,
        _: &JsValue,
    ) -> Result<(), HostOperationError<JsError, Rejection>> {
        if self.stop == StopAt::Dump {
            Err(HostOperationError::Rejected(self.reason))
        } else {
            Ok(())
        }
    }
    fn parse_retry_after_date(&self, _: &JsString) -> Result<Option<f64>, JsString> {
        Ok(None)
    }
    fn format_epoch_iso(&self, _: i64) -> String {
        "1970-01-01T00:00:00.000Z".into()
    }
    fn streaming_duration_limit(&self) -> Option<String> {
        None
    }
    fn diagnostic(&self, _: &str) {}
}

type Chunk = Result<Frame<Bytes>, HostOperationError<BodyFailure, Rejection>>;
struct Frames(VecDeque<Chunk>);
impl Body for Frames {
    type Data = Bytes;
    type Error = HostOperationError<BodyFailure, Rejection>;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.pop_front())
    }
}
enum Action {
    HeadRejected(Rejection),
    NetworkFailure,
    DetailedNetworkFailure(TransportFailure),
    Response {
        status: u16,
        location: Option<&'static str>,
        frames: VecDeque<Chunk>,
    },
}
struct Script {
    actions: Mutex<VecDeque<Action>>,
    calls: AtomicUsize,
    observed: Mutex<Vec<(String, u64)>>,
}
impl Script {
    fn new(actions: Vec<Action>) -> Self {
        Self {
            actions: Mutex::new(actions.into()),
            calls: AtomicUsize::new(0),
            observed: Mutex::new(Vec::new()),
        }
    }
}
impl DriverHttpTransport for Script {
    type Rejection = Rejection;
    type Preparation = u64;
    fn send(
        &self,
        request: Request<Full<Bytes>>,
        preparation: u64,
    ) -> DriverTransportFuture<Rejection> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.observed
            .lock()
            .unwrap()
            .push((request.uri().path().to_owned(), preparation));
        let action = self
            .actions
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra dispatch");
        let result = match action {
            Action::HeadRejected(reason) => Err(HostOperationError::Rejected(reason)),
            Action::DetailedNetworkFailure(error) => Err(HostOperationError::Original(error)),
            Action::NetworkFailure => Err(HostOperationError::Original(
                TransportFailure::fetch_failed(Some("ECONNRESET".into())),
            )),
            Action::Response {
                status,
                location,
                frames,
            } => {
                let mut builder = Response::builder().status(status);
                if let Some(location) = location {
                    builder = builder.header("location", location);
                }
                let body: DriverBody<Rejection> = Frames(frames).boxed();
                Ok(builder.body(body).unwrap())
            }
        };
        DriverTransportFuture::ready(result)
    }
}
fn data(bytes: &'static [u8]) -> Chunk {
    Ok(Frame::data(Bytes::from_static(bytes)))
}
fn host_error(reason: Rejection) -> Chunk {
    Err(HostOperationError::Rejected(reason))
}
fn response(status: u16, frames: Vec<Chunk>) -> Action {
    Action::Response {
        status,
        location: None,
        frames: frames.into(),
    }
}
fn request_config(protocol: &str, url: &str) -> (JsValue, JsValue) {
    let request = parse_json(r#"{"provider":"p","model":"m","messages":[]}"#).unwrap();
    let config = parse_json(&format!(r#"{{"providers":{{"p":{{"id":"p","protocol":"{protocol}","url":"{url}","retry":{{"streamMaxRetries":1,"requestMaxRetries":1,"baseDelayMs":0,"jitter":0}},"models":{{"m":{{"id":"m","capabilities":{{"supportsStreaming":true}},"multimodal":{{"input":["text"]}}}}}}}}}}}}"#)).unwrap();
    (request, config)
}
fn is_error_event(output: &StreamOutput) -> bool {
    match output {
        StreamOutput::Event(event) => event
            .get("type")
            .and_then(JsValue::as_string)
            .is_some_and(|tag| tag.equals_str("error")),
        StreamOutput::RetryProgress(_) => false,
    }
}
fn assert_host(result: Result<(), DriverFailure<Rejection>>, expected: Rejection) {
    assert!(matches!(result, Err(DriverFailure::Host(reason)) if reason == expected));
}

#[tokio::test]
async fn preflight_denial_never_dispatches_retries_or_emits_provider_error() {
    for protocol in ["openai", "google"] {
        for stop in [StopAt::Materialize, StopAt::Validate, StopAt::Dump] {
            for reason in [Rejection::Denied, Rejection::Deadline] {
                let host = Host { stop, reason };
                let transport = Script::new(vec![]);
                let (request, config) = request_config(protocol, "http://example.test/v1");
                let mut events = Vec::new();
                let result = stream_model(
                    &transport,
                    &host,
                    &request,
                    &config,
                    &StreamOptions::default(),
                    &mut |event| events.push(event),
                )
                .await;
                assert_host(result, reason);
                assert_eq!(
                    transport.calls.load(Ordering::SeqCst),
                    0,
                    "{protocol} {stop:?}"
                );
                assert!(!events.iter().any(is_error_event));
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, StreamOutput::RetryProgress(_)))
                );
            }
        }
    }
}

#[tokio::test]
async fn head_future_denial_is_preserved_without_retry() {
    for reason in [Rejection::Denied, Rejection::Deadline] {
        let transport = Script::new(vec![Action::HeadRejected(reason)]);
        let (request, config) = request_config("openai", "http://example.test/v1");
        let mut events = Vec::new();
        assert_host(
            stream_model(
                &transport,
                &Host {
                    stop: StopAt::None,
                    reason,
                },
                &request,
                &config,
                &StreamOptions::default(),
                &mut |e| events.push(e),
            )
            .await,
            reason,
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        assert!(!events.iter().any(is_error_event));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamOutput::RetryProgress(_)))
        );
    }
}

#[tokio::test]
async fn redirect_second_hop_denial_preserves_preparation_and_is_not_retried() {
    let transport = Script::new(vec![
        Action::Response {
            status: 307,
            location: Some("/next"),
            frames: VecDeque::new(),
        },
        Action::HeadRejected(Rejection::Denied),
    ]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let mut events = Vec::new();
    assert_host(
        stream_model(
            &transport,
            &Host {
                stop: StopAt::None,
                reason: Rejection::Denied,
            },
            &request,
            &config,
            &StreamOptions::default(),
            &mut |e| events.push(e),
        )
        .await,
        Rejection::Denied,
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    let observed = transport.observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[1], ("/next".into(), observed[0].1));
    assert!(!events.iter().any(is_error_event));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamOutput::RetryProgress(_)))
    );
}

#[tokio::test]
async fn body_denial_after_text_is_not_continued_or_retried() {
    let partial = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    let transport = Script::new(vec![response(
        200,
        vec![data(partial), host_error(Rejection::Deadline)],
    )]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let mut events = Vec::new();
    assert_host(
        stream_model(
            &transport,
            &Host {
                stop: StopAt::None,
                reason: Rejection::Deadline,
            },
            &request,
            &config,
            &StreamOptions::default(),
            &mut |e| events.push(e),
        )
        .await,
        Rejection::Deadline,
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert!(events.iter().any(|event| matches!(event, StreamOutput::Event(value) if value.get("type").and_then(JsValue::as_string).is_some_and(|tag| tag.equals_str("text_delta")))));
    assert!(!events.iter().any(is_error_event));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamOutput::RetryProgress(_)))
    );
}

#[tokio::test]
async fn complete_shape_probe_denial_never_falls_back() {
    let transport = Script::new(vec![response(
        200,
        vec![data(b"{\"partial\":1}"), host_error(Rejection::Denied)],
    )]);
    let (request, config) = request_config("openai", "http://example.test");
    let result = complete_model(
        &transport,
        &Host {
            stop: StopAt::None,
            reason: Rejection::Denied,
        },
        &request,
        &config,
        &StreamOptions::default(),
    )
    .await;
    assert!(matches!(
        result,
        Err(DriverFailure::Host(Rejection::Denied))
    ));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn error_body_denial_never_retries_or_becomes_provider_error() {
    let transport = Script::new(vec![response(
        503,
        vec![data(b"{\"error\":"), host_error(Rejection::Denied)],
    )]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let mut events = Vec::new();
    assert_host(
        stream_model(
            &transport,
            &Host {
                stop: StopAt::None,
                reason: Rejection::Denied,
            },
            &request,
            &config,
            &StreamOptions::default(),
            &mut |e| events.push(e),
        )
        .await,
        Rejection::Denied,
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert!(!events.iter().any(is_error_event));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, StreamOutput::RetryProgress(_)))
    );
}

#[tokio::test]
async fn ordinary_retryable_network_failure_still_retries() {
    let complete = b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let transport = Script::new(vec![
        Action::NetworkFailure,
        response(200, vec![data(complete)]),
    ]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let mut events = Vec::new();
    let result = stream_model(
        &transport,
        &Host {
            stop: StopAt::None,
            reason: Rejection::Denied,
        },
        &request,
        &config,
        &StreamOptions::default(),
        &mut |e| events.push(e),
    )
    .await;
    assert!(result.is_ok(), "ordinary network retry must succeed");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamOutput::RetryProgress(_)))
            .count(),
        1
    );
    assert!(!events.iter().any(is_error_event));
}

#[tokio::test]
async fn complete_error_body_denial_preserves_host_reason_without_another_attempt() {
    let transport = Script::new(vec![response(
        503,
        vec![data(b"{\"error\":"), host_error(Rejection::Deadline)],
    )]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let result = complete_model(
        &transport,
        &Host {
            stop: StopAt::None,
            reason: Rejection::Deadline,
        },
        &request,
        &config,
        &StreamOptions::default(),
    )
    .await;
    assert!(matches!(
        result,
        Err(DriverFailure::Host(Rejection::Deadline))
    ));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct Audit {
    staged: Mutex<Vec<pilotdeck_protocol::JsObject>>,
    appended: Mutex<Vec<pilotdeck_protocol::JsObject>>,
}
impl crate::services::InvocationAuditSink for Audit {
    fn stage(&self, record: &pilotdeck_protocol::JsObject) -> std::io::Result<()> {
        self.staged.lock().unwrap().push(record.clone());
        Ok(())
    }
    fn append(&self, record: &pilotdeck_protocol::JsObject) -> std::io::Result<()> {
        self.appended.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[tokio::test]
async fn host_denial_closes_staged_audit_without_fabricating_provider_success() {
    for protocol in ["openai", "google"] {
        let audit = Arc::new(Audit::default());
        let options = StreamOptions {
            invocation: Some(super::InvocationAudit {
                sink: audit.clone(),
                context: pilotdeck_protocol::JsObject::new(),
            }),
            ..StreamOptions::default()
        };
        let transport = Script::new(vec![Action::HeadRejected(Rejection::Denied)]);
        let (request, config) = request_config(protocol, "http://example.test/v1");
        let mut events = Vec::new();
        assert_host(
            stream_model(
                &transport,
                &Host {
                    stop: StopAt::None,
                    reason: Rejection::Denied,
                },
                &request,
                &config,
                &options,
                &mut |e| events.push(e),
            )
            .await,
            Rejection::Denied,
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        assert_eq!(audit.staged.lock().unwrap().len(), 1);
        let appended = audit.appended.lock().unwrap();
        assert_eq!(appended.len(), 1);
        let record = &appended[0];
        assert!(
            record
                .get("outcome")
                .and_then(JsValue::as_string)
                .is_some_and(|s| s.equals_str("incomplete"))
        );
        assert_eq!(record.get("responseComplete"), Some(&JsValue::Bool(false)));
        assert!(!events.iter().any(is_error_event));
    }
}

#[tokio::test]
async fn error_body_denial_settles_staged_audit() {
    let audit = Arc::new(Audit::default());
    let options = StreamOptions {
        invocation: Some(super::InvocationAudit {
            sink: audit.clone(),
            context: pilotdeck_protocol::JsObject::new(),
        }),
        ..StreamOptions::default()
    };
    let transport = Script::new(vec![response(
        503,
        vec![data(b"{\"error\":"), host_error(Rejection::Denied)],
    )]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let mut events = Vec::new();
    assert_host(
        stream_model(
            &transport,
            &Host {
                stop: StopAt::None,
                reason: Rejection::Denied,
            },
            &request,
            &config,
            &options,
            &mut |e| events.push(e),
        )
        .await,
        Rejection::Denied,
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(audit.staged.lock().unwrap().len(), 1);
    let appended = audit.appended.lock().unwrap();
    assert_eq!(
        appended.len(),
        1,
        "a denied error-body read must settle its staged audit"
    );
    assert!(
        appended[0]
            .get("outcome")
            .and_then(JsValue::as_string)
            .is_some_and(|s| s.equals_str("incomplete"))
    );
    assert_eq!(
        appended[0].get("responseComplete"),
        Some(&JsValue::Bool(false))
    );
    assert!(!events.iter().any(is_error_event));
}

#[tokio::test]
async fn owned_network_failure_uses_the_original_retry_path() {
    let complete = b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let transport = Script::new(vec![
        Action::DetailedNetworkFailure(TransportFailure {
            cause_code: Some(std::borrow::Cow::Owned(String::from("ECONNRESET"))),
            message: "fetch failed".into(),
        }),
        response(200, vec![data(complete)]),
    ]);
    let (request, config) = request_config("openai", "http://example.test/v1");
    let mut events = Vec::new();
    let result = stream_model(
        &transport,
        &Host {
            stop: StopAt::None,
            reason: Rejection::Denied,
        },
        &request,
        &config,
        &StreamOptions::default(),
        &mut |event| events.push(event),
    )
    .await;
    assert!(result.is_ok(), "owned ordinary network retry must succeed");
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamOutput::RetryProgress(_)))
            .count(),
        1
    );
    assert!(!events.iter().any(is_error_event));
}

#[tokio::test]
async fn owned_decode_cause_keeps_the_original_body_error_projection() {
    for reason in [
        std::borrow::Cow::Borrowed("fixture decode cause"),
        std::borrow::Cow::Owned(String::from("runtime decode cause")),
    ] {
        let mut body = Frames(VecDeque::from([Err(HostOperationError::Original(
            BodyFailure::Decode(reason),
        ))]));
        let error = super::response_text(&mut body, &StreamOptions::default(), false)
            .await
            .unwrap_err();
        match error {
            DriverFailure::Model(error) => {
                assert_eq!(error.name(), JsString::from("TypeError"));
                assert_eq!(error.message_text(), "terminated");
            }
            _ => panic!("real decode error must retain its original projection"),
        }
    }
}
