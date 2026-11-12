//! `pilotdeck gateway setup [platform]` (src/cli/commands/gatewaySetup.ts).
//!
//! Interactive setup uses native HTTP and the shared iLink login client.
//! Credentials and YAML are written only after the corresponding confirmation.
use crate::Failure;
use crate::paths::{self, Env, js_trim};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::time::Duration;

mod network;
mod persistence;
use network::{HttpNetwork, SetupNetwork};
use persistence::{WeComConfig, enable_weixin, write_feishu, write_wecom};
use std::io::{Read, Write};
use std::path::PathBuf;

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
#[derive(Debug)]
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

trait QuestionInput {
    fn ask(&mut self, prompt: &str) -> Step<String>;
}

impl Questions {
    fn new() -> Self {
        Self {
            partial: Vec::new(),
            closed: false,
        }
    }
}

impl QuestionInput for Questions {
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
    match wizard(argv, &paths, &mut questions, &mut HttpNetwork::new()) {
        Ok(()) | Err(Stop::Abandoned) => Ok(0),
        Err(Stop::Failure(failure)) => Err(failure),
    }
}

fn wizard(
    argv: &[String],
    paths: &Paths,
    q: &mut dyn QuestionInput,
    net: &mut dyn SetupNetwork,
) -> Step {
    log("\n╔══════════════════════════════════════════════════╗");
    log("║  PilotDeck Gateway Setup                        ║");
    log("║  配置 IM 平台连接                                ║");
    log("╚══════════════════════════════════════════════════╝\n");
    let platform = argv.first().map(|value| value.to_lowercase());
    match platform.as_deref() {
        Some("feishu" | "lark") => setup_feishu(paths, q, net),
        Some("weixin" | "wechat") => setup_weixin(paths, q, net),
        Some("wecom" | "work-weixin" | "workwechat") => setup_wecom(paths, q, net),
        _ => match select_platform(paths, q)? {
            Some("feishu") => setup_feishu(paths, q, net),
            Some("weixin") => setup_weixin(paths, q, net),
            Some("wecom") => setup_wecom(paths, q, net),
            Some(_) => {
                setup_feishu(paths, q, net)?;
                log("");
                setup_weixin(paths, q, net)?;
                log("");
                setup_wecom(paths, q, net)
            }
            None => {
                log("未选择任何平台，退出。");
                Ok(())
            }
        },
    }
}

fn select_platform(paths: &Paths, q: &mut dyn QuestionInput) -> Step<Option<&'static str>> {
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
fn answer(q: &mut dyn QuestionInput, prompt: &str) -> Step<String> {
    Ok(js_trim(&q.ask(prompt)?).to_lowercase())
}

/// `(await rl.question(prompt)).trim()`.
fn trimmed(q: &mut dyn QuestionInput, prompt: &str) -> Step<String> {
    Ok(js_trim(&q.ask(prompt)?).to_owned())
}

fn header(title: &str) {
    log(RULE);
    log(title);
    log(&format!("{RULE}\n"));
}

/// setupFeishu (gatewaySetup.ts:88-171).
fn setup_feishu(paths: &Paths, q: &mut dyn QuestionInput, net: &mut dyn SetupNetwork) -> Step {
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
    log("\n🔍 验证凭据...");
    let token_url = format!("{console_url}/open-apis/auth/v3/tenant_access_token/internal");
    if let Err(error) = check_feishu(net, &token_url, &app_id, &app_secret) {
        log(&format!("\n❌ 凭据验证失败: {error}"));
        let proceed = answer(q, "是否仍然保存配置？ [y/N]: ")?;
        if proceed != "y" && proceed != "yes" {
            log("已取消。");
            return Ok(());
        }
    } else {
        log("✅ 凭据验证通过！");
    }
    write_feishu(
        paths,
        &app_id,
        &app_secret,
        if domain == "lark" { "lark" } else { "feishu" },
    )?;
    log("\n✅ 飞书配置已写入 pilotdeck.yaml");
    log("   连接模式: stream (WebSocket, 推荐 — 无需公网 IP)");
    log("   重启 PilotDeck 服务后生效\n");
    Ok(())
}

/// setupWeixin (gatewaySetup.ts:500-583).
fn setup_weixin(paths: &Paths, q: &mut dyn QuestionInput, net: &mut dyn SetupNetwork) -> Step {
    header("  微信 iLink 配置");
    if let Some(account) = weixin_account(paths) {
        log(&format!("已有凭据 (accountId: {account})"));
        let again = answer(q, "重新登录？ [y/N]: ")?;
        if again != "y" && again != "yes" {
            enable_weixin(paths)?;
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
    log("\n╔══════════════════════════════════════════════╗");
    log("║  请用微信扫描二维码                          ║");
    log("╚══════════════════════════════════════════════╝\n");
    let login = net.weixin_login();
    let result = login.and_then(|creds| {
        persistence::save_weixin_credentials(paths, &creds).map_err(failure_message)?;
        enable_weixin(paths).map_err(failure_message)?;
        Ok(creds)
    });
    match result {
        Ok(creds) => {
            log("\n✅ 微信登录成功！");
            log(&format!("   账号 ID: {}", creds.account_id));
            log(&format!(
                "   凭据已保存到: {}",
                paths.weixin_credentials.display()
            ));
            log("   重启 PilotDeck 服务后生效\n");
        }
        Err(error) => {
            log(&format!("\n❌ 微信登录失败: {error}"));
            log("   请检查网络连接后重试。\n");
        }
    }
    Ok(())
}

/// setupWeCom (gatewaySetup.ts:259-362).
fn setup_wecom(paths: &Paths, q: &mut dyn QuestionInput, net: &mut dyn SetupNetwork) -> Step {
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
            write_wecom(
                paths,
                WeComConfig {
                    bot_id: current_bot_id,
                    secret: current_secret,
                    websocket_url: nullish_string(&[
                        wecom(&["extra", "websocket_url"]),
                        wecom(&["extra", "websocketUrl"]),
                        Some(&JsValue::from(WECOM_DEFAULT_WS_URL)),
                    ]),
                    dm_policy: normalize_policy(wecom(&["extra", "dm_policy"]), "open"),
                    allow_from: setup_list(nullish(&[
                        wecom(&["extra", "allow_from"]),
                        wecom(&["extra", "allowFrom"]),
                    ])),
                    group_policy: normalize_policy(wecom(&["extra", "group_policy"]), "disabled"),
                    group_allow_from: setup_list(nullish(&[
                        wecom(&["extra", "group_allow_from"]),
                        wecom(&["extra", "groupAllowFrom"]),
                    ])),
                    groups: wecom(&["extra", "groups"])
                        .filter(|value| value.as_object().is_some())
                        .cloned(),
                },
            )?;
            log("企业微信已启用（使用现有凭据）。\n");
            return Ok(());
        }
    }
    log("企业微信 AI Bot 通过官方 WebSocket 网关连接，无需公网回调地址。");
    log("请确保你有企业微信组织管理员权限，或已在管理后台拿到 Bot ID 和 Secret。\n");
    let scan = answer(q, "是否扫码自动创建/获取 Bot ID 和 Secret？ [Y/n]: ")?;
    let credentials = if scan != "n" && scan != "no" {
        let credentials = scan_wecom(net);
        if credentials.is_none() {
            log("\n扫码未完成或接口不可用，将使用手动输入模式。\n");
        }
        credentials
    } else {
        None
    };
    let (bot_id, secret) = match credentials {
        Some(credentials) => credentials,
        None => {
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
            (bot_id, secret)
        }
    };
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
    let allow_from = if dm_policy == "allowlist" {
        let value = trimmed(
            q,
            "允许私聊的企业微信用户 ID（逗号分隔，留空则暂时禁用私聊）: ",
        )?;
        let list = setup_list(Some(&JsValue::String(value.into())));
        if list.is_empty() {
            dm_policy = "disabled";
        }
        list
    } else {
        vec![]
    };
    let group = trimmed(
        q,
        "群聊访问策略 [1=disabled 推荐, 2=open, 3=allowlist] (默认 1): ",
    )?;
    let group_policy = match group.as_str() {
        "2" => "open",
        "3" => "allowlist",
        _ => "disabled",
    };
    let group_allow_from = if group_policy == "allowlist" {
        let value = trimmed(q, "允许响应的群聊 chat_id（逗号分隔）: ")?;
        setup_list(Some(&JsValue::String(value.into())))
    } else {
        vec![]
    };
    write_wecom(
        paths,
        WeComConfig {
            bot_id,
            secret,
            websocket_url,
            dm_policy: dm_policy.into(),
            allow_from,
            group_policy: group_policy.into(),
            group_allow_from,
            groups: None,
        },
    )?;
    log("\n企业微信配置已写入 pilotdeck.yaml");
    log("连接模式: AI Bot WebSocket (无需公网 IP)");
    log("重启 PilotDeck 服务后生效。\n");
    Ok(())
}

fn failure_message(error: Failure) -> String {
    match error {
        Failure::Error(message) => message,
        Failure::Todo(message) => message.into(),
    }
}

fn check_feishu(
    net: &mut dyn SetupNetwork,
    url: &str,
    app_id: &str,
    secret: &str,
) -> Result<(), String> {
    let mut body = JsObject::new();
    body.insert("app_id".into(), app_id.into());
    body.insert("app_secret".into(), secret.into());
    let body = pilotdeck_protocol::stringify_json(&JsValue::Object(body))
        .unwrap()
        .unwrap();
    let response = net.request(
        "POST",
        url,
        &[("Content-Type", "application/json; charset=utf-8")],
        &body,
        false,
    )?;
    let json = network::parse_response(&response)?;
    if matches!(json, JsValue::Null) {
        return Err("Cannot read properties of null (reading 'code')".into());
    }
    if matches!(json.get("code"), Some(JsValue::Number(0.0)))
        && truthy(json.get("tenant_access_token"))
    {
        Ok(())
    } else {
        let code = json
            .get("code")
            .map_or_else(|| "undefined".into(), js_to_string);
        let message = nullish_string(&[json.get("msg"), Some(&JsValue::from("unknown"))]);
        Err(format!("code={code} msg={message}"))
    }
}

fn scan_wecom(net: &mut dyn SetupNetwork) -> Option<(String, String)> {
    match scan_wecom_result(net) {
        Ok(result) => result,
        Err(error) => {
            log(&format!("企业微信扫码失败: {error}"));
            None
        }
    }
}

fn wecom_json(net: &mut dyn SetupNetwork, url: &str) -> Result<JsValue, String> {
    let response = net.request("GET", url, &[("User-Agent", "PilotDeck/1.0")], "", true)?;
    if !(200..300).contains(&response.status) {
        return Err(format!("HTTP {}", response.status));
    }
    let json = network::parse_response(&response)?;
    Ok(if json.as_object().is_some() {
        json
    } else {
        JsValue::Object(JsObject::new())
    })
}

fn scan_wecom_result(net: &mut dyn SetupNetwork) -> Result<Option<(String, String)>, String> {
    log("正在连接企业微信生成扫码链接...");
    let generated = wecom_json(
        net,
        "https://work.weixin.qq.com/ai/qc/generate?source=hermes",
    )?;
    let data = generated
        .get("data")
        .filter(|value| value.as_object().is_some());
    let scode = nullish_string(&[data.and_then(|value| value.get("scode"))]);
    let auth_url = nullish_string(&[data.and_then(|value| value.get("auth_url"))]);
    let scode = js_trim(&scode);
    let auth_url = js_trim(&auth_url);
    if scode.is_empty() || auth_url.is_empty() {
        log("企业微信扫码接口返回格式异常。");
        return Ok(None);
    }
    if data
        .and_then(|value| value.get("scode"))
        .is_some_and(|value| !well_formed_string(value))
    {
        return Err("URI malformed".into());
    }
    let encoded = encode_component(scode);
    log("\n请用企业微信手机端扫描/打开以下链接：");
    log(auth_url);
    log(&format!(
        "\n如果无法直接扫码，请打开：\nhttps://work.weixin.qq.com/ai/qc/gen?source=hermes&scode={encoded}\n"
    ));
    log("等待扫码结果...");
    let deadline = net.now() + Duration::from_secs(300);
    let url = format!("https://work.weixin.qq.com/ai/qc/query_result?scode={encoded}");
    while net.now() < deadline {
        let result = wecom_json(net, &url).ok();
        let data = result
            .as_ref()
            .and_then(|value| value.get("data"))
            .filter(|value| value.as_object().is_some());
        let status = nullish_string(&[data.and_then(|value| value.get("status"))]);
        if status.to_lowercase() == "success" {
            let bot = data
                .and_then(|value| value.get("bot_info"))
                .filter(|value| value.as_object().is_some());
            let bot_id = nullish_string(&[
                bot.and_then(|value| value.get("botid")),
                bot.and_then(|value| value.get("bot_id")),
            ]);
            let secret = nullish_string(&[bot.and_then(|value| value.get("secret"))]);
            let bot_id = js_trim(&bot_id);
            let secret = js_trim(&secret);
            if !bot_id.is_empty() && !secret.is_empty() {
                return Ok(Some((bot_id.into(), secret.into())));
            }
            log("扫码成功但未返回完整 Bot 凭据。");
            return Ok(None);
        }
        net.sleep(Duration::from_secs(3));
    }
    log("企业微信扫码超时。");
    Ok(None)
}

// String(array) joins its elements, while plain objects use a fixed tag. Keep
// this check before the UTF-8 display conversion so encodeURIComponent rejects
// an isolated UTF-16 surrogate exactly as the baseline does.
fn well_formed_string(value: &JsValue) -> bool {
    match value {
        JsValue::String(value) => value.to_utf8().is_ok(),
        JsValue::Array(values) => values.iter().all(well_formed_string),
        _ => true,
    }
}

fn encode_component(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

fn nullish<'a>(values: &[Option<&'a JsValue>]) -> Option<&'a JsValue> {
    values
        .iter()
        .flatten()
        .copied()
        .find(|value| !matches!(value, JsValue::Null | JsValue::Undefined))
}

fn normalize_policy(value: Option<&JsValue>, fallback: &str) -> String {
    let raw = nullish_string(&[value]);
    let raw = js_trim(&raw).to_lowercase();
    if matches!(raw.as_str(), "open" | "allowlist" | "disabled") {
        raw
    } else {
        fallback.into()
    }
}

fn setup_list(value: Option<&JsValue>) -> Vec<String> {
    let values = match value {
        Some(JsValue::Array(items)) => items.iter().map(js_to_string).collect(),
        Some(JsValue::String(value)) => js_to_string(&JsValue::String(value.clone()))
            .split(',')
            .map(str::to_owned)
            .collect(),
        _ => vec![],
    };
    values
        .iter()
        .map(|value| js_trim(value).to_owned())
        .filter(|value| !value.is_empty())
        .collect()
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

#[cfg(test)]
mod flow_tests;
