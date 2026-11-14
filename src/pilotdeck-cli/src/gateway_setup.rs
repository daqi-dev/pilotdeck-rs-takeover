//! `pilotdeck gateway setup [platform]` (src/cli/commands/gatewaySetup.ts).
//!
//! The wizard's question flow, prompts and every branch that does not call an
//! external service are ported, including the three local writes of
//! `pilotdeck.yaml` (Weixin with existing credentials, WeCom keeping its
//! configuration, WeCom configured by hand), which re-serialize the whole
//! document the way the `yaml` library's `stringify` does. The three branches
//! that talk to an external service (Feishu credential check, WeCom QR scan,
//! Weixin iLink login) are out of scope for the Rust build
//! (state/takeover.md, decision 13) and stop with an explicit error, exit 2.
use crate::Failure;
use crate::paths::{self, Env, js_trim};
use pilotdeck_protocol::{JsString, JsValue};
use pilotdeck_yaml::stringify::{EditableDocument, StringifyError, StringifyOptions};
use pilotdeck_yaml::value::{Container, Value, ValueGraph};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

const WECOM_DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";
const RULE: &str = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";

/// Where the wizard reads and writes (gatewaySetup.ts:7-9): `PILOT_HOME` and
/// `PILOTDECK_CONFIG_PATH` taken as given when non-empty, not resolved like
/// the rest of the CLI.
struct Paths {
    yaml: PathBuf,
    weixin_credentials: PathBuf,
}

impl Paths {
    fn new(env: &Env) -> Result<Self, Failure> {
        let home = match env.get("PILOT_HOME").filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => paths::homedir(env)?.join(".pilotdeck"),
        };
        let yaml = match env
            .get("PILOTDECK_CONFIG_PATH")
            .filter(|value| !value.is_empty())
        {
            Some(value) => PathBuf::from(value),
            None => home.join("pilotdeck.yaml"),
        };
        Ok(Self {
            yaml,
            weixin_credentials: home.join("weixin-credentials.json"),
        })
    }
}

/// How the wizard ends when not by returning: the `node:readline/promises`
/// question it is waiting on can no longer be answered (stdin ended), so the
/// event loop drains and the process exits 0 without another line.
enum Stop {
    Abandoned,
    Failure(Failure),
}

impl From<Failure> for Stop {
    fn from(failure: Failure) -> Self {
        Stop::Failure(failure)
    }
}

type Step<T = ()> = Result<T, Stop>;

/// `createInterface({ input: process.stdin, output: process.stdout })` and
/// `rl.question`. readline emits every line of a received chunk at once; only
/// a question already pending takes one, so the first line answers it and
/// the rest of that chunk is lost. An unterminated tail waits for its line
/// end in a later chunk; when stdin ends first it is dropped.
struct Questions {
    partial: Vec<u8>,
    closed: bool,
}

impl Questions {
    fn new() -> Self {
        Self {
            partial: Vec::new(),
            closed: false,
        }
    }

    fn ask(&mut self, prompt: &str) -> Step<String> {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(prompt.as_bytes());
        let _ = out.flush();
        drop(out);
        if self.closed {
            return Err(Stop::Abandoned);
        }
        let mut stdin = std::io::stdin().lock();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = match stdin.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            if read == 0 {
                // An unterminated tail never answers the pending question
                // (stdin "q" without a newline leaves the menu waiting).
                self.closed = true;
                return Err(Stop::Abandoned);
            }
            self.partial.extend_from_slice(&buffer[..read]);
            if let Some(end) = self.partial.iter().position(|&b| b == b'\n' || b == b'\r') {
                let line = String::from_utf8_lossy(&self.partial[..end]).into_owned();
                // Every further complete line of this chunk has no taker.
                let last = self
                    .partial
                    .iter()
                    .rposition(|&b| b == b'\n' || b == b'\r')
                    .unwrap_or(end);
                self.partial.drain(..=last);
                return Ok(line);
            }
        }
    }
}

fn log(line: &str) {
    crate::stdout_line(line);
}

/// runGatewaySetup (gatewaySetup.ts:29-62).
pub fn run(argv: &[String], env: &Env) -> Result<u8, Failure> {
    let paths = Paths::new(env)?;
    let mut questions = Questions::new();
    match wizard(argv, &paths, &mut questions) {
        Ok(()) | Err(Stop::Abandoned) => Ok(0),
        Err(Stop::Failure(failure)) => Err(failure),
    }
}

fn wizard(argv: &[String], paths: &Paths, q: &mut Questions) -> Step {
    log("\n╔══════════════════════════════════════════════════╗");
    log("║  PilotDeck Gateway Setup                        ║");
    log("║  配置 IM 平台连接                                ║");
    log("╚══════════════════════════════════════════════════╝\n");
    let platform = argv.first().map(|value| value.to_lowercase());
    match platform.as_deref() {
        Some("feishu" | "lark") => setup_feishu(paths, q),
        Some("weixin" | "wechat") => setup_weixin(paths, q),
        Some("wecom" | "work-weixin" | "workwechat") => setup_wecom(paths, q),
        _ => match select_platform(paths, q)? {
            Some("feishu") => setup_feishu(paths, q),
            Some("weixin") => setup_weixin(paths, q),
            Some("wecom") => setup_wecom(paths, q),
            Some(_) => {
                setup_feishu(paths, q)?;
                log("");
                setup_weixin(paths, q)?;
                log("");
                setup_wecom(paths, q)
            }
            None => {
                log("未选择任何平台，退出。");
                Ok(())
            }
        },
    }
}

fn select_platform(paths: &Paths, q: &mut Questions) -> Step<Option<&'static str>> {
    let config = load_yaml_config(paths);
    let enabled = |adapter: &str| truthy(at(&config, &["adapters", adapter, "enabled"]));
    let feishu = if enabled("feishu") {
        "✅ 已启用"
    } else {
        "未配置"
    };
    let weixin = if paths.weixin_credentials.exists() {
        "✅ 已有凭据"
    } else {
        "未配置"
    };
    let wecom = if enabled("wecom") {
        "✅ 已启用"
    } else {
        "未配置"
    };
    log("可配置的平台：");
    log(&format!("  1) 飞书 / Lark      [{feishu}]"));
    log(&format!("  2) 微信 (iLink)     [{weixin}]"));
    log(&format!("  3) 企业微信 WeCom   [{wecom}]"));
    log("  4) 全部配置");
    log("  q) 退出\n");
    let answer = answer(q, "请选择 [1/2/3/4/q]: ")?;
    Ok(match answer.as_str() {
        "1" | "feishu" => Some("feishu"),
        "2" | "weixin" => Some("weixin"),
        "3" | "wecom" => Some("wecom"),
        "4" | "all" => Some("all"),
        _ => None,
    })
}

/// `(await rl.question(prompt)).trim().toLowerCase()`.
fn answer(q: &mut Questions, prompt: &str) -> Step<String> {
    Ok(js_trim(&q.ask(prompt)?).to_lowercase())
}

/// `(await rl.question(prompt)).trim()`.
fn trimmed(q: &mut Questions, prompt: &str) -> Step<String> {
    Ok(js_trim(&q.ask(prompt)?).to_owned())
}

fn header(title: &str) {
    log(RULE);
    log(title);
    log(&format!("{RULE}\n"));
}

/// setupFeishu (gatewaySetup.ts:88-171).
fn setup_feishu(paths: &Paths, q: &mut Questions) -> Step {
    header("  飞书 / Lark 配置");
    let config = load_yaml_config(paths);
    let feishu = ["adapters", "feishu"];
    let current_app_id = or_empty(at(&config, &[feishu[0], feishu[1], "appId"]));
    let current_secret = or_empty(at(&config, &[feishu[0], feishu[1], "appSecret"]));
    let current_domain = at(&config, &[feishu[0], feishu[1], "domainName"])
        .filter(|value| truthy(Some(value)))
        .map_or_else(|| "feishu".to_owned(), js_to_string);
    let domain = answer(
        q,
        &format!("使用哪个平台？ [feishu/lark] (当前: {current_domain}): "),
    )?;
    let console_url = if domain == "lark" {
        "https://open.larksuite.com"
    } else {
        "https://open.feishu.cn"
    };
    // attemptFeishuQRCreation: the baseline install's Lark SDK default export
    // has no AppTicketManager, so it returns null without asking.
    log(&format!("\n请在 {console_url} 创建企业自建应用："));
    log("  1. 创建应用 → 获取 App ID 和 App Secret");
    log("  2. 权限管理 → 添加以下权限：");
    log("     - im:message (收发消息)");
    log("     - im:message:send_as_bot (以机器人身份发消息)");
    log("     - im:resource (访问图片/文件)");
    log("     - im:chat (群聊元信息)");
    log("  3. 事件订阅 → 添加 im.message.receive_v1");
    log("  4. 版本管理 → 发布版本\n");
    let id_hint = if current_app_id.is_empty() {
        String::new()
    } else {
        format!(" (当前: {})", mask_secret(&current_app_id))
    };
    let app_id = or_current(trimmed(q, &format!("App ID{id_hint}: "))?, &current_app_id);
    let secret_hint = if current_secret.is_empty() {
        ""
    } else {
        " (回车保留当前值)"
    };
    let app_secret = or_current(
        trimmed(q, &format!("App Secret{secret_hint}: "))?,
        &current_secret,
    );
    if app_id.is_empty() || app_secret.is_empty() {
        log("\n⚠️  未提供完整凭据，跳过飞书配置。");
        return Ok(());
    }
    Err(Failure::Unsupported("pilotdeck gateway setup: the Feishu credential check").into())
}

/// setupWeixin (gatewaySetup.ts:500-583).
fn setup_weixin(paths: &Paths, q: &mut Questions) -> Step {
    header("  微信 iLink 配置");
    if let Some(account) = weixin_account(paths) {
        log(&format!("已有凭据 (accountId: {account})"));
        let again = answer(q, "重新登录？ [y/N]: ")?;
        if again != "y" && again != "yes" {
            // enableWeixinConfig (gatewaySetup.ts:604-609).
            save_adapter(
                paths,
                "weixin",
                Fresh::Object(vec![("enabled", Fresh::Bool(true))]),
            )?;
            log("✅ 微信已启用（使用现有凭据）\n");
            return Ok(());
        }
    }
    log("⚠️  注意：");
    log("  - 使用 iLink Bot API，非企业微信");
    log("  - 建议使用小号登录，有封号风险");
    log("  - 群聊功能默认禁用\n");
    let proceed = answer(q, "继续扫码登录？ [Y/n]: ")?;
    if proceed == "n" || proceed == "no" {
        log("已取消。");
        return Ok(());
    }
    Err(Failure::Unsupported("pilotdeck gateway setup: the Weixin iLink QR login").into())
}

/// setupWeCom (gatewaySetup.ts:259-362).
fn setup_wecom(paths: &Paths, q: &mut Questions) -> Step {
    header("  企业微信 WeCom AI Bot 配置");
    let config = load_yaml_config(paths);
    let wecom = |path: &[&str]| {
        let mut full = vec!["adapters", "wecom"];
        full.extend_from_slice(path);
        at(&config, &full)
    };
    let current_bot_id = nullish_string(&[
        wecom(&["token"]),
        wecom(&["extra", "bot_id"]),
        wecom(&["extra", "botId"]),
    ]);
    let current_secret =
        nullish_string(&[wecom(&["extra", "secret"]), wecom(&["extra", "botSecret"])]);
    if !current_bot_id.is_empty() && !current_secret.is_empty() {
        log(&format!(
            "已有企业微信配置 (Bot ID: {})",
            mask_secret(&current_bot_id)
        ));
        let again = answer(q, "重新配置？ [y/N]: ")?;
        if again != "y" && again != "yes" {
            let ws_url = nullish_string(&[
                wecom(&["extra", "websocket_url"]),
                wecom(&["extra", "websocketUrl"]),
                Some(&JsValue::String(WECOM_DEFAULT_WS_URL.into())),
            ]);
            let groups = groups_object(paths);
            write_wecom_config(
                paths,
                WeComConfig {
                    bot_id: current_bot_id,
                    secret: current_secret,
                    websocket_url: ws_url,
                    dm_policy: normalize_policy(wecom(&["extra", "dm_policy"]), "open"),
                    allow_from: coerce_list(nullish_or(
                        wecom(&["extra", "allow_from"]),
                        wecom(&["extra", "allowFrom"]),
                    )),
                    group_policy: normalize_policy(wecom(&["extra", "group_policy"]), "disabled"),
                    group_allow_from: coerce_list(nullish_or(
                        wecom(&["extra", "group_allow_from"]),
                        wecom(&["extra", "groupAllowFrom"]),
                    )),
                    groups,
                },
            )?;
            log("企业微信已启用（使用现有凭据）。\n");
            return Ok(());
        }
    }
    log("企业微信 AI Bot 通过官方 WebSocket 网关连接，无需公网回调地址。");
    log("请确保你有企业微信组织管理员权限，或已在管理后台拿到 Bot ID 和 Secret。\n");
    let scan = answer(q, "是否扫码自动创建/获取 Bot ID 和 Secret？ [Y/n]: ")?;
    if scan != "n" && scan != "no" {
        return Err(Failure::Unsupported("pilotdeck gateway setup: the WeCom QR scan").into());
    }
    log("手动配置步骤：");
    log("  1. 登录企业微信管理后台");
    log("  2. 进入应用管理，创建 AI Bot / 智能机器人");
    log("  3. 选择 API 模式");
    log("  4. 复制 Bot ID 和 Secret\n");
    let bot_hint = if current_bot_id.is_empty() {
        String::new()
    } else {
        format!(" (当前: {})", mask_secret(&current_bot_id))
    };
    let bot_id = or_current(trimmed(q, &format!("Bot ID{bot_hint}: "))?, &current_bot_id);
    let secret_hint = if current_secret.is_empty() {
        ""
    } else {
        " (回车保留当前值)"
    };
    let secret = or_current(
        trimmed(q, &format!("Secret{secret_hint}: "))?,
        &current_secret,
    );
    if bot_id.is_empty() || secret.is_empty() {
        log("\n未提供完整 Bot ID / Secret，跳过企业微信配置。\n");
        return Ok(());
    }
    let websocket_url = or_current(
        trimmed(
            q,
            &format!("WebSocket URL (默认: {WECOM_DEFAULT_WS_URL}): "),
        )?,
        WECOM_DEFAULT_WS_URL,
    );
    let dm = trimmed(
        q,
        "私聊访问策略 [1=open 推荐, 2=allowlist, 3=disabled] (默认 1): ",
    )?;
    let mut dm_policy = match dm.as_str() {
        "2" => "allowlist",
        "3" => "disabled",
        _ => "open",
    };
    let mut allow_from = Vec::new();
    if dm_policy == "allowlist" {
        let allowed = trimmed(
            q,
            "允许私聊的企业微信用户 ID（逗号分隔，留空则暂时禁用私聊）: ",
        )?;
        allow_from = split_list(&allowed);
        if allow_from.is_empty() {
            dm_policy = "disabled";
        }
    }
    let group = trimmed(
        q,
        "群聊访问策略 [1=disabled 推荐, 2=open, 3=allowlist] (默认 1): ",
    )?;
    let group_policy = match group.as_str() {
        "2" => "open",
        "3" => "allowlist",
        _ => "disabled",
    };
    let mut group_allow_from = Vec::new();
    if group_policy == "allowlist" {
        group_allow_from = split_list(&trimmed(q, "允许响应的群聊 chat_id（逗号分隔）: ")?);
    }
    write_wecom_config(
        paths,
        WeComConfig {
            bot_id,
            secret,
            websocket_url,
            dm_policy: dm_policy.to_owned(),
            allow_from,
            group_policy: group_policy.to_owned(),
            group_allow_from,
            groups: None,
        },
    )?;
    log("\n企业微信配置已写入 pilotdeck.yaml");
    log("连接模式: AI Bot WebSocket (无需公网 IP)");
    log("重启 PilotDeck 服务后生效。\n");
    Ok(())
}

/// The `cfg` of writeWeComConfig (gatewaySetup.ts:445-478).
struct WeComConfig {
    bot_id: String,
    secret: String,
    websocket_url: String,
    dm_policy: String,
    allow_from: Vec<String>,
    group_policy: String,
    group_allow_from: Vec<String>,
    /// `extra.groups` of the configuration read when the wizard started, a
    /// plain object taken over as is.
    groups: Option<ValueGraph>,
}

/// writeWeComConfig (gatewaySetup.ts:445-478).
fn write_wecom_config(paths: &Paths, cfg: WeComConfig) -> Step {
    let websocket_url = if cfg.websocket_url.is_empty() {
        WECOM_DEFAULT_WS_URL.to_owned()
    } else {
        cfg.websocket_url
    };
    let mut extra = vec![
        ("secret", Fresh::Str(cfg.secret)),
        ("websocket_url", Fresh::Str(websocket_url)),
        ("dm_policy", Fresh::Str(cfg.dm_policy)),
        ("group_policy", Fresh::Str(cfg.group_policy)),
    ];
    if !cfg.allow_from.is_empty() {
        extra.push(("allow_from", Fresh::List(cfg.allow_from)));
    }
    if !cfg.group_allow_from.is_empty() {
        extra.push(("group_allow_from", Fresh::List(cfg.group_allow_from)));
    }
    if let Some(groups) = cfg.groups {
        extra.push(("groups", Fresh::Imported(groups)));
    }
    save_adapter(
        paths,
        "wecom",
        Fresh::Object(vec![
            ("enabled", Fresh::Bool(true)),
            ("token", Fresh::Str(cfg.bot_id)),
            ("extra", Fresh::Object(extra)),
        ]),
    )
}

/// normalizeWeComSetupPolicy (gatewaySetup.ts:480-483).
fn normalize_policy(value: Option<&JsValue>, fallback: &str) -> String {
    let raw = nullish_string(&[value]);
    let raw = js_trim(&raw).to_lowercase();
    if matches!(raw.as_str(), "open" | "allowlist" | "disabled") {
        raw
    } else {
        fallback.to_owned()
    }
}

/// coerceSetupList (gatewaySetup.ts:485-493).
fn coerce_list(value: Option<&JsValue>) -> Vec<String> {
    match value {
        Some(JsValue::Array(items)) => items
            .iter()
            .map(|item| js_trim(&js_to_string(item)).to_owned())
            .filter(|item| !item.is_empty())
            .collect(),
        Some(JsValue::String(text)) => split_list(&js_to_string(&JsValue::String(text.clone()))),
        _ => Vec::new(),
    }
}

/// `value.split(",").map(item => item.trim()).filter(Boolean)`.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| js_trim(item).to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

/// `a ?? b`.
fn nullish_or<'a>(a: Option<&'a JsValue>, b: Option<&'a JsValue>) -> Option<&'a JsValue> {
    a.filter(|value| !matches!(value, JsValue::Null | JsValue::Undefined))
        .or(b)
}

/// `isPlainObject(currentWeCom?.extra?.groups) ? currentWeCom.extra.groups :
/// undefined`, as a value graph rooted at that object (it comes from the
/// wizard's first read of the file, so it shares no identity with the copy
/// writeWeComConfig reads again).
fn groups_object(paths: &Paths) -> Option<ValueGraph> {
    let graph = load_yaml_graph(paths)?;
    let mut value = graph.root.clone();
    for key in ["adapters", "wecom", "extra", "groups"] {
        let Value::Ref(id) = value else { return None };
        let Container::Object(entries) = &graph.nodes[id] else {
            return None;
        };
        value = entries.iter().find(|(k, _)| k.equals_str(key))?.1.clone();
    }
    let Value::Ref(id) = value else { return None };
    matches!(graph.nodes[id], Container::Object(_)).then(|| ValueGraph {
        root: value,
        nodes: graph.nodes,
    })
}

/// A value the wizard builds for `pilotdeck.yaml`.
enum Fresh {
    Bool(bool),
    Str(String),
    List(Vec<String>),
    Object(Vec<(&'static str, Fresh)>),
    /// A value of another parse of the file, carried over with its own
    /// internal sharing.
    Imported(ValueGraph),
}

fn insert_fresh(graph: &mut ValueGraph, value: Fresh) -> Value {
    match value {
        Fresh::Bool(b) => Value::Bool(b),
        Fresh::Str(s) => Value::String(Arc::new(JsString::from(s))),
        Fresh::List(items) => {
            let items = items
                .into_iter()
                .map(|s| Value::String(Arc::new(JsString::from(s))))
                .collect();
            graph.nodes.push(Container::Array(items));
            Value::Ref(graph.nodes.len() - 1)
        }
        Fresh::Object(entries) => {
            let entries = entries
                .into_iter()
                .map(|(key, value)| (JsString::from(key), insert_fresh(graph, value)))
                .collect();
            graph.nodes.push(Container::Object(entries));
            Value::Ref(graph.nodes.len() - 1)
        }
        Fresh::Imported(source) => import_value(graph, &source, &source.root, &mut HashMap::new()),
    }
}

/// Copy `value` of `source` into `graph` with fresh containers, keeping the
/// sharing among them.
fn import_value(
    graph: &mut ValueGraph,
    source: &ValueGraph,
    value: &Value,
    copied: &mut HashMap<usize, usize>,
) -> Value {
    let Value::Ref(id) = value else {
        return value.clone();
    };
    if let Some(&new) = copied.get(id) {
        return Value::Ref(new);
    }
    graph.nodes.push(Container::Array(Vec::new()));
    let new = graph.nodes.len() - 1;
    copied.insert(*id, new);
    let container = match &source.nodes[*id] {
        Container::Object(entries) => Container::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), import_value(graph, source, v, copied)))
                .collect(),
        ),
        Container::Array(items) => Container::Array(
            items
                .iter()
                .map(|v| import_value(graph, source, v, copied))
                .collect(),
        ),
        Container::Map(entries) => Container::Map(
            entries
                .iter()
                .map(|(k, v)| {
                    (
                        import_value(graph, source, k, copied),
                        import_value(graph, source, v, copied),
                    )
                })
                .collect(),
        ),
        Container::Set(items) => Container::Set(
            items
                .iter()
                .map(|v| import_value(graph, source, v, copied))
                .collect(),
        ),
    };
    graph.nodes[new] = container;
    Value::Ref(new)
}

/// `!value` of a JS value in a graph.
fn graph_truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => *n != 0.0 && !n.is_nan(),
        Value::String(s) => !s.is_empty(),
        Value::Symbol { .. } | Value::Ref(_) => true,
    }
}

/// V8's strict-mode `TypeError` for a property assignment on a primitive.
fn assign_on_primitive(key: &str, value: &Value) -> Failure {
    let (kind, text) = match value {
        Value::String(s) => ("string", js_to_string(&JsValue::String(s.as_ref().clone()))),
        Value::Number(n) => ("number", pilotdeck_protocol::number::number_to_string(*n)),
        Value::Bool(b) => ("boolean", b.to_string()),
        _ => ("symbol", "Symbol()".to_owned()),
    };
    Failure::Error(format!("Cannot create property '{key}' on {kind} '{text}'"))
}

/// `obj[key] = value` on a graph object: replaces in place or appends.
fn set_entry(graph: &mut ValueGraph, object: usize, key: &str, value: Value) {
    if let Container::Object(entries) = &mut graph.nodes[object] {
        match entries.iter_mut().find(|(k, _)| k.equals_str(key)) {
            Some(entry) => entry.1 = value,
            None => entries.push((JsString::from(key), value)),
        }
    }
}

/// `const yamlConfig = loadYamlConfig() ?? {}; if (!yamlConfig.adapters)
/// yamlConfig.adapters = {}; yamlConfig.adapters[name] = adapter;
/// saveYamlConfig(yamlConfig);` (gatewaySetup.ts:255-266, 459-477, 604-609).
fn save_adapter(paths: &Paths, name: &str, adapter: Fresh) -> Step {
    let mut graph = load_yaml_graph(paths).unwrap_or(ValueGraph {
        root: Value::Null,
        nodes: Vec::new(),
    });
    if matches!(graph.root, Value::Null | Value::Undefined) {
        graph.nodes.push(Container::Object(Vec::new()));
        graph.root = Value::Ref(graph.nodes.len() - 1);
    }
    let root = match graph.root {
        Value::Ref(id) => id,
        ref primitive => return Err(assign_on_primitive("adapters", primitive).into()),
    };
    // An array root takes the assignments as non-index properties, which
    // stringify does not write: the file is rewritten unchanged.
    if let Container::Object(entries) = &graph.nodes[root] {
        let current = entries
            .iter()
            .find(|(k, _)| k.equals_str("adapters"))
            .map(|(_, v)| v.clone());
        let adapters = match current {
            Some(value) if graph_truthy(&value) => value,
            _ => {
                graph.nodes.push(Container::Object(Vec::new()));
                let fresh = Value::Ref(graph.nodes.len() - 1);
                set_entry(&mut graph, root, "adapters", fresh.clone());
                fresh
            }
        };
        match adapters {
            Value::Ref(id) => {
                if matches!(graph.nodes[id], Container::Object(_)) {
                    let value = insert_fresh(&mut graph, adapter);
                    set_entry(&mut graph, id, name, value);
                }
            }
            ref primitive => return Err(assign_on_primitive(name, primitive).into()),
        }
    }
    save_yaml_config(paths, &graph)
}

/// saveYamlConfig (gatewaySetup.ts:623-631).
fn save_yaml_config(paths: &Paths, graph: &ValueGraph) -> Step {
    let text = EditableDocument::stringify_value(
        graph,
        StringifyOptions {
            line_width: 0,
            single_quote: Some(false),
        },
    )
    .map_err(|error| {
        Failure::Error(match error {
            StringifyError::Error(message) | StringifyError::Unsupported(message) => message,
        })
    })?;
    if let Some(dir) = paths
        .yaml
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir).map_err(|error| {
            Failure::Error(paths::node_fs_message_or_unknown(&error, "mkdir", dir))
        })?;
    }
    std::fs::write(&paths.yaml, text).map_err(|error| {
        Failure::Error(paths::node_fs_message_or_unknown(
            &error,
            "open",
            &paths.yaml,
        ))
    })?;
    Ok(())
}

/// loadYamlConfig as the value graph saveYamlConfig re-serializes (object
/// identity kept), `None` where loadYamlConfig returns `null`.
fn load_yaml_graph(paths: &Paths) -> Option<ValueGraph> {
    let raw = std::fs::read(&paths.yaml).ok()?;
    let source = JsString::from(String::from_utf8_lossy(&raw).into_owned());
    let document = pilotdeck_yaml::parse_document(&source, pilotdeck_yaml::Options::default());
    if !document.errors.is_empty() {
        return None;
    }
    document.to_json().ok()
}

/// loadWeixinCredentials (gatewaySetup.ts:585-595): the account id of a
/// readable credentials file whose baseUrl, botToken and accountId are all
/// truthy.
fn weixin_account(paths: &Paths) -> Option<String> {
    let raw = std::fs::read(&paths.weixin_credentials).ok()?;
    let data = pilotdeck_protocol::parse_json(&String::from_utf8_lossy(&raw)).ok()?;
    let field = |name: &str| data.get(name).filter(|value| truthy(Some(value)));
    field("baseUrl")?;
    field("botToken")?;
    field("accountId").map(js_to_string)
}

/// loadYamlConfig (gatewaySetup.ts:612-621): `parse(raw)` of the config file,
/// `null` when it is missing or does not parse.
fn load_yaml_config(paths: &Paths) -> Option<JsValue> {
    let raw = std::fs::read(&paths.yaml).ok()?;
    let source = JsString::from(String::from_utf8_lossy(&raw).into_owned());
    let document = pilotdeck_yaml::parse_document(&source, pilotdeck_yaml::Options::default());
    if !document.errors.is_empty() {
        return None;
    }
    document.to_json().ok()?.to_js_value().ok()
}

/// `config?.a?.b?.c`.
fn at<'a>(config: &'a Option<JsValue>, path: &[&str]) -> Option<&'a JsValue> {
    let mut value = config.as_ref()?;
    for key in path {
        value = value.as_object()?.get(key)?;
    }
    Some(value)
}

fn truthy(value: Option<&JsValue>) -> bool {
    match value {
        None | Some(JsValue::Null | JsValue::Undefined) => false,
        Some(JsValue::Bool(value)) => *value,
        Some(JsValue::Number(value)) => *value != 0.0 && !value.is_nan(),
        Some(JsValue::String(value)) => !value.is_empty(),
        Some(_) => true,
    }
}

/// `String(value)` for the scalar values a config holds.
fn js_to_string(value: &JsValue) -> String {
    match value {
        JsValue::String(value) => value
            .to_utf8()
            .unwrap_or_else(|_| String::from_utf16_lossy(value.units())),
        JsValue::Number(value) => pilotdeck_protocol::number::number_to_string(*value),
        JsValue::Bool(value) => value.to_string(),
        JsValue::Null => "null".into(),
        JsValue::Undefined => "undefined".into(),
        JsValue::Array(items) => items
            .iter()
            .map(|item| match item {
                JsValue::Null | JsValue::Undefined => String::new(),
                item => js_to_string(item),
            })
            .collect::<Vec<_>>()
            .join(","),
        _ => "[object Object]".into(),
    }
}

/// `value || ""` for a string-valued config field.
fn or_empty(value: Option<&JsValue>) -> String {
    value
        .filter(|value| truthy(Some(value)))
        .map(js_to_string)
        .unwrap_or_default()
}

/// `String(a ?? b ?? ... ?? "")`.
fn nullish_string(values: &[Option<&JsValue>]) -> String {
    values
        .iter()
        .flatten()
        .find(|value| !matches!(value, JsValue::Null | JsValue::Undefined))
        .map(|value| js_to_string(value))
        .unwrap_or_default()
}

/// `answer || current`.
fn or_current(answer: String, current: &str) -> String {
    if answer.is_empty() {
        current.to_owned()
    } else {
        answer
    }
}

/// maskSecret (gatewaySetup.ts:633-636), in UTF-16 code units.
fn mask_secret(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= 8 {
        return value.to_owned();
    }
    format!(
        "{}…{}",
        String::from_utf16_lossy(&units[..4]),
        String::from_utf16_lossy(&units[units.len() - 4..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_keeps_short_values_and_cuts_utf16_units() {
        assert_eq!(mask_secret("12345678"), "12345678");
        assert_eq!(mask_secret("123456789"), "1234…6789");
        assert_eq!(mask_secret("ab😀cdefgh"), "ab😀…efgh");
    }

    #[test]
    fn nullish_chain_skips_only_null_and_undefined() {
        let empty = JsValue::String("".into());
        let id = JsValue::Number(42.0);
        assert_eq!(
            nullish_string(&[None, Some(&JsValue::Null), Some(&id)]),
            "42"
        );
        assert_eq!(nullish_string(&[Some(&empty), Some(&id)]), "");
        assert_eq!(nullish_string(&[None, None]), "");
    }
}
