//! Native host services for channel commands, shared by all adapters.
use super::{CommandError, CommandFuture, CommandHost, Project, Status, UpdateResult, search};
use pilotdeck_protocol::{JsString, JsValue};
use std::{path::PathBuf, time::Instant};

static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// Call at the earliest executable entry. Embedded hosts must call explicitly.
pub fn record_process_start() {
    PROCESS_START.get_or_init(Instant::now);
}
pub fn process_start() -> Option<Instant> {
    PROCESS_START.get().copied()
}

pub struct NativeCommandsHost {
    home: PathBuf,
    cwd: PathBuf,
    executable: Option<PathBuf>,
    started: Option<Instant>,
}
impl NativeCommandsHost {
    pub fn new(
        home: PathBuf,
        cwd: PathBuf,
        executable: Option<PathBuf>,
        started: Option<Instant>,
    ) -> Self {
        Self {
            home,
            cwd,
            executable,
            started,
        }
    }
}
impl CommandHost for NativeCommandsHost {
    fn list_projects(&self) -> CommandFuture<'_, Vec<Project>> {
        Box::pin(async move {
            let home = self.home.clone();
            let cwd = self.cwd.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::metadata::Metadata {
                    home,
                    cwd,
                    files: crate::metadata::NativeFiles,
                }
                .list_projects()
            })
            .await
            .map_err(|error| CommandError(error.to_string().into()))?
            .map_err(|error| CommandError(error.to_string().into()))?;
            let Some(JsValue::Array(projects)) = result.get("projects") else {
                return Err(CommandError("invalid project catalog".into()));
            };
            projects
                .iter()
                .map(|p| {
                    let field = |key| {
                        p.get(key)
                            .and_then(JsValue::as_string)
                            .cloned()
                            .ok_or_else(|| {
                                CommandError(format!("invalid project field: {key}").into())
                            })
                    };
                    Ok(Project {
                        name: field("name")?,
                        project_key: field("projectKey")?,
                        full_path: field("fullPath")?,
                    })
                })
                .collect()
        })
    }
    fn run_update(&self, check: bool) -> CommandFuture<'_, UpdateResult> {
        Box::pin(super::update::run(self.executable.as_deref(), check))
    }
    fn update_operation(
        &self,
        check: bool,
    ) -> Result<Option<super::update::UpdateOperation>, CommandError> {
        super::update::start(self.executable.as_deref(), check).map(Some)
    }
    fn status(&self) -> CommandFuture<'_, Status> {
        Box::pin(async move {
            async fn git(cwd: &std::path::Path, args: &[&str]) -> Result<JsString, CommandError> {
                let output = tokio::process::Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|e| CommandError(e.to_string().into()))?;
                if !output.status.success() {
                    return Err(CommandError(
                        format!(
                            "Command failed: git {}\n{}",
                            args.join(" "),
                            String::from_utf8_lossy(&output.stderr)
                        )
                        .into(),
                    ));
                }
                Ok(String::from_utf8_lossy(&output.stdout).as_ref().into())
            }
            let cwd = self
                .executable
                .as_deref()
                .and_then(std::path::Path::parent)
                .unwrap_or(&self.cwd);
            let branch = git(cwd, &["branch", "--show-current"]).await?;
            let commit = git(cwd, &["log", "--oneline", "-1", "HEAD"]).await?;
            Ok(Status {
                branch,
                commit,
                uptime_seconds: self
                    .started
                    .ok_or_else(|| CommandError("process start was not recorded".into()))?
                    .elapsed()
                    .as_secs(),
                node: "not used (native Rust)".into(),
                platform: match std::env::consts::OS {
                    "macos" => "darwin",
                    other => other,
                }
                .into(),
            })
        })
    }
    fn search<'a>(
        &'a self,
        raw: &'a JsString,
        project: Option<&'a JsString>,
    ) -> CommandFuture<'a, Option<JsString>> {
        Box::pin(async move {
            let tokens: Vec<JsString> = raw
                .units()
                .split(|unit| super::super::sms::trim_units(std::slice::from_ref(unit)).is_empty())
                .filter(|part| !part.is_empty())
                .map(|part| JsString::from_utf16(part.to_vec()))
                .collect();
            let mut index = 0;
            let mut query = Vec::new();
            let mut all = false;
            let mut limit = 20.0;
            let mut regex = false;
            let mut sensitive = false;
            let mut role = "all";
            let mut session = None;
            while index < tokens.len() {
                let token = &tokens[index];
                if token.equals_str("--all") || token.equals_str("-a") {
                    all = true;
                } else if token.equals_str("--regex") || token.equals_str("-E") {
                    regex = true;
                } else if token.equals_str("--case-sensitive") {
                    sensitive = true;
                } else if token.equals_str("--limit") || token.equals_str("-n") {
                    if let Some(value) = tokens
                        .get(index + 1)
                        .and_then(|v| crate::js_value::to_number(&JsValue::String(v.clone())).ok())
                        .filter(|n| n.is_finite() && *n > 0.0)
                    {
                        limit = value.floor().min(100.0);
                        index += 1;
                    }
                } else if token.equals_str("--role") || token.equals_str("-r") {
                    if let Some(value) = tokens.get(index + 1).and_then(|v| {
                        if v.equals_str("user") {
                            Some("user")
                        } else if v.equals_str("assistant") {
                            Some("assistant")
                        } else if v.equals_str("all") {
                            Some("all")
                        } else {
                            None
                        }
                    }) {
                        role = value;
                        index += 1;
                    }
                } else if token.equals_str("--session") || token.equals_str("-s") {
                    if let Some(value) = tokens.get(index + 1) {
                        session = Some(value.clone());
                        index += 1;
                    }
                } else {
                    query.push(token.clone());
                }
                index += 1;
            }
            let mut query_units = Vec::new();
            for (i, part) in query.iter().enumerate() {
                if i > 0 {
                    query_units.push(32);
                }
                query_units.extend_from_slice(part.units());
            }
            let query = JsString::from_utf16(query_units);
            if query.is_empty() {
                return Ok(Some("用法：/search <关键词> [--all] [--limit N] [--role user|assistant]\n示例：/search docker 部署".into()));
            }
            let project = project.cloned();
            let home = self.home.clone();
            let cwd = self.cwd.clone();
            let include_project = all || project.as_ref().is_none_or(JsString::is_empty);
            let result = tokio::task::spawn_blocking(move || {
                search::search(search::SearchInput {
                    home: &home,
                    cwd: &cwd,
                    project: if all { None } else { project.as_ref() },
                    session: session.as_ref(),
                    query: &query,
                    limit,
                    regex,
                    sensitive,
                    role,
                })
            })
            .await
            .map_err(|error| CommandError(error.to_string().into()))?
            .map_err(|error| match error {
                search::Failure::Error(message) => CommandError(message.into()),
                search::Failure::JsError(message) => CommandError(message),
            })?;
            Ok(Some(format_search(&result, include_project)))
        })
    }
}
fn format_search(result: &search::SearchResult, projects: bool) -> JsString {
    use super::{concat, joined, text};
    if result.matches.is_empty() {
        return concat(&[
            &text("未找到包含「"),
            &result.query.0,
            &text(&format!(
                "」的聊天记录。\n已扫描 {} 个会话。",
                result.sessions_scanned
            )),
        ]);
    }
    let mut lines = vec![
        concat(&[
            &text(&format!("🔍 找到 {} 条匹配「", result.matches.len())),
            &result.query.0,
            &text(if result.truncated {
                "」（结果已截断，可加大 --limit）"
            } else {
                "」"
            }),
        ]),
        text(""),
    ];
    for (i, item) in result.matches.iter().enumerate() {
        let units = item.session_id.0.units();
        let short = if units.len() > 12 {
            concat(&[&JsString::from_utf16(units[..8].to_vec()), &text("…")])
        } else {
            item.session_id.0.clone()
        };
        let project = if projects && !item.project_key.is_empty() {
            format!(
                " · {}",
                item.project_key
                    .rsplit(['/', '\\'])
                    .next()
                    .filter(|x| !x.is_empty())
                    .unwrap_or(&item.project_key)
            )
        } else {
            String::new()
        };
        lines.push(concat(&[
            &text(&format!("{}. **", i + 1)),
            &item.session_title.0,
            &text("** (`"),
            &short,
            &text(&format!(
                "`){}\n   {} · ",
                project,
                if item.role == "user" {
                    "用户"
                } else {
                    "助手"
                }
            )),
            &when(&item.created_at.0),
            &text("\n   > "),
            &item.snippet.0,
        ]));
        lines.push(text(""));
    }
    lines.push(text(
        "提示：在 Web UI 中打开对应会话后，可用 /search 结果中的片段定位消息。",
    ));
    joined(&lines)
}
fn when(value: &JsString) -> JsString {
    if value.is_empty() {
        return "未知时间".into();
    }
    use chrono::{Datelike, TimeZone, Timelike};
    let Ok(Some(ms)) = pilotdeck_transport::system_clock::parse_node22_date(value) else {
        return value.clone();
    };
    let Some(date) = chrono::Local.timestamp_millis_opt(ms as i64).single() else {
        return value.clone();
    };
    format!(
        "{}月{}日 {:02}:{:02}",
        date.month(),
        date.day(),
        date.hour(),
        date.minute()
    )
    .into()
}
