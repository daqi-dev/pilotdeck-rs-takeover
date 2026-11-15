//! Transport extraction in progress; see README for provenance and status.
use crate::executor::ScopeExecutor;
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioIo;
use pilotdeck_protocol::JsValue;
use tower_service::Service;

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub mod encoding;
mod tls;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The caller preserves Dial/Network sources, including trusted host errors.
#[derive(Debug)]
pub enum ExchangeError {
    InvalidTarget,
    Cancelled,
    Dial(BoxError),
    Network(BoxError),
    Encoding(TransportFailure),
}

use pilotdeck_model_runtime::TransportResult;
pub use pilotdeck_model_runtime::{
    BodyFailure, ModelBody, ModelHttpTransport, TransportFailure, TransportFuture,
};

/// Captures the original gateway proxy and CA snapshot once at trusted setup.
#[derive(Clone)]
pub struct NetworkConfig {
    tls: rustls::ClientConfig,
    proxy: Option<ProxyConfig>,
}
impl NetworkConfig {
    pub fn from_settings(settings: Option<&ProxySettings>) -> Self {
        Self {
            tls: webpki_client_config(),
            proxy: ProxyConfig::from_settings(settings),
        }
    }

    /// Check that a trusted caller's registry covers every proxy credential
    /// representation retained by this exact frozen configuration.
    pub fn proxy_secrets_covered(&self, mut covered: impl FnMut(&str) -> bool) -> bool {
        let Some(config) = &self.proxy else {
            return true;
        };
        let Some(proxy) = &config.proxy else {
            return true;
        };
        let mut values = Vec::new();
        for raw in [Some(proxy.username()), proxy.password()]
            .into_iter()
            .flatten()
        {
            if !raw.is_empty() {
                values.push(raw.to_owned());
                if let Ok(decoded) = percent_decode(raw) {
                    values.push(decoded);
                }
            }
        }
        if let Some(authorization) = config.authorization() {
            if let Some(token) = authorization.strip_prefix("Basic ") {
                values.push(token.to_owned());
            }
            values.push(authorization);
        }
        values
            .iter()
            .filter(|value| !value.is_empty())
            .all(|value| covered(value))
    }
}

/// hyper + rustls (ring, TLS 1.2/1.3), HTTP/1.1, `http` and `https`.
#[derive(Clone)]
pub struct HyperModelTransport {
    client: Client<hyper_rustls::HttpsConnector<ProxyConnect>, Full<Bytes>>,
    dialer: Arc<dyn ScopedDial>,
    proxy: Option<ProxyConfig>,
}
impl HyperModelTransport {
    /// Original ambient Gateway transport with bundled/explicit CA sources.
    pub fn new() -> Self {
        Self::for_scope(
            Arc::new(AmbientDial),
            &NetworkConfig::from_settings(None),
            ScopeExecutor::ambient(),
        )
    }
    pub fn with_proxy_settings(settings: Option<&ProxySettings>) -> Self {
        Self::for_scope(
            Arc::new(AmbientDial),
            &NetworkConfig::from_settings(settings),
            ScopeExecutor::ambient(),
        )
    }
    pub fn with_roots(roots: rustls::RootCertStore) -> Self {
        Self::from_config(
            rooted_client_config(roots),
            ProxyConfig::from_settings(None),
        )
    }
    pub fn from_config(config: rustls::ClientConfig, proxy: Option<ProxyConfig>) -> Self {
        Self::with_connector(
            config,
            proxy,
            Arc::new(AmbientDial),
            ScopeExecutor::ambient(),
        )
    }
    /// Create a pool belonging to exactly one host-minted scope.
    pub fn for_scope(
        dialer: Arc<dyn ScopedDial>,
        config: &NetworkConfig,
        executor: ScopeExecutor,
    ) -> Self {
        Self::with_connector(config.tls.clone(), config.proxy.clone(), dialer, executor)
    }
    /// One authorized exchange. The owner chooses how to project structured
    /// dial failures and when to mark dispatch. No Kernel types live here.
    pub async fn send_checked(
        &self,
        request: Request<Full<Bytes>>,
        before_dispatch: impl FnOnce() -> bool,
    ) -> Result<hyper::Response<ModelBody>, ExchangeError> {
        let logical = request.uri().clone();
        let physical = if let Some(proxy) = self.proxy.as_ref().filter(|p| p.should_proxy(&logical))
        {
            let (host, port) = proxy.host_port();
            proxy
                .proxy
                .as_ref()
                .map(|p| proxy_endpoint(p, &host, port))
                .ok_or(ExchangeError::InvalidTarget)?
        } else {
            direct_endpoint(&logical).ok_or(ExchangeError::InvalidTarget)?
        };
        self.dialer
            .authorize(logical, physical)
            .map_err(ExchangeError::Dial)?;
        let method = request.method().clone();
        let follow = request
            .extensions()
            .get::<pilotdeck_model_runtime::fetch_redirect::FollowRedirects>()
            .is_some();
        if !before_dispatch() {
            return Err(ExchangeError::Cancelled);
        }
        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| ExchangeError::Network(Box::new(e)))?;
        encoding::decode_response(
            &method,
            response.map(|body| body.map_err(|_| BodyFailure::Terminated).boxed()),
            follow,
        )
        .map_err(ExchangeError::Encoding)
    }
    fn builder() -> rustls::ConfigBuilder<rustls::ClientConfig, rustls::WantsVerifier> {
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
    }
    fn with_connector(
        config: rustls::ClientConfig,
        proxy: Option<ProxyConfig>,
        dialer: Arc<dyn ScopedDial>,
        executor: ScopeExecutor,
    ) -> Self {
        Self {
            client: Client::builder(executor).build(proxy_https_connector_scoped(
                config,
                proxy.clone(),
                dialer.clone(),
            )),
            dialer,
            proxy,
        }
    }
}

/// One deadline around a connector's whole connect future, DNS and TLS
/// handshake included. undici's `connectTimeout` (10 s by default) spans
/// socket establishment *including* the TLS handshake, while hyper's
/// `HttpConnector::set_connect_timeout` only bounds the TCP phase
/// (`node_modules/undici/lib/core/connect.js:69,126`). Call sites that need
/// undici's semantics wrap the shared connector with this without changing
/// [`https_connector`] itself. hyper-util's `Connect` is blanket-implemented
/// for `tower_service::Service<Uri>`, so this wrapper is the seam.
#[derive(Clone)]
pub struct ConnectDeadline<C> {
    inner: C,
    timeout: Duration,
}
impl<C> ConnectDeadline<C> {
    pub fn new(inner: C, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}
impl<C> tower_service::Service<hyper::Uri> for ConnectDeadline<C>
where
    C: tower_service::Service<hyper::Uri> + Send + Sync + 'static,
    C::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    C::Future: Send + 'static,
{
    type Response = C::Response;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context).map_err(Into::into)
    }
    fn call(&mut self, destination: hyper::Uri) -> Self::Future {
        let timeout = self.timeout;
        let connect = self.inner.call(destination);
        Box::pin(async move {
            match tokio::time::timeout(timeout, connect).await {
                Ok(result) => result.map_err(Into::into),
                // TS reports this as `TypeError: fetch failed` (cause
                // `UND_ERR_CONNECT_TIMEOUT`, probe measured ~10.5 s); callers
                // map the message the same way they map any fetch failure.
                Err(_) => Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "connect timeout",
                ))
                    as Box<dyn std::error::Error + Send + Sync>),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Outgoing HTTP proxy (TS `src/cli/proxy.ts`)
//
// The baseline installs an undici `EnvHttpProxyAgent` as the process-wide
// dispatcher at CLI startup, so every `fetch` (model, hook, MCP) uses it. This
// port keeps the agent semantics and exposes them through [`proxy_https_connector`],
// which all HTTP transports (MCP, model, and hook) now use.
//
// Sources and priority copied from `src/cli/proxy.ts:20-31` and
// `pilotdeck.ts:28,48-51,150`:
// - URL: `PILOTDECK_PROXY > https_proxy > HTTPS_PROXY > http_proxy >
//   HTTP_PROXY`; at startup an env URL wins over `proxy.url`, on hot reload
//   `proxy.url` wins. Empty strings are falsy.
// - noProxy: `no_proxy || NO_PROXY`; the config `noProxy` is appended only on
//   hot reload (`pilotdeck.ts:150`), not by startup's
//   `installGlobalProxy(config.url)`. `buildNoProxy` (`proxy.ts:159-166`) always appends
//   `127.0.0.1` and `localhost`, so loopback MCP endpoints never use a proxy.
//   Entries split on commas or whitespace, an optional `*.`/`.` prefix is
//   stripped, and an entry matches its exact host or a `.<entry>` subdomain,
//   optionally pinned to a port
//   (`node_modules/undici/lib/dispatcher/env-http-proxy-agent.js:82-140`).
//
// HTTP/HTTPS CONNECT and SOCKS5 (including `socks:`) share this connector.
// A structurally invalid proxy fails closed, including NO_PROXY destinations.
// At startup only, otherwise valid URLs with undecodable userinfo leave the
// direct dispatcher active, matching TS; reload failures still fail closed.
//
// Every HTTP/HTTPS-proxied request, including plain `http:` targets, uses
// `CONNECT` because undici's ProxyAgent defaults to `proxyTunnel: true`.
// Proxy credentials come from the URL userinfo and become
// `Proxy-Authorization: Basic base64(decodeURIComponent(user):decodeURIComponent(password))`.

/// The environment keys the gateway's proxy configuration reads, in lookup
/// order: the five URL candidates first, then the `no_proxy` pair. Single
/// source of this list — `tests/common/proxy_env.rs` re-exports it so the
/// child-process env scrubbing cannot drift from what production reads.
pub const PROXY_ENV_KEYS: [&str; 7] = [
    "PILOTDECK_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "http_proxy",
    "HTTP_PROXY",
    "no_proxy",
    "NO_PROXY",
];

/// A rejected CONNECT, shaped like the undici cause chain the TS transport
/// flattens (`fetch failed: <cause>` in the legacy SSE rejection).
#[derive(Debug)]
pub struct ProxyError {
    pub cause: String,
    managed: Option<BoxError>,
}
impl ProxyError {
    fn new(cause: String) -> Self {
        Self {
            cause,
            managed: None,
        }
    }
    fn managed(error: BoxError) -> Self {
        Self {
            cause: "managed connection failed".into(),
            managed: Some(error),
        }
    }
    fn with_source(cause: String, error: BoxError) -> Self {
        Self {
            cause,
            managed: Some(error),
        }
    }
}
impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.cause)
    }
}
impl std::error::Error for ProxyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.managed
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NoProxyEntry {
    hostname: String,
    /// `Number.parseInt(port, 10)` (`env-http-proxy-agent.js:126`): `None` is
    /// JS `0`, which is falsy and therefore matches every port. `Some` must
    /// equal the target port. A numeric overflow stays nonzero, so (like JS's
    /// unbounded Number) it can never equal a 16-bit port.
    port: Option<u64>,
}

/// Proxy sources from `pilotdeck.yaml` (`proxy` in `src/pilot/cli/config`).
///
/// Mirrors `parseProxyConfig` (`src/pilot/config/loadPilotConfig.ts:856-911`):
/// a bare string is a URL, an object needs a non-empty `url`, and `noProxy`
/// is kept only when non-empty. `mode` selects the TS install path:
/// [`ProxyInstallMode::Startup`] is `installGlobalProxy` from `pilotdeck.ts:48`
/// (env URL wins, config is the fallback) and [`ProxyInstallMode::Reload`] is
/// `reinstallGlobalProxy` from `pilotdeck.ts:150` (config URL wins and its
/// `noProxy` is appended to `buildNoProxy`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxySettings {
    pub url: String,
    pub no_proxy: Option<String>,
    pub mode: ProxyInstallMode,
}

/// Which TS install path the config source came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProxyInstallMode {
    /// `installGlobalProxy(config.url)` during `server` startup.
    #[default]
    Startup,
    /// `reinstallGlobalProxy(config.url, config.noProxy)` on config change.
    Reload,
}

impl ProxySettings {
    /// Parse a `proxy` config value (string or `{url, noProxy}`). Empty URLs
    /// are absent, exactly like `parseProxyConfig` and the `if
    /// (snapshot.config.proxy?.url)` guard in `pilotdeck.ts:48`.
    pub fn from_config(value: &JsValue) -> Option<Self> {
        if let Some(url) = value.as_string() {
            let url = url.to_utf8().ok()?;
            return (!url.is_empty()).then(|| Self {
                url,
                ..Self::default()
            });
        }
        let object = value.as_object()?;
        let url = object.get("url")?.as_string()?.to_utf8().ok()?;
        if url.is_empty() {
            return None;
        }
        let no_proxy = object
            .get("noProxy")
            .and_then(|value| value.as_string())
            .and_then(|value| value.to_utf8().ok())
            .filter(|value| !value.is_empty());
        Some(Self {
            url,
            no_proxy,
            mode: ProxyInstallMode::Startup,
        })
    }
}

/// `||` truthiness for a config/env string: `""` is absent.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}
fn non_empty_opt(value: Option<&str>) -> Option<String> {
    value.and_then(non_empty)
}

/// The active proxy (undici `EnvHttpProxyAgent` + `buildNoProxy`).
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    proxy: Option<url::Url>,
    /// The joined value the baseline logs and matches `"*"` against.
    no_proxy: String,
    entries: Vec<NoProxyEntry>,
    /// The CONNECT `Proxy-Authorization` value, built at construction like
    /// `ProxyAgent`'s constructor. `decodeURIComponent` throws on malformed
    /// userinfo, which makes the TS install fail. Keep that error distinct
    /// from invalid URL structure so startup can retain direct transport.
    authorization: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyParseError {
    Configuration,
    Userinfo,
}

impl ProxyConfig {
    /// Testable seam over the same lookup order, so env precedence is pinned
    /// without mutating the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        Self::from_settings_lookup(None, lookup)
    }

    /// Resolve the config source against the env source. `None` or
    /// [`ProxyInstallMode::Startup`] keeps the env URL ahead of config and
    /// ignores `noProxy`; [`ProxyInstallMode::Reload`] is the TS hot-reload
    /// path where the config URL and `noProxy` replace the env ones.
    pub fn from_settings(settings: Option<&ProxySettings>) -> Option<Self> {
        Self::from_settings_lookup(settings, |key| std::env::var(key).ok())
    }

    fn from_settings_lookup(
        settings: Option<&ProxySettings>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let env_url = PROXY_ENV_KEYS
            .iter()
            // The five URL candidates; the `no_proxy` pair is read below.
            .take(5)
            .find_map(|&key| lookup(key).filter(|value| !value.is_empty()));
        let (proxy_url, extra_no_proxy) = match settings {
            None => (env_url?, None),
            Some(settings) if settings.mode == ProxyInstallMode::Reload => {
                // `reinstallGlobalProxy`: config URL is authoritative and its
                // noProxy is passed as `extraNoProxy`.
                let url = non_empty(&settings.url)?;
                (url, non_empty_opt(settings.no_proxy.as_deref()))
            }
            Some(settings) => match env_url {
                // `installGlobalProxy`: an env-sourced dispatcher wins over
                // the first config install (`proxy.ts:64-69`).
                Some(url) => (url, None),
                None => {
                    // `installGlobalProxy(config.url)` only passes the URL;
                    // `config.noProxy` is first honored on the reload path.
                    let url = non_empty(&settings.url)?;
                    (url, None)
                }
            },
        };
        let user_no_proxy = PROXY_ENV_KEYS
            .iter()
            .skip(5)
            .find_map(|&key| lookup(key).filter(|value| !value.is_empty()))
            .unwrap_or_default();
        let parsed = Self::new_with_extra(&proxy_url, &user_no_proxy, extra_no_proxy.as_deref());
        // TS first tries env, then config at startup. An invalid env install
        // must not prevent a valid config from being installed.
        let parsed = parsed.or_else(|error| {
            let Some(settings) = settings.filter(|settings| {
                settings.mode == ProxyInstallMode::Startup && !settings.url.is_empty()
            }) else {
                return Err(error);
            };
            Self::new_with_extra(&settings.url, &user_no_proxy, None).map_err(|fallback| {
                // A failed fallback must not hide a structural env error (or
                // vice versa). Only userinfo-only startup failures go direct.
                if error == ProxyParseError::Userinfo && fallback == ProxyParseError::Userinfo {
                    ProxyParseError::Userinfo
                } else {
                    ProxyParseError::Configuration
                }
            })
        });
        if matches!(parsed, Err(ProxyParseError::Userinfo))
            && settings.is_none_or(|settings| settings.mode == ProxyInstallMode::Startup)
        {
            return None;
        }
        Some(parsed.unwrap_or_else(|_| Self {
            proxy: None,
            no_proxy: String::new(),
            entries: Vec::new(),
            authorization: None,
        }))
    }

    pub fn new(proxy_url: &str, user_no_proxy: &str) -> Option<Self> {
        Self::new_with_extra(proxy_url, user_no_proxy, None).ok()
    }

    fn new_with_extra(
        proxy_url: &str,
        user_no_proxy: &str,
        extra_no_proxy: Option<&str>,
    ) -> Result<Self, ProxyParseError> {
        let proxy = url::Url::parse(proxy_url).map_err(|_| ProxyParseError::Configuration)?;
        if proxy.host_str().is_none()
            || !matches!(proxy.scheme(), "http" | "https" | "socks5" | "socks")
        {
            return Err(ProxyParseError::Configuration);
        }
        if matches!(proxy.scheme(), "http" | "https")
            && (proxy.path() != "/"
                || proxy.query().is_some_and(|value| !value.is_empty())
                || proxy.fragment().is_some_and(|value| !value.is_empty()))
        {
            return Err(ProxyParseError::Configuration);
        }
        let no_proxy = [
            Some(user_no_proxy),
            extra_no_proxy,
            Some("127.0.0.1"),
            Some("localhost"),
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",");
        let mut entries = Vec::new();
        for entry in
            no_proxy.split(|character: char| character == ',' || crate::js_space(character))
        {
            if entry.is_empty() {
                continue;
            }
            // JS `/^(.+):(\d+)$/`: a trailing numeric port, anything else is
            // part of the hostname (`[::1]` stays bracketed). `Number.parseInt`
            // has no 32-bit limit, so an overflowing numeric port must stay a
            // nonzero value that never equals a real port instead of becoming
            // the wildcard `0`.
            let (host, port) = match entry.rsplit_once(':') {
                Some((host, port))
                    if !host.is_empty()
                        && !port.is_empty()
                        && port.bytes().all(|byte| byte.is_ascii_digit()) =>
                {
                    (host, Some(port.parse::<u64>().unwrap_or(u64::MAX)))
                }
                _ => (entry, None),
            };
            let hostname = host
                .strip_prefix("*.")
                .or_else(|| host.strip_prefix('.'))
                .unwrap_or(host)
                .to_ascii_lowercase();
            entries.push(NoProxyEntry {
                hostname,
                // JS `0` is falsy: it matches every port.
                port: port.filter(|port| *port != 0),
            });
        }
        let authorization = proxy_authorization(&proxy).map_err(|_| ProxyParseError::Userinfo)?;
        Ok(Self {
            proxy: Some(proxy),
            no_proxy,
            entries,
            authorization,
        })
    }

    /// `EnvHttpProxyAgent.#shouldProxy` against a hyper URI.
    pub fn should_proxy(&self, uri: &hyper::Uri) -> bool {
        let Some(host) = uri.host() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        let port = u64::from(uri.port_u16().unwrap_or(match uri.scheme_str() {
            Some("https") => 443,
            _ => 80,
        }));
        if self.entries.is_empty() {
            return true;
        }
        if self.no_proxy == "*" {
            return false;
        }
        !self.entries.iter().any(|entry| {
            if entry.port.is_some_and(|entry_port| entry_port != port) {
                return false;
            }
            host == entry.hostname || host.ends_with(&format!(".{}", entry.hostname))
        })
    }

    fn host_port(&self) -> (String, u16) {
        let Some(proxy) = &self.proxy else {
            return (String::new(), 0);
        };
        let host = match proxy.host() {
            Some(url::Host::Ipv6(address)) => address.to_string(),
            Some(url::Host::Ipv4(address)) => address.to_string(),
            Some(url::Host::Domain(name)) => name.to_owned(),
            None => String::new(),
        };
        let port = if matches!(proxy.scheme(), "socks" | "socks5") {
            proxy.port().filter(|port| *port != 0).unwrap_or(1080)
        } else {
            proxy.port_or_known_default().unwrap_or(80)
        };
        (host, port)
    }

    /// `host:port` as Node's socket errors print it (IPv6 without brackets).
    fn address(&self) -> String {
        let (host, port) = self.host_port();
        format!("{host}:{port}")
    }

    /// `Proxy-Authorization` from URL userinfo (`undici/lib/dispatcher/
    /// proxy-agent.js:113-121`); no header when there is no username.
    fn authorization(&self) -> Option<String> {
        self.authorization.clone()
    }

    fn connect_error(&self, error: std::io::Error) -> ProxyError {
        let address = self.address();
        let cause = match error.kind() {
            std::io::ErrorKind::ConnectionRefused => {
                format!("connect ECONNREFUSED {address}")
            }
            std::io::ErrorKind::TimedOut => {
                format!("Connect Timeout Error (attempted address: {address}, timeout: 10000ms)")
            }
            _ if error
                .to_string()
                .contains("failed to lookup address information") =>
            {
                format!("getaddrinfo ENOTFOUND {}", self.host_port().0)
            }
            _ => format!("connect {address}"),
        };
        ProxyError::new(cause)
    }

    fn connect_error_with_source(&self, error: std::io::Error, preserve: bool) -> ProxyError {
        if !preserve {
            return self.connect_error(error);
        }
        let cause = self
            .connect_error(std::io::Error::new(error.kind(), error.to_string()))
            .cause;
        ProxyError::with_source(cause, Box::new(error))
    }

    fn response_error(status: u16) -> ProxyError {
        // eventsource@3 flattens the fetch rejection as
        // `<DOMException name>: <message>: <cause.message>`; the fetch layer
        // produced a "Request was cancelled." DOMException whose cause is the
        // undici ProxyAgent tunnel error.
        ProxyError::new(format!(
            "Error: Request was cancelled.: Proxy response ({status}) !== 200 when HTTP Tunneling"
        ))
    }

    /// Open one `CONNECT` tunnel; the returned stream is the raw tunnel plus
    /// any bytes the proxy already sent after its response headers.
    async fn tunnel(
        &self,
        target: &hyper::Uri,
        tls: Arc<rustls::ClientConfig>,
        dialer: Arc<dyn ScopedDial>,
    ) -> Result<(Box<dyn ProxyIo>, Vec<u8>), ProxyError> {
        let proxy = self.proxy.as_ref().ok_or_else(|| {
            ProxyError::new(
                "Invalid or unsupported proxy configuration; direct connection refused".into(),
            )
        })?;
        let (host, port) = self.host_port();
        let physical = proxy_endpoint(proxy, &host, port);
        let preserve = dialer.preserve_error_source();
        let stream = dialer
            .connect(target.clone(), physical, true)
            .await
            .map_err(|error| match error.downcast::<std::io::Error>() {
                Ok(io) => self.connect_error_with_source(*io, preserve),
                Err(other) => ProxyError::managed(other),
            })?;
        let mut stream: Box<dyn ProxyIo> = if proxy.scheme() == "https" {
            let name = rustls::pki_types::ServerName::try_from(host)
                .map_err(|_| ProxyError::new("Invalid HTTPS proxy hostname".into()))?;
            Box::new(
                tokio_rustls::TlsConnector::from(tls)
                    .connect(name, stream)
                    .await
                    .map_err(|error| {
                        let message = "HTTPS proxy TLS handshake failed".to_string();
                        if preserve {
                            ProxyError::with_source(message, Box::new(error))
                        } else {
                            ProxyError::new(message)
                        }
                    })?,
            )
        } else {
            stream
        };
        if matches!(proxy.scheme(), "socks5" | "socks") {
            crate::proxy_socks::connect(&mut stream, proxy, target)
                .await
                .map_err(|error| {
                    let message = error.to_string();
                    if preserve {
                        ProxyError::with_source(message, Box::new(error))
                    } else {
                        ProxyError::new(message)
                    }
                })?;
            return Ok((stream, Vec::new()));
        }
        let (authority, host_header) = connect_target(target);
        let mut request = format!(
            "CONNECT {authority} HTTP/1.1\r\nhost: {host_header}\r\nproxy-connection: keep-alive\r\n"
        );
        if let Some(auth) = self.authorization() {
            request.push_str(&format!("proxy-authorization: {auth}\r\n"));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|error| self.connect_error_with_source(error, preserve))?;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            if let Some(at) = find_header_end(&buffer) {
                break at;
            }
            if buffer.len() > 64 * 1024 {
                return Err(ProxyError::new(
                    "Proxy response (0) !== 200 when HTTP Tunneling".into(),
                ));
            }
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|error| self.connect_error_with_source(error, preserve))?;
            if read == 0 {
                return Err(ProxyError::new(
                    "Proxy response (0) !== 200 when HTTP Tunneling".into(),
                ));
            }
            buffer.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]);
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse::<u16>().ok())
            .unwrap_or(0);
        if status != 200 {
            return Err(Self::response_error(status));
        }
        Ok((stream, buffer[header_end + 4..].to_vec()))
    }
}

/// `CONNECT` target and `host` header for one URL, matching undici
/// `proxy-agent.js:183-196`: `opts.host` is the normalized `URL.host` (default
/// port omitted, IPv6 brackets kept), `requestedPath` appends `:defaultPort`
/// only when the URL had no port, and the header is `opts.host` unchanged.
fn connect_target(target: &hyper::Uri) -> (String, String) {
    let host = target.host().unwrap_or_default();
    let default_port = match target.scheme_str() {
        Some("https") => 443,
        _ => 80,
    };
    let port = target.port_u16().unwrap_or(default_port);
    let authority = format!("{host}:{port}");
    let header = if port == default_port {
        host.to_owned()
    } else {
        authority.clone()
    };
    (authority, header)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// `ProxyAgent`'s constructor header: `Basic base64(decodeURIComponent(user):
/// decodeURIComponent(password))`, or `None` for no username. An invalid
/// escape or a percent-decoded byte sequence that is not UTF-8 makes
/// `decodeURIComponent` throw `URIError: URI malformed` in TS, which aborts
/// the whole proxy install. The caller chooses startup direct fallback or
/// reload fail-closed; malformed credentials are never sent to a proxy.
fn proxy_authorization(proxy: &url::Url) -> Result<Option<String>, ()> {
    let username = proxy.username();
    if username.is_empty() {
        return Ok(None);
    }
    let username = percent_decode(username)?;
    let password = match proxy.password() {
        Some(password) => percent_decode(password)?,
        None => String::new(),
    };
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    Ok(Some(format!("Basic {token}")))
}

/// `decodeURIComponent` for proxy userinfo: `%XX` escapes are decoded and the
/// result must be valid UTF-8. Malformed escapes (`%ZZ`, a truncated `%A`) and
/// invalid UTF-8 (`%FF`) are errors, matching the `URIError` the TS install
/// raises (`undici/lib/dispatcher/proxy-agent.js:129,131`).
fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_digit),
                bytes.get(index + 2).copied().and_then(hex_digit),
            ) else {
                return Err(());
            };
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Socket bytes already read past the CONNECT response, then the socket.
/// Both the tcp and proxy paths return this same type so
/// `HttpsConnector` can wrap one connector type.
pub struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    position: usize,
}
impl<S> PrefixedStream<S> {
    fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self {
            inner,
            prefix,
            position: 0,
        }
    }
}
impl<S: hyper::rt::Read + Unpin> hyper::rt::Read for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        mut buffer: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() {
            let count = (this.prefix.len() - this.position).min(buffer.remaining());
            buffer.put_slice(&this.prefix[this.position..this.position + count]);
            this.position += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(context, buffer)
    }
}
impl<S> hyper_util::client::legacy::connect::Connection for PrefixedStream<S> {
    fn connected(&self) -> hyper_util::client::legacy::connect::Connected {
        hyper_util::client::legacy::connect::Connected::new()
    }
}
impl<S: hyper::rt::Write + Unpin> hyper::rt::Write for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(context, buffer)
    }
    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

/// TCP connector that delegates every direct or proxy socket to a
/// scope-bound managed dialer. `HttpsConnector` terminates target TLS over
/// the direct stream or the established proxy tunnel.
pub trait ProxyIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ProxyIo for T {}

/// Scoped transport provided only by trusted composition. It owns authorization
/// and actual DNS/socket establishment; no ambient fallback is permitted.
/// API is provisional until the kernel bridge is finalized.
/// Route authority and stream establishment supplied by the owning host.
/// Owned asynchronous result of opening one injected TCP stream.
pub type DialFuture = Pin<Box<dyn Future<Output = Result<Box<dyn ProxyIo>, BoxError>> + Send>>;

pub trait ScopedDial: Send + Sync {
    /// Managed dials retain nested error sources for policy classification.
    /// Ambient Gateway keeps its original proxy error chain.
    fn preserve_error_source(&self) -> bool {
        false
    }
    /// Recheck before each dispatch, including a pooled-connection reuse.
    fn authorize(&self, logical: hyper::Uri, physical_endpoint: String) -> Result<(), BoxError>;
    fn connect(&self, logical: hyper::Uri, physical_endpoint: String, proxy: bool) -> DialFuture;
}

fn endpoint(scheme: &str, host: &str, port: u16) -> String {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!("{scheme}://{host}:{port}")
}
fn proxy_endpoint(proxy: &url::Url, host: &str, port: u16) -> String {
    endpoint(proxy.scheme(), host, port)
}
fn direct_endpoint(target: &hyper::Uri) -> Option<String> {
    let host = target.host()?;
    let scheme = target.scheme_str()?;
    let port = target
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some(endpoint(scheme, host, port))
}

#[derive(Clone)]
pub struct ProxyConnect {
    dialer: Arc<dyn ScopedDial>,
    proxy: Option<Arc<ProxyConfig>>,
    tls: Arc<rustls::ClientConfig>,
}
impl tower_service::Service<hyper::Uri> for ProxyConnect {
    type Response = PrefixedStream<TokioIo<Box<dyn ProxyIo>>>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, destination: hyper::Uri) -> Self::Future {
        let dialer = self.dialer.clone();
        if let Some(proxy) = self
            .proxy
            .clone()
            .filter(|proxy| proxy.should_proxy(&destination))
        {
            let tls = self.tls.clone();
            return Box::pin(async move {
                let (stream, prefix) = tokio::time::timeout(
                    Duration::from_secs(10),
                    proxy.tunnel(&destination, tls, dialer),
                )
                .await
                .map_err(|_| proxy.connect_error(std::io::ErrorKind::TimedOut.into()))?
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
                Ok(PrefixedStream::new(TokioIo::new(stream), prefix))
            });
        }
        Box::pin(async move {
            let physical = direct_endpoint(&destination).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing target authority")
            })?;
            // The managed dialer must preserve the original direct connector's
            // TCP-phase timeout; do not add an outer DNS/TLS deadline here.
            let stream = dialer.connect(destination, physical, false).await?;
            Ok(PrefixedStream::new(TokioIo::new(stream), Vec::new()))
        })
    }
}

pub fn proxy_https_connector_scoped(
    config: rustls::ClientConfig,
    proxy: Option<ProxyConfig>,
    dialer: Arc<dyn ScopedDial>,
) -> hyper_rustls::HttpsConnector<ProxyConnect> {
    let tls = Arc::new(config.clone());
    hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(ProxyConnect {
            tls,
            dialer,
            proxy: proxy.map(Arc::new),
        })
}

/// Gateway's original ambient connector; retained for existing MCP, hook and
/// count transports. The managed model host uses the scoped variant above.
pub fn proxy_https_connector(
    config: rustls::ClientConfig,
    proxy: Option<ProxyConfig>,
) -> hyper_rustls::HttpsConnector<ProxyConnect> {
    proxy_https_connector_scoped(config, proxy, Arc::new(AmbientDial))
}

/// Original gateway direct connector, also retained for its existing callers.
pub fn https_connector(
    config: rustls::ClientConfig,
) -> hyper_rustls::HttpsConnector<HttpConnector> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(10)));
    hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(http)
}

#[derive(Clone)]
pub struct AmbientDial;
impl ScopedDial for AmbientDial {
    fn authorize(&self, _: hyper::Uri, _: String) -> Result<(), BoxError> {
        Ok(())
    }
    fn connect(&self, logical: hyper::Uri, physical: String, proxy: bool) -> DialFuture {
        Box::pin(async move {
            if proxy {
                let url = url::Url::parse(&physical)?;
                let host = url.host_str().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing proxy host")
                })?;
                let port = url.port_or_known_default().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing proxy port")
                })?;
                let stream = tokio::net::TcpStream::connect((
                    host.trim_start_matches('[').trim_end_matches(']'),
                    port,
                ))
                .await?;
                return Ok(Box::new(stream) as Box<dyn ProxyIo>);
            }
            let mut connector = HttpConnector::new();
            connector.enforce_http(false);
            connector.set_connect_timeout(Some(Duration::from_secs(10)));
            std::future::poll_fn(|cx| connector.poll_ready(cx)).await?;
            let stream = connector.call(logical).await?;
            Ok(Box::new(stream.into_inner()) as Box<dyn ProxyIo>)
        })
    }
}

/// Shared default trust configuration, including explicit extra CA sources.
pub fn webpki_client_config() -> rustls::ClientConfig {
    tls::client_config()
}
/// rustls client config trusting exactly `roots`.
pub fn rooted_client_config(roots: rustls::RootCertStore) -> rustls::ClientConfig {
    HyperModelTransport::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
}
impl Default for HyperModelTransport {
    fn default() -> Self {
        Self::new()
    }
}
impl ModelHttpTransport for HyperModelTransport {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let client = self.client.clone();
        let dialer = self.dialer.clone();
        let proxy = self.proxy.clone();
        let method = request.method().clone();
        let follow_redirects = request
            .extensions()
            .get::<pilotdeck_model_runtime::fetch_redirect::FollowRedirects>()
            .is_some();
        TransportFuture::io(async move {
            let logical = request.uri().clone();
            let physical = if let Some(proxy) = proxy.as_ref().filter(|p| p.should_proxy(&logical))
            {
                let (host, port) = proxy.host_port();
                proxy
                    .proxy
                    .as_ref()
                    .map(|p| proxy_endpoint(p, &host, port))
                    .ok_or_else(|| TransportFailure::fetch_failed(None))?
            } else {
                direct_endpoint(&logical).ok_or_else(|| TransportFailure::fetch_failed(None))?
            };
            dialer
                .authorize(logical, physical)
                .map_err(|_| TransportFailure::fetch_failed(None))?;
            let response = client
                .request(request)
                .await
                .map_err(|error| TransportFailure::fetch_failed(cause_code(&error)))?;
            encoding::decode_response(
                &method,
                response.map(|body| body.map_err(|_| BodyFailure::Terminated).boxed()),
                follow_redirects,
            )
        })
    }
}

/// undici's `cause.code` for a hyper/rustls/io failure chain.
pub fn cause_code(error: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = current {
        if let Some(tls) = error.downcast_ref::<rustls::Error>() {
            return Some(match tls {
                rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName)
                | rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForNameContext { .. },
                ) => "ERR_TLS_CERT_ALTNAME_INVALID",
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer) => {
                    "UNABLE_TO_GET_ISSUER_CERT_LOCALLY"
                }
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::Expired
                    | rustls::CertificateError::ExpiredContext { .. },
                ) => "CERT_HAS_EXPIRED",
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidYet
                    | rustls::CertificateError::NotValidYetContext { .. },
                ) => "CERT_NOT_YET_VALID",
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::InvalidPurpose
                    | rustls::CertificateError::InvalidPurposeContext { .. },
                ) => "INVALID_PURPOSE",
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature) => {
                    "CERT_SIGNATURE_FAILURE"
                }
                rustls::Error::InvalidCertificate(_) => "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
                _ => "ERR_SSL_PROTOCOL_ERROR",
            });
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if let Some(code) = io.get_ref().and_then(|inner| {
                let inner: &(dyn std::error::Error + 'static) = inner;
                cause_code(inner)
            }) {
                return Some(code);
            }
            return match io.kind() {
                std::io::ErrorKind::ConnectionRefused => Some("ECONNREFUSED"),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
                    Some("ECONNRESET")
                }
                std::io::ErrorKind::TimedOut => Some("ETIMEDOUT"),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe => {
                    Some("UND_ERR_SOCKET")
                }
                _ if io.to_string().contains("failed to lookup address") => Some("ENOTFOUND"),
                _ => None,
            };
        }
        if let Some(hyper) = error.downcast_ref::<hyper::Error>()
            && (hyper.is_incomplete_message() || hyper.is_closed())
        {
            return Some("UND_ERR_SOCKET");
        }
        current = error.source();
    }
    None
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod extraction_tests {
    use super::*;

    #[derive(Debug)]
    struct Denied;
    impl std::fmt::Display for Denied {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("scope denied")
        }
    }
    impl std::error::Error for Denied {}
    struct DeniedDuringTunnel;
    impl tokio::io::AsyncRead for DeniedDuringTunnel {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other(Denied)))
        }
    }
    impl tokio::io::AsyncWrite for DeniedDuringTunnel {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            b: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(b.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    struct TestDial;
    impl ScopedDial for TestDial {
        fn preserve_error_source(&self) -> bool {
            true
        }
        fn authorize(&self, _: hyper::Uri, _: String) -> Result<(), BoxError> {
            Ok(())
        }
        fn connect(&self, _: hyper::Uri, _: String, _: bool) -> DialFuture {
            Box::pin(async { Ok(Box::new(DeniedDuringTunnel) as Box<dyn ProxyIo>) })
        }
    }
    #[tokio::test]
    async fn tunnel_read_keeps_the_original_denial_in_the_error_chain() {
        let proxy = ProxyConfig::new("http://proxy.test:3128", "").unwrap();
        let mut connector =
            proxy_https_connector_scoped(webpki_client_config(), Some(proxy), Arc::new(TestDial));
        let error = match connector
            .call("http://provider.test/".parse().unwrap())
            .await
        {
            Ok(_) => panic!("test tunnel unexpectedly succeeded"),
            Err(error) => error,
        };
        fn contains_denied(error: &(dyn std::error::Error + 'static)) -> bool {
            if error.downcast_ref::<Denied>().is_some() {
                return true;
            }
            if let Some(io) = error.downcast_ref::<std::io::Error>()
                && io.get_ref().is_some_and(|inner| contains_denied(inner))
            {
                return true;
            }
            error.source().is_some_and(contains_denied)
        }
        if contains_denied(error.as_ref()) {
            return;
        }
        panic!("CONNECT denial was lost from the error chain");
    }

    #[test]
    fn ambient_proxy_error_keeps_original_absent_source() {
        use std::error::Error;
        let proxy = ProxyConfig::new("http://proxy.test:3128", "").unwrap();
        let error =
            proxy.connect_error_with_source(std::io::ErrorKind::ConnectionRefused.into(), false);
        assert!(error.source().is_none());
    }

    #[test]
    fn physical_endpoints_keep_scheme_and_omit_proxy_credentials() {
        let proxy = url::Url::parse("socks5://user:password@[::1]:1080").unwrap();
        assert_eq!(proxy_endpoint(&proxy, "::1", 1080), "socks5://[::1]:1080");
        let uri: hyper::Uri = "https://example.test/path?q=secret".parse().unwrap();
        assert_eq!(
            direct_endpoint(&uri).as_deref(),
            Some("https://example.test:443")
        );
    }
}

// Migrated from Gateway runtime/model/http.rs; keep its original coverage.
#[cfg(test)]
mod proxy_tests {
    use super::*;
    use pilotdeck_protocol::parse_json;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn proxy_env_priority_matches_the_ts_cli() {
        // `PILOTDECK_PROXY > https_proxy > HTTPS_PROXY > http_proxy > HTTP_PROXY`
        let config = ProxyConfig::from_lookup(lookup(&[
            ("HTTP_PROXY", "http://http:1"),
            ("http_proxy", "http://http-lower:2"),
            ("HTTPS_PROXY", "http://https-upper:3"),
            ("https_proxy", "http://https-lower:4"),
            ("PILOTDECK_PROXY", "http://pilotdeck:5"),
        ]))
        .expect("proxy configured");
        assert_eq!(config.host_port(), ("pilotdeck".into(), 5));

        let config = ProxyConfig::from_lookup(lookup(&[
            ("HTTP_PROXY", "http://http:1"),
            ("http_proxy", "http://http-lower:2"),
            ("HTTPS_PROXY", "http://https-upper:3"),
            ("https_proxy", "http://https-lower:4"),
        ]))
        .expect("proxy configured");
        assert_eq!(config.host_port(), ("https-lower".into(), 4));

        let config = ProxyConfig::from_lookup(lookup(&[
            ("HTTP_PROXY", "http://http:1"),
            ("http_proxy", "http://http-lower:2"),
            ("HTTPS_PROXY", "http://https-upper:3"),
        ]))
        .expect("proxy configured");
        assert_eq!(config.host_port(), ("https-upper".into(), 3));

        // Empty values are falsy in `env.X || env.Y` and fall through.
        let config = ProxyConfig::from_lookup(lookup(&[
            ("HTTP_PROXY", "http://http:1"),
            ("HTTPS_PROXY", ""),
            ("PILOTDECK_PROXY", ""),
        ]))
        .expect("proxy configured");
        assert_eq!(config.host_port(), ("http".into(), 1));

        assert!(ProxyConfig::from_lookup(lookup(&[])).is_none());
        // A failed installation stays configured and fails closed (#676).
        let invalid = ProxyConfig::from_lookup(lookup(&[("HTTP_PROXY", "127.0.0.1:1")])).unwrap();
        assert!(invalid.proxy.is_none());
        assert!(invalid.should_proxy(&"http://127.0.0.1/".parse().unwrap()));
    }

    #[test]
    fn proxy_config_source_priority_matches_ts_install_paths() {
        let settings = |url: &str, no_proxy: Option<&str>, mode| ProxySettings {
            url: url.into(),
            no_proxy: no_proxy.map(Into::into),
            mode,
        };
        let env = lookup(&[("HTTP_PROXY", "http://env:1"), ("no_proxy", "env.example")]);

        // Startup: the env dispatcher wins and the config source (including
        // its noProxy) is not installed (`proxy.ts:64-69`).
        let config = ProxyConfig::from_settings_lookup(
            Some(&settings(
                "http://config:9",
                Some("config.example"),
                ProxyInstallMode::Startup,
            )),
            env,
        )
        .expect("env proxy");
        assert_eq!(config.host_port(), ("env".into(), 1));
        assert_eq!(config.no_proxy, "env.example,127.0.0.1,localhost");

        // Startup: without an env URL the config URL is the fallback, but the
        // config noProxy is not passed to `installGlobalProxy(config.url)`.
        let config = ProxyConfig::from_settings_lookup(
            Some(&settings(
                "http://config:9",
                Some("config.example"),
                ProxyInstallMode::Startup,
            )),
            lookup(&[("HTTP_PROXY", ""), ("no_proxy", "env.example")]),
        )
        .expect("config proxy");
        assert_eq!(config.host_port(), ("config".into(), 9));
        assert_eq!(config.no_proxy, "env.example,127.0.0.1,localhost");

        // Reload: `reinstallGlobalProxy` replaces the env dispatcher and
        // appends the config noProxy (`pilotdeck.ts:150`).
        let config = ProxyConfig::from_settings_lookup(
            Some(&settings(
                "http://config:9",
                Some("config.example"),
                ProxyInstallMode::Reload,
            )),
            lookup(&[("HTTP_PROXY", "http://env:1"), ("no_proxy", "env.example")]),
        )
        .expect("reloaded config proxy");
        assert_eq!(config.host_port(), ("config".into(), 9));
        assert_eq!(
            config.no_proxy,
            "env.example,config.example,127.0.0.1,localhost"
        );

        // Reload with no config URL removes the proxy (`reinstallGlobalProxy`
        // applies the direct dispatcher).
        assert!(
            ProxyConfig::from_settings_lookup(
                Some(&settings("", None, ProxyInstallMode::Reload)),
                lookup(&[("HTTP_PROXY", "http://env:1")]),
            )
            .is_none()
        );
    }

    #[test]
    fn proxy_settings_from_config_match_parse_proxy_config() {
        let parse = |text: &str| ProxySettings::from_config(&parse_json(text).unwrap());
        assert_eq!(
            parse("\"http://proxy:1\""),
            Some(ProxySettings {
                url: "http://proxy:1".into(),
                no_proxy: None,
                mode: ProxyInstallMode::Startup,
            })
        );
        assert_eq!(
            parse("{\"url\":\"http://proxy:2\",\"noProxy\":\"a.example,b.example\"}"),
            Some(ProxySettings {
                url: "http://proxy:2".into(),
                no_proxy: Some("a.example,b.example".into()),
                mode: ProxyInstallMode::Startup,
            })
        );
        // Empty strings are absent, exactly like `parseProxyConfig`.
        assert_eq!(parse("{\"url\":\"\"}"), None);
        assert_eq!(parse("{}"), None);
        assert_eq!(
            parse("{\"url\":\"http://p\",\"noProxy\":\"\"}"),
            Some(ProxySettings {
                url: "http://p".into(),
                no_proxy: None,
                mode: ProxyInstallMode::Startup,
            })
        );
        assert_eq!(parse("42"), None);
    }

    #[test]
    fn no_proxy_parsing_and_matching_match_undici() {
        let config = ProxyConfig::new("http://proxy:3128", "").expect("config");
        assert_eq!(config.no_proxy, "127.0.0.1,localhost");
        assert!(config.should_proxy(&"http://example.com/".parse().unwrap()));
        assert!(!config.should_proxy(&"http://127.0.0.1:9/".parse().unwrap()));
        assert!(!config.should_proxy(&"https://LOCALHOST/".parse().unwrap()));
        assert!(!config.should_proxy(&"http://sub.localhost/".parse().unwrap()));

        let config = ProxyConfig::new(
            "http://proxy:3128",
            " example.com:8080,\t.internal\n*.corp,127.0.0.9:1 ",
        )
        .expect("config");
        assert_eq!(
            config.no_proxy,
            " example.com:8080,\t.internal\n*.corp,127.0.0.9:1 ,127.0.0.1,localhost"
        );
        let bypass = |uri: &str| !config.should_proxy(&uri.parse().unwrap());
        assert!(bypass("http://example.com:8080/"));
        assert!(!bypass("http://example.com:8081/"));
        assert!(bypass("http://a.example.com:8080/"));
        assert!(bypass("http://a.internal/"));
        assert!(bypass("http://a.corp/"));
        assert!(bypass("http://corp/"));
        assert!(!bypass("http://notcorp/"));
        assert!(bypass("http://127.0.0.9:1/"));
        assert!(!bypass("http://127.0.0.9:2/"));

        // TS `buildNoProxy` joins entries, so a user `*` never equals `"*"`;
        // the wildcard branch is unreachable through the env path. Pin it.
        let config = ProxyConfig::new("http://proxy", "*").expect("config");
        assert_eq!(config.no_proxy, "*,127.0.0.1,localhost");
        assert!(config.should_proxy(&"http://example.com/".parse().unwrap()));
    }

    #[test]
    fn proxy_auth_uses_basic_credentials_from_the_url() {
        let config = ProxyConfig::new("http://us%20er:p%40ss@127.0.0.1:3128", "").expect("config");
        assert_eq!(
            config.authorization().as_deref(),
            Some("Basic dXMgZXI6cEBzcw==") // base64("us er:p@ss")
        );
        let username_only = ProxyConfig::new("http://user@127.0.0.1:3128", "").expect("config");
        assert_eq!(
            username_only.authorization().as_deref(),
            Some("Basic dXNlcjo=") // base64("user:")
        );
        let anonymous = ProxyConfig::new("http://127.0.0.1:3128", "").expect("config");
        assert!(anonymous.authorization().is_none());
    }

    #[test]
    fn no_proxy_port_parsing_matches_number_parse_int() {
        // `example.com:4294967296` is a truthy number that never equals a real
        // target port, so the entry must not bypass; the old `u32` overflow
        // to 0 made it a wildcard.
        let config = ProxyConfig::new(
            "http://proxy:3128",
            "example.com:4294967296,big.test:999999999999999999999999999999",
        )
        .expect("config");
        for uri in [
            "http://example.com/",
            "http://example.com:8080/",
            "http://example.com:65535/",
            "http://big.test:8080/",
        ] {
            assert!(
                config.should_proxy(&uri.parse().unwrap()),
                "{uri} must go through the proxy"
            );
        }

        // `key:0` is falsy in `if (entry.port && ...)`, so it is the wildcard
        // form and bypasses every port of that host.
        let config = ProxyConfig::new("http://proxy:3128", "example.com:0").expect("config");
        for uri in ["http://example.com/", "http://example.com:8080/"] {
            assert!(
                !config.should_proxy(&uri.parse().unwrap()),
                "{uri} must bypass (port 0 wildcard)"
            );
        }

        // Anything failing `/^(.+):(\d+)$/` is part of the hostname: none of
        // these entries can match `example.com`.
        let config = ProxyConfig::new(
            "http://proxy:3128",
            "example.com:-1,example.com:abc,example.com:",
        )
        .expect("config");
        for uri in ["http://example.com/", "http://example.com:8080/"] {
            assert!(
                config.should_proxy(&uri.parse().unwrap()),
                "{uri} must go through the proxy"
            );
        }
    }

    #[tokio::test]
    async fn invalid_proxy_never_opens_even_a_no_proxy_target() {
        use tower_service::Service;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}/", listener.local_addr().unwrap());
        for url in [
            "not a url",
            "ftp://127.0.0.1:9",
            "http://127.0.0.1:9/path",
            "https://127.0.0.1:9/?q",
            "http://127.0.0.1:9/#fragment",
        ] {
            for mode in [ProxyInstallMode::Startup, ProxyInstallMode::Reload] {
                let settings = ProxySettings {
                    url: url.into(),
                    mode,
                    no_proxy: Some("*".into()),
                };
                let proxy = ProxyConfig::from_settings_lookup(Some(&settings), |_| None);
                let mut connector = proxy_https_connector(webpki_client_config(), proxy);
                let result = connector.call(target.parse().unwrap()).await;
                let error = match result {
                    Ok(_) => panic!("invalid proxy opened target: {url}"),
                    Err(error) => error,
                };
                assert!(
                    error
                        .to_string()
                        .contains("Invalid or unsupported proxy configuration"),
                    "{url}: {error}"
                );
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn proxy_userinfo_install_paths_replay_ts_matrix() {
        use tower_service::Service;
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/mcp-proxy-schemes/userinfo-oracle.json"
        ))
        .unwrap();
        assert_eq!(
            oracle["sourceCommit"],
            "cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"
        );
        let cases = oracle["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 61);
        for case in cases {
            let id = case["id"].as_str().unwrap();
            let fill = |value: &str| value.replace("<proxy>", "127.0.0.1:9");
            let settings = case["config"].as_str().map(|url| ProxySettings {
                url: fill(url),
                no_proxy: None,
                mode: if case["mode"] == "reload" {
                    ProxyInstallMode::Reload
                } else {
                    ProxyInstallMode::Startup
                },
            });
            let proxy = ProxyConfig::from_settings_lookup(settings.as_ref(), |key| {
                (key == case["envKey"].as_str().unwrap_or("PILOTDECK_PROXY"))
                    .then(|| case["env"].as_str().map(fill))
                    .flatten()
            });
            // Explicit differences from the recorded TS behavior, documented
            // in A6.5-MCP-PROXY-FALLBACK-001-R1: structural failures and reload
            // must fail closed. Neither can hide behind a userinfo error.
            let fail_closed = matches!(
                id,
                "bad-env-structural-config"
                    | "structural-env-bad-config"
                    | "valid-env-bad-reload"
                    | "bad-userinfo-invalid-path"
                    | "bad-userinfo-unsupported"
            );
            if fail_closed {
                assert!(proxy.as_ref().is_some_and(|p| p.proxy.is_none()), "{id}");
            } else if case["ts"]["state"] == "proxy" {
                let expected =
                    url::Url::parse(&fill(case["ts"]["proxyUrl"].as_str().unwrap())).unwrap();
                assert_eq!(
                    proxy.as_ref().and_then(|p| p.proxy.as_ref()),
                    Some(&expected),
                    "{id}"
                );
            } else {
                assert!(proxy.is_none(), "{id}: TS kept direct transport");
            }

            // Exercise the production connector, not just the parsing seam.
            // A loopback destination still catches accidentally fail-closed
            // userinfo and accidentally direct structural/reload failures.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = format!("http://{}/", listener.local_addr().unwrap());
            let mut connector = proxy_https_connector(webpki_client_config(), proxy);
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                connector.call(target.parse().unwrap()),
            )
            .await
            .expect("local connector deadline");
            if fail_closed {
                let error = match result {
                    Ok(_) => panic!("{id}: rejected configuration opened target"),
                    Err(error) => error,
                };
                assert!(
                    error
                        .to_string()
                        .contains("Invalid or unsupported proxy configuration"),
                    "{id}: {error}"
                );
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), listener.accept())
                        .await
                        .is_err(),
                    "{id}: target received connection"
                );
            } else {
                assert!(result.is_ok(), "{id}: TS direct connection must succeed");
                tokio::time::timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .expect("target receipt deadline")
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn proxy_handshake_has_a_whole_connect_deadline() {
        use tower_service::Service;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut connector = proxy_https_connector(
            webpki_client_config(),
            ProxyConfig::new(&format!("http://{address}"), ""),
        );
        let request =
            tokio::spawn(async move { connector.call("http://mcp.test/".parse().unwrap()).await });
        let (_stalled, _) = listener.accept().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        let result = tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .expect("proxy handshake deadline missing")
            .unwrap();
        let error = match result {
            Ok(_) => panic!("stalled proxy connected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("Connect Timeout Error"),
            "{error}"
        );
    }

    #[test]
    fn proxy_scheme_defaults_and_invalid_origins() {
        for (url, port) in [
            ("http://proxy", 80),
            ("http://proxy:0", 0),
            ("https://proxy", 443),
            ("https://proxy:0", 0),
            ("socks5://proxy", 1080),
            ("socks://proxy:0", 1080),
        ] {
            assert_eq!(
                ProxyConfig::new(url, "").unwrap().host_port().1,
                port,
                "{url}"
            );
        }
        for url in [
            "http://proxy/path",
            "https://proxy/?q",
            "http://proxy/#frag",
            "ftp://proxy",
            "not a url",
        ] {
            assert!(ProxyConfig::new(url, "").is_none(), "{url}");
        }
        for url in [
            "http://proxy/?#",
            "socks5://proxy/path",
            "socks://proxy/?q#frag",
        ] {
            assert!(ProxyConfig::new(url, "").is_some(), "{url}");
        }
    }

    #[test]
    fn proxy_auth_decode_matches_decode_uri_component() {
        let authorization = |url: &str| {
            ProxyConfig::new(url, "").map(|config| config.authorization().unwrap_or_default())
        };
        assert_eq!(
            authorization("http://us%20er:p%40ss@127.0.0.1:3128").as_deref(),
            Some("Basic dXMgZXI6cEBzcw==")
        );
        // `%2F` and a multi-byte UTF-8 sequence decode like
        // decodeURIComponent before Base64.
        assert_eq!(
            authorization("http://u%2Fser:p%C3%A4ss@127.0.0.1:3128").as_deref(),
            Some("Basic dS9zZXI6cMOkc3M=")
        );
        // `%ZZ`, a truncated escape and `%FF` (invalid UTF-8) throw
        // `URIError: URI malformed` in TS, failing the whole install; here the
        // config is absent, so no proxy and no credentials are used.
        assert!(authorization("http://user%ZZ:pass@127.0.0.1:3128").is_none());
        assert!(authorization("http://user%2:pass@127.0.0.1:3128").is_none());
        assert!(authorization("http://user:%FF@127.0.0.1:3128").is_none());
        assert!(authorization("http://user:pass%FF@127.0.0.1:3128").is_none());
    }

    #[test]
    fn connect_target_uses_the_normalized_url_host() {
        let target = |uri: &str| {
            let uri: hyper::Uri = uri.parse().unwrap();
            connect_target(&uri)
        };
        assert_eq!(
            target("http://example.com/"),
            ("example.com:80".into(), "example.com".into())
        );
        // An explicit default port is normalized away from `URL.host`.
        assert_eq!(
            target("http://example.com:80/"),
            ("example.com:80".into(), "example.com".into())
        );
        assert_eq!(
            target("http://example.com:8080/"),
            ("example.com:8080".into(), "example.com:8080".into())
        );
        assert_eq!(
            target("https://example.com/"),
            ("example.com:443".into(), "example.com".into())
        );
        assert_eq!(target("http://[::1]/"), ("[::1]:80".into(), "[::1]".into()));
    }
}
