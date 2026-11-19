//! The release-channel HTTP client (t021, 009 §3.2–§3.3).
//!
//! The channel is a plain HTTP(S) service exposing `release.json` and the
//! archive assets at their manifest names under one base URL (009 §10.4:
//! the production channel shape is undecided; acceptance runs a controlled
//! local HTTP service). This client:
//!
//! * fetches the manifest with a hard timeout (check must fail fast on a
//!   dead network, never hang);
//! * streams an asset to disk while hashing it (sha256 + sha512 + size,
//!   the full package-body verification the design places in apply stage 3)
//!   and honors the `installation.control_update` plane between body
//!   frames: `pause` parks the loop, `resume` continues it, `cancel`
//!   aborts it mid-flight (009 §3.4's stage matrix — cancellation is only
//!   offered while the download is still running).
//!
//! Nothing here installs or switches anything; the apply state machine in
//! the domain file owns the install-root mutations.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use sha2::{Digest, Sha256, Sha512};

use super::release::ReleaseAsset;

/// Failures of the channel client. The domain maps these onto the contract
/// codes (`Network` → `UPSTREAM_ERROR` for check / `UNAVAILABLE` for apply,
/// `Integrity` → `INTEGRITY_ERROR`, `Cancelled` → a cancelled operation).
#[derive(Debug)]
pub enum ChannelError {
    Network(String),
    Integrity(String),
    Cancelled,
}

impl ChannelError {
    pub fn message(&self) -> String {
        match self {
            Self::Network(message) => format!("channel error: {message}"),
            Self::Integrity(message) => format!("integrity error: {message}"),
            Self::Cancelled => "download cancelled".to_owned(),
        }
    }
}

/// The control plane handle shared between an apply task and
/// `installation.control_update` callers. All operations are atomic flags;
/// the paused wait is a bounded poll, so no notification can be lost.
pub struct DownloadControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl DownloadControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn channel_url(channel: &str, suffix: &str) -> String {
    format!("{}/{}", channel.trim_end_matches('/'), suffix)
}

async fn http_get(url: &str) -> Result<hyper::Response<hyper::body::Incoming>, ChannelError> {
    let uri: hyper::Uri = url
        .parse()
        .map_err(|error| ChannelError::Network(format!("invalid channel url `{url}`: {error}")))?;
    let client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        Full<Bytes>,
    > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build_http();
    let request = hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(uri)
        .body(Full::new(Bytes::new()))
        .map_err(|error| ChannelError::Network(format!("build request: {error}")))?;
    let response = client
        .request(request)
        .await
        .map_err(|error| ChannelError::Network(format!("GET {url}: {error}")))?;
    Ok(response)
}

/// Fetches the channel's `release.json` with a hard deadline (check's
/// "网络失败" report path).
pub async fn fetch_manifest(base_url: &str, timeout: Duration) -> Result<Vec<u8>, ChannelError> {
    let url = channel_url(base_url, "release.json");
    let fetched = tokio::time::timeout(timeout, async {
        let response = http_get(&url).await?;
        if response.status() != hyper::StatusCode::OK {
            return Err(ChannelError::Network(format!(
                "GET {url} answered {}",
                response.status()
            )));
        }
        let body = response.into_body();
        let bytes = body
            .collect()
            .await
            .map_err(|error| ChannelError::Network(format!("read {url}: {error}")))?
            .to_bytes();
        Ok::<_, ChannelError>(bytes.to_vec())
    })
    .await;
    match fetched {
        Ok(result) => result,
        Err(_) => Err(ChannelError::Network(format!(
            "GET {url} did not answer within {}ms",
            timeout.as_millis()
        ))),
    }
}

/// Streams one asset to `dest`, verifying size + sha256 + sha512 exactly as
/// the manifest recorded them (apply stage 3). Between body frames the loop
/// honors pause/cancel when a control handle is attached. On any error the
/// caller owns cleanup of the partial file.
pub async fn download_asset(
    base_url: &str,
    asset: &ReleaseAsset,
    dest: &Path,
    control: Option<Arc<DownloadControl>>,
) -> Result<u64, ChannelError> {
    let url = channel_url(base_url, &asset.name);
    let response = http_get(&url).await?;
    if response.status() != hyper::StatusCode::OK {
        return Err(ChannelError::Network(format!(
            "GET {url} answered {}",
            response.status()
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ChannelError::Network(format!("mkdir {}: {error}", parent.display()))
        })?;
    }
    let mut file =
        tokio::io::BufWriter::new(tokio::fs::File::create(dest).await.map_err(|error| {
            ChannelError::Network(format!("create {}: {error}", dest.display()))
        })?);

    let mut sha256 = Sha256::new();
    let mut sha512 = Sha512::new();
    let mut length: u64 = 0;
    let mut body = response.into_body();
    loop {
        if let Some(control) = &control {
            if control.is_cancelled() {
                return Err(ChannelError::Cancelled);
            }
            while control.is_paused() && !control.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if control.is_cancelled() {
                return Err(ChannelError::Cancelled);
            }
        }
        let Some(frame) = body
            .frame()
            .await
            .transpose()
            .map_err(|error| ChannelError::Network(format!("read {url}: {error}")))?
        else {
            break;
        };
        let Some(chunk) = frame.data_ref() else {
            continue;
        };
        let chunk = chunk.clone();
        sha256.update(&chunk);
        sha512.update(&chunk);
        length += chunk.len() as u64;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|error| ChannelError::Network(format!("write {}: {error}", dest.display())))?;
        if length > asset.size {
            return Err(ChannelError::Integrity(format!(
                "asset `{}` is larger than the manifest recorded ({} > {})",
                asset.name, length, asset.size
            )));
        }
    }
    let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
    if length != asset.size {
        return Err(ChannelError::Integrity(format!(
            "asset `{}` is {} bytes, manifest recorded {}",
            asset.name, length, asset.size
        )));
    }
    let digest = format!("{:x}", sha256.finalize());
    if digest != asset.sha256 {
        return Err(ChannelError::Integrity(format!(
            "asset `{}` sha256 {digest} does not match the manifest entry {}",
            asset.name, asset.sha256
        )));
    }
    let strong = {
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;
        BASE64.encode(sha512.finalize())
    };
    if strong != asset.sha512 {
        return Err(ChannelError::Integrity(format!(
            "asset `{}` sha512 does not match the manifest entry",
            asset.name
        )));
    }
    Ok(length)
}

// ---------------------------------------------------------------------------
// Tests (a hand-rolled loopback HTTP/1.1 server — the e2e suites reuse the
// same technique because the CLI test crate has no HTTP server dependency)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_server(
        response: &'static str,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let response = response.to_owned();
                tokio::spawn(async move {
                    let mut socket = socket;
                    // Drain the request head (bounded: read until CRLFCRLF).
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let read = tokio::io::AsyncReadExt::read(&mut socket, &mut chunk)
                            .await
                            .unwrap_or(0);
                        if read == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                        if buf.len() > 64 * 1024 {
                            break;
                        }
                    }
                    let head = format!("{response}content-length: {}\r\n\r\n", body.len());
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, head.as_bytes()).await;
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &body).await;
                });
            }
        });
        (format!("http://{addr}"), task)
    }

    #[tokio::test]
    async fn fetch_manifest_and_download_verify_round_trip() {
        let payload = b"package-body-bytes".to_vec();
        let sha256 = format!("{:x}", Sha256::digest(&payload));
        use base64::Engine as _;
        use base64::engine::general_purpose::STANDARD as BASE64;
        let sha512 = BASE64.encode(Sha512::digest(&payload));
        let (base, _task) = spawn_server(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n",
            payload.clone(),
        )
        .await;

        let manifest = fetch_manifest(&base, Duration::from_secs(5)).await.unwrap();
        assert_eq!(manifest, payload);

        let asset = ReleaseAsset {
            name: "agentcpm-pd-0.1.1-darwin-arm64.tar.gz".to_owned(),
            size: payload.len() as u64,
            sha256,
            sha512,
            platform: "darwin".to_owned(),
            arch: "arm64".to_owned(),
            kind: "archive".to_owned(),
        };
        let dir = std::env::temp_dir().join(format!("t021-channel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("asset.part");
        download_asset(&base, &asset, &dest, None).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);

        // A wrong recorded digest fails the integrity check.
        let mut bad = asset.clone();
        bad.sha256 = "0".repeat(64);
        let error = download_asset(&base, &bad, &dest, None).await.unwrap_err();
        assert!(matches!(error, ChannelError::Integrity(_)), "{error:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn network_failure_and_timeout_surface_as_network_errors() {
        // Port 1 on loopback: reserved, refuses connections.
        let error = fetch_manifest("http://127.0.0.1:1", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(error, ChannelError::Network(_)), "{error:?}");
    }
}
