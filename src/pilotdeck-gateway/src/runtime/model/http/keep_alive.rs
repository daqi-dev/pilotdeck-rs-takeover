//! 空闲连接的保活期限，照 undici 的做法（基线经 undici 发模型请求）。
//!
//! undici 在每个响应结束后按服务端的 `Keep-Alive: timeout=N` 提示决定这条连接还能空闲多久：
//! `min(N 秒 - keepAliveTimeoutThreshold, keepAliveMaxTimeout)`，不大于 0 就不再复用；没有提示时用
//! `keepAliveTimeout`（`node_modules/undici/lib/dispatcher/client-h1.js:644-658`，
//! `parseKeepAliveTimeout` 在 `lib/core/util.js:385-393`）。基线装的全局 Agent 只设了
//! `keepAliveTimeout: 120_000`（`src/cli/proxy.ts:149-160`、`src/model/streaming/streamModel.ts:63`），
//! 另两项是 undici 的缺省：阈值 2 秒、上限 600 秒（`lib/dispatcher/client.js:254-256`）。
//!
//! hyper 的连接池不看这个提示，只有一个全局空闲时限（缺省 90 秒）。服务端只保活 5 秒时（Node 的
//! `http.Server` 缺省就是这样），空闲 5 秒上下复用的连接会被服务端同时关掉，请求以网络错误失败，
//! 多出一次重试；基线在空闲 3 秒时就已经弃用这条连接，不会碰上。
//!
//! 做法：连接底层的流（`PrefixedStream`）带一份 [`KeepAliveState`]，经连接元数据交给发请求的一方；
//! 响应体读完时记下空闲期限；连接空闲时 hyper 一直在读这条流（为了发现对端关闭），期限一到就让这次
//! 读返回 EOF，hyper 随即关掉连接、不再放回池里。流上一有写入（下一个请求）就清掉期限。
//! 没有经过 [`KeepAliveState::mark_idle`] 的连接（别的客户端共用这个连接器时）永远不会被这样关掉。

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use tokio::time::{Instant, Sleep};

/// undici `keepAliveTimeoutThreshold` 的缺省值。
const THRESHOLD_MS: u64 = 2_000;
/// undici `keepAliveMaxTimeout` 的缺省值。
const MAX_MS: u64 = 600_000;
/// 基线全局 Agent 的 `keepAliveTimeout`（服务端没给提示时）。
const DEFAULT_MS: u64 = 120_000;

/// 一条连接的空闲期限，连接器和发请求的一方共用。
#[derive(Default)]
pub(crate) struct KeepAliveState {
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    deadline: Option<Instant>,
    reader: Option<Waker>,
}

impl KeepAliveState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 流上有写入：连接又在用了，取消空闲期限。
    pub(crate) fn mark_busy(&self) {
        self.lock().deadline = None;
    }

    /// 响应读完，连接从现在起空闲 `idle` 之后弃用。叫醒正在等读的连接任务，让它挂上计时器。
    pub(crate) fn mark_idle(&self, idle: Duration) {
        let waker = {
            let mut slot = self.lock();
            slot.deadline = Some(Instant::now() + idle);
            slot.reader.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// 底层读挂起时调用：空闲期限已到返回 true（调用方把这次读当作 EOF）。
    pub(crate) fn idle_expired(
        &self,
        context: &mut Context<'_>,
        sleep: &mut Option<Pin<Box<Sleep>>>,
    ) -> bool {
        let deadline = {
            let mut slot = self.lock();
            slot.reader = Some(context.waker().clone());
            slot.deadline
        };
        let Some(deadline) = deadline else {
            *sleep = None;
            return false;
        };
        match sleep {
            Some(timer) if timer.deadline() == deadline => {}
            _ => *sleep = Some(Box::pin(tokio::time::sleep_until(deadline))),
        }
        sleep
            .as_mut()
            .is_some_and(|timer| timer.as_mut().poll(context).is_ready())
    }
}

/// 放进 hyper 连接元数据（`Connected::extra`）里的句柄。
#[derive(Clone)]
pub(crate) struct KeepAliveHandle(pub(crate) Arc<KeepAliveState>);

/// 这个响应结束后连接还能空闲多久（见模块说明）。`keep-alive` 头有多个时 undici 把值首尾相接。
pub(crate) fn idle_after(headers: &hyper::HeaderMap) -> Duration {
    let joined: String = headers
        .get_all(hyper::header::HeaderName::from_static("keep-alive"))
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect();
    match hint_ms(&joined) {
        Some(hint) => Duration::from_millis(hint.saturating_sub(THRESHOLD_MS).min(MAX_MS)),
        None => Duration::from_millis(DEFAULT_MS),
    }
}

/// `/timeout=(\d+)/` 的第一处匹配，乘 1000。
fn hint_ms(value: &str) -> Option<u64> {
    let mut rest = value;
    while let Some(at) = rest.find("timeout=") {
        let tail = &rest[at + "timeout=".len()..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            let seconds = tail[..digits].bytes().fold(0u64, |acc, digit| {
                acc.saturating_mul(10)
                    .saturating_add(u64::from(digit - b'0'))
            });
            return Some(seconds.saturating_mul(1000));
        }
        rest = tail;
    }
    None
}

/// 响应体读到结尾时把连接记为空闲。没读完就丢掉的响应体，hyper 不会复用它的连接，不用管。
pub(crate) struct IdleOnEnd<B> {
    inner: B,
    pending: Option<(Arc<KeepAliveState>, Duration)>,
}

impl<B: Body> IdleOnEnd<B> {
    pub(crate) fn new(inner: B, keep_alive: Option<(Arc<KeepAliveState>, Duration)>) -> Self {
        let mut body = Self {
            inner,
            pending: keep_alive,
        };
        if body.inner.is_end_stream() {
            body.finish();
        }
        body
    }

    fn finish(&mut self) {
        if let Some((state, idle)) = self.pending.take() {
            state.mark_idle(idle);
        }
    }
}

impl<B: Body<Data = Bytes> + Unpin> Body for IdleOnEnd<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(context);
        match &polled {
            Poll::Ready(None) => this.finish(),
            Poll::Ready(Some(Ok(_))) if this.inner.is_end_stream() => this.finish(),
            Poll::Ready(Some(Err(_))) => this.pending = None,
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> hyper::HeaderMap {
        let mut map = hyper::HeaderMap::new();
        for value in values {
            map.append("keep-alive", value.parse().unwrap());
        }
        map
    }

    #[test]
    fn idle_follows_undici_hint_rules() {
        // Node http.Server 的缺省提示：5 秒 → 空闲 3 秒后弃用。
        assert_eq!(idle_after(&headers(&["timeout=5"])), Duration::from_secs(3));
        // 没有提示：基线 Agent 的 keepAliveTimeout。
        assert_eq!(idle_after(&headers(&[])), Duration::from_secs(120));
        assert_eq!(idle_after(&headers(&["max=100"])), Duration::from_secs(120));
        // 不大于阈值：不再复用。
        assert_eq!(idle_after(&headers(&["timeout=2"])), Duration::ZERO);
        assert_eq!(idle_after(&headers(&["timeout=1"])), Duration::ZERO);
        // 上限 600 秒。
        assert_eq!(
            idle_after(&headers(&["timeout=9999"])),
            Duration::from_secs(600)
        );
        // 正则不要求词边界，取第一处带数字的匹配；多个头首尾相接。
        assert_eq!(
            idle_after(&headers(&["xtimeout=7, timeout=9"])),
            Duration::from_secs(5)
        );
        assert_eq!(
            idle_after(&headers(&["timeout=, timeout=4"])),
            Duration::from_secs(2)
        );
        assert_eq!(
            idle_after(&headers(&["max=5, time", "out=6"])),
            Duration::from_secs(4)
        );
    }

    /// 服务端提示保活 3 秒：空闲 0.2 秒内复用同一条连接；空闲超过 1 秒（3 − 2）后改开新连接。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transport_retires_idle_connection_after_server_hint() {
        use super::super::{HyperModelTransport, ModelHttpTransport};
        use http_body_util::{BodyExt, Full};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut pending = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        let read = match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => read,
                        };
                        pending.extend_from_slice(&chunk[..read]);
                        while let Some(end) = pending.windows(4).position(|w| w == b"\r\n\r\n") {
                            pending.drain(..end + 4);
                            let reply = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nkeep-alive: timeout=3\r\n\r\nok";
                            if socket.write_all(reply).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });

        let transport = HyperModelTransport::new();
        let fetch = || async {
            let request = hyper::Request::get(format!("http://{address}/"))
                .body(Full::new(Bytes::new()))
                .unwrap();
            let Ok(response) = transport.send(request).await else {
                panic!("response");
            };
            let Ok(body) = response.into_body().collect().await else {
                panic!("body");
            };
            assert_eq!(body.to_bytes(), Bytes::from_static(b"ok"));
        };
        fetch().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        fetch().await;
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "空闲不到期限时复用");
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        fetch().await;
        assert_eq!(accepted.load(Ordering::SeqCst), 2, "空闲超过期限后开新连接");
    }
}
