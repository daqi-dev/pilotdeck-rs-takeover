//! Native fetch boundary for the setup wizard; the trait keeps external traffic
//! out of feature tests without adding environment variables to the CLI.
use super::{JsValue, log};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request};
use pilotdeck_gateway::channel::weixin::{
    WeixinLoginCallbacks, WeixinLoginResult, default_login_base_url, login_with_qr,
};
use pilotdeck_gateway::runtime::model::http::{
    HyperModelTransport, ModelHttpTransport, TransportFailure, TransportFuture, fetch,
};
use std::time::{Duration, Instant};

// pilotdeck.ts:28 installs cli/proxy.ts:createLongTimeoutOptions before setup:
// both undici budgets are 600_000 ms. A redirect starts a fresh header budget;
// bodyTimeout is the idle interval between individual body frames.
const TRANSPORT_TIMEOUT: Duration = Duration::from_secs(600);

struct SetupTransport<T>(T);
impl<T: ModelHttpTransport> ModelHttpTransport for SetupTransport<T> {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let exchange = self.0.send(request);
        TransportFuture::io(async move {
            tokio::time::timeout(TRANSPORT_TIMEOUT, exchange)
                .await
                .map_err(|_| TransportFailure::fetch_failed(None))?
        })
    }
}

pub(super) struct Response {
    pub status: u16,
    pub body: String,
}

pub(super) trait SetupNetwork {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        require_ok: bool,
    ) -> Result<Response, String>;
    fn weixin_login(&mut self) -> Result<WeixinLoginResult, String>;
    fn now(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
}

pub(super) struct HttpNetwork {
    start: Instant,
}

impl HttpNetwork {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

impl SetupNetwork for HttpNetwork {
    fn request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &str,
        require_ok: bool,
    ) -> Result<Response, String> {
        runtime()?.block_on(fetch_json(method, url, headers, body, require_ok))
    }

    fn weixin_login(&mut self) -> Result<WeixinLoginResult, String> {
        runtime()?
            .block_on(login_with_qr(
                WeixinLoginCallbacks {
                    on_qr_code: Box::new(|url| {
                        log("扫码登录链接：");
                        log(&format!("  {}\n", String::from_utf16_lossy(url.units())));
                        log("如果终端无法显示二维码，请在浏览器中打开上述链接。\n");
                    }),
                    on_status_change: Box::new(|status| {
                        let label = match status {
                            "waiting" => "⏳ 等待扫码...",
                            "scanned" => "📱 已扫码，等待确认...",
                            "expired" => "⏰ 二维码已过期，正在刷新...",
                            "refreshing" => "🔄 刷新中...",
                            _ => {
                                log(&format!("状态: {status}"));
                                return;
                            }
                        };
                        log(label);
                    }),
                },
                &default_login_base_url(),
            ))
            .map_err(|error| error.message)
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

async fn fetch_json(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
    require_ok: bool,
) -> Result<Response, String> {
    let transport = SetupTransport(HyperModelTransport::new());
    let url = url::Url::parse(url).map_err(|_| "fetch failed".to_owned())?;
    let method = Method::from_bytes(method.as_bytes()).map_err(|_| "fetch failed".to_owned())?;
    let mut request_headers = HeaderMap::new();
    for (name, value) in headers {
        request_headers.insert(
            hyper::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "fetch failed".to_owned())?,
            hyper::header::HeaderValue::from_str(value).map_err(|_| "fetch failed".to_owned())?,
        );
    }
    let response = fetch::fetch(
        &transport,
        url,
        method,
        request_headers,
        Bytes::from(body.to_owned()),
    )
    .await
    .map_err(|error| error.message)?;
    let status = response.status().as_u16();
    if require_ok && !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let mut response_body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(frame) = tokio::time::timeout(TRANSPORT_TIMEOUT, response_body.frame())
        .await
        .map_err(|_| "terminated".to_owned())?
    {
        let frame = frame.map_err(|error| error.to_string())?;
        if let Some(chunk) = frame.data_ref() {
            if bytes.len().saturating_add(chunk.len()) > 0x1fff_ffe8 {
                return Err("Cannot create a string longer than 0x1fffffe8 characters".into());
            }
            bytes.extend_from_slice(chunk);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(Response {
        status,
        body: text.strip_prefix('\u{feff}').unwrap_or(&text).into(),
    })
}

pub(super) fn parse_response(response: &Response) -> Result<JsValue, String> {
    if let Some(error) = pilotdeck_protocol::json_syntax::parse_error(&response.body) {
        return Err(error);
    }
    pilotdeck_protocol::parse_json(&response.body).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilotdeck_gateway::runtime::model::http::ModelBody;
    use std::sync::{Arc, Mutex};

    struct DelayedTransport {
        delays: Arc<Mutex<std::collections::VecDeque<(Duration, bool)>>>,
    }
    impl ModelHttpTransport for DelayedTransport {
        fn send(&self, _request: Request<Full<Bytes>>) -> TransportFuture {
            let (delay, redirect) = self.delays.lock().unwrap().pop_front().unwrap();
            TransportFuture::io(async move {
                tokio::time::sleep(delay).await;
                let response = if redirect {
                    hyper::Response::builder()
                        .status(302)
                        .header("location", "/next")
                } else {
                    hyper::Response::builder().status(200)
                };
                let body: ModelBody = Full::new(Bytes::from_static(b"{}"))
                    .map_err(|never| match never {})
                    .boxed();
                Ok(response.body(body).unwrap())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn each_redirect_has_its_own_six_hundred_second_header_budget() {
        let inner = DelayedTransport {
            delays: Arc::new(Mutex::new(
                [
                    (Duration::from_secs(400), true),
                    (Duration::from_secs(400), false),
                ]
                .into(),
            )),
        };
        let transport = SetupTransport(inner);
        let before = tokio::time::Instant::now();
        let response = fetch::fetch(
            &transport,
            "http://fixture.invalid/start".parse().unwrap(),
            Method::GET,
            HeaderMap::new(),
            Bytes::new(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(before.elapsed(), Duration::from_secs(800));
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_hop_exceeding_the_header_budget_fails() {
        let inner = DelayedTransport {
            delays: Arc::new(Mutex::new([(Duration::from_secs(601), false)].into())),
        };
        let transport = SetupTransport(inner);
        let before = tokio::time::Instant::now();
        let result = fetch::fetch(
            &transport,
            "http://fixture.invalid/start".parse().unwrap(),
            Method::GET,
            HeaderMap::new(),
            Bytes::new(),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("hanging response must time out"),
            Err(error) => error,
        };
        assert_eq!(error.message, "fetch failed");
        assert_eq!(before.elapsed(), TRANSPORT_TIMEOUT);
    }
}
