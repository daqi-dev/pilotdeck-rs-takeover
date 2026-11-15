use super::*;
use crate::attempt::Attempt;
use agentcpm_core::guard::SecretRegistry;
use agentcpm_iface::env::http_pull::{HttpExchangeControl, HttpResources};
use bytes::Bytes;
use http_body_util::BodyExt;

struct OneError(Option<BodyFailure>);
impl hyper::body::Body for OneError {
    type Data = Bytes;
    type Error = BodyFailure;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, BodyFailure>>> {
        Poll::Ready(self.0.take().map(Err))
    }
}

#[tokio::test]
async fn actual_body_failures_publish_distinct_kinds_without_fabricating_release() {
    for (source, expected_kind, expected_detail) in [
        (
            BodyFailure::Terminated,
            HttpFailureKind::Network,
            "terminated",
        ),
        (
            BodyFailure::Decode("fixture decoder cause"),
            HttpFailureKind::Decode,
            "terminated",
        ),
        (
            BodyFailure::StringTooLong,
            HttpFailureKind::ResponseBodyTooLarge,
            pilotdeck_model_runtime::transport::STRING_TOO_LONG,
        ),
    ] {
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
            OneError(Some(source)).boxed(),
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
        let Some(Err(failure)) = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await
        else {
            panic!("the actual failing body must deliver its typed error");
        };
        assert_eq!(failure.phase, HttpFailurePhase::ResponseBody);
        assert_eq!(failure.kind, expected_kind);
        assert_eq!(failure.detail, expected_detail);
        if let BodyFailure::Decode(reason) = source {
            assert_eq!(failure.network_cause.unwrap().message, reason);
        }
        assert_eq!(attempt.snapshot().end, Some(HttpExchangeEnd::Failed));
        assert!(
            !attempt.has_owner(),
            "the failed body must drop its real owner"
        );
        assert_eq!(attempt.snapshot().resources, HttpResources::Held);
        // This fixture has no socket/pool/helper. Only after checking its
        // sole owner is gone may its trusted owner acknowledge convergence.
        assert!(attempt.resources_converged());
        assert_eq!(attempt.snapshot().resources, HttpResources::Released);
    }
}
