//! H1 bundle composition against cc7c4e6, captured by
//! tests/fixtures/h1-channel-bundle/capture.mjs. Real NativeChannels, loader,
//! and ChannelServer; controlled channel handles never contact a service.
//! These tests do not stand in for the formal config-reload caller.
use crate::channel::{
    loader::ChannelLoaderMounts,
    protocol::{
        BoxFuture, ChannelAdapter, ChannelError, ChannelHandle, ChannelKey, ChannelStartDeps,
        ReconcileResult,
    },
};
use crate::native_process::channels::{NativeChannels, NativeChannelsOptions};
use pilotdeck_protocol::{JsObject, JsValue, parse_json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const INITIAL: &str = r#"{"generation":"initial","adapters":{"homeassistant":{"enabled":true,"token":"ha-one","extra":{"url":"http://127.0.0.1:1/ha-one"}},"signal":{"enabled":true,"extra":{"restUrl":"http://127.0.0.1:1/signal-one","account":"signal-one"}}}}"#;
const UPDATED: &str = r#"{"generation":"updated","adapters":{"signal":{"enabled":true,"extra":{"restUrl":"http://127.0.0.1:1/signal-two","account":"signal-two"}},"mattermost":{"enabled":true,"token":"mm-two","extra":{"serverUrl":"http://127.0.0.1:1/mm-two","teamId":"team-two"}},"homeassistant":{"enabled":false}}}"#;

fn json(text: &str) -> JsValue {
    parse_json(text).expect("H1 fixture JSON")
}
fn field(value: &JsValue, key: &str) -> String {
    value
        .get(key)
        .and_then(JsValue::as_string)
        .map(|text| String::from_utf16_lossy(text.units()))
        .unwrap_or_else(|| panic!("H1 expected string field {key}"))
}
fn keys(values: &[ChannelKey]) -> Vec<String> {
    values.iter().map(ToString::to_string).collect()
}
fn outcome(result: ReconcileResult, started: &[&str], stopped: &[&str]) {
    assert_eq!(keys(&result.started), started);
    assert_eq!(keys(&result.stopped), stopped);
}

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pdrs-h1-bundle-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&path).expect("H1 isolated pilot home");
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Start {
        key: String,
        label: String,
        generation: String,
    },
    Stop {
        key: String,
        label: String,
        reason: String,
    },
}
fn start(key: &str, label: &str) -> Event {
    Event::Start {
        key: key.into(),
        label: label.into(),
        generation: "initial".into(),
    }
}
fn stop(key: &str, label: &str, reason: &str) -> Event {
    Event::Stop {
        key: key.into(),
        label: label.into(),
        reason: reason.into(),
    }
}
type Events = Arc<Mutex<Vec<Event>>>;
type Constructions = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

struct Controlled {
    key: ChannelKey,
    label: String,
    events: Events,
}
impl ChannelAdapter for Controlled {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }
    fn start(
        &self,
        deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        let generation = field(
            deps.config.as_deref().expect("H1 initial start config"),
            "generation",
        );
        self.events.lock().expect("H1 events").push(Event::Start {
            key: self.key.to_string(),
            label: self.label.clone(),
            generation,
        });
        let key = self.key.to_string();
        let label = self.label.clone();
        let events = self.events.clone();
        Box::pin(async move {
            if label == "candidate-fail" {
                return Err(ChannelError::error(format!(
                    "{key} start rejected: {label}"
                )));
            }
            Ok(Arc::new(ControlledHandle {
                key,
                label,
                events,
                stopped_once: AtomicBool::new(false),
            }) as Arc<dyn ChannelHandle>)
        })
    }
}
struct ControlledHandle {
    key: String,
    label: String,
    events: Events,
    stopped_once: AtomicBool,
}
impl ChannelHandle for ControlledHandle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        self.events
            .lock()
            .expect("H1 events")
            .push(stop(&self.key, &self.label, reason));
        let failure = self.label == "stop-fail" && !self.stopped_once.swap(true, Ordering::SeqCst);
        let message = format!("{} stop rejected: {}", self.key, self.label);
        Box::pin(async move {
            if failure {
                Err(ChannelError::error(message))
            } else {
                Ok(())
            }
        })
    }
}

struct Harness {
    channels: NativeChannels,
    events: Events,
    constructions: Constructions,
    _home: Root,
}
impl Harness {
    fn new(initial: &str) -> Self {
        let home = Root::new();
        let events = Events::default();
        let constructions = Constructions::default();
        let mut mounts = ChannelLoaderMounts::default();
        for (key, fields) in [
            ("mattermost", vec!["token", "serverUrl", "teamId"]),
            ("signal", vec!["restUrl", "account"]),
            ("homeassistant", vec!["url", "token"]),
        ] {
            let events = events.clone();
            let constructions = constructions.clone();
            mounts.register(
                key,
                Arc::new(move |options: &JsObject| {
                    let options = JsValue::Object(options.clone());
                    let captured = fields
                        .iter()
                        .map(|name| ((*name).to_owned(), field(&options, name)))
                        .collect();
                    constructions
                        .lock()
                        .expect("H1 constructions")
                        .push((key.into(), captured));
                    Ok(Arc::new(Controlled {
                        key: ChannelKey::new(key),
                        label: field(&options, if key == "signal" { "account" } else { "token" }),
                        events: events.clone(),
                    }) as Arc<dyn ChannelAdapter>)
                }),
            );
        }
        let channels = NativeChannels::new_with_loader_mounts(
            NativeChannelsOptions {
                pilot_home: home.0.clone(),
                config: Arc::new(json(initial)),
                weixin: Default::default(),
                weixin_maker: None,
                feishu: Default::default(),
                log: None,
                report_status: Some(Arc::new(|_, _| {})),
                close_host: None,
            },
            mounts,
        );
        Self {
            channels,
            events,
            constructions,
            _home: home,
        }
    }
    fn trace(&self) -> Vec<Event> {
        self.events.lock().expect("H1 events").clone()
    }
    async fn manual_start(&self, key: &str, label: &str) {
        self.channels
            .server()
            .hot_start_channel(Arc::new(Controlled {
                key: ChannelKey::new(key),
                label: label.into(),
                events: self.events.clone(),
            }))
            .await
            .expect("H1 manual controlled start");
    }
    async fn reload(&self, value: &JsValue) -> ReconcileResult {
        self.channels
            .reload(value)
            .await
            .expect("H1 reload succeeds")
    }
}

#[tokio::test]
async fn h1_reload_rebuilds_configured_channels_in_loader_order_and_preserves_start_snapshot() {
    let h = Harness::new(INITIAL);
    let startup = h.channels.create_startup_configured();
    assert_eq!(
        startup
            .iter()
            .map(|channel| channel.channel_key().to_string())
            .collect::<Vec<_>>(),
        ["signal", "homeassistant"]
    );
    for channel in startup {
        h.channels
            .server()
            .hot_start_channel(channel)
            .await
            .expect("H1 startup");
    }
    outcome(
        h.reload(&json(UPDATED)).await,
        &["mattermost", "signal"],
        &["homeassistant"],
    );
    let expected: Vec<(String, Vec<(String, String)>)> = [
        (
            "signal",
            vec![
                ("restUrl", "http://127.0.0.1:1/signal-one"),
                ("account", "signal-one"),
            ],
        ),
        (
            "homeassistant",
            vec![("url", "http://127.0.0.1:1/ha-one"), ("token", "ha-one")],
        ),
        (
            "mattermost",
            vec![
                ("token", "mm-two"),
                ("serverUrl", "http://127.0.0.1:1/mm-two"),
                ("teamId", "team-two"),
            ],
        ),
        (
            "signal",
            vec![
                ("restUrl", "http://127.0.0.1:1/signal-two"),
                ("account", "signal-two"),
            ],
        ),
    ]
    .into_iter()
    .map(|(key, fields)| {
        (
            key.to_owned(),
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        )
    })
    .collect::<Vec<_>>();
    assert_eq!(*h.constructions.lock().expect("H1 constructions"), expected);
    assert_eq!(
        h.trace(),
        [
            start("signal", "signal-one"),
            start("homeassistant", "ha-one"),
            stop("homeassistant", "ha-one", "config-disabled"),
            start("mattermost", "mm-two"),
            stop("signal", "signal-one", "hot-reload"),
            start("signal", "signal-two"),
        ]
    );
    outcome(h.reload(&json("{}")).await, &[], &["signal", "mattermost"]);
    assert_eq!(
        &h.trace()[6..],
        [
            stop("signal", "signal-two", "config-disabled"),
            stop("mattermost", "mm-two", "config-disabled")
        ]
    );
    h.channels.close().await.expect("H1 close");
}

#[tokio::test]
async fn h1_reload_retains_startup_and_later_managed_keys_after_disable_but_leaves_unowned_keys() {
    let h = Harness::new(INITIAL);
    // Construction itself adds managed keys, before any channel has started.
    assert_eq!(h.channels.create_startup_configured().len(), 2);
    let only_mm = json(
        r#"{"adapters":{"mattermost":{"enabled":true,"token":"mm-two","extra":{"serverUrl":"http://127.0.0.1:1/mm-two","teamId":"team-two"}}}}"#,
    );
    outcome(h.reload(&only_mm).await, &["mattermost"], &[]);
    outcome(h.reload(&json("{}")).await, &[], &["mattermost"]);
    let managed = [
        "feishu",
        "weixin",
        "qq",
        "wecom",
        "signal",
        "homeassistant",
        "mattermost",
    ];
    for key in managed {
        h.manual_start(key, "manual-history").await;
    }
    h.manual_start("unowned-channel", "unowned").await;
    let before = h.trace().len();
    outcome(h.reload(&json("{}")).await, &[], &managed);
    assert_eq!(
        h.trace()[before..],
        managed.map(|key| stop(key, "manual-history", "config-disabled"))
    );
    outcome(h.reload(&json("{}")).await, &[], &[]);
    h.channels.close().await.expect("H1 close unowned channel");
    assert_eq!(
        h.trace().last(),
        Some(&stop("unowned-channel", "unowned", "server-shutdown"))
    );
}

#[tokio::test]
async fn h1_reload_start_failure_restores_previous_and_stop_failure_remains_retryable() {
    let h = Harness::new(r#"{"generation":"initial"}"#);
    let signal = |account: &str| {
        json(&format!(
            r#"{{"generation":"updated","adapters":{{"signal":{{"enabled":true,"extra":{{"restUrl":"http://127.0.0.1:1/signal","account":"{account}"}}}}}}}}"#
        ))
    };
    outcome(h.reload(&signal("restore-base")).await, &["signal"], &[]);
    let error = h
        .channels
        .reload(&signal("candidate-fail"))
        .await
        .expect_err("H1 failed start must not report success");
    assert_eq!(error.message(), "signal start rejected: candidate-fail");
    assert_eq!(
        h.trace(),
        [
            start("signal", "restore-base"),
            stop("signal", "restore-base", "hot-reload"),
            start("signal", "candidate-fail"),
            start("signal", "restore-base")
        ]
    );
    outcome(h.reload(&json("{}")).await, &[], &["signal"]);
    assert_eq!(
        h.trace().last(),
        Some(&stop("signal", "restore-base", "config-disabled"))
    );
    outcome(h.reload(&signal("stop-fail")).await, &["signal"], &[]);
    let before = h.trace().len();
    let error = h
        .channels
        .reload(&json("{}"))
        .await
        .expect_err("H1 failed stop must not report success");
    assert_eq!(error.message(), "signal stop rejected: stop-fail");
    outcome(h.reload(&json("{}")).await, &[], &["signal"]);
    assert_eq!(
        h.trace()[before..],
        [
            stop("signal", "stop-fail", "config-disabled"),
            stop("signal", "stop-fail", "config-disabled")
        ]
    );
    h.channels.close().await.expect("H1 close");
}

// The following tests enter through the real ConfigStore's file load, diff,
// and subscription publication. Only channel network behavior is controlled.
const STORE_BASE: &str = "schemaVersion: 1\nagent: {model: fixture/model}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:1/v1\n      apiKey: mock-only\n      models:\n        model: {}\n";

#[derive(Clone, Default)]
struct QueueTrace {
    rows: Arc<Mutex<Vec<String>>>,
    changed: Arc<tokio::sync::Notify>,
}
impl QueueTrace {
    fn push(&self, value: String) {
        self.rows.lock().expect("H1 queue trace").push(value);
        self.changed.notify_one();
    }
    fn rows(&self) -> Vec<String> {
        self.rows.lock().expect("H1 queue trace").clone()
    }
    async fn until(&self, expected: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let changed = self.changed.notified();
                if self.rows().iter().any(|row| row == expected) {
                    return;
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("H1 queue never observed {expected}: {:?}", self.rows()));
    }
}

struct QueueAdapter {
    key: ChannelKey,
    account: String,
    trace: QueueTrace,
    gate: Arc<tokio::sync::Semaphore>,
}
impl ChannelAdapter for QueueAdapter {
    fn channel_key(&self) -> &ChannelKey {
        &self.key
    }
    fn start(
        &self,
        _deps: ChannelStartDeps,
    ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
        self.trace.push(format!("start:{}", self.account));
        let account = self.account.clone();
        let trace = self.trace.clone();
        let gate = self.gate.clone();
        Box::pin(async move {
            if account == "blocked-first" {
                gate.acquire().await.expect("H1 start gate").forget();
            }
            if account == "candidate-fail" {
                return Err(ChannelError::error("signal start rejected: candidate-fail"));
            }
            Ok(Arc::new(QueueHandle { account, trace }) as Arc<dyn ChannelHandle>)
        })
    }
}
struct QueueHandle {
    account: String,
    trace: QueueTrace,
}
impl ChannelHandle for QueueHandle {
    fn stop(&self, reason: &str) -> BoxFuture<Result<(), ChannelError>> {
        self.trace.push(format!("stop:{}:{reason}", self.account));
        Box::pin(async { Ok(()) })
    }
}

struct StoreHarness {
    channels: Arc<NativeChannels>,
    store: crate::config::store::ConfigStore,
    trace: QueueTrace,
    logs: QueueTrace,
    gate: Arc<tokio::sync::Semaphore>,
    home: Root,
}
impl StoreHarness {
    fn new() -> Self {
        Self::new_with_active(true)
    }
    fn new_with_active(active: bool) -> Self {
        use crate::config::{
            Env,
            store::{ConfigStore, StoreOptions},
        };
        let home = Root::new();
        std::fs::write(
            home.0.join("pilotdeck.yaml"),
            format!("{STORE_BASE}adapters: {{}}\n"),
        )
        .expect("H1 initial real store file");
        let home_text = home.0.to_string_lossy().into_owned();
        let mut options = StoreOptions::new(
            &home.0,
            &home.0,
            Env::from([
                ("HOME".into(), home_text.clone()),
                ("PILOT_HOME".into(), home_text),
            ]),
        );
        options.loaded_at = Arc::new(|| "1970-01-01T00:00:00.000Z".into());
        let store = ConfigStore::new(options).expect("H1 real store initial load");
        let config = store
            .snapshot()
            .get("config")
            .cloned()
            .expect("H1 store config snapshot");
        let trace = QueueTrace::default();
        let logs = QueueTrace::default();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut mounts = ChannelLoaderMounts::default();
        let adapter_trace = trace.clone();
        let adapter_gate = gate.clone();
        mounts.register(
            "signal",
            Arc::new(move |options| {
                let account = field(&JsValue::Object(options.clone()), "account");
                adapter_trace.push(format!("construct:{account}"));
                Ok(Arc::new(QueueAdapter {
                    key: ChannelKey::new("signal"),
                    account,
                    trace: adapter_trace.clone(),
                    gate: adapter_gate.clone(),
                }) as Arc<dyn ChannelAdapter>)
            }),
        );
        let output = logs.clone();
        let channels = Arc::new(NativeChannels::new_with_loader_mounts(
            NativeChannelsOptions {
                pilot_home: home.0.clone(),
                config: Arc::new(config),
                weixin: Default::default(),
                weixin_maker: None,
                feishu: Default::default(),
                log: Some(Arc::new(move |level, message| {
                    output.push(format!("{level}:{message}"))
                })),
                report_status: Some(Arc::new(|_, _| {})),
                close_host: None,
            },
            mounts,
        ));
        channels
            .attach_config_store(&store)
            .expect("H1 install real store subscription");
        if active {
            channels.activate_config_reload();
        }
        Self {
            channels,
            store,
            trace,
            logs,
            gate,
            home,
        }
    }
    async fn publish(&self, account: Option<&str>) {
        let adapters = match account {
            Some(account) => format!(
                "adapters:\n  signal:\n    enabled: true\n    extra:\n      restUrl: http://127.0.0.1:1/signal\n      account: {account}\n"
            ),
            None => "adapters: {}\n".to_owned(),
        };
        std::fs::write(
            self.home.0.join("pilotdeck.yaml"),
            format!("{STORE_BASE}{adapters}"),
        )
        .expect("H1 update actual config file");
        self.store
            .reload("h1-test-file-update")
            .await
            .expect("H1 real store reload succeeds");
    }
    async fn close(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), self.channels.close())
            .await
            .expect("H1 bounded close")
            .expect("H1 close succeeds");
    }
}

#[tokio::test]
async fn h1_config_store_reload_keeps_each_queued_snapshot_in_event_order() {
    let h = StoreHarness::new();
    h.publish(Some("blocked-first")).await;
    h.trace.until("start:blocked-first").await;
    h.publish(Some("second")).await;
    h.publish(Some("third")).await;
    assert_eq!(
        h.trace.rows(),
        ["construct:blocked-first", "start:blocked-first"]
    );
    h.gate.add_permits(1);
    h.trace.until("start:third").await;
    h.close().await;
    assert_eq!(
        h.trace.rows(),
        [
            "construct:blocked-first",
            "start:blocked-first",
            "construct:second",
            "stop:blocked-first:hot-reload",
            "start:second",
            "construct:third",
            "stop:second:hot-reload",
            "start:third",
            "stop:third:server-shutdown",
        ]
    );
    assert_eq!(
        h.logs.rows(),
        vec!["info:[pilotdeck] Adapter hot-reload complete: signal=started"; 3]
    );
}

#[tokio::test]
async fn h1_config_store_close_can_be_cancelled_then_retried_while_draining_queued_reload() {
    let h = StoreHarness::new();
    h.publish(Some("blocked-first")).await;
    h.trace.until("start:blocked-first").await;
    h.publish(Some("second")).await;
    let mut closing = Box::pin(h.channels.close());
    std::future::poll_fn(|context| {
        assert!(
            std::future::Future::poll(closing.as_mut(), context).is_pending(),
            "H1 close must wait for blocked start"
        );
        std::task::Poll::Ready(())
    })
    .await;
    drop(closing);
    assert_eq!(
        h.store.listener_count(),
        0,
        "H1 shutdown unsubscribes before drain completes"
    );
    // This real store event occurs after shutdown began. It must not enter
    // the queue, whereas the already published second event must finish.
    h.publish(Some("ignored-during-close")).await;
    h.gate.add_permits(1);
    h.close().await;
    assert_eq!(
        h.trace.rows(),
        [
            "construct:blocked-first",
            "start:blocked-first",
            "construct:second",
            "stop:blocked-first:hot-reload",
            "start:second",
            "stop:second:server-shutdown",
        ]
    );
    h.close().await;
}

#[tokio::test]
async fn h1_config_store_events_after_close_do_not_reconstruct_channels() {
    let h = StoreHarness::new();
    h.publish(Some("before-close")).await;
    h.trace.until("start:before-close").await;
    h.close().await;
    let closed = h.trace.rows();
    assert_eq!(
        h.store.listener_count(),
        0,
        "H1 closed driver owns no callback"
    );
    let observed_events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = observed_events.clone();
    let subscription = h.store.subscribe(Arc::new(move |event| {
        assert!(
            event
                .changed_paths
                .iter()
                .any(|path| String::from_utf16_lossy(path.units()).starts_with("adapters."))
        );
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    h.publish(Some("after-close")).await;
    h.publish(None).await;
    // The real store publishes both changes. Its only remaining listener
    // is this observer, so the closed driver cannot enqueue either snapshot.
    assert_eq!(h.store.listener_count(), 1);
    assert_eq!(observed_events.load(Ordering::SeqCst), 2);
    assert_eq!(h.trace.rows(), closed);
    assert_eq!(
        h.logs.rows(),
        ["info:[pilotdeck] Adapter hot-reload complete: signal=started"]
    );
    assert_eq!(
        h.channels
            .attach_config_store(&h.store)
            .expect_err("H1 cannot resubscribe after close")
            .message(),
        "Channel server is closing"
    );
    subscription.unsubscribe();
}

#[tokio::test]
async fn h1_config_store_failed_channel_start_warns_and_next_event_still_runs() {
    let h = StoreHarness::new();
    h.publish(Some("candidate-fail")).await;
    h.logs
        .until("warn:[pilotdeck] adapter hot-reload failed: signal start rejected: candidate-fail")
        .await;
    h.publish(Some("recovered")).await;
    h.trace.until("start:recovered").await;
    h.close().await;
    assert_eq!(
        h.trace.rows(),
        [
            "construct:candidate-fail",
            "start:candidate-fail",
            "construct:recovered",
            "start:recovered",
            "stop:recovered:server-shutdown",
        ]
    );
    assert_eq!(
        h.logs.rows(),
        [
            "warn:[pilotdeck] adapter hot-reload failed: signal start rejected: candidate-fail",
            "info:[pilotdeck] Adapter hot-reload complete: signal=started",
        ]
    );
}

#[tokio::test]
async fn h1_adapter_events_before_activation_do_not_start_channels() {
    let h = StoreHarness::new_with_active(false);
    h.publish(Some("before")).await;
    assert!(
        h.trace.rows().is_empty(),
        "pre-start config cannot start adapters"
    );
    h.channels.activate_config_reload();
    h.publish(Some("after")).await;
    h.trace.until("start:after").await;
    assert!(!h.trace.rows().iter().any(|row| row.contains("before")));
    h.close().await;
}

#[test]
fn h1_owned_wrapper_supplies_its_gateway_without_changing_channel_key() {
    struct Capture {
        key: ChannelKey,
        seen: Arc<Mutex<Option<Arc<dyn crate::channel::protocol::TurnGateway>>>>,
    }
    impl ChannelAdapter for Capture {
        fn channel_key(&self) -> &ChannelKey {
            &self.key
        }
        fn start(
            &self,
            deps: ChannelStartDeps,
        ) -> BoxFuture<Result<Arc<dyn ChannelHandle>, ChannelError>> {
            *self.seen.lock().unwrap() = deps.turn_gateway;
            Box::pin(async { Err(ChannelError::error("capture only")) })
        }
    }
    let seen = Arc::new(Mutex::new(None));
    let legacy = Arc::new(super::FeishuTurnGateway::new());
    let owned = Arc::new(legacy.owned());
    let owned_port: Arc<dyn crate::channel::protocol::TurnGateway> = owned.clone();
    let wrapper = super::OwnedChannel {
        inner: Arc::new(Capture {
            key: ChannelKey::new("custom-name"),
            seen: seen.clone(),
        }),
        gateway: owned,
    };
    let deps = ChannelStartDeps {
        turn_gateway: Some(legacy),
        ..Default::default()
    };
    let future = wrapper.start(deps);
    assert!(Arc::ptr_eq(
        seen.lock().unwrap().as_ref().unwrap(),
        &owned_port
    ));
    assert_eq!(wrapper.channel_key(), &ChannelKey::new("custom-name"));
    drop(future);
}

#[tokio::test]
async fn qq_wecom_bundle_constructs_only_for_strict_true() {
    for key in ["qq", "wecom"] {
        for (enabled, expected) in [
            ("true", true),
            ("1", false),
            ("-1", false),
            ("\"true\"", false),
            ("[]", false),
            ("{}", false),
            ("false", false),
            ("0", false),
            ("null", false),
            ("\"\"", false),
        ] {
            let harness = Harness::new(&format!(
                "{{\"adapters\":{{\"{key}\":{{\"enabled\":{enabled}}}}}}}"
            ));
            let built = if key == "qq" {
                harness.channels.create_startup_qq().await
            } else {
                harness.channels.create_startup_wecom().await
            };
            crate::negative_assert_eq!(
                built.is_some(),
                expected,
                "#H1 bundle production constructor uses strict true"
            );
            if let Some(channel) = built {
                assert_eq!(channel.channel_key().to_string(), key);
            }
        }
        let harness = Harness::new(&format!("{{\"adapters\":{{\"{key}\":{{}}}}}}"));
        assert!(
            if key == "qq" {
                harness.channels.create_startup_qq().await
            } else {
                harness.channels.create_startup_wecom().await
            }
            .is_none()
        );
    }
}

#[tokio::test]
async fn qq_wecom_bundle_loads_mapper_and_saves_exact_state() {
    use crate::channel::{qq::QQSessionMapperState, wecom::WeComSessionMapperState};
    let harness = Harness::new(
        r#"{"adapters":{"qq":{"enabled":true,"appId":"app","clientSecret":"secret","allowGroups":["group"],"triggerPrefixes":["!"],"maxMessageLength":99},"wecom":{"enabled":true,"token":"bot","extra":{"groupSessionsPerUser":false}}}}"#,
    );
    let qq_saved = json(r#"{"activeByChatKey":{"g:u":"qq:saved"}}"#);
    let wc_saved = json(
        r#"{"activeByChatId":{"wecom:dm=c":"wecom:saved"},"projectByScopeKey":{"wecom:dm=c":"project"}}"#,
    );
    harness.channels.state.save(&"qq".into(), qq_saved.clone());
    harness
        .channels
        .state
        .save(&"wecom".into(), wc_saved.clone());
    harness.channels.state.flush().await.unwrap();
    let mut qq = harness
        .channels
        .configured_qq_options(&harness.channels.config)
        .await;
    assert_eq!(qq.app_id, Some("app".into()));
    assert_eq!(qq.client_secret, Some("secret".into()));
    assert_eq!(qq.allow_groups, Some(vec!["group".into()]));
    assert_eq!(qq.trigger_prefixes, Some(vec!["!".into()]));
    assert_eq!(qq.max_message_length, Some(99));
    assert_eq!(qq.mapper.take().unwrap().snapshot().to_js(), qq_saved);
    let wc = harness
        .channels
        .configured_wecom_options(&harness.channels.config)
        .await;
    assert_eq!(wc.bot_key, Some("bot".into()));
    assert_eq!(wc.mapper.unwrap().snapshot().to_js(), wc_saved);
    assert_eq!(
        wc.extra.unwrap().get("groupSessionsPerUser"),
        Some(&JsValue::Bool(false))
    );
    let qq_next = json(r#"{"activeByChatKey":{"second":"s2","first":"s1"}}"#);
    let wc_next = json(
        r#"{"activeByChatId":{"second":"s2","first":"s1"},"projectByScopeKey":{"second":"p2","first":"p1"}}"#,
    );
    qq.on_state_change.unwrap()(QQSessionMapperState::from_js(&qq_next));
    wc.on_state_change.unwrap()(WeComSessionMapperState::from_js(&wc_next));
    harness.channels.state.flush().await.unwrap();
    assert_eq!(
        harness.channels.state.load(&"qq".into()).await,
        Some(qq_next)
    );
    assert_eq!(
        harness.channels.state.load(&"wecom".into()).await,
        Some(wc_next)
    );
}
