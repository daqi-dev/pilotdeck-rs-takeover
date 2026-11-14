//! Config-based channel enabling (`src/adapters/channel/loadEnabledChannels.ts`).
//!
//! The TS CHANNEL_LOADERS table is a fixed 15-entry record: iteration order,
//! the `cfg?.enabled` truthiness gate, the per-key `cfg`-to-options
//! extraction (absent optionals stay present-but-undefined properties), and
//! error-then-continue on a failed construction. The real channel classes
//! land with the A8.3-ch-* units, so each entry's constructor is a mount
//! point; an unregistered key enabled in config fails closed exactly like a
//! failed dynamic import and the loop moves on.

use std::{collections::HashMap, sync::Arc};

use pilotdeck_protocol::{JsObject, JsString, JsValue};

use super::protocol::{ChannelAdapter, ChannelError};

/// `Object.entries(CHANNEL_LOADERS)` order.
pub const CHANNEL_LOADER_KEYS: [&str; 15] = [
    "telegram",
    "discord",
    "slack",
    "matrix",
    "mattermost",
    "signal",
    "whatsapp",
    "bluebubbles",
    "dingtalk",
    "wecomCallback",
    "email",
    "sms",
    "homeassistant",
    "apiServer",
    "webhook",
];

/// The 12 channel config keys without a Rust implementation (#1302 B,
/// #1386). The set is ten `CHANNEL_LOADER_KEYS` members plus `qq` and
/// `wecom`, which TS builds through `ChannelAdapterBundle` instead of the
/// loader table. None of them refuses startup (2026-11-13,
/// `state/takeover.md` 人已定 12): an enabled one starts as the baseline's
/// one-line stand-in ([`unavailable_start`], [`bundle_unavailable_start`]),
/// and a configuration the baseline would actually connect with starts as a
/// stand-in that logs [`rust_unavailable_line`]. The order is reverse
/// `CHANNEL_LOADER_KEYS` order, then `qq`, then `wecom`.
pub const UNMIGRATED_CHANNEL_KEYS: &[&str] = &[
    "homeassistant",
    "email",
    "dingtalk",
    "whatsapp",
    "signal",
    "mattermost",
    "matrix",
    "slack",
    "discord",
    "telegram",
    "qq",
    "wecom",
];

/// The two keys whose TS gate is strict. `qq` and `wecom` are built by
/// `ChannelAdapterBundle.createQQ` / `createWeCom`, which both start with
/// `if (settings?.enabled !== true) return undefined` — a config of
/// `{"enabled": 1}` or `{"enabled": "true"}` builds nothing. The other ten
/// keys are consumed by `loadEnabledChannels`'s `if (!cfg?.enabled)
/// continue`, i.e. JS truthiness. Both sides coerce `enabled` to a strict
/// bool before their gates on a real config file (`config/snapshot.rs`
/// here, `parseGatewayConfig`'s `booleanField` in TS), so the split only
/// shows on a raw object.
#[cfg(test)]
pub(crate) const STRICT_TRUE_CHANNEL_KEYS: &[&str] = &["qq", "wecom"];

/// The TS `enabled` gate for one key: strict `=== true` for the
/// `ChannelAdapterBundle` keys, JS truthiness (`js_truthy`) for the loader
/// table.
#[cfg(test)]
fn enabled_by_baseline_gate(key: &str, cfg: &JsValue) -> bool {
    if STRICT_TRUE_CHANNEL_KEYS.contains(&key) {
        matches!(cfg.get("enabled"), Some(JsValue::Bool(true)))
    } else {
        js_truthy(cfg.get("enabled"))
    }
}

/// The line a channel logs (logger `error`, like the baseline's `not
/// installed` lines) when its configuration is one the baseline would
/// actually connect with. Rust has no production implementation of these
/// channels (`state/takeover.md` 人已定 6 / 12), so the stand-in starts with
/// a no-op handle and the rest of the server is unaffected. This is Rust's
/// own wording — the baseline connects at this point — and an intended
/// divergence.
pub fn rust_unavailable_line(key: &str) -> String {
    format!("{key}: not available in the Rust gateway; this channel will not connect")
}

/// The first enabled key, in [`UNMIGRATED_CHANNEL_KEYS`] order, whose
/// configuration the baseline would actually connect with — the keys that
/// start as the [`rust_unavailable_line`] stand-in. Native startup used to
/// refuse exactly these configurations (#1302 B); the classification stays
/// pinned by the channel tests through this helper.
#[cfg(test)]
pub(crate) fn connecting_unmigrated_channel(adapters: Option<&JsValue>) -> Option<&'static str> {
    connecting_unmigrated_channel_with_env(adapters, &process_env)
}

/// [`connecting_unmigrated_channel`] over an explicit environment lookup.
#[cfg(test)]
pub(crate) fn connecting_unmigrated_channel_with_env(
    adapters: Option<&JsValue>,
    env: &EnvLookupFn,
) -> Option<&'static str> {
    let adapters = adapters?;
    for key in UNMIGRATED_CHANNEL_KEYS {
        let Some(cfg) = adapters.get(key) else {
            continue;
        };
        if !enabled_by_baseline_gate(key, cfg) {
            continue;
        }
        if let Some(UnavailableStart::Logged { .. }) = bundle_unavailable_start(key, cfg, env) {
            continue;
        }
        if CHANNEL_LOADER_KEYS.contains(key) {
            let options = build_options(key, cfg, &LoadEnabledChannelsOptions { pilot_home: None });
            if !matches!(
                unavailable_start(key, &options, env),
                Some(UnavailableStart::Unsupported)
            ) {
                continue;
            }
        }
        return Some(key);
    }
    None
}

/// `process.env[name]`.
pub type EnvLookupFn = dyn Fn(&str) -> Option<JsString> + Send + Sync;

pub(crate) fn process_env(name: &str) -> Option<JsString> {
    std::env::var(name).ok().map(JsString::from)
}

/// How a loader-table channel without a Rust implementation starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnavailableStart {
    /// The baseline's start logs this line through `logger.error` and returns
    /// a no-op handle; the process keeps running.
    Line(&'static str),
    /// Like [`Self::Line`], with the line built from the options and the
    /// logger level spelled out (`info` goes to stdout, the rest to stderr).
    Logged { level: &'static str, line: String },
    /// The constructor throws a `TypeError` with this message on an option
    /// of the wrong type: `loadEnabledChannels` logs `[adapters] Failed to
    /// load channel "<key>": TypeError: <message>` and moves on
    /// (loadEnabledChannels.ts:132-136).
    ConstructorTypeError(&'static str),
    /// `start` rejects with a `TypeError` with this message before any
    /// connection is made: the server logs `[adapters] channel <key> start
    /// failed: <message>` and keeps running (pilotdeckServer.ts:275-285).
    StartTypeError(&'static str),
    /// The baseline would go further (a real connection). Rust starts a
    /// stand-in that logs [`rust_unavailable_line`] instead.
    Unsupported,
}

/// The six loader channels whose SDK the baseline does not ship
/// (`grammy`, `discord.js`, `@slack/bolt`, `matrix-bot-sdk`,
/// `dingtalk-stream`, `imapflow` / `nodemailer` are absent from its
/// `package.json`): `start` always stops at the `not installed` branch
/// (TelegramChannel.ts:50-53, DiscordChannel.ts:48-51, SlackChannel.ts:49-52,
/// MatrixChannel.ts:61-64, DingTalkChannel.ts:56-59, EmailChannel.ts:61-64).
const SDK_MISSING_LINES: &[(&str, &str)] = &[
    (
        "telegram",
        "telegram: grammy not installed; run `npm install grammy`",
    ),
    (
        "discord",
        "discord: discord.js not installed; run `npm install discord.js`",
    ),
    (
        "slack",
        "slack: @slack/bolt not installed; run `npm install @slack/bolt`",
    ),
    (
        "matrix",
        "matrix: matrix-bot-sdk not installed; run `npm install matrix-bot-sdk`",
    ),
    (
        "dingtalk",
        "dingtalk: dingtalk-stream not installed; run `npm install dingtalk-stream`",
    ),
    (
        "email",
        "email: imapflow and/or nodemailer not installed; run `npm install imapflow nodemailer`",
    ),
];

/// The loader channels Rust does not implement, by key: what the baseline's
/// constructor and first start check do with these options (`options` is the
/// object [`build_options`] makes) and this environment. `None` for keys Rust
/// implements.
pub fn unavailable_start(
    key: &str,
    options: &JsObject,
    env: &EnvLookupFn,
) -> Option<UnavailableStart> {
    use UnavailableStart::{ConstructorTypeError, Line, Logged, StartTypeError, Unsupported};
    let undefined = JsValue::Undefined;
    let is_string = |value: &JsValue| matches!(value, JsValue::String(_));
    let option = |name: &str| options.get(name).unwrap_or(&undefined).clone();
    // `options.x ?? process.env.X ?? fallback`.
    let either = |name: &str, var: &str, fallback: JsValue| {
        let value = option(name);
        if !nullish(&value) {
            return value;
        }
        env(var).map(JsValue::String).unwrap_or(fallback)
    };
    let truthy = |value: &JsValue| js_truthy(Some(value));
    if let Some((_, line)) = SDK_MISSING_LINES.iter().find(|(name, _)| *name == key) {
        // MatrixChannel's constructor calls `.replace` on the homeserver
        // (MatrixChannel.ts:49) before `start` is ever reached; V8 names the
        // compiled call expression in its message.
        if key == "matrix" && !is_string(&either("homeserver", "MATRIX_HOMESERVER", "".into())) {
            return Some(ConstructorTypeError(
                "(options.homeserver ?? process.env.MATRIX_HOMESERVER ?? \"\").replace is not a function",
            ));
        }
        return Some(Line(line));
    }
    match key {
        // MattermostChannel.ts:48-49 constructor, :57-60 start; the first
        // thing start does with the URL is `this.serverUrl.replace(...)`
        // (:66), before any request.
        "mattermost" => {
            let token = either("token", "MATTERMOST_TOKEN", "".into());
            let server_url = either("serverUrl", "MATTERMOST_URL", "".into());
            Some(if !truthy(&server_url) || !truthy(&token) {
                Line(
                    "mattermost: serverUrl and token are required (MATTERMOST_URL, MATTERMOST_TOKEN)",
                )
            } else if let JsValue::String(server_url) = &server_url {
                // The token check is `fetch(apiBase + "/users/me")` (:68-74,
                // :289-290): a fetch that rejects before any connection is
                // logged and start returns a no-op handle.
                match mattermost_token_check_failure(&String::from_utf16_lossy(server_url.units()))
                {
                    Some(error) => Logged {
                        level: "error",
                        line: format!("mattermost: token check failed: {error}"),
                    },
                    None => Unsupported,
                }
            } else {
                StartTypeError("this.serverUrl.replace is not a function")
            })
        }
        // SignalChannel.ts:83-87 constructor (`normalizeBaseUrl` calls
        // `url.replace`, :27-29), :93-96 start.
        "signal" => {
            let rest_url = either("restUrl", "SIGNAL_HTTP_URL", "http://127.0.0.1:8080".into());
            let account = either("account", "SIGNAL_ACCOUNT", "".into());
            Some(if !is_string(&rest_url) {
                ConstructorTypeError("url.replace is not a function")
            } else if !truthy(&account) {
                Line("signal: SIGNAL_ACCOUNT / account not set")
            } else {
                Unsupported
            })
        }
        // WhatsAppChannel.ts:51-54 constructor (`.trim()`, then
        // `normalizeBaseUrl`'s `url.replace`, :27-29), :61-64 start.
        "whatsapp" => {
            let bridge_path = either("bridgePath", "WHATSAPP_BRIDGE_PATH", "".into());
            let bridge_url = either("bridgeUrl", "WHATSAPP_BRIDGE_URL", "".into());
            let JsValue::String(path) = &bridge_path else {
                return Some(ConstructorTypeError(
                    "(options.bridgePath ?? process.env.WHATSAPP_BRIDGE_PATH ?? \"\").trim is not a function",
                ));
            };
            Some(if !is_string(&bridge_url) {
                ConstructorTypeError("url.replace is not a function")
            } else if crate::metadata::js_trim(&String::from_utf16_lossy(path.units())).is_empty() {
                Line("whatsapp: bridgePath / WHATSAPP_BRIDGE_PATH is required")
            } else {
                Unsupported
            })
        }
        // HomeAssistantChannel.ts:67 constructor, :76-79 start; with a token,
        // start's `openSocket` first calls `httpToWs(this.url)` (:35-36),
        // whose `base.startsWith` throws on a non-string URL before any
        // connection.
        "homeassistant" => {
            let token = either("token", "HASS_TOKEN", JsValue::Undefined);
            let url = either("url", "HASS_URL", "http://127.0.0.1:8123".into());
            Some(if !truthy(&token) {
                Line("homeassistant: HASS_TOKEN not set")
            } else if let JsValue::String(url) = &url {
                // `new URL(...)` (:36) runs inside the auth promise's
                // executor, so an unparsable URL rejects start with its
                // TypeError before any socket opens.
                let url = String::from_utf16_lossy(url.units());
                let base = if url.starts_with("http") {
                    url
                } else {
                    format!("http://{url}")
                };
                if url::Url::parse(&base).is_err() {
                    StartTypeError("Invalid URL")
                } else {
                    Unsupported
                }
            } else {
                StartTypeError("base.startsWith is not a function")
            })
        }
        _ => None,
    }
}

/// `${e}` of the rejection MattermostChannel's token check
/// (`fetch(apiBase + "/users/me")`) settles with before any connection is
/// made, as Node's fetch reports it: an unparsable URL, a URL carrying
/// credentials, or a scheme fetch does not serve. `None` when the request
/// would go out (http, https) or be answered locally (`data:`); the baseline
/// follows either with a real connection.
fn mattermost_token_check_failure(server_url: &str) -> Option<String> {
    // `this.serverUrl.replace(/\/$/, "")`: one trailing slash.
    let api_base = server_url.strip_suffix('/').unwrap_or(server_url);
    let target = format!("{api_base}/users/me");
    let Ok(parsed) = url::Url::parse(&target) else {
        return Some(format!("TypeError: Failed to parse URL from {target}"));
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some(format!(
            "TypeError: Request cannot be constructed from a URL that includes credentials: {target}"
        ));
    }
    match parsed.scheme() {
        "http" | "https" | "data" => None,
        _ => Some("TypeError: fetch failed".to_string()),
    }
}

/// How `ChannelAdapterBundle`'s `qq` / `wecom` channel starts from its parsed
/// settings (`cfg`, the snapshot's `adapters.<key>`, read when `enabled ===
/// true`) and this environment. Neither channel has a Rust implementation:
/// without usable credentials the baseline's start logs one line and returns
/// a no-op handle ([`UnavailableStart::Logged`]); with them it connects
/// ([`UnavailableStart::Unsupported`], a [`rust_unavailable_line`] stand-in
/// here). `None` for any other key.
pub fn bundle_unavailable_start(
    key: &str,
    cfg: &JsValue,
    env: &EnvLookupFn,
) -> Option<UnavailableStart> {
    // `a ?? b ?? ... ?? ""`.
    let coalesce = |values: [JsValue; 4]| {
        values
            .into_iter()
            .find(|value| !nullish(value))
            .unwrap_or_else(|| "".into())
    };
    let env_value = |name: &str| env(name).map(JsValue::String).unwrap_or_else(undefined);
    match key {
        // QQChannel.ts:40-43 constructor (`options.appId ??
        // process.env.QQ_BOT_APPID ?? ""`), :55-58 start (`logger.info`).
        "qq" => {
            let app_id = coalesce([
                field(cfg, "appId"),
                env_value("QQ_BOT_APPID"),
                undefined(),
                undefined(),
            ]);
            let secret = coalesce([
                field(cfg, "clientSecret"),
                env_value("QQ_BOT_SECRET"),
                undefined(),
                undefined(),
            ]);
            Some(if !js_truthy(Some(&app_id)) || !js_truthy(Some(&secret)) {
                UnavailableStart::Logged {
                    level: "info",
                    line: "qq: QQ_BOT_APPID or QQ_BOT_SECRET not set, QQ channel disabled."
                        .to_string(),
                }
            } else {
                UnavailableStart::Unsupported
            })
        }
        // WeComChannel.ts:200-206 constructor: `String(options.botKey ??
        // ex.bot_id ?? ex.botId ?? process.env.WECOM_BOT_ID ?? "").trim()`
        // (`botKey` is the settings' `token`, ChannelAdapterBundle.ts:177)
        // and `String(ex.botSecret ?? ex.secret ?? process.env.WECOM_SECRET
        // ?? "").trim()`, `ex` being `{}` unless `extra` is an object;
        // :245-248 start (`logger.error`).
        "wecom" => {
            let bot_id = coalesce([
                field(cfg, "token"),
                extra_field(cfg, "bot_id"),
                extra_field(cfg, "botId"),
                env_value("WECOM_BOT_ID"),
            ]);
            let secret = coalesce([
                extra_field(cfg, "botSecret"),
                extra_field(cfg, "secret"),
                env_value("WECOM_SECRET"),
                undefined(),
            ]);
            // A value `String` throws on (an object with its own `toString`)
            // makes the baseline constructor throw and the server fail to
            // start: that is not a one-line start.
            let blank = |value: &JsValue| {
                crate::js_value::to_string_units_checked(value).map(|units| {
                    crate::metadata::js_trim(&String::from_utf16_lossy(&units)).is_empty()
                })
            };
            Some(match (blank(&bot_id), blank(&secret)) {
                (Ok(id_blank), Ok(secret_blank)) if id_blank || secret_blank => {
                    UnavailableStart::Logged {
                        level: "error",
                        line: "wecom: botKey (bot_id) and secret are required".to_string(),
                    }
                }
                _ => UnavailableStart::Unsupported,
            })
        }
        _ => None,
    }
}

/// The `ChannelAdapterBundle` keys that are not in [`CHANNEL_LOADER_KEYS`].
/// TS builds `weixin` and `feishu` through `ChannelAdapterBundle.createWeixin`
/// / `createFeishu` rather than the loader table, and neither is in
/// [`UNMIGRATED_CHANNEL_KEYS`] — the two bundle keys that are (`qq`, `wecom`)
/// start as stand-ins (`unavailable::UnavailableChannel`). Without this list
/// the same silent skip would simply reappear under a different key name
/// (#1694 §7).
pub const BUNDLE_CHANNEL_KEYS: &[&str] = &["weixin", "feishu"];

/// The refusal reason [`reject_unimplemented_bundle_channels`] prints. The
/// production declaration covers both bundle keys, so it is reachable only
/// if a bundle key loses its constructor.
pub const UNMIGRATED_REASON: &str = "未迁移到 Rust，需继续使用 TS gateway";

/// `#1694 §7`: an enabled `ChannelAdapterBundle` key with no assembly
/// implementation refuses startup — never a silent skip, a downgrade to disabled, or a fallback to TS. The
/// assembly declares what it actually constructs (`implemented`); a key it
/// cannot build is rejected before any channel is constructed.
///
/// Both constructors gate on strict `=== true` (`ChannelAdapterBundle.ts:146`,
/// `createFeishu`), which is why this check does not use [`js_truthy`]: on a
/// raw object `{"enabled": 1}` builds nothing in TS and must not reject. The
/// production path feeds the parsed snapshot, where `config/snapshot.rs`
/// (`enabled_only`, `feishu`) has already collapsed `enabled` to a real bool,
/// so the two gates agree on every config file.
pub fn reject_unimplemented_bundle_channels(
    adapters: Option<&JsValue>,
    implemented: &[&str],
) -> Result<(), ChannelError> {
    let Some(adapters) = adapters else {
        return Ok(());
    };
    for key in BUNDLE_CHANNEL_KEYS {
        let Some(cfg) = adapters.get(key) else {
            continue;
        };
        // Strict `=== true`, matching both bundle constructors' own gate. The
        // value is bound first so this line does not repeat the byte anchor the
        // frozen `channel-unmigrated` mutation controls pin on
        // `rejects_enabled` (that anchor must stay unique).
        let enabled = cfg.get("enabled");
        if !matches!(enabled, Some(JsValue::Bool(true))) {
            continue;
        }
        if implemented.contains(key) {
            continue;
        }
        return Err(ChannelError::error(format!(
            "[adapters] channel \"{key}\" {UNMIGRATED_REASON}"
        )));
    }
    Ok(())
}

/// One mount point: constructs the channel from its option object.
pub type ChannelAdapterConstructor =
    Arc<dyn Fn(&JsObject) -> Result<Arc<dyn ChannelAdapter>, ChannelError> + Send + Sync>;

#[derive(Default)]
pub struct ChannelLoaderMounts {
    creators: HashMap<&'static str, ChannelAdapterConstructor>,
}

impl ChannelLoaderMounts {
    pub fn register(&mut self, key: &'static str, creator: ChannelAdapterConstructor) {
        self.creators.insert(key, creator);
    }
    pub fn is_registered(&self, key: &str) -> bool {
        self.creators.contains_key(key)
    }
    fn creator(&self, key: &str) -> ChannelAdapterConstructor {
        self.creators.get(key).expect("registered key").clone()
    }
}

pub struct LoadEnabledChannelsOptions {
    pub pilot_home: Option<JsString>,
}

/// `loadEnabledChannels(adapters, options)`: synchronous like the TS body's
/// sequential `await` chain — constructions and error lines interleave in
/// table order. `console.error` lines are reported through `console_error`.
pub fn load_enabled_channels(
    adapters: Option<&JsValue>,
    options: &LoadEnabledChannelsOptions,
    mounts: &ChannelLoaderMounts,
    console_error: &mut dyn FnMut(String),
) -> Vec<Arc<dyn ChannelAdapter>> {
    let Some(adapters) = adapters else {
        return Vec::new();
    };
    let mut channels = Vec::new();
    for key in CHANNEL_LOADER_KEYS {
        let Some(cfg) = adapters.get(key) else {
            continue;
        };
        if !js_truthy(cfg.get("enabled")) {
            continue;
        }
        let constructor = match mounts.is_registered(key).then(|| mounts.creator(key)) {
            Some(constructor) => constructor,
            None => {
                console_error(format!(
                    "[adapters] Failed to load channel \"{key}\": Error: channel module not registered: {key}"
                ));
                continue;
            }
        };
        let built = build_options(key, cfg, options);
        match constructor(&built) {
            Ok(channel) => channels.push(channel),
            Err(error) => console_error(format!(
                "[adapters] Failed to load channel \"{key}\": {}",
                error.to_js_string()
            )),
        }
    }
    channels
}

/// JavaScript truthiness for the `cfg?.enabled` gate (and, since #1334, the
/// webhook channel's `route.secret || globalSecret` and extractText gates).
pub(super) fn js_truthy(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Undefined) | Some(JsValue::Null) | Some(JsValue::Bool(false)) => false,
        Some(JsValue::Bool(true)) | Some(JsValue::Object(_)) | Some(JsValue::Array(_)) => true,
        Some(JsValue::Number(number)) => *number != 0.0 && !number.is_nan(),
        Some(JsValue::String(text)) => !text.is_empty(),
    }
}

fn undefined() -> JsValue {
    JsValue::Undefined
}

/// `cfg.token`, `cfg.webhookUrl`, ... — the property when present, an
/// undefined value otherwise.
fn field(cfg: &JsValue, name: &str) -> JsValue {
    cfg.get(name).cloned().unwrap_or_else(undefined)
}

fn extra_field(cfg: &JsValue, name: &str) -> JsValue {
    cfg.get("extra")
        .and_then(|extra| extra.as_object())
        .and_then(|extra| extra.get(name))
        .cloned()
        .unwrap_or_else(undefined)
}

fn nullish(value: &JsValue) -> bool {
    matches!(value, JsValue::Undefined | JsValue::Null)
}

/// The exact option object each TS loader literal builds, insertion order
/// included; absent config values become present-but-undefined properties.
fn build_options(key: &str, cfg: &JsValue, options: &LoadEnabledChannelsOptions) -> JsObject {
    let mut built = JsObject::new();
    let insert = |object: &mut JsObject, name: &str, value: JsValue| {
        object.insert(name.into(), value);
    };
    match key {
        "telegram" => {
            insert(&mut built, "token", field(cfg, "token"));
            insert(&mut built, "webhookUrl", field(cfg, "webhookUrl"));
        }
        "discord" => {
            insert(&mut built, "token", field(cfg, "token"));
        }
        "slack" => {
            insert(&mut built, "botToken", field(cfg, "token"));
            insert(&mut built, "appToken", extra_field(cfg, "appToken"));
        }
        "matrix" => {
            insert(&mut built, "accessToken", field(cfg, "token"));
            insert(&mut built, "homeserver", extra_field(cfg, "homeserver"));
            insert(&mut built, "userId", extra_field(cfg, "userId"));
            insert(
                &mut built,
                "pilotHome",
                options
                    .pilot_home
                    .clone()
                    .map(JsValue::String)
                    .unwrap_or_else(undefined),
            );
        }
        "mattermost" => {
            insert(&mut built, "token", field(cfg, "token"));
            insert(&mut built, "serverUrl", extra_field(cfg, "serverUrl"));
            insert(&mut built, "teamId", extra_field(cfg, "teamId"));
        }
        "signal" => {
            insert(&mut built, "restUrl", extra_field(cfg, "restUrl"));
            insert(&mut built, "account", extra_field(cfg, "account"));
        }
        "whatsapp" => {
            insert(&mut built, "bridgePath", extra_field(cfg, "bridgePath"));
            insert(&mut built, "bridgeUrl", extra_field(cfg, "bridgeUrl"));
        }
        "bluebubbles" => {
            insert(&mut built, "serverUrl", extra_field(cfg, "serverUrl"));
            let token = field(cfg, "token");
            // `cfg.token ?? cfg.extra?.password`: nullish token falls back.
            insert(
                &mut built,
                "password",
                if nullish(&token) {
                    extra_field(cfg, "password")
                } else {
                    token
                },
            );
        }
        "dingtalk" => {
            insert(&mut built, "clientId", extra_field(cfg, "clientId"));
            insert(&mut built, "clientSecret", extra_field(cfg, "clientSecret"));
        }
        "wecomCallback" => {
            insert(&mut built, "corpId", extra_field(cfg, "corpId"));
            insert(&mut built, "agentId", extra_field(cfg, "agentId"));
            insert(&mut built, "secret", extra_field(cfg, "secret"));
            insert(&mut built, "token", field(cfg, "token"));
            insert(
                &mut built,
                "encodingAesKey",
                extra_field(cfg, "encodingAesKey"),
            );
            insert(&mut built, "port", extra_field(cfg, "port"));
        }
        "email" | "sms" => {
            insert(&mut built, "extra", field(cfg, "extra"));
        }
        "homeassistant" => {
            insert(&mut built, "url", extra_field(cfg, "url"));
            insert(&mut built, "token", field(cfg, "token"));
        }
        "apiServer" => {
            insert(&mut built, "port", extra_field(cfg, "port"));
            insert(&mut built, "apiKey", field(cfg, "apiKey"));
        }
        "webhook" => {
            insert(&mut built, "port", extra_field(cfg, "port"));
            insert(&mut built, "secret", extra_field(cfg, "secret"));
        }
        _ => unreachable!("key comes from CHANNEL_LOADER_KEYS"),
    }
    built
}
