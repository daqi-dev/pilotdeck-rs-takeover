//! Shared IM slash commands. Host operations are injected; this module never
//! invokes a shell, contacts a provider, or creates a second gateway.
use pilotdeck_protocol::JsString;
use std::{future::Future, pin::Pin};

pub type CommandFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, CommandError>> + Send + 'a>>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandError(pub JsString);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub name: JsString,
    pub project_key: JsString,
    pub full_path: JsString,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateResult {
    pub code: Option<i32>,
    pub output: JsString,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub branch: JsString,
    pub commit: JsString,
    pub uptime_seconds: u64,
    pub node: JsString,
    pub platform: JsString,
}

/// Implementations must call the real native host services. A missing search
/// capability is represented by `None`, matching the baseline context gate.
pub trait CommandHost: Send + Sync {
    fn list_projects(&self) -> CommandFuture<'_, Vec<Project>>;
    fn run_update(&self, check: bool) -> CommandFuture<'_, UpdateResult>;
    fn status(&self) -> CommandFuture<'_, Status>;
    fn search<'a>(
        &'a self,
        raw_args: &'a JsString,
        project: Option<&'a JsString>,
    ) -> CommandFuture<'a, Option<JsString>>;
}
/// Per-message reply and mutable adapter-local session scope. The callbacks
/// must preserve their actual delivery/mapper failure instead of hiding it.
pub trait CommandContext: Send + Sync {
    fn reply<'a>(&'a self, text: &'a JsString) -> CommandFuture<'a, ()>;
    fn current_project(&self) -> Option<JsString>;
    fn bind_project(&self, project: &JsString) -> Result<(), CommandError>;
    fn reset_session(&self) -> Result<(), CommandError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Name {
    New,
    Plan,
    Projects,
    SwitchProject,
    Update,
    Status,
    Search,
    Help,
}
pub struct Resolved {
    pub name: Name,
    pub arg: JsString,
}
fn trim(s: &JsString) -> JsString {
    JsString::from_utf16(super::sms::trim_units(s.units()).to_vec())
}
fn lower(s: &JsString) -> JsString {
    let mut out = Vec::new();
    let mut segment = String::new();
    for value in std::char::decode_utf16(s.units().iter().copied()) {
        match value {
            Ok(c) => segment.push(c),
            Err(e) => {
                out.extend(segment.to_lowercase().encode_utf16());
                segment.clear();
                out.push(e.unpaired_surrogate());
            }
        }
    }
    out.extend(segment.to_lowercase().encode_utf16());
    JsString::from_utf16(out)
}
fn concat(parts: &[&JsString]) -> JsString {
    JsString::from_utf16(
        parts
            .iter()
            .flat_map(|p| p.units().iter().copied())
            .collect(),
    )
}
fn text(s: &str) -> JsString {
    s.into()
}
fn joined(lines: &[JsString]) -> JsString {
    let mut result = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            result.push(10);
        }
        result.extend_from_slice(line.units());
    }
    JsString::from_utf16(result)
}
pub fn resolve(input: &JsString) -> Option<Resolved> {
    let input = trim(input);
    let units = input.units();
    if units.first() != Some(&(b'/' as u16)) {
        return None;
    }
    let space = units.iter().position(|u| *u == 32);
    let end = space.filter(|index| *index > 0).unwrap_or(units.len());
    let name = lower(&JsString::from_utf16(units[1..end].to_vec()));
    let arg = space
        .filter(|index| *index > 0)
        .map(|index| trim(&JsString::from_utf16(units[index + 1..].to_vec())))
        .unwrap_or_default();
    let name = if name.equals_str("new") {
        Name::New
    } else if name.equals_str("plan") {
        Name::Plan
    } else if name.equals_str("projects") || name.equals_str("项目列表") {
        Name::Projects
    } else if name.equals_str("switch-project") || name.equals_str("切换项目") {
        Name::SwitchProject
    } else if name.equals_str("update") || name.equals_str("升级") || name.equals_str("更新") {
        Name::Update
    } else if name.equals_str("status") || name.equals_str("状态") {
        Name::Status
    } else if name.equals_str("search")
        || name.equals_str("find")
        || name.equals_str("grep")
        || name.equals_str("搜索")
    {
        Name::Search
    } else if name.equals_str("help") || name.equals_str("帮助") {
        Name::Help
    } else {
        return None;
    };
    Some(Resolved { name, arg })
}
const HELP: &str = "📋 可用命令：\n\n/new — Create a new conversation session\n/plan — 先生成计划并请求确认\n/projects (/项目列表) — List available projects\n/switch-project (/切换项目) — Switch active project for this chat\n/update (/升级, /更新) — Install the latest supported Release and request a Web service restart\n/status (/状态) — Show PilotDeck status and version\n/search (/find, /grep, /搜索) — Search chat history across sessions\n/help (/帮助) — Show available commands";
async fn say(ctx: &dyn CommandContext, s: impl Into<JsString>) -> Result<(), CommandError> {
    ctx.reply(&s.into()).await
}
fn contains(haystack: &JsString, needle: &JsString) -> bool {
    needle.units().is_empty()
        || haystack
            .units()
            .windows(needle.units().len())
            .any(|part| part == needle.units())
}
async fn handle(
    name: Name,
    arg: &JsString,
    host: &dyn CommandHost,
    ctx: &dyn CommandContext,
) -> Result<(), CommandError> {
    match name {
        Name::New | Name::Plan => unreachable!(),
        Name::Help => say(ctx, HELP).await,
        Name::Projects => {
            let projects = host.list_projects().await?;
            if projects.is_empty() {
                return say(ctx, "暂无项目。使用 Web UI 创建 WorkSpace 后即可在此切换。").await;
            }
            let current = ctx.current_project();
            let mut lines = vec![text("📂 项目列表："), text("")];
            for p in projects {
                lines.push(concat(&[
                    &text("• "),
                    &p.name,
                    &text(if current.as_ref() == Some(&p.project_key) {
                        " ✅"
                    } else {
                        ""
                    }),
                ]));
            }
            lines.push(text(""));
            lines.push(text("发送 /switch-project <项目名> 切换 WorkSpace"));
            ctx.reply(&joined(&lines)).await
        }
        Name::SwitchProject => {
            if arg.units().is_empty() {
                return say(
                    ctx,
                    "用法：/switch-project <项目名>\n\n发送 /projects 查看可用项目。",
                )
                .await;
            }
            let projects = host.list_projects().await?;
            let lower_arg = lower(arg);
            let target = projects
                .iter()
                .find(|p| p.name == *arg)
                .or_else(|| projects.iter().find(|p| lower(&p.name) == lower_arg))
                .or_else(|| {
                    projects
                        .iter()
                        .find(|p| contains(&lower(&p.name), &lower_arg))
                });
            if let Some(p) = target {
                ctx.bind_project(&p.project_key)?;
                ctx.reset_session()?;
                ctx.reply(&concat(&[
                    &text("已切换到项目："),
                    &p.name,
                    &text("\n路径："),
                    &p.full_path,
                    &text("\n（已自动创建新会话）"),
                ]))
                .await
            } else {
                ctx.reply(&concat(&[
                    &text("未找到匹配「"),
                    arg,
                    &text("」的项目。\n\n发送 /projects 查看可用项目。"),
                ]))
                .await
            }
        }
        Name::Update => {
            let check = arg.equals_str("check");
            if !arg.units().is_empty() && !check {
                return say(ctx,"用法：/update check 检查 Release；/update 更新并请求重启。未运行 Web 服务时，请使用 CLI 手动更新。").await;
            }
            say(
                ctx,
                if check {
                    "⏳ 正在检查 Release 更新..."
                } else {
                    "⏳ 正在检查部署资格并更新 Release，完成后向 Web 服务请求重启..."
                },
            )
            .await?;
            match host.run_update(check).await {
                Ok(result) if result.code == Some(2) => {
                    say(ctx, "✅ 已是最新 Release，无需更新。").await
                }
                Ok(result) => {
                    // JS splits on LF or CRLF, but a terminal bare CR is data.
                    let units = result.output.units();
                    let mut lines = Vec::new();
                    let mut start = 0;
                    for (index, unit) in units.iter().enumerate() {
                        if *unit == 10 {
                            let end = if index > start && units[index - 1] == 13 {
                                index - 1
                            } else {
                                index
                            };
                            if end > start {
                                lines.push(&units[start..end]);
                            }
                            start = index + 1;
                        }
                    }
                    if start < units.len() {
                        lines.push(&units[start..]);
                    }
                    let last = lines.last().copied();
                    let message = last
                        .map(|v| JsString::from_utf16(v.to_vec()))
                        .unwrap_or_else(|| text("更新命令未返回结果，请查看设置页。"));
                    ctx.reply(&concat(&[
                        &text(if result.code == Some(0) {
                            "✅ "
                        } else {
                            "❌ "
                        }),
                        &message,
                    ]))
                    .await
                }
                Err(e) => ctx.reply(&concat(&[&text("❌ 更新失败："), &e.0])).await,
            }
        }
        Name::Status => match host.status().await {
            Ok(s) => {
                let minutes = s.uptime_seconds / 60;
                let uptime = if minutes >= 60 {
                    format!("{}h {}m", minutes / 60, minutes % 60)
                } else {
                    format!("{minutes}m")
                };
                ctx.reply(&joined(&[
                    text("📊 PilotDeck Status"),
                    text(""),
                    concat(&[&text("分支: "), &trim(&s.branch)]),
                    concat(&[&text("提交: "), &trim(&s.commit)]),
                    text(&format!("运行时间: {uptime}")),
                    concat(&[&text("Node: "), &s.node]),
                    concat(&[&text("平台: "), &s.platform]),
                ]))
                .await
            }
            Err(e) => ctx.reply(&concat(&[&text("获取状态失败: "), &e.0])).await,
        },
        Name::Search => {
            if arg.units().is_empty() {
                return say(ctx,"用法：/search <关键词> [--all] [--limit N] [--role user|assistant]\n示例：/search docker 部署").await;
            }
            let project = ctx.current_project();
            match host.search(arg, project.as_ref()).await? {
                Some(output) => ctx.reply(&output).await,
                None => say(ctx, "聊天记录搜索当前不可用。").await,
            }
        }
    }
}
/// False means the caller must retain normal mapper/gateway handling. A reply
/// failure from the error-reporting path is returned to the adapter.
pub async fn execute(
    input: &JsString,
    host: &dyn CommandHost,
    ctx: &dyn CommandContext,
) -> Result<bool, CommandError> {
    let Some(resolved) = resolve(input) else {
        return Ok(false);
    };
    if matches!(resolved.name, Name::New | Name::Plan) {
        return Ok(false);
    }
    if let Err(error) = handle(resolved.name, &resolved.arg, host, ctx).await {
        ctx.reply(&concat(&[&text("命令执行失败: "), &error.0]))
            .await?;
    }
    Ok(true)
}
#[cfg(test)]
mod tests;
