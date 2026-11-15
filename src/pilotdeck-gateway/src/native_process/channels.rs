//! A8.3 channel assembly for the native process (#1328): the weixin and
//! feishu subsets of `cli/ChannelAdapterBundle.ts` (the `adapters.<key>.enabled`
//! gate, durable mapper state, `hotStart<Key>`, `reload`, `flush`) on the
//! #1327 `ChannelServer` lifecycle host, plus the CLI `prepareWeixinLogin`
//! callback (`pilotdeck.ts:240-247`) and the `/feishu/webhook` slot the native
//! HTTP listener mounts. The other bundle channels (qq/wecom and the
//! loader-table ones) land with their own A8.3-ch-* units; the native process
//! reconciles only the keys it constructs, so `managedChannelKeys` carries
//! exactly those.
#[cfg(test)]
#[path = "channels_h1_tests.rs"]
mod h1_tests;
#[cfg(test)]
#[path = "channels_network_tests.rs"]
mod network_tests;
#[path = "channels_reload.rs"]
mod reload;

use crate::channel::{
    feishu::{FeishuChannel, FeishuOptions, FeishuWebhookSlot},
    im_interaction::{InteractionResponder, ResponderRef, helper_interaction},
    loader::{ChannelLoaderMounts, LoadEnabledChannelsOptions, load_enabled_channels},
    network::ChannelNetwork,
    protocol::{
        ChannelAdapter, ChannelError, ChannelKey, ChannelLog, ChannelStartDeps, ChannelState,
        ChannelStatus, CloseHost, ReconcileInput, ReconcileResult, StatusReporter, TurnGateway,
    },
    runtime_status::{
        ChannelRuntimeStatusReporter, ChannelRuntimeStatusUpdate,
        create_channel_runtime_status_reporter,
    },
    server::ChannelServer,
    turn_submitter::SubmitterTurnGateway,
    weixin::{
        ChannelStatePersistence, WeixinChannel, WeixinChannelOptions, WeixinChannelStatus,
        WeixinGatewayPort, WeixinSessionMapper, WeixinStateChange,
    },
};
use pilotdeck_protocol::{JsString, JsValue};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use super::methods::PrepareWeixinLogin;
use crate::runtime::turn::submit::TurnSubmitter;

/// The channel keys this assembly manages (TS `managedChannelKeys` subset).
fn weixin_key() -> ChannelKey {
    ChannelKey::new("weixin")
}
fn feishu_key() -> ChannelKey {
    ChannelKey::new("feishu")
}

/// The `ChannelAdapterBundle` keys this assembly actually constructs (#1694
/// §7). `crate::channel::loader::reject_unimplemented_bundle_channels` refuses
/// any *other* enabled bundle key instead of letting the native process start
/// with a channel that would never be built. An A8.3 bundle unit adds its key
/// here in the same change that adds its construction.
pub const IMPLEMENTED_BUNDLE_CHANNEL_KEYS: &[&str] = &["feishu", "weixin", "qq", "wecom"];

/// A satisfied close host: the native embedding drains its WS server before
/// the host cleanup closes the channel server, so there is no gateway-WS
/// close left to order behind the channel stops. The real WS shutdown
/// ordering is the #1328 pr2 follow-up noted in the plan.
pub fn noop_close_host() -> CloseHost {
    Arc::new(|| Box::pin(async { Ok(()) }))
}

/// TS `consoleLogger` (`pilotdeckServer.ts:67-72`): the channel log every
/// adapter's `start` receives — `info` to stdout, `warn` / `error` to stderr.
/// Until #2301 only the feishu/weixin startup existed and the assembly left
/// the log unset, so no channel line was observable; the loader-table keys
/// make that visible (the `api-server: listening on …` / `sms: twilio not
/// installed…` anchors the #1853 r1 probe recorded on the TS side).
pub fn console_channel_log() -> ChannelLog {
    Arc::new(|level: &str, message: &str| {
        write_console_line(
            level,
            message,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )
    })
}

/// `consoleLogger`'s one line over an explicit sink pair: `info` to the out
/// sink, `warn` / `error` to the err sink. The production closure passes the
/// real stdout/stderr; the routing and failing-sink controls drive this same
/// entry (#2301 fix-r2 review P1 #5 — a control that only exercised the
/// helper could not see the production closure regress).
pub(crate) fn write_console_line(
    level: &str,
    message: &str,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) {
    match level {
        "info" => console_log_line(out, message),
        _ => console_log_line(err, message),
    }
}

/// One console line on an explicit sink. `console.log` / `console.error`
/// never fail the channel lifecycle, while `println!` panics when the write
/// fails (std behavior) — a closed stdout would abort an adapter's task after
/// it saved its listener state, stranding the listener (#2301 fix-r1 review
/// P0). The write error is dropped instead, like TS's best-effort console.
pub fn console_log_line(sink: &mut dyn std::io::Write, message: &str) {
    let _ = writeln!(sink, "{message}");
}

/// The production loader-table mounts (#2301): one per implemented loader
/// key. The new IM mounts receive the native responder and one host-owned
/// network dispatcher. Each construction owns separate interaction state;
/// changing the dispatcher affects subsequent requests of existing adapters.
/// The legacy SMS/Slack/BlueBubbles mounts retain their earlier wiring.
/// `production_mounts_cover_every_non_rejected_loader_key` keeps this closed
/// against `CHANNEL_LOADER_KEYS`: a key that leaves the rejection table
/// without a mount here would return to the silent skip #2301 removed.
#[cfg(test)]
pub(crate) fn production_loader_mounts() -> ChannelLoaderMounts {
    production_loader_mounts_with(
        Arc::new(DeferredInteractionResponder::default()),
        Arc::new(ChannelNetwork::default()),
    )
}

fn production_loader_mounts_with(
    responder: ResponderRef,
    network: Arc<ChannelNetwork>,
) -> ChannelLoaderMounts {
    let mut mounts = ChannelLoaderMounts::default();
    crate::channel::sms::register_with_network(&mut mounts, responder.clone(), network.clone());
    crate::channel::bluebubbles::register_with_network(
        &mut mounts,
        responder.clone(),
        network.clone(),
    );
    crate::channel::slack::register_with_network(&mut mounts, responder.clone(), network.clone());
    crate::channel::api_server::register(&mut mounts);
    crate::channel::webhook::register(&mut mounts);
    crate::channel::signal::register_with_network(&mut mounts, responder.clone(), network.clone());
    crate::channel::mattermost::register_with_network(
        &mut mounts,
        responder.clone(),
        network.clone(),
    );
    crate::channel::homeassistant::register_with_network(
        &mut mounts,
        responder.clone(),
        network.clone(),
    );
    crate::channel::email::register_with_network(&mut mounts, responder.clone(), network.clone());
    crate::channel::dingtalk::register_with_network(
        &mut mounts,
        responder.clone(),
        network.clone(),
    );
    crate::channel::discord::register_with_network(&mut mounts, responder.clone(), network.clone());
    crate::channel::matrix::register_with_network(&mut mounts, responder.clone(), network.clone());
    crate::channel::telegram::register_with_network(
        &mut mounts,
        responder.clone(),
        network.clone(),
    );
    crate::channel::whatsapp::register_with_network(
        &mut mounts,
        responder.clone(),
        network.clone(),
    );
    mounts.register(
        "wecomCallback",
        // The constructor currently accepts an already-built helper, so this
        // registry is rebuilt for each load. No pending chat state is shared
        // between old and replacement adapter instances.
        crate::channel::wecom_callback::WeComCallbackChannel::constructor(helper_interaction(
            responder,
        )),
    );
    mounts
}

/// TS `ChannelAdapterBundleOptions.createWeixin` — the constructor seam the
/// bundle tests use to swap in a controlled channel.
pub type WeixinChannelMaker =
    Arc<dyn Fn(WeixinChannelOptions) -> Arc<dyn ChannelAdapter> + Send + Sync>;
fn default_weixin_maker() -> WeixinChannelMaker {
    Arc::new(|options| Arc::new(WeixinChannel::new(options)))
}

pub struct NativeChannelsOptions {
    /// Pilot home: state dir `resolve(pilotHome, "channels")` holds the mapper
    /// state files and the runtime-status sink.
    pub pilot_home: PathBuf,
    /// The loaded config snapshot's `config` object (`baseDeps.config`,
    /// pilotdeckServer.ts:78). Drives the `adapters.weixin.enabled` gate.
    pub config: Arc<JsValue>,
    /// The weixin construction seams. Production passes the defaults (real
    /// auth host); tests point the QR login and ilink client at a loopback
    /// mock. `mapper`/`on_state_change` are replaced per construction, and
    /// `runtime_status` is wired to the shared status sink.
    pub weixin: WeixinChannelOptions,
    /// TS `createWeixin` seam: builds the adapter from its options. Tests
    /// swap in a controlled channel to observe the lifecycle.
    pub weixin_maker: Option<WeixinChannelMaker>,
    /// The feishu construction seams (app id/secret, encrypt key, verify
    /// token, connection mode, domain). Production passes the defaults; tests
    /// point `base_url` at a loopback Lark mock.
    pub feishu: FeishuOptions,
    /// Test seam over the console channel log.
    pub log: Option<ChannelLog>,
    /// Test seam over the runtime-status sink (default: the
    /// `ChannelRuntimeStatus` file writer).
    pub report_status: Option<ChannelRuntimeStatusReporter>,
    /// The ChannelServer's close host (default: the no-op embedding host).
    pub close_host: Option<CloseHost>,
}

pub struct NativeChannels {
    server: ChannelServer,
    state: ChannelStatePersistence,
    weixin: WeixinChannelOptions,
    weixin_maker: WeixinChannelMaker,
    gateway: OnceLock<Arc<dyn WeixinGatewayPort>>,
    feishu: FeishuOptions,
    /// `activeFeishu`: the `/feishu/webhook` slot. Published at construction
    /// by every feishu adapter this assembly builds.
    feishu_slot: Arc<FeishuWebhookSlot>,
    /// The A7 port each feishu adapter receives through `ChannelStartDeps`.
    /// The submitter only exists after the host builder ran, so the adapter
    /// gets this handle and waits for `set_turn_submitter`.
    feishu_gateway: Arc<FeishuTurnGateway>,
    owned_gateway: Arc<FeishuTurnGateway>,
    config: Arc<JsValue>,
    loader_mounts: Option<ChannelLoaderMounts>,
    /// TS retains every successfully constructed configured key, including
    /// after it is disabled, so a subsequent reconcile still owns that key.
    managed_channel_keys: Mutex<Vec<ChannelKey>>,
    reload_driver: Mutex<Option<Arc<reload::ReloadDriver>>>,
    closing: AtomicBool,
    reload_active: AtomicBool,
    log: Option<ChannelLog>,
    interaction_responder: Arc<DeferredInteractionResponder>,
    network: Arc<ChannelNetwork>,
    command_host: OnceLock<Arc<dyn crate::channel::commands::CommandHost>>,
}

#[derive(Default)]
struct DeferredInteractionResponder(OnceLock<ResponderRef>);

impl InteractionResponder for DeferredInteractionResponder {
    fn respond_elicitation(
        &self,
        response: crate::channel::sms::ElicitationResponse,
    ) -> crate::channel::protocol::BoxFuture<Result<(), ChannelError>> {
        match self.0.get() {
            Some(responder) => responder.respond_elicitation(response),
            None => Box::pin(async { Err(ChannelError::error("Gateway not ready")) }),
        }
    }

    fn permission_decide(
        &self,
        decision: crate::channel::sms::PermissionDecision,
    ) -> crate::channel::protocol::BoxFuture<
        Result<crate::channel::sms::PermissionDecisionOutcome, ChannelError>,
    > {
        match self.0.get() {
            Some(responder) => responder.permission_decide(decision),
            None => Box::pin(async { Err(ChannelError::error("Gateway not ready")) }),
        }
    }
}

/// The `ChannelStartDeps.turn_gateway` the whole channel server is
/// constructed with (feishu was the first consumer, hence the name). It
/// forwards to a [`SubmitterTurnGateway`] once the assembly publishes the
/// native submitter; until then `submit` fails with the same
/// "not started" shape as a missing gateway. Each request keeps its own
/// channel identity: every adapter submits its `TurnRequest.channel_key`,
/// and the gateway carries it through (#2301 fix-r1 — it used to overwrite
/// the requester's key with this handle's construction-time feishu one).
struct FeishuTurnGateway {
    submitter: Arc<OnceLock<Arc<TurnSubmitter>>>,
    owned: bool,
}

impl FeishuTurnGateway {
    fn new() -> Self {
        Self {
            submitter: Arc::new(OnceLock::new()),
            owned: false,
        }
    }

    fn owned(&self) -> Self {
        Self {
            submitter: self.submitter.clone(),
            owned: true,
        }
    }

    fn set(&self, submitter: Arc<TurnSubmitter>) -> bool {
        self.submitter.set(submitter).is_ok()
    }

    fn inner(&self) -> Option<SubmitterTurnGateway> {
        self.submitter.get().map(|submitter| {
            if self.owned {
                SubmitterTurnGateway::new_owned(submitter.clone())
            } else {
                SubmitterTurnGateway::new(submitter.clone())
            }
        })
    }
}

impl TurnGateway for FeishuTurnGateway {
    fn submit(
        &self,
        request: crate::channel::protocol::TurnRequest,
    ) -> crate::channel::protocol::BoxFuture<
        Result<
            Box<dyn crate::channel::protocol::TurnEventStream>,
            crate::channel::protocol::ChannelError,
        >,
    > {
        match self.inner() {
            Some(gateway) => gateway.submit(request),
            None => Box::pin(async {
                Err(crate::channel::protocol::ChannelError::error(
                    "Gateway not ready",
                ))
            }),
        }
    }
}

/// Adapter-level capability selection; request channel strings never grant it.
struct OwnedChannel {
    inner: Arc<dyn ChannelAdapter>,
    gateway: Arc<FeishuTurnGateway>,
}
impl ChannelAdapter for OwnedChannel {
    fn channel_key(&self) -> &ChannelKey {
        self.inner.channel_key()
    }
    fn start(
        &self,
        mut deps: ChannelStartDeps,
    ) -> crate::channel::protocol::BoxFuture<
        Result<Arc<dyn crate::channel::protocol::ChannelHandle>, ChannelError>,
    > {
        deps.turn_gateway = Some(self.gateway.clone());
        self.inner.start(deps)
    }
    fn deliver_cron_result(
        &self,
        delivery: &crate::channel::im_cron::CronResultDelivery,
    ) -> crate::channel::protocol::BoxFuture<bool> {
        self.inner.deliver_cron_result(delivery)
    }
}

impl NativeChannels {
    pub fn new(options: NativeChannelsOptions) -> Self {
        let responder = Arc::new(DeferredInteractionResponder::default());
        Self::with_loader_mounts(options, None, responder)
    }

    #[cfg(test)]
    pub(crate) fn new_with_loader_mounts(
        options: NativeChannelsOptions,
        mounts: ChannelLoaderMounts,
    ) -> Self {
        Self::with_loader_mounts(
            options,
            Some(mounts),
            Arc::new(DeferredInteractionResponder::default()),
        )
    }

    fn with_loader_mounts(
        options: NativeChannelsOptions,
        loader_mounts: Option<ChannelLoaderMounts>,
        interaction_responder: Arc<DeferredInteractionResponder>,
    ) -> Self {
        let runtime = options
            .report_status
            .unwrap_or_else(|| create_channel_runtime_status_reporter(options.pilot_home.clone()));
        // pilotdeckServer.ts:72-83: one status sink serves both the lifecycle
        // reporter and the adapter's own login-state reports.
        let lifecycle_runtime = runtime.clone();
        let report_status: StatusReporter = Arc::new(move |status| {
            lifecycle_runtime(status.key.as_js(), &lifecycle_update(&status));
        });
        let weixin_runtime = runtime.clone();
        let mut weixin = options.weixin;
        weixin.runtime_status = Some(Arc::new(move |status| {
            weixin_runtime(weixin_key().as_js(), &weixin_update(&status));
        }));
        let state = ChannelStatePersistence::new(options.pilot_home.join("channels"));
        let feishu_gateway = Arc::new(FeishuTurnGateway::new());
        let owned_gateway = Arc::new(feishu_gateway.owned());
        let turn_gateway: Arc<dyn TurnGateway> = feishu_gateway.clone();
        let network = Arc::new(ChannelNetwork::new(options.config.get("proxy")));
        Self {
            server: ChannelServer::new(
                ChannelStartDeps {
                    config: Some(options.config.clone()),
                    report_status: Some(report_status),
                    log: options.log.clone(),
                    // weixin submits through its own `WeixinChannelOptions.gateway`
                    // port (`set_gateway_port`); feishu uses the shared
                    // channel-server port, which the `FeishuTurnGateway` handle
                    // fills in once the native submitter exists.
                    turn_gateway: Some(turn_gateway),
                },
                options.close_host.unwrap_or_else(noop_close_host),
            ),
            state,
            weixin,
            weixin_maker: options.weixin_maker.unwrap_or_else(default_weixin_maker),
            gateway: OnceLock::new(),
            feishu: options.feishu,
            feishu_slot: Arc::new(FeishuWebhookSlot::new()),
            feishu_gateway,
            owned_gateway,
            config: options.config,
            loader_mounts,
            managed_channel_keys: Mutex::new(
                ["feishu", "weixin", "qq", "wecom"]
                    .into_iter()
                    .map(ChannelKey::new)
                    .collect(),
            ),
            reload_driver: Mutex::new(None),
            closing: AtomicBool::new(false),
            reload_active: AtomicBool::new(false),
            log: options.log,
            interaction_responder,
            network,
            command_host: OnceLock::new(),
        }
    }

    pub fn set_command_host(&self, host: Arc<dyn crate::channel::commands::CommandHost>) -> bool {
        self.command_host.set(host).is_ok()
    }

    pub(crate) fn apply_proxy_config(&self, config: &JsValue) {
        self.network.update_proxy(config.get("proxy"));
    }

    pub fn server(&self) -> &ChannelServer {
        &self.server
    }

    /// Publish the same interaction coordinator used by the native turn host
    /// before starting channels. A missing publication fails explicitly; it
    /// never turns permission or elicitation answers into successful no-ops.
    pub fn set_interaction_responder(&self, responder: ResponderRef) -> bool {
        self.interaction_responder.0.set(responder).is_ok()
    }

    /// Subscribe before config watching starts. Proxy updates apply immediately;
    /// adapter updates are gated until startup has installed the channels. The same store drives settings/RPC/file reloads;
    /// this subscription owns only the adapter portion of each event.
    pub fn attach_config_store(
        self: &Arc<Self>,
        store: &crate::config::store::ConfigStore,
    ) -> Result<(), ChannelError> {
        let mut slot = self
            .reload_driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.closing.load(Ordering::SeqCst) {
            return Err(ChannelError::error("Channel server is closing"));
        }
        if slot.is_some() {
            return Err(ChannelError::error(
                "Channel config subscription already installed",
            ));
        }
        *slot = Some(Arc::new(reload::ReloadDriver::new(self, store)?));
        Ok(())
    }

    /// Open the adapter reload gate after startup channels have been queued.
    pub fn activate_config_reload(&self) {
        self.reload_active.store(true, Ordering::SeqCst);
    }

    pub(crate) fn reload_log(&self, level: &str, message: &str) {
        if let Some(log) = &self.log {
            log(level, message);
        } else {
            console_channel_log()(level, message);
        }
    }

    /// The `activeFeishu` slot the native HTTP listener mounts at
    /// `/feishu/webhook`. Stable for the life of the assembly, so the listener
    /// can hold the resolver while the adapter comes and goes.
    pub fn feishu_webhook_slot(&self) -> Arc<FeishuWebhookSlot> {
        self.feishu_slot.clone()
    }

    /// Publish the A7 submitter the feishu adapter submits through.
    pub fn set_turn_submitter(&self, submitter: Arc<TurnSubmitter>) -> bool {
        self.feishu_gateway.set(submitter)
    }

    /// Publish the gateway port the channel submits through. The assembly
    /// calls this once the native submitter exists; every weixin channel
    /// constructed afterwards (startup or hot start) shares it, like the TS
    /// `baseDeps.gateway` each `start()` receives.
    pub fn set_gateway_port(&self, port: Arc<dyn WeixinGatewayPort>) -> bool {
        self.gateway.set(port).is_ok()
    }

    /// ChannelAdapterBundle.makeWeixin: mapper from the persisted state (or a
    /// fresh one), `onStateChange` persisting back through the debounce.
    async fn make_weixin(&self) -> Arc<dyn ChannelAdapter> {
        let saved = self.state.load(weixin_key().as_js()).await;
        let mapper = match saved {
            Some(saved) => Arc::new(WeixinSessionMapper::from_saved(&saved)),
            None => Arc::new(WeixinSessionMapper::new()),
        };
        let state = self.state.clone();
        let on_state_change: WeixinStateChange =
            Arc::new(move |value| state.save(weixin_key().as_js(), value.clone()));
        let mut options = self.weixin.clone();
        options.mapper = mapper;
        options.on_state_change = Some(on_state_change);
        options.gateway = self.gateway.get().cloned();
        (self.weixin_maker)(options)
    }

    /// ChannelAdapterBundle.createWeixin: `adapters.weixin.enabled === true`
    /// gates the startup adapter (ChannelAdapterBundle.ts:145-152).
    pub async fn create_startup_weixin(&self) -> Option<Arc<dyn ChannelAdapter>> {
        if weixin_enabled(&self.config) {
            Some(self.make_weixin().await)
        } else {
            None
        }
    }

    /// ChannelAdapterBundle.hotStartWeixin: state.load → makeWeixin →
    /// hotStartChannel (the channel server replaces any previous instance).
    pub async fn hot_start_weixin(&self) -> Result<(), ChannelError> {
        let channel = self.make_weixin().await;
        self.server.hot_start_channel(channel).await
    }

    /// `ChannelAdapterBundle.createStartupAdapters`'s configured half (#2301):
    /// `loadEnabledChannels(config.adapters)` over the production mounts —
    /// the six implemented loader-table keys construct in table order, a
    /// construction failure `console.error`s and continues (the
    /// error-then-continue loop, `loadEnabledChannels.ts:132-136`), and no
    /// options travel on this path: TS calls
    /// `this.loadConfiguredChannels(config.adapters)` with no second
    /// argument, so `pilotHome` stays `undefined` (only the matrix loader
    /// reads it, and matrix is not implemented). The error port is the same
    /// best-effort console write as the channel log: `assembly::start` calls
    /// this synchronously before any channel starts, outside every
    /// `catch_unwind`, so an `eprintln!` here (std panic on a failed write,
    /// #2301 fix-r2 review P0 #2) would abort the startup of every already
    /// constructed channel; TS's `console.error` never throws.
    pub fn create_startup_configured(&self) -> Vec<Arc<dyn ChannelAdapter>> {
        self.create_configured_with(&self.config)
    }

    fn create_configured_with(&self, config: &JsValue) -> Vec<Arc<dyn ChannelAdapter>> {
        let production_mounts;
        let mounts = match &self.loader_mounts {
            Some(mounts) => mounts,
            None => {
                production_mounts = production_loader_mounts_with(
                    self.interaction_responder.clone(),
                    self.network.clone(),
                );
                &production_mounts
            }
        };
        let channels = load_enabled_channels(
            config.get("adapters"),
            &LoadEnabledChannelsOptions { pilot_home: None },
            mounts,
            &mut |message| console_log_line(&mut std::io::stderr(), &message),
        );
        // Only adapters whose stop contract owns and drains its turns receive
        // this capability. The default deps remain legacy for every other key.
        let channels: Vec<Arc<dyn ChannelAdapter>> = channels
            .into_iter()
            .map(|channel| {
                if [
                    "bluebubbles",
                    "sms",
                    "slack",
                    "signal",
                    "mattermost",
                    "homeassistant",
                    "email",
                    "dingtalk",
                    "discord",
                    "matrix",
                    "telegram",
                    "whatsapp",
                ]
                .iter()
                .any(|key| channel.channel_key().as_js().equals_str(key))
                {
                    Arc::new(OwnedChannel {
                        inner: channel,
                        gateway: self.owned_gateway.clone(),
                    }) as Arc<dyn ChannelAdapter>
                } else {
                    channel
                }
            })
            .collect();
        let mut managed = self
            .managed_channel_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for channel in &channels {
            if !managed.contains(channel.channel_key()) {
                managed.push(channel.channel_key().clone());
            }
        }
        channels
    }

    /// ChannelAdapterBundle.makeFeishu: mapper from the persisted state,
    /// `onStateChange` persisting back through the debounce, and the active
    /// slot published at construction (TS assigns `activeFeishu` before
    /// `start`).
    async fn make_feishu(&self) -> Arc<dyn ChannelAdapter> {
        self.make_feishu_with(&self.config).await
    }

    /// [`Self::make_feishu`] against an explicit config: `reload` must build
    /// from the config it was handed, not the one the assembly started with
    /// (`ChannelAdapterBundle.createStartupAdapters(config)` reads the passed
    /// snapshot, so a reloaded app secret / verify token / domain takes
    /// effect).
    async fn make_feishu_with(&self, config: &JsValue) -> Arc<dyn ChannelAdapter> {
        let saved = self.state.load(feishu_key().as_js()).await;
        let state = self.state.clone();
        let on_state_change =
            Arc::new(move |value: JsValue| state.save(feishu_key().as_js(), value));
        Arc::new(
            FeishuChannel::for_assembly(
                self.configured_feishu_options(config),
                helper_interaction(self.interaction_responder.clone()),
                self.feishu_slot.clone(),
                saved.as_ref(),
                Some(on_state_change),
            )
            .with_network(self.network.clone()),
        )
    }

    /// `createFeishu`'s constructor argument: the `adapters.feishu` settings
    /// (an absent field stays `undefined` → the constructor's default). The
    /// assembly-level options supply the loopback `base_url` seam tests need;
    /// a real config keeps the origin its `domainName` selects.
    fn configured_feishu_options(&self, config: &JsValue) -> FeishuOptions {
        let Some(settings) = config
            .get("adapters")
            .and_then(|adapters| adapters.get("feishu"))
            .and_then(JsValue::as_object)
        else {
            return self.feishu.clone();
        };
        let mut options = FeishuOptions::from_object(settings);
        if self.feishu.base_url != crate::channel::feishu::FEISHU_BASE_URL
            && self.feishu.base_url != crate::channel::feishu::LARK_BASE_URL
        {
            options.base_url = self.feishu.base_url.clone();
        }
        options
    }

    /// ChannelAdapterBundle.createFeishu: `adapters.feishu.enabled === true`.
    pub async fn create_startup_feishu(&self) -> Option<Arc<dyn ChannelAdapter>> {
        if feishu_enabled(&self.config) {
            Some(self.make_feishu().await)
        } else {
            None
        }
    }

    /// ChannelAdapterBundle.hotStartFeishu. Pure reservation: the TS bundle
    /// only defines `hotStartWeixin` (the feishu login callback does not
    /// exist), and the native process has no feishu config hot-reload event
    /// source yet, so nothing calls this. Kept as the symmetric seam for the
    /// day a feishu hot start is wired.
    #[allow(dead_code)]
    pub async fn hot_start_feishu(&self) -> Result<(), ChannelError> {
        let channel = self.make_feishu().await;
        self.server.hot_start_channel(channel).await
    }

    /// ChannelAdapterBundle.reload: rebuild configured adapters from the new
    /// snapshot and reconcile against every key this bundle has ever managed.
    /// The server's base start dependencies remain its initial snapshot, as
    /// in TS; only constructor options come from this reload's config.
    #[allow(dead_code)]
    pub async fn reload(&self, config: &JsValue) -> Result<ReconcileResult, ChannelError> {
        let mut channels = Vec::new();
        if feishu_enabled(config) {
            channels.push(self.make_feishu_with(config).await);
        }
        if weixin_enabled(config) {
            channels.push(self.make_weixin().await);
        }
        if bundle_enabled(config, "qq") {
            channels.push(self.make_qq_with(config).await);
        }
        if bundle_enabled(config, "wecom") {
            channels.push(self.make_wecom_with(config).await);
        }
        channels.extend(self.create_configured_with(config));
        let managed_channel_keys = self
            .managed_channel_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.server
            .reconcile_channels(ReconcileInput {
                channels,
                managed_channel_keys,
            })
            .await
    }

    async fn configured_qq_options(
        &self,
        config: &JsValue,
    ) -> crate::channel::qq::QQChannelOptions {
        use crate::channel::qq::{QQChannelOptions, QQSessionMapper, QQSessionMapperState};
        let settings = config
            .get("adapters")
            .and_then(|v| v.get("qq"))
            .and_then(JsValue::as_object)
            .cloned()
            .unwrap_or_default();
        let mut options =
            QQChannelOptions::from_config(&settings).with_network(self.network.clone());
        if let Some(saved) = self.state.load(&"qq".into()).await {
            options.mapper = Some(QQSessionMapper::new(
                QQSessionMapperState::from_js(&saved),
                Box::new(|| uuid::Uuid::new_v4().to_string().into()),
            ));
        }
        let state = self.state.clone();
        options.on_state_change = Some(Arc::new(move |value| {
            state.save(&"qq".into(), value.to_js())
        }));
        options
    }

    async fn configured_wecom_options(
        &self,
        config: &JsValue,
    ) -> crate::channel::wecom::WeComChannelOptions {
        use crate::channel::wecom::{
            WeComChannelOptions, WeComSessionMapper, WeComSessionMapperState,
        };
        let settings = config.get("adapters").and_then(|v| v.get("wecom"));
        let saved = self.state.load(&"wecom".into()).await;
        let mapper = saved.as_ref().map(|value| {
            WeComSessionMapper::new(
                WeComSessionMapperState::from_js(value),
                Arc::new(|| uuid::Uuid::new_v4().to_string().into()),
            )
        });
        let state = self.state.clone();
        WeComChannelOptions {
            bot_key: settings
                .and_then(|v| v.get("token"))
                .and_then(JsValue::as_string)
                .cloned(),
            extra: settings
                .and_then(|v| v.get("extra"))
                .and_then(JsValue::as_object)
                .cloned(),
            mapper,
            command_host: self.command_host.get().cloned(),
            on_state_change: Some(Arc::new(move |value| {
                state.save(&"wecom".into(), value.to_js())
            })),
            ..Default::default()
        }
    }

    async fn make_qq_with(&self, config: &JsValue) -> Arc<dyn ChannelAdapter> {
        let channel = crate::channel::qq::QQChannel::new(
            self.configured_qq_options(config).await,
            crate::channel::im_interaction::js_helper_interaction(
                self.interaction_responder.clone(),
            ),
        );
        Arc::new(OwnedChannel {
            inner: Arc::new(channel),
            gateway: self.owned_gateway.clone(),
        })
    }

    async fn make_wecom_with(&self, config: &JsValue) -> Arc<dyn ChannelAdapter> {
        let channel = crate::channel::wecom::WeComChannel::with_network(
            self.configured_wecom_options(config).await,
            crate::channel::im_interaction::js_helper_interaction(
                self.interaction_responder.clone(),
            ),
            self.network.clone(),
        );
        Arc::new(OwnedChannel {
            inner: Arc::new(channel),
            gateway: self.owned_gateway.clone(),
        })
    }

    pub async fn create_startup_qq(&self) -> Option<Arc<dyn ChannelAdapter>> {
        if bundle_enabled(&self.config, "qq") {
            Some(self.make_qq_with(&self.config).await)
        } else {
            None
        }
    }

    pub async fn create_startup_wecom(&self) -> Option<Arc<dyn ChannelAdapter>> {
        if bundle_enabled(&self.config, "wecom") {
            Some(self.make_wecom_with(&self.config).await)
        } else {
            None
        }
    }

    /// ChannelAdapterBundle.flush.
    pub async fn flush(&self) -> Result<(), String> {
        self.state.flush().await
    }

    /// pilotdeckServer.closeServer's channel half (stop every running channel
    /// through the lifecycle core, then run the close host).
    pub async fn close(&self) -> Result<(), ChannelError> {
        self.closing.store(true, Ordering::SeqCst);
        let reload = self
            .reload_driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let reload_result = match reload {
            Some(driver) => driver.drain().await,
            None => Ok(()),
        };
        let stop_result = self.server.close().await;
        match (reload_result, stop_result) {
            (Err(reload), Err(stop)) => Err(ChannelError::Aggregate {
                message: "Channel shutdown failed".to_owned(),
                errors: vec![reload, stop],
            }),
            (reload, stop) => reload.and(stop),
        }
    }

    /// The CLI `prepareWeixinLogin` callback (pilotdeck.ts:240-247):
    /// `requestedAt` is captured before the hot start and returned with
    /// `requested: true` after it. The `!serverRef` unsupported branch is
    /// unreachable for the native process — the channel server is constructed
    /// with the gateway — so the no-callback shape stays the `Requests` field
    /// staying `None`.
    pub fn prepare_login_callback(self: &Arc<Self>) -> PrepareWeixinLogin {
        let channels = self.clone();
        Arc::new(move || {
            let channels = channels.clone();
            Box::pin(async move {
                let requested_at = now_iso();
                channels.hot_start_weixin().await.map_err(|error| {
                    crate::runtime::connection::RequestError::new(error.message())
                })?;
                Ok(crate::metadata::object([
                    ("requested", JsValue::Bool(true)),
                    ("requestedAt", JsValue::String(requested_at)),
                ]))
            })
        })
    }
}

/// `config.adapters?.weixin?.enabled !== true` → disabled
/// (ChannelAdapterBundle.ts:146 — a strict `=== true` gate).
fn weixin_enabled(config: &JsValue) -> bool {
    config
        .get("adapters")
        .and_then(|adapters| adapters.get("weixin"))
        .and_then(|weixin| weixin.get("enabled"))
        == Some(&JsValue::Bool(true))
}

/// `config.adapters?.feishu?.enabled === true`.
fn feishu_enabled(config: &JsValue) -> bool {
    config
        .get("adapters")
        .and_then(|adapters| adapters.get("feishu"))
        .and_then(|feishu| feishu.get("enabled"))
        == Some(&JsValue::Bool(true))
}

fn lifecycle_update(status: &ChannelStatus) -> ChannelRuntimeStatusUpdate {
    let state = match status.state {
        ChannelState::Starting => "starting",
        ChannelState::Connected => "connected",
        ChannelState::Stopped => "stopped",
        ChannelState::Failed => "failed",
    };
    ChannelRuntimeStatusUpdate {
        state: state.to_string(),
        message: Some(status.message.clone()),
        account_id: None,
        error: status.error.clone(),
        qr_url: None,
    }
}

fn weixin_update(status: &WeixinChannelStatus) -> ChannelRuntimeStatusUpdate {
    ChannelRuntimeStatusUpdate {
        state: status.state.as_str().to_string(),
        message: Some(status.message.clone()),
        account_id: status.account_id.clone(),
        error: status.error.clone(),
        qr_url: status.qr_url.clone(),
    }
}

fn now_iso() -> JsString {
    use pilotdeck_transport::{server::ExecutionClock, system_clock::SystemExecutionClock};
    SystemExecutionClock.now_iso()
}

fn bundle_enabled(config: &JsValue, key: &str) -> bool {
    matches!(
        config
            .get("adapters")
            .and_then(|v| v.get(key))
            .and_then(|v| v.get("enabled")),
        Some(JsValue::Bool(true))
    )
}
