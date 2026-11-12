//! probeGatewayServer (src/gateway/client/probeServer.ts:10-31): the fixed
//! local-server probe that `pilotdeck cron`, the default CliChannel and the
//! TUI run before deciding whether a PilotDeck server is reachable.
use crate::paths::{self, Env};
use bytes::Bytes;
use hyper::{HeaderMap, Method};
use pilotdeck_gateway::runtime::model::http::{HyperModelTransport, fetch::fetch};
use std::path::Path;
use std::time::{Duration, Instant};
use url::Url;

/// `options.url ?? "http://127.0.0.1:18789"`: none of the CLI callers pass a
/// URL, so the address is fixed.
const HEALTH_URL: &str = "http://127.0.0.1:18789/health";
/// `options.timeoutMs ?? 200`, an abort over the whole `fetch`.
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// `GET /health` answered 2xx within the budget, and a non-empty token file
/// exists (`readGatewayAuthToken`). Any failure, including an unreadable token
/// path, is "no server" exactly like the baseline's `catch`. Returns the token
/// `connectRemoteGatewayIfAvailable` would connect with.
pub fn probe_gateway_server(env: &Env, cwd: &Path) -> Option<String> {
    if !health_ok() {
        return None;
    }
    let path = paths::gateway_token_path(env, cwd).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let token = crate::paths::js_trim(&String::from_utf8_lossy(&bytes)).to_owned();
    (!token.is_empty()).then_some(token)
}

fn health_ok() -> bool {
    health_ok_at(HEALTH_URL, PROBE_TIMEOUT)
}

fn health_ok_at(url: &str, budget: Duration) -> bool {
    let started = Instant::now();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let Some(remaining) = budget.checked_sub(started.elapsed()) else {
        return false;
    };
    runtime.block_on(async {
        tokio::time::timeout(remaining, fetch_health(url))
            .await
            .unwrap_or(false)
            && started.elapsed() < budget
    })
}

/// `fetch` resolves after complete response headers, without consuming the
/// body. Follow its GET redirect policy under the caller's one abort budget.
async fn fetch_health(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    let transport = HyperModelTransport::new();
    fetch(&transport, url, Method::GET, HeaderMap::new(), Bytes::new())
        .await
        .is_ok_and(|response| response.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    /// The fixed TS entry was run against these same wire responses with a
    /// supplied token. No real gateway, token path or fixed port is involved.
    #[test]
    fn health_headers_redirects_and_abort_match_fixed_ts() {
        let oracle: Value =
            serde_json::from_str(include_str!("../tests/fixtures/c1-probe/oracle.json"))
                .expect("C1 probe oracle JSON");
        assert_eq!(
            oracle["baseline"],
            "cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85"
        );
        let cases = oracle["cases"].as_array().expect("C1 probe cases");
        assert_eq!(cases.len(), 9);
        for case in cases {
            let mode = case["mode"].as_str().expect("C1 probe case mode");
            let (ok, requests) = run_probe_case(mode);
            assert_eq!(Value::Bool(ok), case["ok"], "C1 probe {mode}");
            assert_eq!(
                Value::from(requests),
                case["requests"],
                "C1 probe {mode} requests"
            );
        }
    }

    fn run_probe_case(mode: &str) -> (bool, usize) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("C1 probe owned mock");
        listener
            .set_nonblocking(true)
            .expect("C1 probe nonblocking mock");
        let address = listener.local_addr().expect("C1 probe mock address");
        let mode = mode.to_owned();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let (stop, stopped) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if stopped.try_recv().is_ok() {
                    break;
                }
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("C1 probe accept: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("C1 mock read timeout");
                let mut raw = Vec::new();
                let mut buffer = [0; 512];
                while !raw.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).expect("C1 probe request");
                    if count == 0 {
                        return;
                    }
                    raw.extend_from_slice(&buffer[..count]);
                }
                observed.fetch_add(1, Ordering::SeqCst);
                let request = String::from_utf8_lossy(&raw);
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .expect("C1 probe request path");
                let redirect = |location: &str| {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                };
                let response = match mode.as_str() {
                    "head-only" => "HTTP/1.1 200 OK\r\n".to_owned(),
                    "body-pending" => "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx".to_owned(),
                    "bad-header" => "HTTP/1.1 200 OK\r\ninvalid header\r\n\r\n".to_owned(),
                    "status-error" => "HTTP/1.1 503 Busy\r\nContent-Length: 0\r\n\r\n".to_owned(),
                    "redirect-loop" => redirect("/health"),
                    "redirect-credentials" => redirect(&format!("http://user@{address}/healthy")),
                    "redirect-port" => redirect("http://127.0.0.1:25/healthy"),
                    "redirect" if path == "/health" => redirect("/healthy"),
                    _ => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned(),
                };
                socket
                    .write_all(response.as_bytes())
                    .expect("C1 probe response");
                if matches!(mode.as_str(), "head-only" | "body-pending") {
                    let _ = stopped.recv_timeout(Duration::from_secs(1));
                    break;
                }
            }
        });
        let ok = health_ok_at(&format!("http://{address}/health"), PROBE_TIMEOUT);
        let _ = stop.send(());
        server.join().expect("C1 probe owned server joined");
        (ok, requests.load(Ordering::SeqCst))
    }
}
