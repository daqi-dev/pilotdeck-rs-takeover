//! Pull-body projection for the trusted HTTP adapter (not yet wired).
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use crate::attempt::{Attempt, AttemptOwner};
use agentcpm_core::guard::RegistryStreamRedactor;
use agentcpm_iface::env::http_pull::{
    HttpBody, HttpExchangeEnd, HttpFailure, HttpFailureKind, HttpFailurePhase, HttpFrame,
    HttpNetworkCause,
};
use hyper::body::Body;
use pilotdeck_http_transport::transport::{BodyFailure, ModelBody};

/// Signals terminal body outcomes to the eventual attempt lifecycle owner.
/// The owner must independently establish when Hyper's helper and lease have
/// stopped before it reports `Released`.
pub(crate) enum BodyEnd {
    Eof,
    Failed,
    Stopped,
}

pub(crate) type RedactDetail = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Constructor-only grouping; callbacks are immediately moved into their
/// existing individual fields so terminal cleanup ordering is unchanged.
pub(crate) struct BodyHooks {
    pub(crate) decoded_progress: Option<Arc<AtomicBool>>,
    pub(crate) redact_detail: RedactDetail,
}

pub(crate) struct PullBody {
    inner: Option<ModelBody>,
    owner: Option<AttemptOwner>,
    attempt: Arc<Attempt>,
    redactor: Option<RegistryStreamRedactor>,
    authorize: Option<Arc<dyn Fn() -> Result<(), HttpFailure> + Send + Sync>>,
    end: Option<Arc<dyn Fn(BodyEnd) -> HttpExchangeEnd + Send + Sync>>,
    finished: bool,
    tail_delivered: bool,
    decoded_progress: Option<Arc<AtomicBool>>,
    redact_detail: Option<RedactDetail>,
}

impl PullBody {
    pub(crate) fn new(
        inner: ModelBody,
        owner: AttemptOwner,
        attempt: Arc<Attempt>,
        redactor: RegistryStreamRedactor,
        authorize: Arc<dyn Fn() -> Result<(), HttpFailure> + Send + Sync>,
        end: Arc<dyn Fn(BodyEnd) -> HttpExchangeEnd + Send + Sync>,
        hooks: BodyHooks,
    ) -> Self {
        let BodyHooks {
            decoded_progress,
            redact_detail,
        } = hooks;
        Self {
            inner: Some(inner),
            owner: Some(owner),
            attempt,
            redactor: Some(redactor),
            authorize: Some(authorize),
            end: Some(end),
            finished: false,
            tail_delivered: false,
            decoded_progress,
            redact_detail: Some(redact_detail),
        }
    }

    /// Clear all attempt-owned secret-bearing values before the owner can
    /// reach zero. Otherwise a retained, finished body shell can hold a secret
    /// registry beyond the reported resource-release boundary.
    fn terminate(&mut self, end: BodyEnd) -> HttpExchangeEnd {
        self.redactor.take();
        self.inner.take();
        self.redact_detail.take();
        self.authorize.take();
        self.decoded_progress.take();
        let callback = self.end.take().expect("active body end callback");
        let actual = callback(end);
        drop(callback);
        self.owner.take();
        self.finished = true;
        actual
    }

    fn cancelled(&mut self) -> Poll<Option<Result<HttpFrame, HttpFailure>>> {
        self.terminate(BodyEnd::Stopped);
        Poll::Ready(Some(Err(HttpFailure::new(
            HttpFailurePhase::ResponseBody,
            HttpFailureKind::Cancelled,
            "attempt stopped",
        ))))
    }

    fn fail(&mut self, mut error: HttpFailure) -> Poll<Option<Result<HttpFrame, HttpFailure>>> {
        // This also filters typed scope errors before releasing the registry.
        let redact = self.redact_detail.as_ref().expect("active redactor");
        error.detail = redact(&error.detail);
        if let Some(cause) = &mut error.network_cause {
            cause.message = redact(&cause.message);
            if let Some(code) = &mut cause.code {
                *code = redact(code);
            }
        }
        let end = self.terminate(BodyEnd::Failed);
        let error = if end == HttpExchangeEnd::StoppedLocally {
            HttpFailure::new(
                HttpFailurePhase::ResponseBody,
                HttpFailureKind::Cancelled,
                "attempt stopped",
            )
        } else {
            error
        };
        Poll::Ready(Some(Err(error)))
    }
}

impl HttpBody for PullBody {
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<HttpFrame, HttpFailure>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        self.attempt.register_body_waker(cx.waker());
        if self.attempt.is_stopped() {
            return self.cancelled();
        }
        if let Err(error) = (self.authorize.as_ref().expect("active body authorizer"))() {
            return self.fail(error);
        }
        if self.tail_delivered {
            if self.terminate(BodyEnd::Eof) == HttpExchangeEnd::StoppedLocally {
                return Poll::Ready(Some(Err(HttpFailure::new(
                    HttpFailurePhase::ResponseBody,
                    HttpFailureKind::Cancelled,
                    "attempt stopped",
                ))));
            }
            return Poll::Ready(None);
        }
        // Yield after a bounded run of empty frames/trailers; no fake progress.
        for _ in 0..16 {
            if self.attempt.is_stopped() {
                return self.cancelled();
            }
            if let Err(error) = (self.authorize.as_ref().expect("active body authorizer"))() {
                return self.fail(error);
            }
            let frame = Pin::new(self.inner.as_mut().expect("active body")).poll_frame(cx);
            if self.attempt.is_stopped() {
                return self.cancelled();
            }
            let wire_progress = self
                .decoded_progress
                .as_ref()
                .map(|progress| progress.swap(false, Ordering::Relaxed));
            match frame {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    self.inner.take();
                    let tail = self
                        .redactor
                        .take()
                        .map(RegistryStreamRedactor::finish)
                        .unwrap_or_default();
                    if tail.is_empty() {
                        if self.terminate(BodyEnd::Eof) == HttpExchangeEnd::StoppedLocally {
                            return Poll::Ready(Some(Err(HttpFailure::new(
                                HttpFailurePhase::ResponseBody,
                                HttpFailureKind::Cancelled,
                                "attempt stopped",
                            ))));
                        }
                        return Poll::Ready(None);
                    }
                    self.tail_delivered = true;
                    return Poll::Ready(Some(Ok(HttpFrame::Data(tail))));
                }
                Poll::Ready(Some(Err(error))) => {
                    let failure = match error {
                        BodyFailure::Decode(reason) => HttpFailure {
                            phase: HttpFailurePhase::ResponseBody,
                            kind: HttpFailureKind::Decode,
                            detail: "terminated".into(),
                            network_cause: Some(HttpNetworkCause {
                                code: None,
                                message: reason.into(),
                            }),
                        },
                        BodyFailure::Terminated => HttpFailure::new(
                            HttpFailurePhase::ResponseBody,
                            HttpFailureKind::Network,
                            "terminated",
                        ),
                        BodyFailure::StringTooLong => HttpFailure::new(
                            HttpFailurePhase::ResponseBody,
                            HttpFailureKind::ResponseBodyTooLarge,
                            pilotdeck_model_runtime::transport::STRING_TOO_LONG,
                        ),
                    };
                    return self.fail(failure);
                }
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        // The inherited decoder emits an empty data frame only
                        // when new wire bytes arrived but no decoded bytes were ready.
                        if data.is_empty() {
                            if wire_progress == Some(true) {
                                return Poll::Ready(Some(Ok(HttpFrame::Progress)));
                            }
                            continue;
                        }
                        let safe = self.redactor.as_mut().expect("active redactor").push(&data);
                        if safe.is_empty() {
                            if wire_progress.unwrap_or(true) {
                                return Poll::Ready(Some(Ok(HttpFrame::Progress)));
                            }
                            continue;
                        }
                        return Poll::Ready(Some(Ok(HttpFrame::Data(safe))));
                    }
                    // Trailers do not constitute wire-body progress.
                }
            }
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Drop for PullBody {
    fn drop(&mut self) {
        if !self.finished {
            self.terminate(BodyEnd::Stopped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attempt::Attempt;
    use agentcpm_core::guard::SecretRegistry;
    use agentcpm_iface::env::http_pull::HttpExchangeControl;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn eof_filters_secret_before_publishing_terminal_end() {
        let registry = SecretRegistry::new([("provider/key".into(), "secret".into())]);
        let body = Full::new(Bytes::from_static(b"abcsecret"))
            .map_err(|never| match never {})
            .boxed();
        let attempts = Attempt::new();
        let outcomes = Arc::new(AtomicUsize::new(0));
        let observed = outcomes.clone();
        let mut body = Box::pin(PullBody::new(
            body,
            attempts.owner(),
            attempts.clone(),
            registry.stream_redactor(),
            Arc::new(|| Ok(())),
            Arc::new(move |end| {
                if matches!(end, BodyEnd::Eof) {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                HttpExchangeEnd::Eof
            }),
            BodyHooks {
                decoded_progress: None,
                redact_detail: Arc::new(str::to_owned),
            },
        ));
        let first = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        assert!(matches!(first, Some(Ok(HttpFrame::Data(data))) if data == b"abc***"));
        assert_eq!(outcomes.load(Ordering::SeqCst), 0);
        assert!(
            std::future::poll_fn(|cx| body.as_mut().poll_frame(cx))
                .await
                .is_none()
        );
        assert_eq!(outcomes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dropped_partial_secret_never_flushes_and_reports_stop() {
        let registry = SecretRegistry::new([("provider/key".into(), "secret".into())]);
        let body = Full::new(Bytes::from_static(b"abcsec"))
            .map_err(|never| match never {})
            .boxed();
        let attempts = Attempt::new();
        let outcomes = Arc::new(AtomicUsize::new(0));
        let observed = outcomes.clone();
        let mut body = Box::pin(PullBody::new(
            body,
            attempts.owner(),
            attempts.clone(),
            registry.stream_redactor(),
            Arc::new(|| Ok(())),
            Arc::new(move |end| {
                if matches!(end, BodyEnd::Stopped) {
                    observed.fetch_add(1, Ordering::SeqCst);
                }
                HttpExchangeEnd::StoppedLocally
            }),
            BodyHooks {
                decoded_progress: None,
                redact_detail: Arc::new(str::to_owned),
            },
        ));
        let first = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        assert!(matches!(first, Some(Ok(HttpFrame::Data(data))) if data == b"abc"));
        drop(body);
        assert_eq!(outcomes.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn buffered_decoded_bytes_do_not_forge_wire_progress() {
        let registry = SecretRegistry::new([("provider/key".into(), "secret".into())]);
        let input = Full::new(Bytes::from_static(b"sec"))
            .map_err(|never| match never {})
            .boxed();
        let attempt = Attempt::new();
        // No new wire bytes were read in this poll: decoded bytes may be
        // emitted from the decoder's existing buffer.
        let wire = Arc::new(AtomicBool::new(false));
        let mut body = Box::pin(PullBody::new(
            input,
            attempt.owner(),
            attempt.clone(),
            registry.stream_redactor(),
            Arc::new(|| Ok(())),
            Arc::new(|_| HttpExchangeEnd::Eof),
            BodyHooks {
                decoded_progress: Some(wire),
                redact_detail: Arc::new(str::to_owned),
            },
        ));
        let frame = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        assert!(matches!(frame, Some(Ok(HttpFrame::Data(data))) if data == b"***"));
    }

    struct ErrorAndStop(Arc<Attempt>);
    impl hyper::body::Body for ErrorAndStop {
        type Data = Bytes;
        type Error = BodyFailure;
        fn poll_frame(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BodyFailure>>> {
            self.0.stop_local();
            Poll::Ready(Some(Err(BodyFailure::Terminated)))
        }
    }
    #[tokio::test]
    async fn stop_during_ready_error_keeps_receipt_and_delivery_consistent() {
        let registry = SecretRegistry::new(Vec::<(String, String)>::new());
        let attempt = Attempt::new();
        let ended = attempt.clone();
        let on_end = Arc::new(move |end| match end {
            BodyEnd::Eof => ended.finish(HttpExchangeEnd::Eof),
            BodyEnd::Failed => ended.finish(HttpExchangeEnd::Failed),
            BodyEnd::Stopped => {
                ended.stop_local();
                ended.snapshot().end.unwrap()
            }
        });
        let mut body = Box::pin(PullBody::new(
            ErrorAndStop(attempt.clone()).boxed(),
            attempt.owner(),
            attempt.clone(),
            registry.stream_redactor(),
            Arc::new(|| Ok(())),
            on_end,
            BodyHooks {
                decoded_progress: None,
                redact_detail: Arc::new(str::to_owned),
            },
        ));
        let frame = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        assert!(matches!(
            frame,
            Some(Err(HttpFailure {
                kind: HttpFailureKind::Cancelled,
                ..
            }))
        ));
        assert_eq!(
            attempt.snapshot().end,
            Some(HttpExchangeEnd::StoppedLocally)
        );
    }
    #[tokio::test]
    async fn finished_body_shell_does_not_retain_secret_registry() {
        let registry = Arc::new(SecretRegistry::new([(
            "test/key".into(),
            "synthetic-secret".into(),
        )]));
        let weak = Arc::downgrade(&registry);
        let held_registry = registry.clone();
        let redact: Arc<dyn Fn(&str) -> String + Send + Sync> =
            Arc::new(move |value| held_registry.redact(value));
        let attempt = Attempt::new();
        let input = Full::new(Bytes::from_static(b"safe"))
            .map_err(|never| match never {})
            .boxed();
        let mut body = Box::pin(PullBody::new(
            input,
            attempt.owner(),
            attempt.clone(),
            registry.stream_redactor(),
            Arc::new(|| Ok(())),
            Arc::new(|_| HttpExchangeEnd::Eof),
            BodyHooks {
                decoded_progress: None,
                redact_detail: redact,
            },
        ));
        drop(registry);
        assert!(weak.upgrade().is_some());
        assert!(matches!(
            std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await,
            Some(Ok(HttpFrame::Data(_)))
        ));
        assert!(
            std::future::poll_fn(|cx| body.as_mut().poll_frame(cx))
                .await
                .is_none()
        );
        assert!(
            weak.upgrade().is_none(),
            "finished body shell retained the registry"
        );
        assert!(body.finished);
    }
}

#[cfg(test)]
#[path = "body_kind_tests.rs"]
mod kind_tests;
