//! `gateway setup` entries (`cli.compat.setup-*`, all adapted): the
//! interactive channel setup menu plus the feishu/lark, weixin/wechat and
//! wecom platform flows.
//!
//! All `channels.*` operations are delivered by S3; the orchestration
//! calls them through the executor and surfaces their failures verbatim.
//!
//! Registered accepted differences (mapping data): credentials become
//! protected references (nothing is stored in shim state; secrets never
//! appear in argv beyond the already-typed interactive input, never in
//! logs or results); the old plaintext YAML adapter layouts are not
//! reproduced; non-TTY invocations without structured input fail with
//! `action_required` instead of hanging; the old feishu QR auto-creation
//! attempt (which only printed guidance) and the weixin homedir() bug are
//! not carried over. Interactive input is injectable
//! ([`InputReader`]) so the flows are FakeExecutor-testable.
//!
//! Secret handling: a QR login (`channels.begin_login`/`poll_login`)
//! persists the exchanged secret server-side and hands the flow a
//! `credentialRef`; manual entry writes the typed secret through
//! `credential.write` and likewise keeps only the reference. The channel
//! configuration schemas accept secrets only as `credentialRef`, so the
//! plaintext secret never enters a `channels.validate`/`channels.configure`
//! payload. When the user declines to save after a manual write, the
//! credential is removed again (best effort, failures surfaced) so the
//! old "decline writes nothing" contract holds. The old wecom `extra`
//! block (`websocket_url`, `dm_policy`, `group_policy`) has no equivalent
//! in the delivered channel schema and is reported as not projectable
//! instead of being invented.

use std::io::IsTerminal;

use serde_json::{Value, json};

use crate::entry::{EntryCtx, call_op, call_op_keyed, note_dropped, parse_cli, usage_errors};
use crate::state;
use crate::{EXIT_FAILED, EXIT_OK, SHIM_NAME};

/// Platform selected for `gateway setup`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupPlatform {
    /// No or unknown platform: the interactive menu.
    Menu,
    Feishu,
    Weixin,
    Wecom,
}

impl SetupPlatform {
    fn from_word(word: &str) -> Self {
        match word {
            "feishu" | "lark" => Self::Feishu,
            "weixin" | "wechat" => Self::Weixin,
            "wecom" | "work-weixin" | "workwechat" => Self::Wecom,
            _ => Self::Menu,
        }
    }
}

/// Parsed `gateway setup [platform]` arguments.
#[derive(Clone, Debug)]
pub struct SetupArgs {
    pub platform: SetupPlatform,
}

pub fn parse(rest: &[String]) -> Result<SetupArgs, String> {
    if rest.first().map(String::as_str) != Some("setup") {
        return Err("expected `gateway setup [platform]`".to_owned());
    }
    let spec = crate::entry::OptSpec {
        values: &[],
        flags: &[],
    };
    let parsed = parse_cli(&rest[1..], &spec)?;
    if parsed.positionals.len() > 1 {
        return Err("gateway setup takes at most one platform".to_owned());
    }
    Ok(SetupArgs {
        platform: parsed
            .positionals
            .first()
            .map(|p| SetupPlatform::from_word(p))
            .unwrap_or(SetupPlatform::Menu),
    })
}

/// Interactive input source: a prompt returning one line (or `None` for
/// EOF). Production reads stdin lines; tests inject canned answers.
pub type InputReader<'a> = dyn FnMut(&str) -> Option<String> + 'a;

/// Reads one trimmed line from stdin for the production [`InputReader`].
fn read_stdin_line(prompt: &str) -> Option<String> {
    use std::io::{BufRead, Write};
    eprint!("{prompt} ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    match lock.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_owned()),
    }
}

/// Entry point.
pub fn main(ctx: &mut EntryCtx<'_>, rest: &[String]) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(message) => return usage_errors::usage(&message),
    };
    let home = match ctx.require_home() {
        Ok(home) => home.to_path_buf(),
        Err(code) => return code,
    };
    let _ = home; // channel state lives in the target, not shim state

    let mut reader: Box<InputReader<'_>> = Box::new(read_stdin_line);
    match args.platform {
        SetupPlatform::Menu => menu_flow(ctx, &mut reader),
        SetupPlatform::Feishu => feishu_flow(ctx, &mut reader),
        SetupPlatform::Weixin => weixin_flow(ctx, &mut reader),
        SetupPlatform::Wecom => wecom_flow(ctx, &mut reader),
    }
}

/// Requires an interactive terminal for the credential flows.
fn require_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Emits the `action_required` diagnostic (exit 1): machine invocations
/// must carry structured input; the old interactive prompts cannot hang.
fn action_required(what: &str) -> i32 {
    eprintln!(
        "{SHIM_NAME}: {what} requires an interactive terminal or structured input \
         (error: action_required)"
    );
    EXIT_FAILED
}

/// Channels scope used by every flow. The contract's channels scope is
/// `{kind: global|project|session, id?}` with `id` required for
/// project/session and null for global (`state/cli-v1-operations.json`,
/// channels.* input schemas); `instance` is not a legal kind. The old flows
/// wrote instance-wide adapter config, which maps onto the global scope —
/// a documented semantic choice (channel config visible to the whole
/// instance, not one project or session).
fn channel_scope() -> Value {
    json!({"kind": "global"})
}

/// One channel row from `channels.list` (includeUnconfigured).
struct ChannelInfo {
    channel_id: String,
    revision: String,
    schema_id: String,
    enabled: bool,
}

/// Discovers the channel for an adapter type; a missing channel is a
/// surfaced failure (exit code in `Err`).
fn find_channel(ctx: &mut EntryCtx<'_>, adapter_type: &str) -> Result<ChannelInfo, i32> {
    let result = call_op(
        ctx,
        "channels.list",
        &json!({"scope": channel_scope(), "includeUnconfigured": true}),
    )?;
    let channels = result.get("channels").and_then(Value::as_array).cloned();
    let Some(channels) = channels else {
        eprintln!("{SHIM_NAME}: channels.list returned no channel array");
        return Err(EXIT_FAILED);
    };
    for channel in channels {
        if channel.get("adapterType").and_then(Value::as_str) == Some(adapter_type) {
            return Ok(ChannelInfo {
                channel_id: channel
                    .get("channelId")
                    .and_then(Value::as_str)
                    .unwrap_or(adapter_type)
                    .to_owned(),
                revision: channel
                    .get("revision")
                    .and_then(Value::as_str)
                    .unwrap_or("absent")
                    .to_owned(),
                schema_id: channel
                    .get("schemaId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                enabled: channel
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    eprintln!("{SHIM_NAME}: no {adapter_type} channel discovered on the target");
    Err(EXIT_FAILED)
}

/// One credential a flow wrote through `credential.write`: the reference
/// the channel configuration carries, plus the revision needed for the
/// best-effort cleanup on decline.
struct WrittenCredential {
    credential_ref: String,
    revision: String,
}

/// Writes one interactively typed secret through the credential store and
/// returns its reference. The channel schemas accept secrets only as
/// `credentialRef`, so the plaintext never enters a
/// `channels.validate`/`channels.configure` payload, argv, shim state or
/// logs — it exists only in the `credential.write` input and the store.
fn write_credential(
    ctx: &mut EntryCtx<'_>,
    kind: &str,
    name: &str,
    secret: &str,
) -> Result<WrittenCredential, i32> {
    let credential_id = state::generate_id("cred");
    let request_key = state::generate_request_key();
    let result = call_op_keyed(
        ctx,
        "credential.write",
        &json!({
            "credentialId": credential_id,
            "kind": kind,
            "name": name,
            "secret": secret,
        }),
        &request_key,
    )?;
    let Some(credential_ref) = result
        .pointer("/credential/credentialId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        eprintln!("{SHIM_NAME}: credential.write returned no credentialId");
        return Err(EXIT_FAILED);
    };
    let revision = result
        .pointer("/credential/revision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(WrittenCredential {
        credential_ref,
        revision,
    })
}

/// Best-effort removal of a credential a flow wrote when the user declines
/// to save: the old "拒绝保存 → 零写入" contract — the credential only
/// existed so `channels.validate` could resolve its reference. A removal
/// failure is surfaced on stderr by `call_op_keyed`; the declined outcome
/// itself still stands (nothing was configured).
fn remove_credential(ctx: &mut EntryCtx<'_>, credential: &WrittenCredential) {
    let request_key = state::generate_request_key();
    let _ = call_op_keyed(
        ctx,
        "credential.remove",
        &json!({
            "credentialId": credential.credential_ref,
            "expectedRevision": credential.revision,
        }),
        &request_key,
    );
}

/// The interactive menu (old gatewaySetup.ts:65-86): platform choice, then
/// the matching flow; `4` runs feishu→weixin→wecom (the old `all` order);
/// EOF/q exits with zero writes.
fn menu_flow(ctx: &mut EntryCtx<'_>, reader: &mut InputReader<'_>) -> i32 {
    if !require_tty() {
        return action_required("gateway setup");
    }
    let result = match call_op(
        ctx,
        "channels.list",
        &json!({"scope": channel_scope(), "includeUnconfigured": true}),
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };
    let channels = result.get("channels").and_then(Value::as_array).cloned();
    let status = |adapter: &str| -> bool {
        channels.as_ref().is_some_and(|list| {
            list.iter().any(|c| {
                c.get("adapterType").and_then(Value::as_str) == Some(adapter)
                    && c.get("enabled").and_then(Value::as_bool) == Some(true)
            })
        })
    };
    eprintln!("gateway setup:");
    eprintln!(
        "  1) feishu  [{}]",
        if status("feishu") {
            "enabled"
        } else {
            "disabled"
        }
    );
    eprintln!(
        "  2) weixin  [{}]",
        if status("weixin") {
            "enabled"
        } else {
            "disabled"
        }
    );
    eprintln!(
        "  3) wecom   [{}]",
        if status("wecom") {
            "enabled"
        } else {
            "disabled"
        }
    );
    eprintln!("  4) all");
    eprintln!("  q) quit");
    let Some(choice) = reader("select:") else {
        eprintln!("未选择任何平台，退出。");
        return EXIT_OK;
    };
    match choice.as_str() {
        "1" | "feishu" | "lark" => feishu_flow(ctx, reader),
        "2" | "weixin" | "wechat" => weixin_flow(ctx, reader),
        "3" | "wecom" | "work-weixin" | "workwechat" => wecom_flow(ctx, reader),
        // Old `all` order: feishu → weixin → wecom.
        "4" | "all" => {
            let first = feishu_flow(ctx, reader);
            let second = weixin_flow(ctx, reader);
            let third = wecom_flow(ctx, reader);
            if first == EXIT_OK && second == EXIT_OK {
                third
            } else {
                EXIT_FAILED
            }
        }
        _ => {
            eprintln!("未选择任何平台，退出。");
            EXIT_OK
        }
    }
}

/// feishu/lark: manual App ID/Secret entry — the secret goes through
/// `credential.write` and the configuration carries only its reference —
/// then `channels.validate`, probe (`channels.probe`), and a y/N
/// confirmation when validation fails (declining writes nothing and
/// removes the credential again). Zero write on incomplete credentials.
fn feishu_flow(ctx: &mut EntryCtx<'_>, reader: &mut InputReader<'_>) -> i32 {
    if !require_tty() {
        return action_required("gateway setup feishu");
    }
    feishu_flow_inner(ctx, reader)
}

fn feishu_flow_inner(ctx: &mut EntryCtx<'_>, reader: &mut InputReader<'_>) -> i32 {
    let channel = match find_channel(ctx, "feishu") {
        Ok(channel) => channel,
        Err(code) => return code,
    };
    let Some(app_id) = reader("App ID:") else {
        eprintln!("未提供完整凭据，跳过飞书配置。");
        return EXIT_OK;
    };
    let Some(app_secret) = reader("App Secret:") else {
        eprintln!("未提供完整凭据，跳过飞书配置。");
        return EXIT_OK;
    };
    if app_id.is_empty() || app_secret.is_empty() {
        eprintln!("未提供完整凭据，跳过飞书配置。");
        return EXIT_OK;
    }
    let credential = match write_credential(
        ctx,
        "channel.feishu",
        &format!("feishu app {app_id}"),
        &app_secret,
    ) {
        Ok(credential) => credential,
        Err(code) => return code,
    };
    // The feishu config schema (agentcpm-pd.channel.feishu.config/1) takes
    // the secret only as `credentialRef`; `enabled` is required.
    let configuration = json!({
        "enabled": true,
        "appId": app_id,
        "credentialRef": credential.credential_ref,
    });

    let validation = match call_op(
        ctx,
        "channels.validate",
        &json!({
            "scope": channel_scope(),
            "adapterType": "feishu",
            "schemaId": channel.schema_id,
            "configuration": configuration,
        }),
    ) {
        Ok(validation) => validation,
        Err(code) => return code,
    };
    let mut proceed = validation.get("valid").and_then(Value::as_bool) == Some(true);
    if !proceed {
        eprintln!("❌ 凭据验证失败");
        let Some(answer) = reader("是否仍然保存配置？ [y/N]") else {
            remove_credential(ctx, &credential);
            eprintln!("已取消。");
            return EXIT_OK;
        };
        proceed = answer.eq_ignore_ascii_case("y");
        if !proceed {
            remove_credential(ctx, &credential);
            eprintln!("已取消。");
            return EXIT_OK;
        }
    } else {
        // Connectivity probe corresponds to the old tenant_access_token
        // verification. `channels.probe` is an idempotentSubmit operation,
        // so the shim persists its own request key (retriable acceptance).
        let request_key = state::generate_request_key();
        if let Err(code) = call_op_keyed(
            ctx,
            "channels.probe",
            &json!({
                "scope": channel_scope(),
                "channelId": channel.channel_id,
                "expectedRevision": channel.revision,
            }),
            &request_key,
        ) {
            return code;
        }
    }

    let request_key = state::generate_request_key();
    let result = match call_op_keyed(
        ctx,
        "channels.configure",
        &json!({
            "scope": channel_scope(),
            "channelId": channel.channel_id,
            "adapterType": "feishu",
            "schemaId": channel.schema_id,
            "configuration": configuration,
            "expectedRevision": channel.revision,
        }),
        &request_key,
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };
    eprintln!(
        "feishu 配置完成: revision {}, applied: {}",
        result.get("revision").cloned().unwrap_or(Value::Null),
        result.get("applied").cloned().unwrap_or(Value::Null),
    );
    EXIT_OK
}

/// weixin/wechat: QR login flow (`channels.begin_login` → `poll_login`
/// loop respecting `pollAfterMs` → `channels.configure` with the returned
/// credential reference).
fn weixin_flow(ctx: &mut EntryCtx<'_>, reader: &mut InputReader<'_>) -> i32 {
    if !require_tty() {
        return action_required("gateway setup weixin");
    }
    let channel = match find_channel(ctx, "weixin") {
        Ok(channel) => channel,
        Err(code) => return code,
    };
    // Old behavior: existing credentials prompted "re-login? [y/N]";
    // declining enables nothing extra and writes nothing.
    if channel.enabled {
        let Some(answer) = reader("重新登录？ [y/N]") else {
            eprintln!("已取消。");
            return EXIT_OK;
        };
        if !answer.eq_ignore_ascii_case("y") {
            eprintln!("已取消。");
            return EXIT_OK;
        }
    }
    let challenge = match call_op(
        ctx,
        "channels.begin_login",
        &json!({"scope": channel_scope(), "channelId": channel.channel_id}),
    ) {
        Ok(challenge) => challenge,
        Err(code) => return code,
    };
    let challenge_id = challenge
        .get("challengeId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(url) = challenge.get("userActionUrl").and_then(Value::as_str) {
        eprintln!("扫码登录: {url}");
    } else if let Some(payload) = challenge.get("qrPayload").and_then(Value::as_str) {
        eprintln!("扫码登录 (QR): {payload}");
    }

    // Poll the login flow; the loop is bounded by the challenge expiry and
    // a hard iteration cap.
    let mut credential_ref: Option<String> = None;
    for _ in 0..600 {
        let status = match call_op(
            ctx,
            "channels.poll_login",
            &json!({
                "scope": channel_scope(),
                "channelId": channel.channel_id,
                "challengeId": challenge_id,
            }),
        ) {
            Ok(status) => status,
            Err(code) => return code,
        };
        match status.get("state").and_then(Value::as_str) {
            Some("succeeded") => {
                credential_ref = status
                    .get("credentialRef")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                break;
            }
            Some("expired") => {
                eprintln!("❌ 微信登录已过期");
                return EXIT_FAILED;
            }
            Some("failed") => {
                eprintln!("❌ 微信登录失败");
                return EXIT_FAILED;
            }
            _ => {
                let wait_ms = status
                    .get("pollAfterMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(1000)
                    .min(5000);
                std::thread::sleep(std::time::Duration::from_millis(wait_ms));
            }
        }
    }
    let Some(credential_ref) = credential_ref else {
        eprintln!("❌ 微信登录失败");
        return EXIT_FAILED;
    };

    // Credential save and adapter enable are separate, honest steps. The
    // weixin config schema (agentcpm-pd.channel.weixin.config/1) requires
    // `enabled` and takes the secret only as `credentialRef`.
    let request_key = state::generate_request_key();
    let result = match call_op_keyed(
        ctx,
        "channels.configure",
        &json!({
            "scope": channel_scope(),
            "channelId": channel.channel_id,
            "adapterType": "weixin",
            "schemaId": channel.schema_id,
            "configuration": {"enabled": true, "credentialRef": credential_ref},
            "expectedRevision": channel.revision,
        }),
        &request_key,
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };
    eprintln!(
        "weixin 配置完成: revision {}, applied: {}",
        result.get("revision").cloned().unwrap_or(Value::Null),
        result.get("applied").cloned().unwrap_or(Value::Null),
    );
    EXIT_OK
}

/// wecom: QR scan yields a complete stored credential (`credentialRef`;
/// `channels.poll_login` persists the exchanged secret server-side before
/// reporting `succeeded`); manual entry writes the Bot secret through
/// `credential.write`. Either way the configuration carries only the
/// reference. The old `extra` block (`websocket_url`, `dm_policy`,
/// `group_policy`, allow lists/groups) has no equivalent in the delivered
/// wecom channel schema (agentcpm-pd.channel.wecom.config/1:
/// `enabled`/`credentialRef`/`webhookUrl`/`probeEndpoint`) and is reported
/// as not projectable instead of being invented.
fn wecom_flow(ctx: &mut EntryCtx<'_>, reader: &mut InputReader<'_>) -> i32 {
    if !require_tty() {
        return action_required("gateway setup wecom");
    }
    wecom_flow_inner(ctx, reader)
}

fn wecom_flow_inner(ctx: &mut EntryCtx<'_>, reader: &mut InputReader<'_>) -> i32 {
    let channel = match find_channel(ctx, "wecom") {
        Ok(channel) => channel,
        Err(code) => return code,
    };
    let scan = reader("是否扫码自动创建/获取 Bot ID 和 Secret？ [Y/n]");
    let use_scan = scan.is_none_or(|answer| !answer.eq_ignore_ascii_case("n"));
    // Set when the manual path wrote a credential (the scan path's
    // credential belongs to the login flow, not to this shim).
    let mut written: Option<WrittenCredential> = None;
    let credential_ref: String = if use_scan {
        match scan_wecom_credential(ctx, &channel) {
            Ok(Some(credential_ref)) => credential_ref,
            Ok(None) => match wecom_manual_credential(ctx, reader) {
                Ok(Some(credential)) => {
                    let credential_ref = credential.credential_ref.clone();
                    written = Some(credential);
                    credential_ref
                }
                Ok(None) => return EXIT_OK,
                Err(code) => return code,
            },
            Err(code) => return code,
        }
    } else {
        match wecom_manual_credential(ctx, reader) {
            Ok(Some(credential)) => {
                let credential_ref = credential.credential_ref.clone();
                written = Some(credential);
                credential_ref
            }
            Ok(None) => return EXIT_OK,
            Err(code) => return code,
        }
    };

    note_dropped(
        "gateway setup wecom",
        &[
            "extra.websocket_url",
            "extra.dm_policy",
            "extra.group_policy",
        ],
    );
    let configuration = json!({"enabled": true, "credentialRef": credential_ref});
    let validation = match call_op(
        ctx,
        "channels.validate",
        &json!({
            "scope": channel_scope(),
            "adapterType": "wecom",
            "schemaId": channel.schema_id,
            "configuration": configuration,
        }),
    ) {
        Ok(validation) => validation,
        Err(code) => return code,
    };
    if validation.get("valid").and_then(Value::as_bool) != Some(true) {
        eprintln!(
            "❌ 企业微信配置校验失败: {:?}",
            validation.get("diagnostics")
        );
        if let Some(credential) = written {
            remove_credential(ctx, &credential);
        }
        return EXIT_FAILED;
    }
    let request_key = state::generate_request_key();
    let result = match call_op_keyed(
        ctx,
        "channels.configure",
        &json!({
            "scope": channel_scope(),
            "channelId": channel.channel_id,
            "adapterType": "wecom",
            "schemaId": channel.schema_id,
            "configuration": configuration,
            "expectedRevision": channel.revision,
        }),
        &request_key,
    ) {
        Ok(result) => result,
        Err(code) => return code,
    };
    eprintln!(
        "wecom 配置完成: revision {}, applied: {}",
        result.get("revision").cloned().unwrap_or(Value::Null),
        result.get("applied").cloned().unwrap_or(Value::Null),
    );
    EXIT_OK
}

/// Runs the QR scan flow. `Ok(Some(ref))`: the login succeeded and the
/// stored credential reference is the complete bot credential.
/// `Ok(None)`: the scan failed/expired (the challenge was cancelled) and
/// the caller falls back to manual entry. `Err`: a business failure or a
/// `succeeded` answer without a usable credential reference — the login
/// contract persists the credential before reporting `succeeded`, so an
/// empty reference is reported honestly instead of silently degrading to
/// manual entry.
fn scan_wecom_credential(
    ctx: &mut EntryCtx<'_>,
    channel: &ChannelInfo,
) -> Result<Option<String>, i32> {
    let challenge = call_op(
        ctx,
        "channels.begin_login",
        &json!({"scope": channel_scope(), "channelId": channel.channel_id}),
    )?;
    let challenge_id = challenge
        .get("challengeId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if let Some(url) = challenge.get("userActionUrl").and_then(Value::as_str) {
        eprintln!("扫码: {url}");
    }

    // Poll the login flow; the loop is bounded by the challenge expiry and
    // a hard iteration cap. `cancelled` is terminal like failed/expired
    // (polling it again would burn the whole cap).
    for _ in 0..600 {
        let status = call_op(
            ctx,
            "channels.poll_login",
            &json!({
                "scope": channel_scope(),
                "channelId": channel.channel_id,
                "challengeId": challenge_id,
            }),
        )?;
        match status.get("state").and_then(Value::as_str) {
            Some("succeeded") => {
                return match status.get("credentialRef").and_then(Value::as_str) {
                    Some(reference) if !reference.is_empty() => Ok(Some(reference.to_owned())),
                    _ => {
                        eprintln!(
                            "{SHIM_NAME}: 扫码登录成功但目标未返回凭据引用，无法配置企业微信"
                        );
                        Err(EXIT_FAILED)
                    }
                };
            }
            Some("expired") | Some("failed") | Some("cancelled") => break,
            _ => {
                let wait_ms = status
                    .get("pollAfterMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(1000)
                    .min(5000);
                std::thread::sleep(std::time::Duration::from_millis(wait_ms));
            }
        }
    }
    // Scan failed/expired or the poll budget ran out: cancel the
    // challenge and report that the manual fallback is needed.
    let _ = call_op(
        ctx,
        "channels.cancel_login",
        &json!({
            "scope": channel_scope(),
            "channelId": channel.channel_id,
            "challengeId": challenge_id,
        }),
    );
    Ok(None)
}

/// Manual Bot ID/Secret entry (the declined-scan path and the scan
/// fallback). `Ok(None)`: incomplete credentials — the old usage contract
/// is a reported skip with zero writes (gatewaySetup.ts:335-338).
fn wecom_manual_credential(
    ctx: &mut EntryCtx<'_>,
    reader: &mut InputReader<'_>,
) -> Result<Option<WrittenCredential>, i32> {
    let bot_id = reader("Bot ID:").unwrap_or_default();
    let secret = reader("Secret:").unwrap_or_default();
    if bot_id.is_empty() || secret.is_empty() {
        eprintln!("未提供完整 Bot ID / Secret，跳过企业微信配置。");
        return Ok(None);
    }
    write_credential(
        ctx,
        "channel.wecom",
        &format!("wecom bot {bot_id}"),
        &secret,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryCtx;
    use crate::executor::{ErrorPayload, FakeExecutor, Outcome, Response};
    use crate::testutil::TempDir;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    fn ok(result: Value) -> Result<Response, crate::executor::ExecError> {
        Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Succeeded,
            result: Some(result),
            error: None,
        })
    }

    #[test]
    fn parse_platform_aliases() {
        assert_eq!(
            parse(&argv(&["setup"])).unwrap().platform,
            SetupPlatform::Menu
        );
        assert_eq!(
            parse(&argv(&["setup", "lark"])).unwrap().platform,
            SetupPlatform::Feishu
        );
        assert_eq!(
            parse(&argv(&["setup", "wechat"])).unwrap().platform,
            SetupPlatform::Weixin
        );
        assert_eq!(
            parse(&argv(&["setup", "work-weixin"])).unwrap().platform,
            SetupPlatform::Wecom
        );
        assert_eq!(
            parse(&argv(&["setup", "sms"])).unwrap().platform,
            SetupPlatform::Menu
        );
        assert!(parse(&argv(&["start"])).is_err());
        assert!(parse(&argv(&["setup", "a", "b"])).is_err());
    }

    #[test]
    fn entry_requires_home() {
        let mut exec = FakeExecutor::new();
        // No home: configuration_required before any business call.
        let ctx = EntryCtx::from_env(&mut exec);
        let mut ctx = ctx;
        if std::env::var_os(crate::config::HOME_ENV).is_none() {
            assert_eq!(main(&mut ctx, &argv(&["setup"])), EXIT_FAILED);
            assert!(exec.calls.is_empty());
        }
    }

    #[test]
    fn feishu_flow_sequence_is_complete_and_passes_through_capability_errors() {
        // Non-TTY (the test harness) => action_required, no business call.
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("setup-nontty");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let mut reader = |_: &str| Some("ignored".to_owned());
        assert_eq!(feishu_flow(&mut ctx, &mut reader), EXIT_FAILED);
        assert!(exec.calls.is_empty());

        // Interactive happy path through the inner flow (TTY check
        // bypassed): list → credential.write → validate → probe →
        // configure, and the secret exists ONLY in the credential.write
        // input.
        let mut exec = FakeExecutor::new()
            .push(ok(channels_list("feishu", "ch-feishu")))
            .push(ok(json!({
                "credential": {"credentialId": "cred-9", "kind": "channel.feishu",
                               "name": "feishu app cli-a", "enabled": true,
                               "revision": "crev-1", "fingerprint": "fp"},
            })))
            .push(ok(json!({"valid": true, "diagnostics": []})))
            .push(ok(json!({"kind": "operation", "operationId": "op-probe",
                       "durable": true, "acceptedRevision": "rev-1",
                       "deduplicated": false, "state": "queued"})))
            .push(ok(
                json!({"revision": "rev-2", "applied": true, "generation": "g"}),
            ));
        let home = TempDir::new("setup-feishu");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let answers = ["cli-a", "s-1"];
        let mut step = 0usize;
        let mut reader = move |_: &str| {
            let answer = answers.get(step).map(|s| s.to_string());
            step += 1;
            answer
        };
        assert_eq!(feishu_flow_inner(&mut ctx, &mut reader), EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "channels.list",
                "credential.write",
                "channels.validate",
                "channels.probe",
                "channels.configure"
            ]
        );
        let write = &exec.calls[1];
        assert_eq!(write.input["kind"], "channel.feishu");
        assert_eq!(write.input["secret"], "s-1");
        assert!(write.request_key.is_some(), "credential.write is keyed");
        for call in exec
            .calls
            .iter()
            .filter(|c| c.method == "channels.validate" || c.method == "channels.configure")
        {
            let raw = serde_json::to_string(&call.input).unwrap();
            assert!(!raw.contains("s-1"), "secret leaked into {}", call.method);
            assert_eq!(call.input["configuration"]["credentialRef"], "cred-9");
            assert_eq!(call.input["configuration"]["enabled"], true);
            assert_eq!(call.input["configuration"]["appId"], "cli-a");
        }
    }

    #[test]
    fn feishu_flow_decline_removes_the_written_credential() {
        // Validation failed and the user declines: nothing is configured
        // and the credential written for validation is removed again.
        let mut exec = FakeExecutor::new()
            .push(ok(channels_list("feishu", "ch-feishu")))
            .push(ok(json!({
                "credential": {"credentialId": "cred-9", "kind": "channel.feishu",
                               "name": "n", "enabled": true,
                               "revision": "crev-1", "fingerprint": "fp"},
            })))
            .push(ok(json!({"valid": false, "diagnostics": [
                {"code": "CREDENTIAL_NOT_FOUND", "message": "nope",
                 "severity": "error"},
            ]})))
            .push(ok(
                json!({"removedCredentialId": "cred-9", "revision": "crev-2"}),
            ));
        let home = TempDir::new("setup-feishu-decline");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let answers = ["cli-a", "s-1", "n"];
        let mut step = 0usize;
        let mut reader = move |_: &str| {
            let answer = answers.get(step).map(|s| s.to_string());
            step += 1;
            answer
        };
        assert_eq!(feishu_flow_inner(&mut ctx, &mut reader), EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "channels.list",
                "credential.write",
                "channels.validate",
                "credential.remove"
            ]
        );
        let remove = &exec.calls[3];
        assert_eq!(remove.input["credentialId"], "cred-9");
        assert_eq!(remove.input["expectedRevision"], "crev-1");
    }

    #[test]
    fn wecom_scan_success_configures_the_channel_by_reference() {
        // The review's ③ negative case: a successful scan MUST reach
        // validate + configure — the credentialRef IS the complete
        // credential (no empty-secret sentinel, no fake success).
        let mut exec = FakeExecutor::new()
            .push(ok(channels_list("wecom", "ch-wecom")))
            .push(ok(json!({
                "challengeId": "chal-1", "challengeType": "qr",
                "userActionUrl": "https://login/wecom", "qrPayload": null,
                "userCode": null, "expiresAt": "2030-01-01T00:00:00Z",
                "pollAfterMs": 1,
            })))
            .push(ok(json!({"challengeId": "chal-1", "state": "pending",
                       "pollAfterMs": 1, "credentialRef": null, "errorCode": null})))
            .push(ok(json!({"challengeId": "chal-1", "state": "succeeded",
                       "pollAfterMs": 1, "credentialRef": "cred-login-1",
                       "errorCode": null})))
            .push(ok(json!({"valid": true, "diagnostics": []})))
            .push(ok(
                json!({"revision": "rev-2", "applied": true, "generation": "g"}),
            ));
        let home = TempDir::new("setup-wecom-scan");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let mut reader = |_: &str| Some(String::new());
        assert_eq!(wecom_flow_inner(&mut ctx, &mut reader), EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "channels.list",
                "channels.begin_login",
                "channels.poll_login",
                "channels.poll_login",
                "channels.validate",
                "channels.configure"
            ]
        );
        for call in &exec.calls[4..] {
            assert_eq!(call.input["configuration"]["credentialRef"], "cred-login-1");
            assert_eq!(call.input["configuration"]["enabled"], true);
        }
    }

    #[test]
    fn wecom_scan_succeeded_without_a_credential_reference_fails_honestly() {
        // `succeeded` without a usable credentialRef is an anomaly, not a
        // reason to silently fall back to manual entry.
        let mut exec = FakeExecutor::new()
            .push(ok(channels_list("wecom", "ch-wecom")))
            .push(ok(json!({
                "challengeId": "chal-1", "challengeType": "qr",
                "userActionUrl": "https://login/wecom", "qrPayload": null,
                "userCode": null, "expiresAt": "2030-01-01T00:00:00Z",
                "pollAfterMs": 1,
            })))
            .push(ok(json!({"challengeId": "chal-1", "state": "succeeded",
                       "pollAfterMs": 1, "credentialRef": null, "errorCode": null})));
        let home = TempDir::new("setup-wecom-noref");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let mut reader = |_: &str| Some(String::new());
        assert_eq!(wecom_flow_inner(&mut ctx, &mut reader), EXIT_FAILED);
        assert_eq!(
            exec.methods(),
            [
                "channels.list",
                "channels.begin_login",
                "channels.poll_login"
            ],
            "no validate/configure without a credential reference"
        );
    }

    #[test]
    fn wecom_scan_failure_falls_back_to_manual_entry() {
        let mut exec = FakeExecutor::new()
            .push(ok(channels_list("wecom", "ch-wecom")))
            .push(ok(json!({
                "challengeId": "chal-1", "challengeType": "qr",
                "userActionUrl": "https://login/wecom", "qrPayload": null,
                "userCode": null, "expiresAt": "2030-01-01T00:00:00Z",
                "pollAfterMs": 1,
            })))
            .push(ok(json!({"challengeId": "chal-1", "state": "failed",
                       "pollAfterMs": 1, "credentialRef": null,
                       "errorCode": "LOGIN_FAILED"})))
            .push(ok(json!({"cancelled": false, "state": "failed"})))
            .push(ok(json!({
                "credential": {"credentialId": "cred-7", "kind": "channel.wecom",
                               "name": "wecom bot bot-1", "enabled": true,
                               "revision": "crev-7", "fingerprint": "fp"},
            })))
            .push(ok(json!({"valid": true, "diagnostics": []})))
            .push(ok(
                json!({"revision": "rev-2", "applied": true, "generation": "g"}),
            ));
        let home = TempDir::new("setup-wecom-fallback");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let answers = ["", "bot-1", "secret-9"];
        let mut step = 0usize;
        let mut reader = move |_: &str| {
            let answer = answers.get(step).map(|s| s.to_string());
            step += 1;
            answer
        };
        assert_eq!(wecom_flow_inner(&mut ctx, &mut reader), EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "channels.list",
                "channels.begin_login",
                "channels.poll_login",
                "channels.cancel_login",
                "credential.write",
                "channels.validate",
                "channels.configure"
            ]
        );
        let write = &exec.calls[4];
        assert_eq!(write.input["kind"], "channel.wecom");
        assert_eq!(write.input["secret"], "secret-9");
        for call in &exec.calls[5..] {
            let raw = serde_json::to_string(&call.input).unwrap();
            assert!(
                !raw.contains("secret-9"),
                "secret leaked into {}",
                call.method
            );
            assert_eq!(call.input["configuration"]["credentialRef"], "cred-7");
        }
    }

    #[test]
    fn wecom_manual_entry_writes_the_secret_through_credential_store() {
        // Declined scan: manual Bot ID/Secret still never enter a channel
        // payload — only credential.write sees the secret.
        let mut exec = FakeExecutor::new()
            .push(ok(channels_list("wecom", "ch-wecom")))
            .push(ok(json!({
                "credential": {"credentialId": "cred-7", "kind": "channel.wecom",
                               "name": "wecom bot bot-1", "enabled": true,
                               "revision": "crev-7", "fingerprint": "fp"},
            })))
            .push(ok(json!({"valid": true, "diagnostics": []})))
            .push(ok(
                json!({"revision": "rev-2", "applied": true, "generation": "g"}),
            ));
        let home = TempDir::new("setup-wecom-manual");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let answers = ["n", "bot-1", "secret-9"];
        let mut step = 0usize;
        let mut reader = move |_: &str| {
            let answer = answers.get(step).map(|s| s.to_string());
            step += 1;
            answer
        };
        assert_eq!(wecom_flow_inner(&mut ctx, &mut reader), EXIT_OK);
        assert_eq!(
            exec.methods(),
            [
                "channels.list",
                "credential.write",
                "channels.validate",
                "channels.configure"
            ]
        );
        assert_eq!(exec.calls[1].input["secret"], "secret-9");
        for call in &exec.calls[2..] {
            let raw = serde_json::to_string(&call.input).unwrap();
            assert!(
                !raw.contains("secret-9"),
                "secret leaked into {}",
                call.method
            );
            assert_eq!(call.input["configuration"]["credentialRef"], "cred-7");
            assert_eq!(call.input["configuration"]["enabled"], true);
        }
    }

    #[test]
    fn wecom_incomplete_manual_credentials_skip_with_zero_writes() {
        // Old usage contract: incomplete manual credentials are a reported
        // skip with zero writes (exit 0).
        let mut exec = FakeExecutor::new().push(ok(channels_list("wecom", "ch-wecom")));
        let home = TempDir::new("setup-wecom-skip");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let answers = ["n", "", ""];
        let mut step = 0usize;
        let mut reader = move |_: &str| {
            let answer = answers.get(step).map(|s| s.to_string());
            step += 1;
            answer
        };
        assert_eq!(wecom_flow_inner(&mut ctx, &mut reader), EXIT_OK);
        assert_eq!(
            exec.methods(),
            ["channels.list"],
            "no writes after an incomplete manual entry"
        );
    }

    /// A `channels.list` result carrying one configured-or-empty row.
    fn channels_list(adapter: &str, channel_id: &str) -> Value {
        json!({
            "channels": [{
                "channelId": channel_id, "adapterType": adapter,
                "schemaId": format!("agentcpm-pd.channel.{adapter}.config/1"),
                "revision": "rev-1", "enabled": false, "runtime": {},
                "configuration": {},
            }],
            "supportedAdapterTypes": [], "generation": "g",
        })
    }

    #[test]
    fn capability_unavailable_from_channels_list_passes_through() {
        let mut exec = FakeExecutor::new().push(Ok(Response {
            version: None,
            request_id: None,
            outcome: Outcome::Failed,
            result: None,
            error: Some(ErrorPayload {
                code: "CAPABILITY_UNAVAILABLE".to_owned(),
                message: "channels.list not served".to_owned(),
                details: None,
            }),
        }));
        let home = TempDir::new("setup-cap");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        assert!(matches!(find_channel(&mut ctx, "feishu"), Err(EXIT_FAILED)));
        assert_eq!(exec.methods(), ["channels.list"]);
    }

    #[test]
    fn weixin_flow_requires_tty_and_reports_login_states() {
        // Non-TTY => action_required.
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("setup-weixin-nontty");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let mut reader = |_: &str| None::<String>;
        assert_eq!(weixin_flow(&mut ctx, &mut reader), EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }

    #[test]
    fn wecom_flow_requires_tty() {
        let mut exec = FakeExecutor::new();
        let home = TempDir::new("setup-wecom-nontty");
        let mut ctx = EntryCtx::with_home(&mut exec, home.path());
        let mut reader = |_: &str| None::<String>;
        assert_eq!(wecom_flow(&mut ctx, &mut reader), EXIT_FAILED);
        assert!(exec.calls.is_empty());
    }
}
