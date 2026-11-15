//! Owned production-transport/component checks; formal server is separate.
use super::*;
use crate::channel::{
    protocol::TurnEventStream,
    sms::{ElicitationResponse, PermissionDecision, PermissionDecisionOutcome},
};
use crate::runtime::model::http::{ModelBody, TransportFuture};
use bytes::Bytes;
use futures_util::FutureExt;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
#[derive(Default)]
struct HttpMock {
    incoming: StdMutex<VecDeque<String>>,
    requests: StdMutex<Vec<(String, String, String)>>,
    hang_post: AtomicBool,
    dropped: Arc<AtomicUsize>,
}
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}
impl ModelHttpTransport for HttpMock {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let (parts, body) = request.into_parts();
        let body = body.collect().now_or_never().unwrap().unwrap().to_bytes();
        let method = parts.method.to_string();
        self.requests.lock().unwrap().push((
            method.clone(),
            parts.uri.to_string(),
            String::from_utf8(body.to_vec()).unwrap(),
        ));
        if method == "POST" && self.hang_post.load(Ordering::Acquire) {
            let dropped = Dropped(self.dropped.clone());
            return TransportFuture::io(async move {
                let _dropped = dropped;
                std::future::pending().await
            });
        }
        let body = if method == "GET" {
            self.incoming
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "[]".into())
        } else {
            "{}".into()
        };
        let body: ModelBody =
            BodyExt::boxed(Full::new(Bytes::from(body)).map_err(|error| match error {}));
        TransportFuture::ready(Ok(Response::new(body)))
    }
}
#[derive(Default)]
struct Control {
    cancelled: AtomicUsize,
    fail_cancel: AtomicBool,
    hold_cancel: AtomicBool,
    release: tokio::sync::Notify,
}
struct HeldStream {
    events: tokio::sync::mpsc::UnboundedReceiver<JsValue>,
    control: Arc<Control>,
}
impl TurnEventStream for HeldStream {
    fn next(
        &mut self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<JsValue>, ChannelError>> + Send + '_>,
    > {
        Box::pin(async move { Ok(self.events.recv().await) })
    }
    fn cancel_and_drain(
        &mut self,
        _: JsString,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ChannelError>> + Send + '_>>
    {
        Box::pin(async move {
            self.control.cancelled.fetch_add(1, Ordering::AcqRel);
            if self.control.fail_cancel.swap(false, Ordering::AcqRel) {
                return Err(ChannelError::error("fixture cancel failed"));
            }
            while self.control.hold_cancel.load(Ordering::Acquire) {
                self.control.release.notified().await;
            }
            self.events.close();
            Ok(())
        })
    }
}
struct Gateway {
    receiver: StdMutex<Option<tokio::sync::mpsc::UnboundedReceiver<JsValue>>>,
    control: Arc<Control>,
    submits: AtomicUsize,
}
impl TurnGateway for Gateway {
    fn submit(&self, _: TurnRequest) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
        self.submits.fetch_add(1, Ordering::AcqRel);
        let stream = HeldStream {
            events: self.receiver.lock().unwrap().take().expect("single turn"),
            control: self.control.clone(),
        };
        Box::pin(async move { Ok(Box::new(stream) as Box<dyn TurnEventStream>) })
    }
}
#[derive(Default)]
struct Responder {
    answers: StdMutex<Vec<String>>,
}
impl InteractionResponder for Responder {
    fn respond_elicitation(
        &self,
        response: ElicitationResponse,
    ) -> BoxFuture<Result<(), ChannelError>> {
        self.answers.lock().unwrap().push(format!(
            "elicitation:{}",
            String::from_utf16_lossy(response.session_key.units())
        ));
        Box::pin(async { Ok(()) })
    }
    fn permission_decide(
        &self,
        response: PermissionDecision,
    ) -> BoxFuture<Result<PermissionDecisionOutcome, ChannelError>> {
        self.answers.lock().unwrap().push(format!(
            "permission:{}",
            String::from_utf16_lossy(response.session_key.units())
        ));
        Box::pin(async { Ok(PermissionDecisionOutcome { delivered: true }) })
    }
}
async fn until(predicate: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
struct Fixture {
    channel: BlueBubblesChannel,
    handle: Arc<dyn ChannelHandle>,
    http: Arc<HttpMock>,
    gateway: Arc<Gateway>,
    control: Arc<Control>,
    events: tokio::sync::mpsc::UnboundedSender<JsValue>,
    responder: Arc<Responder>,
}
async fn fixture() -> Fixture {
    let http = Arc::new(HttpMock::default());
    http.incoming
        .lock()
        .unwrap()
        .push_back(r#"[{"guid":"m1","chatGuid":"chat","text":"start"}]"#.into());
    let (control, responder) = (Arc::new(Control::default()), Arc::new(Responder::default()));
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let gateway = Arc::new(Gateway {
        receiver: StdMutex::new(Some(receiver)),
        control: control.clone(),
        submits: AtomicUsize::new(0),
    });
    let channel = BlueBubblesChannel::new_with_transport(
        BlueBubblesOptions {
            server_url: Some("http://127.0.0.1:18081/base".into()),
            password: Some("fixture".into()),
            mapper: None,
            env: None,
        },
        Arc::new(HelperInteraction::new(responder.clone())),
        Some(http.clone()),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(gateway.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    until(|| gateway.submits.load(Ordering::Acquire) == 1).await;
    Fixture {
        channel,
        handle,
        http,
        gateway,
        control,
        events,
        responder,
    }
}
#[tokio::test]
async fn real_helper_answers_permission_and_elicitation_through_injected_transport() {
    let f = fixture().await;
    f.events.send(parse_json(r#"{"type":"permission_request","requestId":"p","toolName":"write","payload":{"path":"fixture"}}"#).unwrap()).unwrap();
    until(|| {
        f.http
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.0 == "POST")
    })
    .await;
    f.channel
        .runtime
        .dispatch_payload(&parse_json(r#"{"chatGuid":"chat","text":"1"}"#).unwrap())
        .await;
    assert!(
        f.responder
            .answers
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.starts_with("permission:"))
    );
    f.events.send(parse_json(r#"{"type":"elicitation_request","requestId":"e","questions":[{"id":"q","header":"h","question":"Choose","options":[{"label":"A"},{"label":"B"}]}]}"#).unwrap()).unwrap();
    until(|| f.channel.runtime.interaction.elicitation_pending("chat")).await;
    f.channel
        .runtime
        .dispatch_payload(&parse_json(r#"{"chatGuid":"chat","text":"1"}"#).unwrap())
        .await;
    assert!(
        f.responder
            .answers
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.starts_with("elicitation:"))
    );
    assert_eq!(f.gateway.submits.load(Ordering::Acquire), 1);
    assert!(
        f.http.requests.lock().unwrap()[0]
            .1
            .starts_with("http://127.0.0.1:18081/api/v1/message?password=fixture&after=")
    );
    f.handle.stop("done").await.unwrap();
    assert_eq!(f.control.cancelled.load(Ordering::Acquire), 1);
}
#[tokio::test]
async fn dropped_stop_waiter_retains_owned_cleanup_until_exact_stream_drains() {
    let f = fixture().await;
    f.control.hold_cancel.store(true, Ordering::Release);
    drop(f.handle.stop("disabled"));
    until(|| f.control.cancelled.load(Ordering::Acquire) == 1).await;
    assert!(!f.channel.runtime.running.load(Ordering::Acquire));
    let mut waiting = f.handle.stop("again");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut waiting)
            .await
            .is_err()
    );
    f.control.hold_cancel.store(false, Ordering::Release);
    f.control.release.notify_one();
    waiting.await.unwrap();
    assert!(f.channel.runtime.tasks.lock().unwrap().is_empty());
}
#[tokio::test]
async fn failed_cancel_is_reported_and_precise_stream_is_retained_for_retry() {
    let f = fixture().await;
    f.control.fail_cancel.store(true, Ordering::Release);
    assert!(f.handle.stop("disabled").await.is_err());
    assert_eq!(f.channel.runtime.failed_streams.lock().unwrap().len(), 1);
    f.handle.stop("retry").await.unwrap();
    assert_eq!(f.control.cancelled.load(Ordering::Acquire), 2);
    assert!(f.channel.runtime.failed_streams.lock().unwrap().is_empty());
}
#[tokio::test]
async fn dropped_outbound_waiter_does_not_hide_http_from_stop() {
    let f = fixture().await;
    f.http.hang_post.store(true, Ordering::Release);
    let runtime = f.channel.runtime.clone();
    let caller = tokio::spawn(async move {
        runtime
            .send_reply(&"cron-chat".into(), &"cron".into())
            .await
    });
    until(|| {
        f.http
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.0 == "POST")
    })
    .await;
    caller.abort();
    let _ = caller.await;
    f.handle.stop("disabled").await.unwrap();
    assert_eq!(f.http.dropped.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn owned_worker_failure_is_never_relabelled_as_a_successful_retry() {
    let f = fixture().await;
    assert!(
        f.channel
            .runtime
            .spawn_task(async { panic!("fixture owned worker failure") })
    );
    assert!(f.handle.stop("failed-worker").await.is_err());
    assert!(f.handle.stop("retry").await.is_err());
    assert_eq!(f.control.cancelled.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn pending_submission_is_dropped_and_owned_stop_does_not_wait_forever() {
    struct PendingGateway {
        polled: Arc<AtomicBool>,
        dropped: Arc<AtomicUsize>,
    }
    impl TurnGateway for PendingGateway {
        fn submit(
            &self,
            _: TurnRequest,
        ) -> BoxFuture<Result<Box<dyn TurnEventStream>, ChannelError>> {
            let polled = self.polled.clone();
            let dropped = Dropped(self.dropped.clone());
            Box::pin(async move {
                let _owner = dropped;
                polled.store(true, Ordering::Release);
                std::future::pending().await
            })
        }
    }
    let http = Arc::new(HttpMock::default());
    http.incoming
        .lock()
        .unwrap()
        .push_back(r#"[{"guid":"pending","chatGuid":"chat","text":"start"}]"#.into());
    let polled = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicUsize::new(0));
    let channel = BlueBubblesChannel::new_with_transport(
        BlueBubblesOptions {
            server_url: Some("http://127.0.0.1:18081".into()),
            password: Some("fixture".into()),
            mapper: None,
            env: None,
        },
        Arc::new(HelperInteraction::new(Arc::new(Responder::default()))),
        Some(http),
    );
    let handle = channel
        .start(ChannelStartDeps {
            turn_gateway: Some(Arc::new(PendingGateway {
                polled: polled.clone(),
                dropped: dropped.clone(),
            })),
            ..Default::default()
        })
        .await
        .unwrap();
    until(|| polled.load(Ordering::Acquire)).await;
    tokio::time::timeout(Duration::from_secs(1), handle.stop("pending-submit"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert!(channel.runtime.tasks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dropping_channel_and_handle_releases_idle_runtime() {
    let http = Arc::new(HttpMock::default());
    let responder = Arc::new(Responder::default());
    let channel = BlueBubblesChannel::new_with_transport(
        BlueBubblesOptions {
            server_url: Some("http://127.0.0.1:18081/base".into()),
            password: Some("fixture".into()),
            mapper: None,
            env: None,
        },
        Arc::new(HelperInteraction::new(responder)),
        Some(http.clone()),
    );
    let weak = Arc::downgrade(&channel.runtime);
    let handle = channel.start(ChannelStartDeps::default()).await.unwrap();
    until(|| !http.requests.lock().unwrap().is_empty()).await;
    drop(handle);
    drop(channel);
    until(|| weak.upgrade().is_none()).await;
}
