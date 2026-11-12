//! Port of TS `src/always-on/workspace/WorkspaceApply.ts`
//! (`generateWorkspaceDiff`) and the apply prompt of
//! `src/always-on/runtime/discoveryPrompts.ts` / `discoveryPrompts.zh.ts`
//! (`buildApplyPrompt` / `buildApplyPromptZh`).
//!
//! The diff is a JS string in the baseline: its length cap and slice count
//! UTF-16 code units, so the port keeps the diff as UTF-16 units end to end
//! and the prompt is assembled as a [`JsString`].
use pilotdeck_protocol::JsString;

/// `MAX_INLINE_DIFF_CHARS` (WorkspaceApply.ts:44).
const MAX_INLINE_DIFF_CHARS: usize = 80_000;

/// `WorkspaceDiff` (WorkspaceApply.ts:12-16).
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceDiff {
    pub diff: Vec<u16>,
    pub file_count: i64,
    pub truncated: bool,
}

impl WorkspaceDiff {
    fn empty() -> Self {
        Self {
            diff: Vec::new(),
            file_count: 0,
            truncated: false,
        }
    }
}

/// `runProcess` (WorkspaceApply.ts:20-42): stdin closed at once, stdout and
/// stderr collected; a spawn failure is exit code -1, a signal death too
/// (`code ?? -1`).
async fn run_process(bin: &str, args: &[&str]) -> (i32, Vec<u8>) {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;
    match output {
        Ok(output) => (output.status.code().unwrap_or(-1), output.stdout),
        Err(_) => (-1, Vec::new()),
    }
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    String::from_utf8_lossy(bytes).encode_utf16().collect()
}

fn capped(full: Vec<u16>, file_count: i64) -> WorkspaceDiff {
    if full.len() > MAX_INLINE_DIFF_CHARS {
        return WorkspaceDiff {
            diff: full[..MAX_INLINE_DIFF_CHARS].to_vec(),
            file_count,
            truncated: true,
        };
    }
    WorkspaceDiff {
        diff: full,
        file_count,
        truncated: false,
    }
}

/// `generateWorkspaceDiff` (WorkspaceApply.ts:51-61): `git-worktree` reads the
/// staged diff against HEAD; every other strategy is the POSIX `diff -ruN`
/// of the project root against the workspace.
pub(crate) async fn generate_workspace_diff(
    strategy: &str,
    workspace_cwd: &str,
    project_root: &str,
) -> WorkspaceDiff {
    if strategy == "git-worktree" {
        return git_worktree_diff(workspace_cwd).await;
    }
    snapshot_copy_diff(workspace_cwd, project_root).await
}

/// WorkspaceApply.ts:63-95.
async fn git_worktree_diff(cwd: &str) -> WorkspaceDiff {
    let (add, _) = run_process("git", &["-C", cwd, "add", "-A"]).await;
    if add != 0 {
        return WorkspaceDiff::empty();
    }
    let (stat_code, stat) =
        run_process("git", &["-C", cwd, "diff", "--cached", "HEAD", "--stat"]).await;
    // `(stdout.match(/\n/g) || []).length - 1`, floored at 0 by every return.
    let file_count = if stat_code == 0 {
        stat.iter().filter(|byte| **byte == b'\n').count() as i64 - 1
    } else {
        0
    }
    .max(0);
    let (diff_code, diff) = run_process("git", &["-C", cwd, "diff", "--cached", "HEAD"]).await;
    let text = utf16(&diff);
    if diff_code != 0 || is_blank(&text) {
        return WorkspaceDiff {
            diff: Vec::new(),
            file_count,
            truncated: false,
        };
    }
    capped(text, file_count)
}

/// WorkspaceApply.ts:97-128. `diff` exits 1 when the trees differ; above 1
/// is an error and reads as no diff.
async fn snapshot_copy_diff(cwd: &str, project_root: &str) -> WorkspaceDiff {
    let (code, stdout) = run_process(
        "diff",
        &[
            "-ruN",
            "--exclude=.git",
            "--exclude=node_modules",
            "--exclude=dist",
            "--exclude=.pilotdeck",
            "--exclude=.pilotdeck-always-on",
            project_root,
            cwd,
        ],
    )
    .await;
    if code > 1 {
        return WorkspaceDiff::empty();
    }
    let text = utf16(&stdout);
    // `(fullDiff.match(/^diff /gm) || []).length` — a line start is the
    // string start or the position after any of JS's line terminators.
    let file_count = count_line_starts(&text, "diff ");
    capped(text, file_count)
}

fn is_line_terminator(unit: u16) -> bool {
    matches!(unit, 0x0a | 0x0d | 0x2028 | 0x2029)
}

fn count_line_starts(text: &[u16], prefix: &str) -> i64 {
    let prefix: Vec<u16> = prefix.encode_utf16().collect();
    let mut count = 0;
    for start in 0..text.len() {
        if (start == 0 || is_line_terminator(text[start - 1])) && text[start..].starts_with(&prefix)
        {
            count += 1;
        }
    }
    count
}

/// `!s.trim()` over JS whitespace (WhiteSpace + LineTerminator).
fn is_blank(text: &[u16]) -> bool {
    text.iter().all(|unit| {
        char::from_u32(u32::from(*unit)).is_some_and(|ch| ch.is_whitespace() || *unit == 0xfeff)
    })
}

/// `BuildApplyPromptInput` (discoveryPrompts.ts:236-243). `title`, `id`,
/// `projectName`, `strategy` and `branchName` are the JS `String()` renderings
/// the template literals produce.
pub(crate) struct ApplyPromptInput<'a> {
    pub plan_title: &'a str,
    pub plan_id: &'a str,
    pub workspace_cwd: &'a str,
    pub workspace_strategy: &'a str,
    pub project_name: &'a str,
    pub project_root: &'a str,
    pub diff: &'a WorkspaceDiff,
    pub branch_name: Option<&'a str>,
    pub language: Option<&'a str>,
}

/// `buildApplyPrompt` (discoveryPrompts.ts:245-308) and its zh-CN twin
/// (discoveryPrompts.zh.ts:183-245).
pub(crate) fn build_apply_prompt(input: &ApplyPromptInput<'_>) -> JsString {
    let zh = input.language == Some("zh-CN");
    let mut lines: Vec<Vec<u16>> = Vec::new();
    let mut push = |line: String| lines.push(line.encode_utf16().collect());
    let (project_name, project_root, title, id) = (
        input.project_name,
        input.project_root,
        input.plan_title,
        input.plan_id,
    );
    if zh {
        push(format!("Always-On 应用变更到项目 \"{project_name}\"。"));
        push(String::new());
        push("你的任务是将隔离工作区中的变更合并到项目根目录。".into());
        push(String::new());
        push("不要进入计划模式。".into());
        push("不要创建新计划——直接应用现有变更。".into());
        push(String::new());
        push(format!("计划: \"{title}\" ({id})"));
        push(format!("项目根目录: {project_root}"));
    } else {
        push(format!("Always-On apply for project \"{project_name}\"."));
        push(String::new());
        push(
            "Your job is to merge changes from the isolated workspace into the project root."
                .into(),
        );
        push(String::new());
        push("Do not enter Plan Mode.".into());
        push("Do not create a new plan — apply the existing changes directly.".into());
        push(String::new());
        push(format!("Plan: \"{title}\" ({id})"));
        push(format!("Project root: {project_root}"));
    }
    // `if (plan.workspace?.cwd)` — the cwd is truthy for every cycle that
    // reaches the apply phase.
    if !input.workspace_cwd.is_empty() {
        let (cwd, strategy) = (input.workspace_cwd, input.workspace_strategy);
        push(if zh {
            format!("隔离工作区: {cwd} ({strategy})")
        } else {
            format!("Isolated workspace: {cwd} ({strategy})")
        });
    }
    if let Some(branch) = input.branch_name.filter(|branch| !branch.is_empty()) {
        push(if zh {
            format!("工作区分支: {branch}")
        } else {
            format!("Workspace branch: {branch}")
        });
    }
    let tail: &[&str] = if zh {
        &[
            "",
            "## 合并方式",
            "",
            "根据实际情况选择最佳的合并策略。你可以完全使用 git 和 shell 工具。",
            "常见方式 (根据情况选择):",
            "  - `git merge` / `git merge --no-ff` 如果工作区在命名分支上",
            "  - `git cherry-pick` 针对单个提交",
            "  - `git diff` + `git apply` 基于补丁的应用",
            "  - 使用 Edit/Write 工具直接编辑文件, 适用于精确的小范围变更",
            "",
            "如果遇到冲突, 请智能解决——不要盲目覆盖。",
            "如果无法解决冲突, 保留标准冲突标记 (<<<< / ==== / >>>>)。",
            "",
        ]
    } else {
        &[
            "",
            "## Merge approach",
            "",
            "Choose the best merge strategy based on the situation. You have full access to git and shell tools.",
            "Common approaches (pick whichever fits):",
            "  - `git merge` / `git merge --no-ff` if the workspace is on a named branch",
            "  - `git cherry-pick` for individual commits",
            "  - `git diff` + `git apply` for patch-based application",
            "  - Direct file edits via Edit/Write tools for surgical changes",
            "",
            "If you encounter conflicts, resolve them intelligently — do not blindly overwrite.",
            "If you cannot resolve a conflict, leave standard conflict markers (<<<< / ==== / >>>>).",
            "",
        ]
    };
    for line in tail {
        push((*line).into());
    }
    let diff = input.diff;
    if is_blank(&diff.diff) {
        push(if zh {
            "工作区未检测到差异。无需应用任何变更。".into()
        } else {
            "No differences detected in the workspace. Nothing to apply.".into()
        });
    } else if diff.truncated {
        let count = diff.file_count;
        if zh {
            push(format!("差异较大 ({count} 个文件), 已截断。"));
            push("请从工作区目录读取相关文件进行对比和应用。".into());
            push(String::new());
            push("截断后的差异 (前部内容):".into());
        } else {
            push(format!(
                "The diff is large ({count} files) and has been truncated."
            ));
            push(
                "Read the relevant files from the workspace directory to compare and apply.".into(),
            );
            push(String::new());
            push("Truncated diff (first portion):".into());
        }
        push(String::new());
        lines.push(diff.diff.clone());
    } else {
        let count = diff.file_count;
        push(if zh {
            format!("变更内容 ({count} 个文件):")
        } else {
            format!(
                "Changes ({count} file{}):",
                if count == 1 { "" } else { "s" }
            )
        });
        push(String::new());
        lines.push(diff.diff.clone());
    }
    let mut units = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            units.push(u16::from(b'\n'));
        }
        units.extend_from_slice(line);
    }
    JsString::from_utf16(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_assert_eq;

    fn diff(text: &str, file_count: i64, truncated: bool) -> WorkspaceDiff {
        WorkspaceDiff {
            diff: text.encode_utf16().collect(),
            file_count,
            truncated,
        }
    }

    fn render(input: &ApplyPromptInput<'_>) -> String {
        build_apply_prompt(input).to_utf8().expect("utf-8")
    }

    fn input<'a>(diff: &'a WorkspaceDiff, language: Option<&'a str>) -> ApplyPromptInput<'a> {
        ApplyPromptInput {
            plan_title: "T",
            plan_id: "c1",
            workspace_cwd: "/ws",
            workspace_strategy: "snapshot-copy",
            project_name: "alpha",
            project_root: "/p",
            diff,
            branch_name: None,
            language,
        }
    }

    // discoveryPrompts.ts:285-305: the empty, truncated and plural shapes.
    #[test]
    fn apply_prompt_tail_shapes() {
        let empty = diff(" \n", 0, false);
        negative_assert_eq!(
            render(&input(&empty, None))
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .to_string(),
            "No differences detected in the workspace. Nothing to apply.".to_string(),
            "empty diff tail"
        );
        let one = diff("diff a\n", 1, false);
        assert!(render(&input(&one, None)).contains("\nChanges (1 file):\n\ndiff a\n"));
        let two = diff("diff a\ndiff b\n", 2, false);
        assert!(render(&input(&two, None)).contains("\nChanges (2 files):\n\n"));
        let cut = diff("x", 9, true);
        assert!(render(&input(&cut, None)).ends_with(
            "The diff is large (9 files) and has been truncated.\nRead the relevant files from the workspace directory to compare and apply.\n\nTruncated diff (first portion):\n\nx"
        ));
        assert!(render(&input(&one, Some("zh-CN"))).contains("\n变更内容 (1 个文件):\n\ndiff a\n"));
    }

    #[test]
    fn counts_diff_line_starts() {
        let text: Vec<u16> = "diff a\n--- x\ndiff b\nxdiff c\r\ndiff d"
            .encode_utf16()
            .collect();
        negative_assert_eq!(count_line_starts(&text, "diff "), 3, "diff headers");
    }
}
