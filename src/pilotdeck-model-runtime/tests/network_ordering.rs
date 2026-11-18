//! Deterministic extraction checks. The original provider TS timing matrix stays
//! at the Gateway entry and must be rerun after the atomic re-export.
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Response,
    body::{Body, Frame},
};
use pilotdeck_model_runtime::{BodyFailure, ModelBody, TransportFuture, network::*};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::Instant;

async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        Poll::Ready(match future.as_mut().poll(cx) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        })
    })
    .await
}
fn observed(value: Exchange<u8>) -> (&'static str, bool) {
    match value {
        Exchange::Response(value, before_abort) => {
            assert_eq!(value, 7);
            ("response", before_abort)
        }
        Exchange::Aborted => ("abort", false),
        Exchange::Timeout { before_abort } => ("timeout", before_abort),
    }
}

#[test]
fn node_delay_normalization_preserves_fractional_and_nonfinite_inputs() {
    for (input, expected) in [
        (0.0, 1),
        (-0.0, 1),
        (-1.0, 1),
        (f64::NAN, 1),
        (f64::INFINITY, 1),
        (f64::NEG_INFINITY, 1),
        (1.9, 1),
        (60.9, 60),
        (2_147_483_647.0, 2_147_483_647),
        (2_147_483_648.0, 1),
    ] {
        assert_eq!(node_timer_duration(input), Duration::from_millis(expected));
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_retains_the_first_event_and_its_source() {
    let timer = Cancellation::new();
    let clone = timer.clone();
    assert!(timer.same_channel(&clone));
    assert!(!timer.same_channel(&Cancellation::new()));
    let at = Instant::now();
    tokio::time::advance(Duration::from_millis(5)).await;
    timer.cancel_at(at);
    clone.cancel_synchronously();
    assert_eq!(timer.cancelled_at(), Some(at));
    assert_eq!(timer.abort_event_at(), Some(at));
    assert!(
        poll_once(Box::pin(clone.cancelled()).as_mut())
            .await
            .is_some()
    );

    let synchronous = Cancellation::new();
    synchronous.abort();
    synchronous.cancel_at(at);
    assert!(synchronous.is_cancelled());
    assert_eq!(synchronous.abort_event_at(), None);
}

async fn late_exchange(
    prearmed: bool,
    response_ms: u64,
    timeout_ms: u64,
    parent_ms: u64,
) -> (&'static str, bool) {
    let signal = Cancellation::new();
    let start = Instant::now();
    let send = ExchangeFuture::after(Duration::from_millis(response_ms), 7);
    let deadline = Some(start + Duration::from_millis(timeout_ms));
    let mut operation = Box::pin(async {
        if prearmed {
            exchange_with_prearmed_parent(
                send,
                signal.cancelled(),
                || signal.abort_event_at(),
                deadline,
            )
            .await
        } else {
            exchange(
                send,
                signal.cancelled(),
                || signal.abort_event_at(),
                deadline,
            )
            .await
        }
    });
    assert!(poll_once(operation.as_mut()).await.is_none());
    tokio::time::advance(Duration::from_millis(55)).await;
    signal.cancel_at(start + Duration::from_millis(parent_ms));
    observed(
        poll_once(operation.as_mut())
            .await
            .expect("all scheduled events are overdue"),
    )
}

#[tokio::test(start_paused = true)]
async fn overdue_response_timeout_and_parent_keep_event_and_registration_order() {
    for (prearmed, response, timeout, parent, expected) in [
        (true, 20, 40, 15, ("abort", false)),
        (true, 20, 40, 25, ("response", true)),
        (true, 20, 40, 20, ("abort", false)),
        (false, 20, 40, 20, ("response", true)),
        (true, 20, 10, 25, ("timeout", true)),
        (true, 20, 20, 25, ("timeout", true)),
        (true, 30, 20, 20, ("abort", false)),
        (false, 30, 20, 20, ("timeout", true)),
    ] {
        assert_eq!(
            late_exchange(prearmed, response, timeout, parent).await,
            expected
        );
    }
}

#[tokio::test(start_paused = true)]
async fn negative_control_wrong_parent_registration_changes_the_observation() {
    let correct = late_exchange(true, 20, 40, 20).await;
    let wrong_entry = late_exchange(false, 20, 40, 20).await;
    assert_eq!(correct, ("abort", false));
    assert_ne!(
        wrong_entry, correct,
        "fixture must catch using the generic entry for a prearmed parent"
    );
}

#[tokio::test(start_paused = true)]
async fn retry_delay_keeps_its_parent_order_and_outer_deadline() {
    for (outer_ms, parent_ms, expected) in [
        (None, 15, ("abort", false)),
        (None, 20, ("response", true)),
        (None, 25, ("response", true)),
        (Some(10), 25, ("timeout", true)),
        (Some(20), 25, ("timeout", true)),
    ] {
        let signal = Cancellation::new();
        let start = Instant::now();
        let mut wait = Box::pin(delay(
            Duration::from_millis(20),
            signal.cancelled(),
            || signal.abort_event_at(),
            outer_ms.map(|ms| start + Duration::from_millis(ms)),
            false,
        ));
        assert!(poll_once(wait.as_mut()).await.is_none());
        tokio::time::advance(Duration::from_millis(55)).await;
        signal.cancel_at(start + Duration::from_millis(parent_ms));
        let outcome = match poll_once(wait.as_mut())
            .await
            .expect("retry events overdue")
        {
            Exchange::Response((), before_abort) => ("response", before_abort),
            Exchange::Timeout { before_abort } => ("timeout", before_abort),
            Exchange::Aborted => ("abort", false),
        };
        assert_eq!(outcome, expected);
    }
}

#[tokio::test(start_paused = true)]
async fn queued_continuation_and_ready_socket_do_not_share_abort_priority() {
    let signal = Cancellation::new();
    signal.abort();
    let queued = exchange(
        ExchangeFuture::ready(7),
        signal.cancelled(),
        || signal.abort_event_at(),
        None,
    )
    .await;
    assert_eq!(observed(queued), ("response", false));
    let io = exchange(
        ExchangeFuture::io(std::future::ready(7)),
        signal.cancelled(),
        || signal.abort_event_at(),
        None,
    )
    .await;
    assert_eq!(observed(io), ("abort", false));
}

struct PendingBody;
impl Body for PendingBody {
    type Data = Bytes;
    type Error = BodyFailure;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyFailure>>> {
        Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn body_continuation_gets_one_poll_and_pending_io_relinquishes_it() {
    let signal = Cancellation::new();
    signal.cancel_at(Instant::now());
    let mut continuation = true;
    let mut ready = Full::new(Bytes::from_static(b"first"));
    let result = body_frame(&mut ready, None, Some(&signal), &mut continuation).await;
    assert!(matches!(result, Exchange::Response(Some(Ok(_)), true)));
    assert!(continuation);
    let mut pending = PendingBody;
    let result = body_frame(&mut pending, None, Some(&signal), &mut continuation).await;
    assert!(matches!(result, Exchange::Aborted));
    assert!(!continuation);
}

struct DelayedBody {
    remaining: u8,
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
}
impl Body for DelayedBody {
    type Data = Bytes;
    type Error = BodyFailure;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyFailure>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_millis(15))));
        std::task::ready!(timer.as_mut().poll(cx));
        self.timer = None;
        self.remaining -= 1;
        Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"chunk")))))
    }
}

#[tokio::test(start_paused = true)]
async fn every_body_read_arms_a_new_idle_deadline() {
    let mut body = DelayedBody {
        remaining: 2,
        timer: None,
    };
    let mut continuation = false;
    let idle = Some(Duration::from_millis(20));
    for _ in 0..2 {
        let mut read = Box::pin(body_frame(&mut body, idle, None, &mut continuation));
        assert!(poll_once(read.as_mut()).await.is_none());
        tokio::time::advance(Duration::from_millis(15)).await;
        assert!(matches!(
            poll_once(read.as_mut()).await,
            Some(Exchange::Response(Some(Ok(_)), false))
        ));
    }
    assert!(matches!(
        body_frame(&mut body, idle, None, &mut continuation).await,
        Exchange::Response(None, false)
    ));
    let mut body = PendingBody;
    let mut read = Box::pin(body_frame(&mut body, idle, None, &mut continuation));
    assert!(poll_once(read.as_mut()).await.is_none());
    tokio::time::advance(Duration::from_millis(19)).await;
    assert!(poll_once(read.as_mut()).await.is_none());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(matches!(
        poll_once(read.as_mut()).await,
        Some(Exchange::Timeout {
            before_abort: false
        })
    ));
}

struct DropProbe(Arc<AtomicUsize>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct OwnedBody(DropProbe);
impl Body for OwnedBody {
    type Data = Bytes;
    type Error = BodyFailure;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyFailure>>> {
        let _ = &self.0;
        Poll::Pending
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_exchange_and_response_releases_owned_resources() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let probe = DropProbe(dropped.clone());
    let send = ExchangeFuture::io(async move {
        let _owned = probe;
        std::future::pending::<u8>().await
    });
    let mut pending = Box::pin(exchange(send, std::future::pending(), || None, None));
    assert!(poll_once(pending.as_mut()).await.is_none());
    drop(pending);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);

    let body: ModelBody = OwnedBody(DropProbe(dropped.clone())).boxed();
    let response = TransportFuture::ready(Ok(Response::new(body)));
    drop(response);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}
