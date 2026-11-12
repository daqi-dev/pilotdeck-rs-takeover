use crate::{
    channel::http_client::{HttpClient, HttpResponse, UNDICI_DEFAULT_TIMEOUT},
    runtime::model::http::{ModelHttpTransport, fetch::fetch},
};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{HeaderMap, Method};
use std::{sync::Arc, time::Duration};
pub(super) enum Client {
    Legacy(HttpClient),
    Network(Arc<dyn ModelHttpTransport>),
}
impl Client {
    pub(super) fn new(transport: Option<Arc<dyn ModelHttpTransport>>) -> Self {
        transport
            .map(Self::Network)
            .unwrap_or_else(|| Self::Legacy(HttpClient::new()))
    }
    pub(super) async fn request_unbounded(
        &self,
        method: &str,
        url: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> Result<HttpResponse, String> {
        match self {
            Self::Legacy(client) => {
                client
                    .request_unbounded(method, url, content_type, body)
                    .await
            }
            Self::Network(transport) => tokio::time::timeout(
                UNDICI_DEFAULT_TIMEOUT,
                exchange(transport.as_ref(), method, url, content_type, None, body),
            )
            .await
            .map_err(|_| "TypeError: fetch failed".to_owned())?,
        }
    }
    pub(super) async fn request_bearer(
        &self,
        method: &str,
        url: &str,
        content_type: Option<&str>,
        bearer: &str,
        body: &str,
        timeout: Duration,
    ) -> Result<HttpResponse, String> {
        match self {
            Self::Legacy(client) => {
                client
                    .request_bearer(method, url, content_type, bearer, body, timeout)
                    .await
            }
            Self::Network(transport) => tokio::time::timeout(
                timeout,
                exchange(
                    transport.as_ref(),
                    method,
                    url,
                    content_type,
                    Some(bearer),
                    body,
                ),
            )
            .await
            .map_err(|_| "TimeoutError: The operation was aborted due to timeout".to_owned())?,
        }
    }
}
async fn exchange(
    transport: &dyn ModelHttpTransport,
    method: &str,
    raw_url: &str,
    content_type: Option<&str>,
    bearer: Option<&str>,
    body: &str,
) -> Result<HttpResponse, String> {
    let url = url::Url::parse(raw_url).map_err(|_| "TypeError: Invalid URL".to_owned())?;
    let mut headers = HeaderMap::new();
    if let Some(value) = content_type {
        headers.insert(
            "content-type",
            value
                .parse()
                .map_err(|_| "TypeError: invalid header".to_owned())?,
        );
    }
    if let Some(value) = bearer {
        headers.insert(
            "authorization",
            format!("Bearer {value}")
                .parse()
                .map_err(|_| "TypeError: invalid header".to_owned())?,
        );
    }
    let response = fetch(
        transport,
        url,
        Method::from_bytes(method.as_bytes())
            .map_err(|_| "TypeError: invalid method".to_owned())?,
        headers,
        Bytes::copy_from_slice(body.as_bytes()),
    )
    .await
    .map_err(|error| format!("TypeError: {}", error.message))?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|_| "TypeError: terminated".to_owned())?
        .to_bytes();
    Ok(HttpResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or(""),
        body,
    })
}
