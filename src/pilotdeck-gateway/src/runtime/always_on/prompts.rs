//! Port of the discovery / workspace / execution / report prompts of TS
//! `src/always-on/runtime/discoveryPrompts.ts` and `discoveryPrompts.zh.ts`
//! (the apply prompt lives with the diff in `apply.rs`). The texts are the
//! baseline's verbatim.

/// JS `String.prototype.trim`.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

fn zh(language: Option<&str>) -> bool {
    language == Some("zh-CN")
}

/// `buildWorkspacePrompt` (discoveryPrompts.ts:154-175 / zh :125-145).
pub(crate) fn build_workspace_prompt(
    project_root: &str,
    plan_title: &str,
    language: Option<&str>,
) -> String {
    if zh(language) {
        return [
            "你正在为 Always-On 计划执行准备一个隔离工作区。".to_string(),
            String::new(),
            format!("项目根目录: {project_root}"),
            format!("计划: \"{plan_title}\""),
            String::new(),
            "可用的工作区策略:".into(),
            "  - `git-worktree`: 在新分支上创建 git worktree。速度快、空间占用少 (使用硬链接)。".into(),
            "    要求项目是 git 仓库且至少有一次提交。若工作区存在未提交更改，Always-On".into(),
            "    会先提交当前全部改动作为 checkpoint，再创建 worktree。".into(),
            "  - `snapshot-copy`: 复制项目目录 (在 APFS/btrfs 上使用 CoW)。适用于任何目录,".into(),
            "    但占用更多磁盘空间。默认忽略 .git、node_modules、dist。".into(),
            String::new(),
            "权限: 本轮运行在 `bypassPermissions` 模式下——所有工具调用均自动允许。".into(),
            String::new(),
            "## 执行步骤".into(),
            "1. 检查项目根目录状态 (如 git 仓库可执行 `git status --porcelain`, 否则执行 `ls`)。".into(),
            "2. 调用 `always_on_prepare_workspace`, 传入选定的策略, 或传入 `auto` 让运行时自动选择。".into(),
        ]
        .join("\n");
    }
    [
        "You are preparing an isolated workspace for an Always-On plan execution.".to_string(),
        String::new(),
        format!("Project root: {project_root}"),
        format!("Plan: \"{plan_title}\""),
        String::new(),
        "Available workspace strategies:".into(),
        "  - `git-worktree`: Creates a git worktree on a new branch. Fast and space-efficient (hard-links).".into(),
        "    Requires a git repo with at least one commit. If the working tree is dirty, Always-On".into(),
        "    checkpoints all current changes before creating the worktree.".into(),
        "  - `snapshot-copy`: Copies the project directory (CoW on APFS/btrfs). Works for any directory".into(),
        "    but uses more disk space. Ignores .git, node_modules, dist by default.".into(),
        String::new(),
        "Permissions: this turn runs in `bypassPermissions` mode — every tool call is auto-allowed.".into(),
        String::new(),
        "## What to do".into(),
        "1. Check the project root state (e.g. `git status --porcelain` if it looks like a git repo, or `ls` otherwise).".into(),
        "2. Call `always_on_prepare_workspace` with the chosen strategy, or `auto` to let the runtime decide.".into(),
    ]
    .join("\n")
}

/// `buildExecutionPrompt` (discoveryPrompts.ts:185-203 / zh :147-163).
pub(crate) fn build_execution_prompt(
    plan_markdown: &str,
    workspace_cwd: &str,
    workspace_strategy: &str,
    language: Option<&str>,
) -> String {
    let plan = js_trim(plan_markdown);
    if zh(language) {
        return [
            "你正在隔离工作区内执行一个 Always-On 发现计划。".to_string(),
            format!("工作区策略: {workspace_strategy}"),
            format!("工作区路径: {workspace_cwd}"),
            String::new(),
            "权限: 本轮运行在 `bypassPermissions` 模式下——所有工具调用均自动允许。".into(),
            "安全边界为工作区本身; 请勿 cd 到工作区外部, 请勿修改用户的项目根目录。".into(),
            String::new(),
            "## 计划".into(),
            plan.into(),
            String::new(),
            "## 执行步骤".into(),
            "1. 按顺序执行 Execution Steps 中的各项步骤。".into(),
            "2. 运行 Verification 列表中的检查项并记录结果。".into(),
            "3. 回复执行总结及验证结果。".into(),
        ]
        .join("\n");
    }
    [
        "You are executing an Always-On discovery plan inside an isolated workspace.".to_string(),
        format!("Workspace strategy: {workspace_strategy}."),
        format!("Workspace cwd: {workspace_cwd}"),
        String::new(),
        "Permissions: this turn runs in `bypassPermissions` mode — every tool call is auto-allowed.".into(),
        "Safety boundary is the workspace itself; do NOT cd outside it, do NOT touch the user's project root.".into(),
        String::new(),
        "## Plan".into(),
        plan.into(),
        String::new(),
        "## What to do".into(),
        "1. Execute the Execution Steps in order.".into(),
        "2. Run the Verification list and record results.".into(),
        "3. Respond with a summary of what was done and the verification outcomes.".into(),
    ]
    .join("\n")
}

/// `buildReportPrompt` (discoveryPrompts.ts:213-234 / zh :165-181).
pub(crate) fn build_report_prompt(
    plan_markdown: &str,
    workspace_cwd: &str,
    workspace_strategy: &str,
    language: Option<&str>,
) -> String {
    let plan = js_trim(plan_markdown);
    if zh(language) {
        return [
            "你正在为已完成的 Always-On 计划执行撰写工作报告。".to_string(),
            format!("工作区策略: {workspace_strategy}"),
            format!("工作区路径: {workspace_cwd}"),
            String::new(),
            "权限: 本轮运行在 `bypassPermissions` 模式下——所有工具调用均自动允许。".into(),
            String::new(),
            "## 已执行的计划".into(),
            plan.into(),
            String::new(),
            "## 执行步骤".into(),
            "1. 查看工作区中的变更 (如 `git diff --stat`、`ls`、阅读相关文件)。".into(),
            "2. 总结执行情况: 执行了哪些步骤、修改了哪些文件、命令输出、验证结果。".into(),
            "3. 调用 `always_on_report` 恰好一次, 提交完整的工作报告 markdown。".into(),
            String::new(),
            "每个章节必须使用 `##`（h2）标题——例如 `## Plan Reference`、`## Steps Performed` 等。".into(),
            "报告章节按以下顺序排列: Plan Reference, Steps Performed, Files Changed, Command Output, Verification Results, Follow-ups, Notes。".into(),
            "缺失的章节将由运行时自动补全。".into(),
        ]
        .join("\n");
    }
    [
        "You are writing a work report for a completed Always-On plan execution.".to_string(),
        format!("Workspace strategy: {workspace_strategy}."),
        format!("Workspace cwd: {workspace_cwd}"),
        String::new(),
        "Permissions: this turn runs in `bypassPermissions` mode — every tool call is auto-allowed.".into(),
        String::new(),
        "## Plan that was executed".into(),
        plan.into(),
        String::new(),
        "## What to do".into(),
        "1. Review the workspace to see what changed (e.g. `git diff --stat`, `ls`, read relevant files).".into(),
        "2. Summarize the execution: what steps were performed, which files were changed, command outputs, and verification results.".into(),
        "3. Call `always_on_report` exactly once with the full work-report markdown.".into(),
        String::new(),
        "Each section MUST use `##` (h2) headers — e.g. `## Plan Reference`, `## Steps Performed`, etc.".into(),
        "Required sections in order: Plan Reference, Steps Performed, Files Changed, Command Output, Verification Results, Follow-ups, Notes.".into(),
        "Missing sections will be filled by the runtime fallback.".into(),
    ]
    .join("\n")
}

/// One `ChatSessionDigest` (ChatDigestBuilder.ts:4-10) as the prompt reads it.
pub(crate) struct DigestSession {
    pub alias: String,
    pub title: String,
    /// `new Date(lastModified).toISOString()`.
    pub last_modified: String,
    pub user_prompts: Vec<String>,
}

/// `ExistingPlanSummary` (discoveryPrompts.ts:16-21), each field its JS
/// `String()` rendering.
pub(crate) struct ExistingPlan {
    pub title: String,
    pub dedupe_key: String,
    pub status: String,
}

pub(crate) struct DiscoveryPromptInput<'a> {
    pub project_root: &'a str,
    pub run_id: &'a str,
    pub created_at: &'a str,
    /// `{ cwd, strategy }` of a live previous workspace.
    pub workspace: Option<(&'a str, &'a str)>,
    pub digest: &'a [DigestSession],
    pub existing_plans: &'a [ExistingPlan],
    pub language: Option<&'a str>,
}

/// `lastModified.replace(/\.\d{3}Z$/, "Z")`.
fn strip_millis(iso: &str) -> String {
    let bytes = iso.as_bytes();
    let n = bytes.len();
    if n >= 5
        && bytes[n - 1] == b'Z'
        && bytes[n - 5] == b'.'
        && bytes[n - 4..n - 1].iter().all(u8::is_ascii_digit)
    {
        return format!("{}Z", &iso[..n - 5]);
    }
    iso.to_string()
}

/// `buildDiscoveryPrompt` (discoveryPrompts.ts:40-145 / zh :15-123).
pub(crate) fn build_discovery_prompt(input: &DiscoveryPromptInput<'_>) -> String {
    let zh = zh(input.language);
    let root = input.project_root;
    let mut lines: Vec<String> = Vec::new();
    let (header, access): (String, Vec<String>) = match input.workspace {
        Some((cwd, strategy)) if zh => (
            format!("你正在为项目执行自主 Always-On 发现任务: {root} (当前在隔离工作区中: {cwd})"),
            vec![
                format!("隔离工作区路径: {cwd}"),
                format!("工作区策略: {strategy}"),
                "此工作区是项目的隔离快照——可在其中自由使用 read_file / glob / bash。".into(),
                format!("请勿 cd 到工作区外部的项目根目录 ({root})。"),
            ],
        ),
        Some((cwd, strategy)) => (
            format!("You are running an autonomous Always-On discovery for project: {root} (working inside isolated workspace: {cwd})"),
            vec![
                format!("Isolated workspace cwd: {cwd}"),
                format!("Workspace strategy: {strategy}."),
                "This workspace is an isolated snapshot of the project — read / glob / bash freely inside it.".into(),
                format!("Do NOT cd outside the workspace to the project root ({root})."),
            ],
        ),
        None if zh => (
            format!("你正在为项目执行自主 Always-On 发现任务: {root}"),
            vec![format!("可使用 read_file / glob / bash 自由读取项目根目录: {root}")],
        ),
        None => (
            format!("You are running an autonomous Always-On discovery for project: {root}"),
            vec![format!("Read the project root at {root} using read_file / glob / bash freely.")],
        ),
    };
    lines.push(header);
    let intro: &[&str] = if zh {
        &[
            "",
            "目标: 最多提出一个有价值的任务。",
            "任务范围包括但不限于: 丰富或补充内容、完成未竟工作、优化结构或布局、",
            "修复错误、提升用户体验, 以及用户在聊天记录中讨论过的任何有价值的事项。",
            "每个计划必须包含至少一个可自动验证的检查步骤",
            "(如: 文件存在、内容匹配预期模式、页面无错误渲染等)。",
            "如果未发现可执行的任务, 不要调用任何工具——仅以简短说明回复原因即可。",
            "",
            "权限: 本轮运行在 `bypassPermissions` 模式下——所有工具调用均自动允许。",
        ]
    } else {
        &[
            "",
            "Goal: identify AT MOST ONE worthwhile task to propose.",
            "Tasks may include enriching or adding content, completing unfinished work,",
            "improving structure or layout, fixing errors, enhancing user experience,",
            "or anything valuable discussed in user chat history.",
            "Each plan must include at least one automatically-checkable verification step",
            "(e.g. file exists, content matches expected pattern, page renders without error).",
            "If nothing actionable is found, do not call any tool — just respond with a short note explaining why.",
            "",
            "Permissions: this turn runs in `bypassPermissions` mode — every tool call is auto-allowed.",
        ]
    };
    lines.extend(intro.iter().map(|line| line.to_string()));
    lines.extend(access);

    // formatChatDigestSection.
    lines.push(String::new());
    if input.digest.is_empty() {
        lines.push(if zh {
            "未找到近期用户对话。请浏览工作区内容以发现有价值的任务。".into()
        } else {
            "No recent user conversations found. Explore the workspace contents to find a worthwhile task.".into()
        });
    } else {
        let head: &[&str] = if zh {
            &[
                "## 近期用户对话",
                "",
                "以下是近期用户-智能体对话的结构化摘要。",
                "这些是用户关注点的主要信号来源。",
                "如需查看某个会话的完整对话, 请使用 sessionId 调用 `always_on_read_chat_history`。",
                "",
            ]
        } else {
            &[
                "## Recent user conversations",
                "",
                "Below is a structured digest of recent user-agent chat sessions.",
                "These are primary signals for what the user cares about.",
                "To see the full conversation of a session, call `always_on_read_chat_history` with its sessionId.",
                "",
            ]
        };
        lines.extend(head.iter().map(|line| line.to_string()));
        for session in input.digest {
            lines.push(format!(
                "- [{}] \"{}\" (sessionId: {})",
                strip_millis(&session.last_modified),
                session.title,
                session.alias
            ));
            for prompt in &session.user_prompts {
                lines.push(format!("  > {}", js_trim(&prompt.replace('\n', " "))));
            }
            lines.push(String::new());
        }
    }

    // formatExistingPlansSection (an empty list contributes no lines).
    lines.push(String::new());
    if !input.existing_plans.is_empty() {
        lines.push(if zh {
            "## 已有 Always-On 计划 (请勿重复这些主题)".into()
        } else {
            "## Existing Always-On plans (do NOT duplicate these topics)".into()
        });
        lines.push(String::new());
        for plan in input.existing_plans {
            lines.push(format!(
                "- [{}] \"{}\" (dedupeKey: {})",
                plan.status, plan.title, plan.dedupe_key
            ));
        }
    }

    let (run_id, created_at) = (input.run_id, input.created_at);
    if zh {
        lines.extend([
            String::new(),
            "如果你发现了一个任务, 请调用 `always_on_discovery_plan` 恰好一次, 提交格式严格的 markdown 计划。".into(),
            "计划结构要求 (自上而下):".into(),
            "  - 一级标题: # <计划标题>".into(),
            "  - 元数据引用块, 首行为 `Always-On Discovery Plan`, 后接键值行:".into(),
            format!("    > id: plan_{run_id}"),
            format!("    > sourceRunId: {run_id}"),
            format!("    > createdAt: {created_at}"),
            format!("    > projectRoot: {root}"),
            "    > dedupeKey: <稳定标识符>".into(),
            "  - 章节按以下顺序排列: ## Summary, ## Rationale, ## Context Signals, ## Proposed Change, ## Execution Steps, ## Verification。".into(),
            "  - Summary 不超过 200 字符, 单段落。".into(),
            "  - Context Signals: 至少一个 `-` 列表项。".into(),
            "  - Execution Steps: 仅使用有序列表 (1., 2., …), 不使用无序列表。".into(),
            "  - Verification: 至少一个 `-` 列表项, 每项必须可机器校验。".into(),
            String::new(),
            "硬性约束:".into(),
            "  - 调用 `always_on_discovery_plan` 超过一次将返回 plan_quota_exhausted。".into(),
            "  - 缺少必要章节、章节顺序错误、或包含模糊 'TODO' 措辞的计划将被拒绝。".into(),
            "  - 不要包含 Risks 或 Rollback 章节。".into(),
        ]);
    } else {
        lines.extend([
            String::new(),
            "If you identify a task, call `always_on_discovery_plan` exactly once with a strictly-formatted markdown plan.".into(),
            "Required plan structure (top to bottom):".into(),
            "  - Level-1 heading: # <plan title>".into(),
            "  - Metadata blockquote, first line `Always-On Discovery Plan`, then keyed lines:".into(),
            format!("    > id: plan_{run_id}"),
            format!("    > sourceRunId: {run_id}"),
            format!("    > createdAt: {created_at}"),
            format!("    > projectRoot: {root}"),
            "    > dedupeKey: <stable identifier>".into(),
            "  - Sections in this exact order: ## Summary, ## Rationale, ## Context Signals, ## Proposed Change, ## Execution Steps, ## Verification.".into(),
            "  - Summary ≤ 200 chars, single paragraph.".into(),
            "  - Context Signals: at least one `-` bullet.".into(),
            "  - Execution Steps: ordered list (1., 2., …) only; no bullets.".into(),
            "  - Verification: at least one `-` bullet, each line must be machine-checkable.".into(),
            String::new(),
            "Hard constraints:".into(),
            "  - Calling `always_on_discovery_plan` more than once returns plan_quota_exhausted.".into(),
            "  - Plans missing or reordering required sections, or containing fuzzy 'TODO' wording, will be rejected.".into(),
            "  - Do not include Risks or Rollback sections.".into(),
        ]);
    }
    lines.join("\n")
}
