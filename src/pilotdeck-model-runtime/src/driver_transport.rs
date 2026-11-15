//! Typed host rejections around the existing HTTP exchange and body contract.
//! The legacy adapter only maps the same poll result; it creates no task, queue,
//! new timer or independent cancellation signal.
use crate::network::ExchangeFuture;
use crate::transport::{BodyFailure, ModelHttpTransport, TransportFailure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response};
use std::{borrow::Cow, convert::Infallible};

/// A product operation error or a typed trusted-host rejection. Host rejection
/// must not be normalized into a provider error or enter product retry policy.
#[derive(Debug)]
pub enum HostOperationError<E, R> {
    Original(E),
    Rejected(R),
}

impl<E> HostOperationError<E, Infallible> {
    pub fn into_original(self) -> E {
        match self {
            Self::Original(error) => error,
            Self::Rejected(never) => match never {},
        }
    }
}

/// A real pre-header network failure. Legacy callers retain borrowed codes;
/// managed transports may carry any owned, already-redacted code verbatim.
/// Neither the code nor the human message grants authority or classifies a
/// trusted-host cancellation, policy refusal or scope deadline as a network error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverNetworkFailure {
    pub cause_code: Option<Cow<'static, str>>,
    pub message: String,
}
impl DriverNetworkFailure {
    pub fn fetch_failed(cause_code: Option<Cow<'static, str>>) -> Self {
        Self {
            cause_code,
            message: "fetch failed".into(),
        }
    }
}
impl From<TransportFailure> for DriverNetworkFailure {
    fn from(error: TransportFailure) -> Self {
        Self {
            cause_code: error.cause_code.map(Cow::Borrowed),
            message: error.message,
        }
    }
}

/// The original body-failure categories with an owned decode-cause boundary.
/// Generic host Resource failures are not string-limit failures; adapters need
/// an explicit public body-too-large classification before selecting that arm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverBodyFailure {
    Terminated,
    StringTooLong,
    Decode(Cow<'static, str>),
}
impl From<BodyFailure> for DriverBodyFailure {
    fn from(error: BodyFailure) -> Self {
        match error {
            BodyFailure::Terminated => Self::Terminated,
            BodyFailure::StringTooLong => Self::StringTooLong,
            BodyFailure::Decode(reason) => Self::Decode(Cow::Borrowed(reason)),
        }
    }
}
impl std::fmt::Display for DriverBodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminated | Self::Decode(_) => f.write_str("terminated"),
            Self::StringTooLong => f.write_str(crate::transport::STRING_TOO_LONG),
        }
    }
}
impl std::error::Error for DriverBodyFailure {}

pub type DriverBody<R> = BoxBody<Bytes, HostOperationError<DriverBodyFailure, R>>;
pub type DriverTransportFailure<R> = HostOperationError<DriverNetworkFailure, R>;
pub type DriverTransportResult<R> = Result<Response<DriverBody<R>>, DriverTransportFailure<R>>;
pub type DriverTransportFuture<R> = ExchangeFuture<DriverTransportResult<R>>;

/// One typed exchange. Network, scope and resource ownership remain with the
/// injected transport; the driver retains the original product event ordering.
pub trait DriverHttpTransport: Send + Sync {
    type Rejection: Send + 'static;
    type Preparation: Clone + Send + Sync + 'static;
    fn send(
        &self,
        request: Request<Full<Bytes>>,
        preparation: Self::Preparation,
    ) -> DriverTransportFuture<Self::Rejection>;
}

/// Source-compatible bridge for the existing Gateway transport contract.
pub struct LegacyTransport<'a>(pub &'a dyn ModelHttpTransport);

impl DriverHttpTransport for LegacyTransport<'_> {
    type Rejection = Infallible;
    type Preparation = ();

    fn send(&self, request: Request<Full<Bytes>>, (): ()) -> DriverTransportFuture<Infallible> {
        self.0.send(request).map(|result| {
            result
                .map(|response| {
                    response.map(|body| {
                        body.map_err(|error| HostOperationError::Original(error.into()))
                            .boxed()
                    })
                })
                .map_err(|error| HostOperationError::Original(error.into()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_failures_preserve_borrowed_causes_and_original_messages() {
        let legacy = TransportFailure {
            cause_code: Some("UNKNOWN_VENDOR_CODE"),
            message: "original transport text".into(),
        };
        let error = DriverNetworkFailure::from(legacy);
        assert!(matches!(
            error.cause_code,
            Some(Cow::Borrowed("UNKNOWN_VENDOR_CODE"))
        ));
        assert_eq!(error.message, "original transport text");
        assert!(
            DriverNetworkFailure::from(TransportFailure::fetch_failed(None))
                .cause_code
                .is_none()
        );
        for legacy in [
            BodyFailure::Terminated,
            BodyFailure::StringTooLong,
            BodyFailure::Decode("incorrect header check"),
        ] {
            let error = DriverBodyFailure::from(legacy);
            assert_eq!(error.to_string(), legacy.to_string());
            if let BodyFailure::Decode(reason) = legacy {
                assert!(
                    matches!(error, DriverBodyFailure::Decode(Cow::Borrowed(actual)) if actual == reason)
                );
            }
        }
    }

    #[test]
    fn owned_causes_are_not_filtered_to_a_static_code_list() {
        let code = String::from("VENDOR_NEW_雪_CODE");
        let error = DriverNetworkFailure {
            cause_code: Some(Cow::Owned(code.clone())),
            message: "provider transport text".into(),
        };
        assert!(matches!(&error.cause_code, Some(Cow::Owned(actual)) if actual == &code));
        assert_eq!(error.message, "provider transport text");
        let reason = String::from("dynamic decoder reason");
        let error = DriverBodyFailure::Decode(Cow::Owned(reason.clone()));
        assert!(
            matches!(&error, DriverBodyFailure::Decode(Cow::Owned(actual)) if actual == &reason)
        );
        assert_eq!(error.to_string(), "terminated");
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_adapter_keeps_the_same_timer_and_microtask_sources() {
        struct Legacy(bool);
        impl ModelHttpTransport for Legacy {
            fn send(&self, _: Request<Full<Bytes>>) -> crate::transport::TransportFuture {
                let result = Err(TransportFailure::fetch_failed(Some("ECONNRESET")));
                if self.0 {
                    ExchangeFuture::ready(result)
                } else {
                    ExchangeFuture::after(std::time::Duration::from_millis(7), result)
                }
            }
        }
        for microtask in [true, false] {
            let before = tokio::time::Instant::now();
            let future =
                LegacyTransport(&Legacy(microtask)).send(Request::new(Full::new(Bytes::new())), ());
            assert_eq!(future.is_microtask(), microtask);
            assert_eq!(
                future.scheduled_at(),
                if microtask {
                    None
                } else {
                    Some(before + std::time::Duration::from_millis(7))
                }
            );
            match future.await {
                Err(HostOperationError::Original(error)) => assert!(matches!(
                    error.cause_code,
                    Some(Cow::Borrowed("ECONNRESET"))
                )),
                _ => panic!("same legacy error must be delivered"),
            }
        }
    }
}
