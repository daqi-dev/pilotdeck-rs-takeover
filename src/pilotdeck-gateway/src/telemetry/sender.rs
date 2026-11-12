use super::{BoxFuture, TelemetryEnv, field, lock, object, string, truthy};
use crate::runtime::{
    model::http::{HyperModelTransport, ModelHttpTransport, fetch::fetch},
    network::node_timer_duration,
};
use bytes::Bytes;
use hyper::{HeaderMap, Method, header::CONTENT_TYPE};
use pilotdeck_protocol::{JsValue, parse_json, stringify_json};
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub base_url: String,
    pub flush_interval_ms: f64,
    pub batch_size: f64,
    pub timeout_ms: f64,
    pub max_retries: f64,
    pub max_queue_size: f64,
    pub queue_file_path: PathBuf,
}
impl TelemetryConfig {
    pub fn from_env(env: &TelemetryEnv, pilot_home: &Path) -> Self {
        let integer = |key, default, zero| {
            env.get(key)
                .and_then(|v| parse_int(v))
                .filter(|v| v.is_finite() && if zero { *v >= 0.0 } else { *v > 0.0 })
                .unwrap_or(default)
        };
        let enabled = env.get("ANALYTICS_ENABLED").is_some_and(|v| {
            matches!(
                crate::metadata::js_trim(v).to_lowercase().as_str(),
                "1" | "true" | "on"
            )
        });
        Self {
            enabled,
            base_url: crate::metadata::js_trim(
                env.get("ANALYTICS_BASE_URL")
                    .map(String::as_str)
                    .unwrap_or("http://tele.pilotdeck.cn"),
            )
            .into(),
            flush_interval_ms: integer("ANALYTICS_FLUSH_INTERVAL_MS", 5000.0, false),
            batch_size: integer("ANALYTICS_BATCH_SIZE", 20.0, false),
            timeout_ms: integer("ANALYTICS_TIMEOUT_MS", 4000.0, false),
            max_retries: integer("ANALYTICS_MAX_RETRIES", 3.0, true),
            max_queue_size: integer("ANALYTICS_MAX_QUEUE_SIZE", 2000.0, false),
            queue_file_path: env
                .get("ANALYTICS_QUEUE_FILE")
                .filter(|v| !v.is_empty())
                .map(|v| {
                    super::context::resolve(
                        &std::env::current_dir().unwrap_or_default(),
                        Path::new(v),
                    )
                })
                .unwrap_or_else(|| pilot_home.join("telemetry/queue.jsonl")),
        }
    }
    pub fn to_value(&self) -> JsValue {
        object([
            ("enabled", JsValue::Bool(self.enabled)),
            ("baseUrl", string(&self.base_url)),
            ("flushIntervalMs", JsValue::Number(self.flush_interval_ms)),
            ("batchSize", JsValue::Number(self.batch_size)),
            ("timeoutMs", JsValue::Number(self.timeout_ms)),
            ("maxRetries", JsValue::Number(self.max_retries)),
            ("maxQueueSize", JsValue::Number(self.max_queue_size)),
            (
                "queueFilePath",
                string(&self.queue_file_path.to_string_lossy()),
            ),
        ])
    }
}
fn parse_int(value: &str) -> Option<f64> {
    let value = crate::metadata::js_trim_start(value);
    let mut chars = value.chars().peekable();
    let sign = if chars.peek() == Some(&'-') {
        chars.next();
        -1.0
    } else {
        if chars.peek() == Some(&'+') {
            chars.next();
        }
        1.0
    };
    let digits: String = chars.take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<f64>().ok().map(|v| sign * v)
    }
}

#[derive(Clone)]
struct Envelope {
    event: JsValue,
    attempts: f64,
}
impl Envelope {
    fn to_value(&self) -> JsValue {
        object([
            ("event", self.event.clone()),
            ("attempts", JsValue::Number(self.attempts)),
        ])
    }
}
#[derive(Default)]
struct Metrics {
    queued: f64,
    sent: f64,
    send_failures: f64,
    retries: f64,
    dropped: f64,
    last_success_at: Option<String>,
}
#[derive(Default)]
struct State {
    queue: VecDeque<Envelope>,
    metrics: Metrics,
    flush: Option<watch::Receiver<bool>>,
    shutdown: Option<watch::Receiver<bool>>,
    timer: Option<JoinHandle<()>>,
}
struct Inner {
    runtime: tokio::runtime::Handle,
    config: Arc<Mutex<TelemetryConfig>>,
    state: Mutex<State>,
    transport: Arc<dyn ModelHttpTransport>,
}
#[derive(Clone)]
pub struct TelemetrySender {
    inner: Arc<Inner>,
}
impl TelemetrySender {
    pub fn new(config: TelemetryConfig, transport: Option<Arc<dyn ModelHttpTransport>>) -> Self {
        Self::shared(Arc::new(Mutex::new(config)), transport)
    }
    pub(super) fn shared(
        config: Arc<Mutex<TelemetryConfig>>,
        transport: Option<Arc<dyn ModelHttpTransport>>,
    ) -> Self {
        let inner = Arc::new(Inner {
            runtime: tokio::runtime::Handle::current(),
            config,
            state: Mutex::new(State::default()),
            transport: transport.unwrap_or_else(|| Arc::new(HyperModelTransport::new())),
        });
        inner.restore();
        let enabled = lock(&inner.config).enabled;
        if enabled {
            inner.start_timer();
        }
        Self { inner }
    }
    pub fn set_enabled(&self, enabled: bool) {
        let mut config = lock(&self.inner.config);
        if config.enabled == enabled {
            return;
        }
        config.enabled = enabled;
        drop(config);
        if enabled {
            self.inner.start_timer();
        } else {
            if let Some(timer) = lock(&self.inner.state).timer.take() {
                timer.abort();
            }
            let _ = self.inner.begin_flush();
        }
    }
    pub fn enqueue(&self, event: JsValue) {
        let config = lock(&self.inner.config).clone();
        if !config.enabled {
            return;
        }
        let mut state = lock(&self.inner.state);
        if state.queue.len() as f64 >= config.max_queue_size {
            state.metrics.dropped += 1.0;
            return;
        }
        state.queue.push_back(Envelope {
            event,
            attempts: 0.0,
        });
        state.metrics.queued += 1.0;
        let flush = state.queue.len() as f64 >= config.batch_size;
        drop(state);
        // JS void flush() claims and removes the batch in this same call stack.
        if flush {
            let _ = self.inner.begin_flush();
        }
    }
    pub fn flush(&self) -> BoxFuture<()> {
        match self.inner.begin_flush() {
            Some(receipt) => Box::pin(wait(receipt)),
            None => Box::pin(async {}),
        }
    }
    pub fn shutdown(&self) -> BoxFuture<()> {
        // Synchronous prefix and persistent receipt: dropping a waiter cannot
        // lose a batch between HTTP acceptance and queue persistence.
        let mut state = lock(&self.inner.state);
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        if let Some(receipt) = state.shutdown.as_ref().filter(|r| !(*r.borrow())) {
            return Box::pin(wait(receipt.clone()));
        }
        let (done, receipt) = watch::channel(false);
        state.shutdown = Some(receipt.clone());
        drop(state);
        let inflight = self
            .inner
            .begin_flush()
            .or_else(|| lock(&self.inner.state).flush.clone());
        let inner = self.inner.clone();
        self.inner.runtime.spawn(async move {
            let _completion = Completion(done);
            if let Some(receipt) = inflight {
                wait(receipt).await;
            }
            inner.persist();
        });
        Box::pin(wait(receipt))
    }
    pub fn snapshot(&self) -> JsValue {
        let state = lock(&self.inner.state);
        let m = &state.metrics;
        let mut value = object([
            ("queued", JsValue::Number(m.queued)),
            ("sent", JsValue::Number(m.sent)),
            ("sendFailures", JsValue::Number(m.send_failures)),
            ("retries", JsValue::Number(m.retries)),
            ("dropped", JsValue::Number(m.dropped)),
            ("queueDepth", JsValue::Number(state.queue.len() as f64)),
        ]);
        if let (JsValue::Object(out), Some(at)) = (&mut value, &m.last_success_at) {
            out.insert("lastSuccessAt".into(), string(at));
        }
        value
    }
    pub fn get_config(&self) -> TelemetryConfig {
        lock(&self.inner.config).clone()
    }
}
struct Completion(watch::Sender<bool>);
impl Drop for Completion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
async fn wait(mut receipt: watch::Receiver<bool>) {
    while !*receipt.borrow_and_update() {
        if receipt.changed().await.is_err() {
            break;
        }
    }
}
impl Inner {
    fn start_timer(self: &Arc<Self>) {
        let mut state = lock(&self.state);
        if state.timer.is_some() {
            return;
        }
        let delay = node_timer_duration(lock(&self.config).flush_interval_ms);
        let weak = Arc::downgrade(self);
        state.timer = Some(self.runtime.spawn(async move {
            let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + delay, delay);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let _ = inner.begin_flush();
            }
        }));
    }
    fn begin_flush(self: &Arc<Self>) -> Option<watch::Receiver<bool>> {
        let config = lock(&self.config).clone();
        let mut state = lock(&self.state);
        if !config.enabled
            || state.queue.is_empty()
            || state.flush.as_ref().is_some_and(|r| !(*r.borrow()))
        {
            return None;
        }
        let batch = take_batch(&mut state.queue, config.batch_size);
        let (done, receipt) = watch::channel(false);
        state.flush = Some(receipt.clone());
        drop(state);
        let inner = self.clone();
        self.runtime.spawn(async move {
            let mut owned = OwnedBatch {
                inner: inner.clone(),
                batch,
                done: Some(done),
            };
            loop {
                let success = inner.send_batch(&owned.batch).await;
                let mut state = lock(&inner.state);
                if success {
                    state.metrics.sent += owned.batch.len() as f64;
                    state.metrics.last_success_at =
                        Some(crate::runtime::context::prompt::iso_timestamp_now());
                    owned.batch.clear();
                    let config = lock(&inner.config);
                    owned.batch = take_batch(&mut state.queue, config.batch_size);
                    if owned.batch.is_empty() {
                        break;
                    }
                } else {
                    let max_retries = lock(&inner.config).max_retries;
                    requeue_failed(&mut state, &mut owned.batch, max_retries);
                    break;
                }
            }
        });
        Some(receipt)
    }
    async fn send_batch(&self, batch: &[Envelope]) -> bool {
        let config = lock(&self.config).clone();
        let base = config
            .base_url
            .strip_suffix('/')
            .unwrap_or(&config.base_url);
        let Ok(url) = url::Url::parse(&format!("{base}/collect")) else {
            return false;
        };
        let Ok(Some(body)) = stringify_json(&JsValue::Array(
            batch.iter().map(|e| e.event.clone()).collect(),
        )) else {
            return false;
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        // Baseline resolves on headers; a pending or malformed response body
        // does not turn a successful analytics upload into a failure.
        tokio::time::timeout(
            node_timer_duration(config.timeout_ms),
            fetch(
                self.transport.as_ref(),
                url,
                Method::POST,
                headers,
                Bytes::from(body),
            ),
        )
        .await
        .is_ok_and(|r| r.is_ok_and(|r| r.status().is_success()))
    }
    fn restore(&self) {
        let path = lock(&self.config).queue_file_path.clone();
        let Ok(raw) = fs::read(path) else {
            return;
        };
        let raw = String::from_utf8_lossy(&raw);
        let mut state = lock(&self.state);
        for line in raw
            .split('\n')
            .map(crate::metadata::js_trim)
            .filter(|l| !l.is_empty())
        {
            let Ok(parsed) = parse_json(line) else {
                continue;
            };
            let event = field(&parsed, "event");
            if !truthy(field(event, "eventId")) {
                continue;
            }
            let number = crate::js_value::to_number(field(&parsed, "attempts")).unwrap_or(f64::NAN);
            let attempts = if number.is_nan() || number == 0.0 {
                0.0
            } else {
                number.max(0.0)
            };
            state.queue.push_back(Envelope {
                event: event.clone(),
                attempts,
            });
        }
    }
    fn persist(&self) {
        let path = lock(&self.config).queue_file_path.clone();
        let state = lock(&self.state);
        if state.queue.is_empty() {
            let _ = fs::remove_file(path);
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let mut lines = String::new();
        for item in &state.queue {
            let Ok(Some(line)) = stringify_json(&item.to_value()) else {
                return;
            };
            lines.push_str(&line);
            lines.push('\n');
        }
        let _ = fs::write(path, lines);
    }
}
fn take_batch(queue: &mut VecDeque<Envelope>, size: f64) -> Vec<Envelope> {
    let count = (size as usize).min(queue.len());
    queue.drain(..count).collect()
}
fn requeue_failed(state: &mut State, batch: &mut Vec<Envelope>, max_retries: f64) {
    state.metrics.send_failures += 1.0;
    for mut item in batch.drain(..) {
        if item.attempts + 1.0 > max_retries {
            state.metrics.dropped += 1.0;
        } else {
            state.metrics.retries += 1.0;
            item.attempts += 1.0;
            state.queue.push_front(item);
        }
    }
}
struct OwnedBatch {
    inner: Arc<Inner>,
    batch: Vec<Envelope>,
    done: Option<watch::Sender<bool>>,
}
impl Drop for OwnedBatch {
    fn drop(&mut self) {
        if !self.batch.is_empty() {
            let max = lock(&self.inner.config).max_retries;
            requeue_failed(&mut lock(&self.inner.state), &mut self.batch, max);
        }
        if let Some(done) = self.done.take() {
            done.send_replace(true);
        }
    }
}
