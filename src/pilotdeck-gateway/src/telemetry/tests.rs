use super::*;
use crate::runtime::model::http::{BodyFailure, ModelBody, ModelHttpTransport, TransportFuture};
use bytes::Bytes;
use futures_util::FutureExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::{Request, Response, body::Frame};
use pilotdeck_protocol::parse_json;
use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

struct Dir(PathBuf);
impl Dir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pilotdeck-o1-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Mock {
    requests: Mutex<Vec<JsValue>>,
    statuses: Mutex<VecDeque<u16>>,
    gates: Mutex<VecDeque<oneshot::Receiver<()>>>,
    dropped: Arc<AtomicBool>,
    pending_body: AtomicBool,
}
fn full() -> ModelBody {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed()
}
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
impl ModelHttpTransport for Mock {
    fn send(&self, request: Request<Full<Bytes>>) -> TransportFuture {
        let body = request
            .into_body()
            .collect()
            .now_or_never()
            .unwrap()
            .unwrap()
            .to_bytes();
        lock(&self.requests).push(parse_json(std::str::from_utf8(&body).unwrap()).unwrap());
        let status = lock(&self.statuses).pop_front().unwrap_or(200);
        let gate = lock(&self.gates).pop_front();
        let guard = DropFlag(self.dropped.clone());
        let pending = self.pending_body.load(Ordering::SeqCst);
        TransportFuture::io(async move {
            let _guard = guard;
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            let body = if pending {
                StreamBody::new(futures_util::stream::pending::<
                    Result<Frame<Bytes>, BodyFailure>,
                >())
                .boxed()
            } else {
                full()
            };
            Ok(Response::builder().status(status).body(body).unwrap())
        })
    }
}
fn env() -> TelemetryEnv {
    [
        ("ANALYTICS_ENABLED", "true"),
        ("ANALYTICS_BASE_URL", "http://127.0.0.1:18081/"),
        ("ANALYTICS_FLUSH_INTERVAL_MS", "600000"),
        ("ANALYTICS_BATCH_SIZE", "100"),
        ("ANALYTICS_TIMEOUT_MS", "1000"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}
fn runtime() -> TelemetryRuntimeContext {
    TelemetryRuntimeContext {
        installation_id: hash_telemetry_id(&"o1-fake-server-token".into()),
        instance_id: "0123456789abcdef01234567".into(),
        deployment_mode: "docker".into(),
        commit_hash: "o1-fixed-commit".into(),
        app_version: "o1-fixed-version".into(),
        platform: "darwin".into(),
    }
}
fn collector(dir: &Dir, mock: Arc<Mock>) -> Arc<TelemetryCollector> {
    let mut options = TelemetryCollectorOptions::new(env(), dir.0.clone(), runtime());
    options.transport = Some(mock);
    TelemetryCollector::new(options)
}
fn expected(id: &str) -> JsValue {
    let oracle = parse_json(include_str!(
        "../../tests/fixtures/o1-telemetry/module-oracle.json"
    ))
    .unwrap();
    let JsValue::Array(rows) = field(&oracle, "rows") else {
        panic!("rows")
    };
    rows.iter()
        .find(|row| {
            field(row, "id")
                .as_string()
                .is_some_and(|s| s.equals_str(id))
        })
        .unwrap()
        .clone()
}
fn normalized(value: &JsValue) -> JsValue {
    match value {
        JsValue::Array(values) => JsValue::Array(values.iter().map(normalized).collect()),
        JsValue::Object(o) => {
            let mut out = JsObject::new();
            for (k, v) in o.own_entries() {
                if v.is_undefined() {
                    continue;
                }
                let value = if k.equals_str("eventId")
                    && v.as_string().is_some_and(|s| s.len_utf16() == 36)
                {
                    let text = v.as_string().unwrap().to_utf8().unwrap();
                    assert_eq!(uuid::Uuid::parse_str(&text).unwrap().get_version_num(), 4);
                    string("<uuid-v4>")
                } else if k.equals_str("occurredAt") || k.equals_str("lastSuccessAt") {
                    let text = v.as_string().unwrap().to_utf8().unwrap();
                    assert_eq!(text.len(), 24);
                    assert!(text.ends_with('Z'));
                    string("<iso-time>")
                } else if k.equals_str("instanceId") {
                    string("<verified-instance-id>")
                } else {
                    normalized(v)
                };
                out.insert(k.clone(), value);
            }
            JsValue::Object(out)
        }
        _ => value.clone(),
    }
}
fn oracle_body(id: &str) -> JsValue {
    let row = expected(id);
    let JsValue::Array(requests) = field(&row, "requests") else {
        panic!("requests")
    };
    field(&requests[0], "body").clone()
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("causal milestone");
}

#[tokio::test]
async fn collector_fields_sanitization_error_expansion_and_metadata_match_executed_ts_oracle() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    let c = collector(&dir, mock.clone());
    c.track(&"feature_used".into(),&parse_json(r#"{"rootLabel":"drop-key","profile":"drop-even-profile","list":["/secret","safe",{},[],null,"C:\\secret"],"nested":{"file":"drop","kept":true,"none":null,"empty":{}},"relative":"./keep","url":"https://example.invalid/path","slash":" /drop ","weird":"\\drop","number":7}"#).unwrap(),&object([("sessionId",string("raw-secret-session"))]));
    c.track_feature_used(&parse_json(r#"{"module":"memory","loopStage":"module_event","sessionId":"same","metadata":{"module":"cron_job","outcome":"denied","loopStage":"override","array":[1,2]}}"#).unwrap());
    c.track_error(&TelemetryErrorValue::ErrorName("TypeError".into()),&parse_json(r#"{"module":"session","ownerModule":"memory","executionKind":"invalid","loopStage":"invalid","errorCategory":"invalid","sessionId":"raw-secret-session","toolName":"bash","metadata":{"runId":"excluded","provider":"p","model":"m","providerBaseUrl":" https://user:password@EXAMPLE.invalid:443/v1/?secret=1#fragment ","extra":"excluded"}}"#).unwrap());
    c.track_error(
        &TelemetryErrorValue::Value(string("message")),
        &object([("module", string("runtime"))]),
    );
    c.flush().await;
    assert_eq!(
        normalized(&lock(&mock.requests)[0]),
        oracle_body("collector-fields")
    );
    c.shutdown().await;
}

#[tokio::test]
async fn gateway_observer_all_recorded_event_variants_match_real_collector_oracle() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    let c = collector(&dir, mock.clone());
    let observer = GatewayAgentEventTelemetryObserver::new(Some(c.clone()));
    let context = GatewayTelemetryContext {
        session_id: "session".into(),
        run_id: "run".into(),
        channel_key: "web".into(),
        permission_mode: "default".into(),
        owner_module: "memory".into(),
        execution_kind: "user_session".into(),
        phase: Some("phase".into()),
    };
    let row = expected("gateway-observer");
    let JsValue::Array(trace) = field(&row, "trace") else {
        panic!("trace")
    };
    let JsValue::Array(events) = field(&trace[0], "inputs") else {
        panic!("inputs")
    };
    for event in events {
        observer.observe(event, &context);
    }
    c.flush().await;
    assert_eq!(
        normalized(&lock(&mock.requests)[0]),
        oracle_body("gateway-observer")
    );
    c.shutdown().await;
}

#[test]
fn turn_context_precedence_matches_executed_ts_resolver() {
    let row = expected("gateway-observer");
    let JsValue::Array(trace) = field(&row, "trace") else {
        panic!("trace")
    };
    let JsValue::Array(cases) = field(&trace[0], "resolved") else {
        panic!("cases")
    };
    for case in cases {
        let input = field(case, "input");
        let actual = resolve_turn_context(
            field(input, "channelKey").as_string().unwrap(),
            field(input, "telemetry"),
        );
        let out = object([
            ("ownerModule", JsValue::String(actual.owner_module)),
            ("executionKind", JsValue::String(actual.execution_kind)),
            (
                "phase",
                actual
                    .phase
                    .map(JsValue::String)
                    .unwrap_or(JsValue::Undefined),
            ),
        ]);
        assert_eq!(normalized(&out), field(case, "value").clone());
    }
}

#[tokio::test]
async fn failed_batches_reverse_before_retry_and_restore_does_not_increment_queued() {
    let dir = Dir::new();
    let mut config = TelemetryConfig::from_env(&env(), &dir.0);
    config.batch_size = 2.0;
    config.max_retries = 2.0;
    config.max_queue_size = 3.0;
    fs::create_dir_all(config.queue_file_path.parent().unwrap()).unwrap();
    fs::write(&config.queue_file_path,"{\"event\":{\"eventId\":\"A\"},\"attempts\":0}\n{\"event\":{\"eventId\":\"B\"},\"attempts\":0}\n{\"event\":{\"eventId\":\"C\"},\"attempts\":0}\n").unwrap();
    let mock = Arc::new(Mock::default());
    *lock(&mock.statuses) = VecDeque::from([503, 503, 503, 200]);
    let sender = TelemetrySender::new(config, Some(mock.clone()));
    sender.enqueue(object([("eventId", string("D"))]));
    let mut snapshots = Vec::new();
    for _ in 0..4 {
        sender.flush().await;
        snapshots.push(normalized(&sender.snapshot()));
    }
    let row = expected("retry-order");
    let JsValue::Array(requests) = field(&row, "requests") else {
        panic!("requests")
    };
    assert_eq!(
        *lock(&mock.requests),
        requests
            .iter()
            .map(|r| field(r, "body").clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(JsValue::Array(snapshots), field(&row, "trace").clone());
    sender.shutdown().await;
}

#[tokio::test]
async fn disabled_restore_preserves_truthy_ids_fractional_attempts_and_over_capacity() {
    let dir = Dir::new();
    let mut config = TelemetryConfig::from_env(&env(), &dir.0);
    config.enabled = false;
    config.max_queue_size = 1.0;
    fs::create_dir_all(config.queue_file_path.parent().unwrap()).unwrap();
    let mut raw =
        " \nbroken\nnull\n{\"event\":{\"eventId\":\"\"}}\n{\"event\":{\"eventId\":0}}\n".to_owned();
    for (i, attempt) in ["-2", "\"2.5\"", "\"not-number\"", "\"Infinity\"", "null"]
        .iter()
        .enumerate()
    {
        raw.push_str(&format!(
            "{{\"event\":{{\"eventId\":\"R{i}\",\"unknown\":\"kept\"}},\"attempts\":{attempt}}}\n"
        ));
    }
    fs::write(&config.queue_file_path, raw).unwrap();
    let path = config.queue_file_path.clone();
    let sender = TelemetrySender::new(config, Some(Arc::new(Mock::default())));
    sender.shutdown().await;
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        field(&expected("jsonl-restore-disabled"), "queue")
            .as_string()
            .unwrap()
            .to_utf8()
            .unwrap()
    );
    assert_eq!(field(&sender.snapshot(), "queued"), &JsValue::Number(0.0));
    assert_eq!(
        field(&sender.snapshot(), "queueDepth"),
        &JsValue::Number(5.0)
    );
}

#[tokio::test(start_paused = true)]
async fn collector_enabled_alias_preserves_measured_timer_behavior() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    let mut options = TelemetryCollectorOptions::new(env(), dir.0.clone(), runtime());
    options
        .env
        .insert("ANALYTICS_ENABLED".into(), "false".into());
    options
        .env
        .insert("ANALYTICS_FLUSH_INTERVAL_MS".into(), "25".into());
    options.transport = Some(mock.clone());
    let c = TelemetryCollector::new(options);
    c.set_enabled(true);
    c.track(&"feature_used".into(), &object([]), &object([]));
    tokio::time::advance(Duration::from_millis(110)).await;
    assert!(lock(&mock.requests).is_empty());
    c.flush().await;
    assert_eq!(lock(&mock.requests).len(), 1);
    c.shutdown().await;
}

#[tokio::test]
async fn enqueue_claims_batch_synchronously_before_next_cap_check_and_shutdown_waiter_can_be_dropped()
 {
    let dir = Dir::new();
    let mut config = TelemetryConfig::from_env(&env(), &dir.0);
    config.batch_size = 1.0;
    config.max_queue_size = 1.0;
    let path = config.queue_file_path.clone();
    let mock = Arc::new(Mock::default());
    let (release, gate) = oneshot::channel();
    lock(&mock.gates).push_back(gate);
    let sender = TelemetrySender::new(config, Some(mock.clone()));
    sender.enqueue(object([("eventId", string("A"))]));
    sender.enqueue(object([("eventId", string("B"))]));
    sender.enqueue(object([("eventId", string("C"))]));
    assert_eq!(field(&sender.snapshot(), "dropped"), &JsValue::Number(1.0));
    until(|| lock(&mock.requests).len() == 1).await;
    // Ordinary flush still returns while another flush is in flight.
    sender.flush().await;
    drop(sender.shutdown());
    let mut second = sender.shutdown();
    assert!(second.as_mut().now_or_never().is_none());
    assert!(!path.exists());
    release.send(()).unwrap();
    second.await;
    assert_eq!(lock(&mock.requests).len(), 2);
    assert_eq!(field(&sender.snapshot(), "sent"), &JsValue::Number(2.0));
    assert!(!path.exists());
}

#[tokio::test(start_paused = true)]
async fn shutdown_uses_existing_timeout_then_persists_the_owned_failed_batch() {
    let dir = Dir::new();
    let mut config = TelemetryConfig::from_env(&env(), &dir.0);
    config.batch_size = 1.0;
    config.timeout_ms = 25.0;
    let path = config.queue_file_path.clone();
    let mock = Arc::new(Mock::default());
    let (_release, gate) = oneshot::channel();
    lock(&mock.gates).push_back(gate);
    let sender = TelemetrySender::new(config, Some(mock.clone()));
    sender.enqueue(object([("eventId", string("held"))]));
    let stop = sender.shutdown();
    until(|| lock(&mock.requests).len() == 1).await;
    tokio::time::advance(Duration::from_millis(25)).await;
    stop.await;
    assert!(mock.dropped.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "{\"event\":{\"eventId\":\"held\"},\"attempts\":1}\n"
    );
}

#[tokio::test]
async fn successful_headers_do_not_wait_for_response_body() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    mock.pending_body.store(true, Ordering::SeqCst);
    let sender = TelemetrySender::new(TelemetryConfig::from_env(&env(), &dir.0), Some(mock));
    sender.enqueue(object([("eventId", string("header-success"))]));
    sender.flush().await;
    assert_eq!(field(&sender.snapshot(), "sent"), &JsValue::Number(1.0));
    sender.shutdown().await;
}

#[test]
fn runtime_context_hashes_token_and_intentionally_keeps_baseline_cache_key() {
    let dir = Dir::new();
    fs::write(dir.0.join("server-token"), "o1-fake-server-token\n").unwrap();
    let mut env = env();
    env.insert("DOCKER_CONTAINER".into(), "1".into());
    env.insert("COMMIT_HASH".into(), "fixed".into());
    env.insert("PILOTDECK_VERSION".into(), "v".into());
    let mut options = RuntimeContextOptions::from_process(env, Some(dir.0.clone()), dir.0.clone());
    let first = resolve_runtime_context(&options).unwrap();
    assert_eq!(
        first.installation_id,
        hash_telemetry_id(&"o1-fake-server-token".into())
    );
    fs::write(dir.0.join("server-token"), "different").unwrap();
    options.env.insert("DOCKER_CONTAINER".into(), "0".into());
    assert_eq!(resolve_runtime_context(&options).unwrap(), first);
    assert_eq!(
        hash_telemetry_id(&JsString::from_utf16(vec![0xd800])),
        hash_telemetry_id(&"�".into())
    );
}

struct GatedObserver {
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    calls: Arc<Mutex<Vec<String>>>,
    name: &'static str,
}

struct AsyncPrefixObserver {
    calls: Arc<Mutex<Vec<String>>>,
    gate: Mutex<Option<oneshot::Receiver<()>>>,
}
impl TelemetryObserver for AsyncPrefixObserver {
    fn observe(&self, _: TelemetryObservation) -> TelemetryObserverResult {
        let calls = self.calls.clone();
        let gate = lock(&self.gate).take().unwrap();
        TelemetryObserverResult::Pending(Box::pin(async move {
            lock(&calls).push("async-prefix".into());
            let _ = gate.await;
            Ok(())
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_observer_prefix_precedes_the_next_observer_in_the_same_dispatch() {
    let registry = TelemetryObserverRegistry::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (release, gate) = oneshot::channel();
    let registration = registry
        .register(
            &"async".into(),
            Arc::new(AsyncPrefixObserver {
                calls: calls.clone(),
                gate: Mutex::new(Some(gate)),
            }),
            None,
        )
        .unwrap();
    registry
        .register(
            &"sync".into(),
            Arc::new(GatedObserver {
                calls: calls.clone(),
                gate: Mutex::new(None),
                name: "sync",
            }),
            None,
        )
        .unwrap();
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    assert_eq!(*lock(&calls), ["async-prefix", "observe:sync"]);
    assert_eq!(registration.in_flight(), 1);
    release.send(()).unwrap();
    registry.dispose().await.unwrap();
}
impl TelemetryObserver for GatedObserver {
    fn observe(&self, _: TelemetryObservation) -> TelemetryObserverResult {
        lock(&self.calls).push(format!("observe:{}", self.name));
        match lock(&self.gate).take() {
            Some(gate) => TelemetryObserverResult::Pending(Box::pin(async move {
                let _ = gate.await;
                Ok(())
            })),
            None => TelemetryObserverResult::Ready(Ok(())),
        }
    }
    fn dispose(&self) -> BoxFuture<Result<(), TelemetryFailure>> {
        lock(&self.calls).push(format!("dispose:{}", self.name));
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn observer_replacement_and_dropped_dispose_waiter_keep_owned_inflight_drain() {
    let registry = TelemetryObserverRegistry::default();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (release, gate) = oneshot::channel();
    let old = registry
        .register(
            &" x ".into(),
            Arc::new(GatedObserver {
                gate: Mutex::new(Some(gate)),
                calls: calls.clone(),
                name: "old",
            }),
            None,
        )
        .unwrap();
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    assert_eq!(old.in_flight(), 1);
    let replacement = registry
        .replace(
            &"x".into(),
            Arc::new(GatedObserver {
                gate: Mutex::new(None),
                calls: calls.clone(),
                name: "new",
            }),
            None,
        )
        .unwrap();
    assert_eq!(replacement.registration.generation(), 2);
    assert_eq!(old.state(), "draining");
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    drop(replacement.previous_disposed);
    drop(registry.dispose());
    let second = old.dispose();
    assert!(!lock(&calls).iter().any(|v| v == "dispose:old"));
    release.send(()).unwrap();
    second.await.unwrap();
    registry.dispose().await.unwrap();
    assert_eq!(registry.state(), "disposed");
    assert_eq!(old.in_flight(), 0);
    assert_eq!(
        lock(&calls).iter().filter(|v| *v == "dispose:old").count(),
        1
    );
    assert!(
        registry
            .register(
                &"late".into(),
                Arc::new(GatedObserver {
                    gate: Mutex::new(None),
                    calls,
                    name: "late"
                }),
                None
            )
            .is_err()
    );
}

#[test]
fn token_replacement_decoding_and_proto_assignment_match_executed_edge_oracle() {
    let oracle = parse_json(include_str!(
        "../../tests/fixtures/o1-telemetry/edges-oracle.json"
    ))
    .unwrap();
    let dir = Dir::new();
    fs::write(
        dir.0.join("server-token"),
        [0xef, 0xbb, 0xbf, 0xff, 0xc3, 0x28, 0x20],
    )
    .unwrap();
    let mut env = env();
    env.insert("DOCKER_CONTAINER".into(), "1".into());
    env.insert("COMMIT_HASH".into(), "edge-fixed".into());
    let context = resolve_runtime_context(&RuntimeContextOptions::from_process(
        env,
        Some(dir.0.clone()),
        dir.0.clone(),
    ))
    .unwrap();
    assert_eq!(
        JsValue::String(context.installation_id),
        field(&oracle, "installationId").clone()
    );
    let sanitized = sanitize_properties(
        &parse_json(r#"{"__proto__":{"kept":"prototype-member"},"normal":"value"}"#).unwrap(),
    );
    let object = sanitized.as_object().unwrap();
    assert!(!object.contains_key("__proto__"));
    assert_eq!(
        object.get_property("kept"),
        Some(&string("prototype-member"))
    );
    assert_eq!(normalized(&sanitized), object_value_normal());
}
fn object_value_normal() -> JsValue {
    object([("normal", string("value"))])
}

#[tokio::test]
async fn zero_inflight_when_drained_captures_ready_before_a_later_observation() {
    let registry = TelemetryObserverRegistry::default();
    let (release, gate) = oneshot::channel();
    let registration = registry
        .register(
            &"x".into(),
            Arc::new(GatedObserver {
                gate: Mutex::new(Some(gate)),
                calls: Arc::default(),
                name: "x",
            }),
            None,
        )
        .unwrap();
    let mut drained = registration.when_drained();
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    assert_eq!(registration.in_flight(), 1);
    assert!(drained.as_mut().now_or_never().is_some());
    release.send(()).unwrap();
    registry.dispose().await.unwrap();
}

struct SequentialGatedObserver(Mutex<VecDeque<oneshot::Receiver<()>>>);
impl TelemetryObserver for SequentialGatedObserver {
    fn observe(&self, _: TelemetryObservation) -> TelemetryObserverResult {
        let gate = lock(&self.0).pop_front().unwrap();
        TelemetryObserverResult::Pending(Box::pin(async move {
            let _ = gate.await;
            Ok(())
        }))
    }
}
#[tokio::test]
async fn nonzero_drain_receipt_stays_completed_after_a_later_observation() {
    let registry = TelemetryObserverRegistry::default();
    let (first_release, first_gate) = oneshot::channel();
    let (second_release, second_gate) = oneshot::channel();
    let registration = registry
        .register(
            &"held".into(),
            Arc::new(SequentialGatedObserver(Mutex::new(VecDeque::from([
                first_gate,
                second_gate,
            ])))),
            None,
        )
        .unwrap();
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    assert_eq!(registration.in_flight(), 1);
    // Do not poll this receipt until a new observation is already in flight.
    let mut old = registration.when_drained();
    first_release.send(()).unwrap();
    until(|| registration.in_flight() == 0).await;
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    assert_eq!(registration.in_flight(), 1);
    let mut new = registration.when_drained();
    assert!(old.as_mut().now_or_never().is_some());
    assert!(new.as_mut().now_or_never().is_none());
    second_release.send(()).unwrap();
    new.await;
    registry.dispose().await.unwrap();
}

struct TraceObserver {
    name: &'static str,
    log: Arc<Mutex<Vec<JsValue>>>,
    gate: Mutex<Option<oneshot::Receiver<()>>>,
    broken: bool,
}
impl TelemetryObserver for TraceObserver {
    fn observe(&self, observation: TelemetryObservation) -> TelemetryObserverResult {
        if self.broken {
            return TelemetryObserverResult::Ready(Err(TelemetryFailure("observer failed".into())));
        }
        let kind = match observation {
            TelemetryObservation::Error { .. } => "error",
            TelemetryObservation::FeatureUsed(_) => "feature_used",
            _ => "other",
        };
        lock(&self.log).push(JsValue::Array(vec![string(self.name), string(kind)]));
        match lock(&self.gate).take() {
            Some(gate) => TelemetryObserverResult::Pending(Box::pin(async move {
                let _ = gate.await;
                Ok(())
            })),
            None => TelemetryObserverResult::Ready(Ok(())),
        }
    }
    fn dispose(&self) -> BoxFuture<Result<(), TelemetryFailure>> {
        lock(&self.log).push(JsValue::Array(vec![string("disposed"), string(self.name)]));
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registry_disposer_start_order_matches_complete_ts_registry_trace() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = TelemetryObserverRegistry::default();
    let (release, gate) = oneshot::channel();
    registry
        .register(
            &"old".into(),
            Arc::new(TraceObserver {
                name: "old",
                log: log.clone(),
                gate: Mutex::new(Some(gate)),
                broken: false,
            }),
            None,
        )
        .unwrap();
    registry
        .register(
            &"broken".into(),
            Arc::new(TraceObserver {
                name: "broken",
                log: log.clone(),
                gate: Mutex::new(None),
                broken: true,
            }),
            None,
        )
        .unwrap();
    registry.observe(TelemetryObservation::Error {
        error: TelemetryErrorValue::ErrorName("TypeError".into()),
        input: object([]),
    });
    let replacement = registry
        .replace(
            &"old".into(),
            Arc::new(TraceObserver {
                name: "new",
                log: log.clone(),
                gate: Mutex::new(None),
                broken: false,
            }),
            None,
        )
        .unwrap();
    registry.observe(TelemetryObservation::FeatureUsed(object([])));
    let closing = registry.dispose();
    until(|| lock(&log).len() == 4).await;
    release.send(()).unwrap();
    replacement.previous_disposed.await.unwrap();
    closing.await.unwrap();
    let row = expected("registry");
    let JsValue::Array(trace) = field(&row, "trace") else {
        panic!("trace")
    };
    assert_eq!(
        JsValue::Array(lock(&log).clone()),
        field(&trace[0], "logs").clone()
    );
}

struct CountShutdown {
    base: Arc<dyn TelemetryClient>,
    count: Arc<std::sync::atomic::AtomicUsize>,
}
impl TelemetryClient for CountShutdown {
    fn track(&self, n: &JsString, p: &JsValue, c: &JsValue) {
        self.base.track(n, p, c);
    }
    fn track_feature_used(&self, i: &JsValue) {
        self.base.track_feature_used(i);
    }
    fn track_feature_loop_stage(&self, i: &JsValue) {
        self.base.track_feature_loop_stage(i);
    }
    fn track_error(&self, e: &TelemetryErrorValue, i: &JsValue) {
        self.base.track_error(e, i);
    }
    fn set_enabled(&self, e: bool) {
        self.base.set_enabled(e);
    }
    fn flush(&self) -> BoxFuture<()> {
        self.base.flush()
    }
    fn shutdown(&self) -> BoxFuture<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.base.shutdown()
    }
    fn snapshot(&self) -> JsValue {
        self.base.snapshot()
    }
    fn get_config(&self) -> JsValue {
        self.base.get_config()
    }
}
#[tokio::test]
async fn bundle_borrowed_client_is_observed_but_not_shut_down() {
    let dir = Dir::new();
    let base = collector(&dir, Arc::new(Mock::default()));
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut options = GatewayTelemetryBundleOptions::new(
        TelemetryEnv::new(),
        dir.0.join("not-created"),
        dir.0.clone(),
    );
    options.telemetry = Some(Arc::new(CountShutdown {
        base: base.clone(),
        count: count.clone(),
    }));
    let bundle = GatewayTelemetryBundle::new(options).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    bundle
        .observers()
        .register(
            &"observed".into(),
            Arc::new(GatedObserver {
                calls: log.clone(),
                gate: Mutex::new(None),
                name: "borrowed",
            }),
            None,
        )
        .unwrap();
    bundle
        .client()
        .track(&"feature_used".into(), &object([]), &object([]));
    bundle.dispose().await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(*lock(&log), ["observe:borrowed", "dispose:borrowed"]);
    assert!(!dir.0.join("not-created").exists());
    base.shutdown().await;
}
#[tokio::test]
async fn bundle_owned_collector_shuts_down_after_observer_error_even_if_waiter_is_dropped() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    lock(&mock.statuses).push_back(503);
    let mut options = GatewayTelemetryBundleOptions::new(env(), dir.0.clone(), dir.0.clone());
    options.context = Some(runtime());
    options.transport = Some(mock.clone());
    let bundle = GatewayTelemetryBundle::new(options).unwrap();
    let (release, gate) = oneshot::channel();
    bundle
        .observers()
        .register(
            &"held".into(),
            Arc::new(GatedObserver {
                calls: Arc::default(),
                gate: Mutex::new(Some(gate)),
                name: "held",
            }),
            Some(Arc::new(|_| {
                Box::pin(async { Err(TelemetryFailure("fixture dispose error".into())) })
            })),
        )
        .unwrap();
    bundle
        .client()
        .track(&"feature_used".into(), &object([]), &object([]));
    drop(bundle.dispose());
    assert!(lock(&mock.requests).is_empty());
    release.send(()).unwrap();
    assert!(bundle.dispose().await.is_err());
    assert_eq!(lock(&mock.requests).len(), 1);
    assert!(dir.0.join("telemetry/queue.jsonl").exists());
    assert!(bundle.dispose().await.is_err());
    assert_eq!(lock(&mock.requests).len(), 1);
}

#[tokio::test]
async fn sender_workers_belong_to_the_collector_runtime_after_a_callers_runtime_stops() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    let (release, gate) = oneshot::channel();
    lock(&mock.gates).push_back(gate);
    let mut config = TelemetryConfig::from_env(&env(), &dir.0);
    config.batch_size = 1.0;
    let sender = TelemetrySender::new(config, Some(mock.clone()));
    let caller = sender.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            caller.enqueue(object([("eventId", string("cross-runtime"))]));
        });
        // The caller's executor ends before its queued work could be polled.
    })
    .join()
    .unwrap();
    until(|| lock(&mock.requests).len() == 1).await;
    assert!(!mock.dropped.load(Ordering::SeqCst));
    let mut shutdown = sender.shutdown();
    assert!(shutdown.as_mut().now_or_never().is_none());
    release.send(()).unwrap();
    shutdown.await;
    assert_eq!(field(&sender.snapshot(), "sent"), &JsValue::Number(1.0));
    assert!(!dir.0.join("telemetry/queue.jsonl").exists());
}

#[tokio::test]
async fn held_observer_and_owned_disposal_survive_the_callers_runtime_stopping() {
    let dir = Dir::new();
    let mock = Arc::new(Mock::default());
    let mut options = GatewayTelemetryBundleOptions::new(env(), dir.0.clone(), dir.0.clone());
    options.context = Some(runtime());
    options.transport = Some(mock.clone());
    let bundle = GatewayTelemetryBundle::new(options).unwrap();
    let (release, gate) = oneshot::channel();
    let log = Arc::new(Mutex::new(Vec::new()));
    let registration = bundle
        .observers()
        .register(
            &"held".into(),
            Arc::new(GatedObserver {
                gate: Mutex::new(Some(gate)),
                calls: log.clone(),
                name: "cross-runtime",
            }),
            None,
        )
        .unwrap();
    let caller = bundle.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            caller
                .client()
                .track(&"feature_used".into(), &object([]), &object([]));
            drop(caller.dispose());
        });
    })
    .join()
    .unwrap();
    assert_eq!(registration.in_flight(), 1);
    let mut dispose = bundle.dispose();
    assert!(dispose.as_mut().now_or_never().is_none());
    assert!(lock(&mock.requests).is_empty());
    assert_eq!(*lock(&log), ["observe:cross-runtime"]);
    release.send(()).unwrap();
    dispose.await.unwrap();
    assert_eq!(
        *lock(&log),
        ["observe:cross-runtime", "dispose:cross-runtime"]
    );
    assert_eq!(lock(&mock.requests).len(), 1);
    assert_eq!(
        field(&bundle.client().snapshot(), "sent"),
        &JsValue::Number(1.0)
    );
    assert!(!dir.0.join("telemetry/queue.jsonl").exists());
}

#[tokio::test]
async fn server_enabled_reload_ignores_unrelated_config_and_unsubscribes() {
    use crate::config::store::{ConfigStore, StoreOptions};
    let dir = Dir::new();
    let base = "schemaVersion: 1\nagent: {model: parity/first}\nmodel:\n  providers:\n    parity:\n      protocol: openai\n      url: http://127.0.0.1:18081/v1\n      apiKey: mock-only\n      models: {first: {}, second: {}}\ntelemetry: {enabled: false}\n";
    let file = dir.0.join("pilotdeck.yaml");
    fs::write(&file, base).unwrap();
    let store = ConfigStore::new(StoreOptions::new(
        &dir.0,
        &dir.0,
        crate::config::Env::from([("PILOT_HOME".into(), dir.0.to_string_lossy().into_owned())]),
    ))
    .unwrap();
    let client = collector(&dir, Arc::new(Mock::default()));
    let subscription = subscribe_enabled_reload(&store, client.clone());
    // The client intentionally differs from the file: unrelated settings must
    // not apply telemetry.enabled again, even when its current value differs.
    fs::write(&file, base.replace("parity/first", "parity/second")).unwrap();
    store.reload("unrelated").await.unwrap();
    assert_eq!(field(&client.get_config(), "enabled"), &JsValue::Bool(true));
    fs::write(&file, base.replace("enabled: false", "enabled: true")).unwrap();
    store.reload("telemetry-on").await.unwrap();
    fs::write(&file, base).unwrap();
    store.reload("telemetry-off").await.unwrap();
    assert_eq!(
        field(&client.get_config(), "enabled"),
        &JsValue::Bool(false)
    );
    subscription.unsubscribe();
    fs::write(&file, base.replace("enabled: false", "enabled: true")).unwrap();
    store.reload("after-unsubscribe").await.unwrap();
    assert_eq!(
        field(&client.get_config(), "enabled"),
        &JsValue::Bool(false)
    );
    client.shutdown().await;
}
