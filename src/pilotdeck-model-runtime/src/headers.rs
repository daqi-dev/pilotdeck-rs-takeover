//! Fetch URL and header validation shared by trusted header preprocessing.
//!
//! This module performs no I/O and does not select credentials or grant network
//! authority. Trusted callers retain full header values and return only their
//! non-secret projection to the ordinary model driver.
use crate::ModelStreamError;
use pilotdeck_model_core::codec::normalize_error::NetworkFetchError;
use pilotdeck_protocol::{JsString, JsValue};

/// Selects the existing streaming or complete request header suffixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestMode {
    Stream,
    Complete,
}

/// An original model failure or a typed rejection by the trusted host.
///
/// Host rejection never enters provider normalization, retry or continuation.
/// It does not by itself prove that an earlier dispatched attempt was released.
#[derive(Debug)]
pub enum DriverFailure<R> {
    Model(ModelStreamError),
    Host(R),
}

impl<R> DriverFailure<R> {
    /// Original errors alone participate in product retry and audit projections.
    pub fn model(&self) -> Option<&ModelStreamError> {
        match self {
            Self::Model(error) => Some(error),
            Self::Host(_) => None,
        }
    }
}

impl<R> From<ModelStreamError> for DriverFailure<R> {
    fn from(error: ModelStreamError) -> Self {
        Self::Model(error)
    }
}

impl DriverFailure<std::convert::Infallible> {
    pub fn into_model(self) -> ModelStreamError {
        match self {
            Self::Model(error) => error,
            Self::Host(never) => match never {},
        }
    }
}

#[derive(Debug)]
pub enum HeaderMaterializeError<R> {
    Original(ModelStreamError),
    Rejected(R),
}

impl<R> From<HeaderMaterializeError<R>> for DriverFailure<R> {
    fn from(error: HeaderMaterializeError<R>) -> Self {
        match error {
            HeaderMaterializeError::Original(error) => Self::Model(error),
            HeaderMaterializeError::Rejected(error) => Self::Host(error),
        }
    }
}

#[derive(Debug)]
pub enum HeaderValidateError<R> {
    Original(NetworkFetchError),
    Rejected(R),
}

impl<R> HeaderValidateError<R> {
    /// Preserve each provider path's original URL/header-error projection.
    pub fn map_original(
        self,
        project: impl FnOnce(NetworkFetchError) -> ModelStreamError,
    ) -> DriverFailure<R> {
        match self {
            Self::Original(error) => DriverFailure::Model(project(error)),
            Self::Rejected(error) => DriverFailure::Host(error),
        }
    }
}

/// No-credential HTTP projection. Secret-bearing header names and positions
/// remain present with redacted values so the existing redirect policy can
/// remove them. Trusted dispatch must never restore a removed header merely
/// because a later redirect returns to an approved origin.
pub struct HeaderProjection<P = ()> {
    pub preparation: P,
    pub url: url::Url,
    pub headers: hyper::HeaderMap,
}

pub type MaterializedHeaders<R, P> = Box<dyn PreparedModelHeaders<Rejection = R, Preparation = P>>;
pub type HeaderMaterializeResult<R, P> =
    Result<MaterializedHeaders<R, P>, HeaderMaterializeError<R>>;

/// Pure preprocessing bound by the trusted product host to one admitted job,
/// effective provider configuration and revision. This port grants no I/O.
///
/// `provider_view` is a no-secret comparison input, never authority to choose
/// another credential. Implementations recheck the actual host binding before
/// materializing. They call the shared provider builders at this exact point,
/// including the complete Google suffix, and keep credential values private.
/// `Rejection = Infallible` preserves the legacy Gateway facade; managed hosts
/// use a typed public rejection and propagate it through `DriverFailure`.
pub trait ModelHeaderService: Send + Sync {
    type Rejection: Send + 'static;
    type Preparation: Clone + Send + Sync + 'static;

    fn materialize(
        &self,
        provider_view: &JsValue,
        final_body: &JsValue,
        mode: RequestMode,
    ) -> HeaderMaterializeResult<Self::Rejection, Self::Preparation>;
}

/// Opaque, trusted header state for one materialized body and admitted job.
/// There is deliberately no accessor for the complete header values.
///
/// The original endpoint fallback loop may validate multiple candidate URLs
/// against this object. It cannot be reused by another job or configuration
/// revision. Validation does no I/O, rechecks the host binding, then runs the
/// shared URL -> whole-record ByteString -> Headers checks before returning a
/// no-secret projection. Both errors and projected values must be secret-safe.
pub trait PreparedModelHeaders: Send + Sync {
    type Rejection: Send + 'static;
    type Preparation: Clone + Send + Sync + 'static;

    fn validate_url(
        &self,
        url: &str,
    ) -> Result<HeaderProjection<Self::Preparation>, HeaderValidateError<Self::Rejection>>;
}

/// `new Request(url, init)` validation plus `new Headers(record)`: the
/// initial URL and header map that `fetch_redirect::follow` then sends,
/// re-checking scheme and bad ports on every hop.
pub fn build_http_request(
    url: &str,
    headers: &[(JsString, JsValue)],
) -> Result<(url::Url, hyper::HeaderMap), NetworkFetchError> {
    let failed = || NetworkFetchError::classify("fetch failed", None);
    let parsed = url::Url::parse(url).map_err(|_| {
        NetworkFetchError::classify(&format!("Failed to parse URL from {url}"), None)
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(NetworkFetchError::classify(
            &format!("Request cannot be constructed from a URL that includes credentials: {url}"),
            None,
        ));
    }
    // WebIDL converts the entire record before validating/appending headers.
    let entries = headers
        .iter()
        .map(|(name, value)| {
            let units =
                pilotdeck_model_core::codec::js::to_string_units(value).map_err(|_| failed())?;
            Ok((
                header_byte_string(name.units())?,
                header_byte_string(&units)?,
            ))
        })
        .collect::<Result<Vec<_>, NetworkFetchError>>()?;
    let mut map = hyper::HeaderMap::new();
    for (name, value) in entries {
        let name = hyper::header::HeaderName::from_bytes(&name).map_err(|_| {
            NetworkFetchError::classify(
                &format!(
                    "Headers.append: \"{}\" is an invalid header name.",
                    name.iter().map(|b| char::from(*b)).collect::<String>()
                ),
                None,
            )
        })?;
        let trim = |b: &u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
        let start = value.iter().position(|b| !trim(b)).unwrap_or(value.len());
        let end = value
            .iter()
            .rposition(|b| !trim(b))
            .map_or(start, |i| i + 1);
        let bytes = &value[start..end];
        if bytes.iter().any(|b| matches!(b, 0 | b'\r' | b'\n')) {
            return Err(NetworkFetchError::classify(
                &format!(
                    "Headers.append: \"{}\" is an invalid header value.",
                    bytes.iter().map(|b| char::from(*b)).collect::<String>()
                ),
                None,
            ));
        }
        let value = hyper::header::HeaderValue::from_bytes(bytes).map_err(|_| failed())?;
        if let Some(existing) = map.get(&name) {
            let mut joined = existing.as_bytes().to_vec();
            joined.extend_from_slice(b", ");
            joined.extend_from_slice(value.as_bytes());
            map.insert(
                name,
                hyper::header::HeaderValue::from_bytes(&joined).map_err(|_| failed())?,
            );
        } else {
            map.insert(name, value);
        }
    }
    Ok((parsed, map))
}

fn header_byte_string(units: &[u16]) -> Result<Vec<u8>, NetworkFetchError> {
    units.iter().enumerate().map(|(index, &unit)| {
        u8::try_from(unit).map_err(|_| NetworkFetchError::classify(&format!("Cannot convert argument to a ByteString because the character at index {index} has a value of {unit} which is greater than 255."), None))
    }).collect()
}
