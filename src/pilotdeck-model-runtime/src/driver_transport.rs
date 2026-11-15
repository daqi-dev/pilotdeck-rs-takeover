//! Typed host rejections around the existing HTTP exchange and body contract.
//! The legacy adapter only maps the same poll result; it creates no task, queue,
//! new timer or independent cancellation signal.
use crate::network::ExchangeFuture;
use crate::transport::{BodyFailure, ModelHttpTransport, TransportFailure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Request, Response};
use std::convert::Infallible;

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

pub type DriverBody<R> = BoxBody<Bytes, HostOperationError<BodyFailure, R>>;
pub type DriverTransportFailure<R> = HostOperationError<TransportFailure, R>;
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
                    response.map(|body| body.map_err(HostOperationError::Original).boxed())
                })
                .map_err(HostOperationError::Original)
        })
    }
}
