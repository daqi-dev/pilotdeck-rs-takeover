//! One native host's current channel network dispatcher. Replacing the
//! snapshot affects subsequent HTTP exchanges and WS handshakes; existing
//! response bodies and sockets retain their original transport ownership.
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use http_body_util::Full;
use hyper::Request;
use pilotdeck_protocol::JsValue;
use tokio_tungstenite::tungstenite::{
    Error, client::IntoClientRequest, handshake::client::Response,
};

use super::ws_transport::{self, ChannelWebSocket};
use crate::runtime::{
    model::http::{
        HyperModelTransport, ModelHttpTransport, ProxyInstallMode, ProxySettings, TransportFailure,
        TransportFuture,
    },
    network::UNDICI_BODY_TIMEOUT,
};

struct Snapshot {
    proxy: Option<ProxySettings>,
    http: Arc<dyn ModelHttpTransport>,
}

impl Snapshot {
    fn new(proxy: Option<ProxySettings>) -> Self {
        let http = Arc::new(HyperModelTransport::with_proxy_settings(proxy.as_ref()));
        Self { proxy, http }
    }
}

pub(crate) struct ChannelNetwork {
    current: RwLock<Snapshot>,
}

impl ChannelNetwork {
    pub(crate) fn new(config_proxy: Option<&JsValue>) -> Self {
        Self {
            current: RwLock::new(Snapshot::new(
                config_proxy.and_then(ProxySettings::from_config),
            )),
        }
    }

    pub(crate) fn update_proxy(&self, config_proxy: Option<&JsValue>) {
        // Reload removal must override the environment. None would mean
        // "resolve the startup environment again", not a direct dispatcher.
        let mut proxy = config_proxy
            .and_then(ProxySettings::from_config)
            .unwrap_or_default();
        proxy.mode = ProxyInstallMode::Reload;
        let snapshot = Snapshot::new(Some(proxy));
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }

    pub(crate) async fn connect_ws<R>(
        &self,
        request: R,
    ) -> Result<(ChannelWebSocket, Response), Error>
    where
        R: IntoClientRequest + Unpin,
    {
        let proxy = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .proxy
            .clone();
        ws_transport::connect_async(request, proxy.as_ref()).await
    }
}

impl Default for ChannelNetwork {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ModelHttpTransport for ChannelNetwork {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let transport = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .http
            .clone();
        // The fixed CLI dispatcher uses a 600-second response-header budget
        // per exchange. Body idle ownership remains with each body reader.
        TransportFuture::io(async move {
            tokio::time::timeout(UNDICI_BODY_TIMEOUT, transport.send(request))
                .await
                .unwrap_or_else(|_| {
                    Err(TransportFailure::fetch_failed(Some(
                        "UND_ERR_HEADERS_TIMEOUT",
                    )))
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn proxy_reload_and_removal_keep_explicit_reload_mode() {
        let config = pilotdeck_protocol::parse_json(
            r#"{"url":"http://127.0.0.1:1","noProxy":"fixture.invalid"}"#,
        )
        .unwrap();
        let network = ChannelNetwork::new(Some(&config));
        let startup = network.current.read().unwrap().proxy.clone().unwrap();
        assert_eq!(startup.mode, ProxyInstallMode::Startup);
        network.update_proxy(Some(&config));
        let reloaded = network.current.read().unwrap().proxy.clone().unwrap();
        assert_eq!(reloaded.mode, ProxyInstallMode::Reload);
        assert_eq!(reloaded.no_proxy.as_deref(), Some("fixture.invalid"));
        network.update_proxy(None);
        let removed = network.current.read().unwrap().proxy.clone().unwrap();
        assert_eq!(removed.mode, ProxyInstallMode::Reload);
        assert!(
            removed.url.is_empty(),
            "reload removal must suppress env proxy fallback"
        );
    }

    struct LiteralTransport(&'static str);
    impl ModelHttpTransport for LiteralTransport {
        fn send(&self, _request: Request<Full<Bytes>>) -> TransportFuture {
            TransportFuture::ready(Ok(hyper::Response::new(
                Full::new(Bytes::from_static(self.0.as_bytes()))
                    .map_err(|never| match never {})
                    .boxed(),
            )))
        }
    }

    fn with_http(http: Arc<dyn ModelHttpTransport>) -> ChannelNetwork {
        ChannelNetwork {
            current: RwLock::new(Snapshot { proxy: None, http }),
        }
    }

    #[tokio::test]
    async fn an_http_exchange_uses_the_snapshot_selected_before_reload() {
        let network = with_http(Arc::new(LiteralTransport("before")));
        let prior = network.send(Request::new(Full::new(Bytes::new())));
        network.update_proxy(None);
        // Replace only the new snapshot's HTTP endpoint with a local test
        // transport; no real proxy or HTTP connection is opened by this test.
        network.current.write().unwrap().http = Arc::new(LiteralTransport("after"));
        let next = network.send(Request::new(Full::new(Bytes::new())));
        let prior_body = prior
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let next_body = next
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(prior_body.as_ref(), b"before");
        assert_eq!(next_body.as_ref(), b"after");
    }

    struct PendingTransport(Arc<AtomicBool>);
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl ModelHttpTransport for PendingTransport {
        fn send(&self, _request: Request<Full<Bytes>>) -> TransportFuture {
            let dropped = Dropped(self.0.clone());
            TransportFuture::io(async move {
                let _dropped = dropped;
                std::future::pending().await
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_header_budget_drops_the_owned_http_exchange() {
        let dropped = Arc::new(AtomicBool::new(false));
        let network = with_http(Arc::new(PendingTransport(dropped.clone())));
        let start = tokio::time::Instant::now();
        let error = network
            .send(Request::new(Full::new(Bytes::new())))
            .await
            .expect_err("H1 pending response headers time out");
        assert_eq!(tokio::time::Instant::now() - start, UNDICI_BODY_TIMEOUT);
        assert_eq!(error.cause_code, Some("UND_ERR_HEADERS_TIMEOUT"));
        assert!(
            dropped.load(Ordering::SeqCst),
            "timed out exchange is dropped"
        );
    }
}
