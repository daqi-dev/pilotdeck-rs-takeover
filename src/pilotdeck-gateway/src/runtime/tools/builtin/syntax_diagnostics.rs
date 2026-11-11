//! Post-write syntax diagnostics (baseline
//! `tool/builtin/filesystem/syntaxDiagnostics.ts`): after `write_file` /
//! `edit_file` store a file, its new content is checked by extension and up
//! to three problems are appended to the tool's success text. `execute_code`
//! uses the Python checker to reject a script before running it.
//!
//! Ported: JSON (V8 `JSON.parse` messages), Python (`python3 -c <checker>`),
//! bash (`bash -n -s`), YAML and Markdown (front matter, code fences).
//!
//! Not ported — these report nothing here while the baseline reports parse
//! errors for malformed content: JavaScript / TypeScript (the baseline runs
//! the `typescript` compiler's parser), HTML (`parse5` error codes) and CSV /
//! TSV (`csv-parse`). Well-formed files of these kinds produce no diagnostics
//! on either side.
#![cfg(unix)]
use crate::{js_value, process_group::GroupGuard, runtime::tools::js_space};
use pilotdeck_protocol::{JsString, JsValue, parse_json};
use regex::Regex;
use std::{process::Stdio, sync::LazyLock, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

const MAX_DIAGNOSTICS: usize = 3;
const CHECKER_TIMEOUT: Duration = Duration::from_millis(2_000);
const MAX_CHECKER_OUTPUT_UNITS: usize = 16_384;

/// Python source the baseline passes to `python3 -c` to compile-check a
/// file: `PYTHON_SYNTAX_CHECKER`, verbatim from
/// `resources/PilotDeck/src/tool/builtin/filesystem/syntaxDiagnostics.ts:51-74`
/// (the template literal after `.trim()`).
const PYTHON_SYNTAX_CHECKER: &str = r#"import json
import sys

filename = sys.argv[1] if len(sys.argv) > 1 else "<stdin>"
source = sys.stdin.read()

try:
    compile(source, filename, "exec")
except SyntaxError as exc:
    print(json.dumps({
        "line": exc.lineno or 1,
        "column": exc.offset or 1,
        "message": exc.msg or "Invalid Python syntax."
    }))
    sys.exit(1)
except ValueError as exc:
    print(json.dumps({
        "line": 1,
        "column": 1,
        "message": str(exc)
    }))
    sys.exit(1)"#;

pub(super) struct SyntaxDiagnostic {
    pub line: f64,
    pub column: f64,
    pub message: String,
}

fn diagnostic(line: usize, column: usize, message: String) -> SyntaxDiagnostic {
    SyntaxDiagnostic {
        line: line as f64,
        column: column as f64,
        message,
    }
}

/// `formatSyntaxDiagnostics(filePath, content)`: the block appended after a
/// blank line to the success text, or `None` when nothing was found.
pub(super) async fn format_syntax_diagnostics(file_path: &str, content: &str) -> Option<String> {
    let diagnostics = collect(file_path, content).await;
    if diagnostics.is_empty() {
        return None;
    }
    let mut lines = vec!["Syntax issues detected:".to_owned()];
    lines.extend(diagnostics.iter().take(MAX_DIAGNOSTICS).map(|diagnostic| {
        format!(
            "- L{}:{} error: {}",
            js_value::number_string(diagnostic.line),
            js_value::number_string(diagnostic.column),
            diagnostic.message
        )
    }));
    Some(lines.join("\n"))
}

/// `syntaxDiagnostics ? `${successText}\n\n${syntaxDiagnostics}` : successText`
/// on the first text block of a write tool's output.
pub(super) async fn append_to_success_text(
    output: &mut crate::runtime::tools::ToolOutput,
    relative_path: &str,
    content: &str,
) {
    let Some(diagnostics) = format_syntax_diagnostics(relative_path, content).await else {
        return;
    };
    if let Some(JsValue::Object(block)) = output.content.first_mut()
        && let Some(text) = block.get("text").and_then(JsValue::as_string).cloned()
    {
        let mut units = text.units().to_vec();
        units.extend(format!("\n\n{diagnostics}").encode_utf16());
        block.insert("text".into(), JsValue::String(JsString::from_utf16(units)));
    }
}

/// `path.extname(filePath).toLowerCase()` and
/// `path.basename(filePath).toLowerCase()`.
fn extension_and_name(file_path: &str) -> (String, String) {
    let name = file_path.rsplit('/').next().unwrap_or(file_path);
    let extension = match name.rfind('.') {
        // A leading dot starts a dotfile name, not an extension.
        Some(index) if index > 0 && !name[..index].chars().all(|c| c == '.') => &name[index..],
        _ => "",
    };
    (extension.to_lowercase(), name.to_lowercase())
}

async fn collect(file_path: &str, content: &str) -> Vec<SyntaxDiagnostic> {
    let (extension, name) = extension_and_name(file_path);
    match extension.as_str() {
        ".json" => json_diagnostics(content),
        ".py" | ".pyw" => python_diagnostics(file_path, content)
            .await
            .into_iter()
            .collect(),
        ".bash" | ".sh" => bash_diagnostics(content).await,
        ".yaml" | ".yml" => yaml_diagnostics(content, 0, ""),
        ".md" | ".markdown" => {
            let mut diagnostics = markdown_frontmatter_diagnostics(content);
            diagnostics.extend(markdown_fence_diagnostics(content));
            diagnostics
        }
        _ if matches!(
            name.as_str(),
            ".bash_login" | ".bash_profile" | ".bashrc" | ".profile"
        ) =>
        {
            bash_diagnostics(content).await
        }
        // `.cjs .cts .js .jsx .mjs .mts .ts .tsx`, `.html .htm`, `.csv .tsv`:
        // see the module docs.
        _ => Vec::new(),
    }
}

/// `trimDiagnosticMessage`: whitespace runs collapse to one space, and a
/// message longer than 220 UTF-16 units is cut to 217 plus `...`.
pub(super) fn trim_diagnostic_message(message: &str) -> String {
    let mut compact = String::new();
    let mut pending_space = false;
    for c in message.chars() {
        if js_space(c) {
            pending_space = true;
            continue;
        }
        if pending_space && !compact.is_empty() {
            compact.push(' ');
        }
        pending_space = false;
        compact.push(c);
    }
    let units: Vec<u16> = compact.encode_utf16().collect();
    if units.len() <= 220 {
        return compact;
    }
    let head = String::from_utf16_lossy(&units[..217]);
    format!("{}...", head.trim_end_matches(js_space))
}

/// `collectJsonDiagnostics`: V8's message, located by its own
/// `(line N column M)` suffix, else by its `position N`, else at the end.
fn json_diagnostics(content: &str) -> Vec<SyntaxDiagnostic> {
    static POSITION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bposition\s+(\d+)\b").expect("valid pattern"));
    static LINE_COLUMN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bline\s+(\d+)\s+column\s+(\d+)\b").expect("valid pattern")
    });
    let Some(message) = pilotdeck_protocol::json_syntax::parse_error(content) else {
        return Vec::new();
    };
    let number = |text: &str| text.parse::<usize>().unwrap_or(usize::MAX);
    let (line, column) = match LINE_COLUMN.captures(&message) {
        Some(found) => (number(&found[1]), number(&found[2])),
        None => {
            let units: Vec<u16> = content.encode_utf16().collect();
            let position = POSITION
                .captures(&message)
                .map_or(units.len(), |found| number(&found[1]))
                .min(units.len());
            // `positionToLineColumn`.
            let (mut line, mut column) = (1, 1);
            for unit in &units[..position] {
                if *unit == '\n' as u16 {
                    line += 1;
                    column = 1;
                } else {
                    column += 1;
                }
            }
            (line, column)
        }
    };
    vec![diagnostic(line, column, trim_diagnostic_message(&message))]
}

struct CheckerResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `runChecker`: the checker runs in the gateway's own cwd and environment
/// with `input` on stdin. `None` when it cannot be started or exceeds the two
/// second budget; output is cut to 16384 UTF-16 units.
async fn run_checker(program: &str, args: &[&str], input: &str) -> Option<CheckerResult> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    pilotdeck_child_signals::install_child_detach_session(command.as_std_mut());
    pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());
    let mut child = command.spawn().ok()?;
    let mut guard = GroupGuard(
        child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
    );
    let mut stdin = child.stdin.take();
    let run = async {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        drop(stdin.take());
        child.wait_with_output().await
    };
    // A timeout drops the run with the guard armed: the group is killed.
    let output = tokio::time::timeout(CHECKER_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    guard.disarm();
    let cut = |bytes: &[u8]| {
        let units: Vec<u16> = String::from_utf8_lossy(bytes)
            .encode_utf16()
            .take(MAX_CHECKER_OUTPUT_UNITS)
            .collect();
        String::from_utf16_lossy(&units)
    };
    Some(CheckerResult {
        code: output.status.code(),
        stdout: cut(&output.stdout),
        stderr: cut(&output.stderr),
    })
}

/// `collectPythonSyntaxDiagnostics(filePath, content)`: `python3 -c <checker>
/// <filePath>`. A checker that exits 0 or prints something other than a JSON
/// object reports nothing.
pub(super) async fn python_diagnostics(file_path: &str, content: &str) -> Option<SyntaxDiagnostic> {
    let result = run_checker(
        "python3",
        &["-c", PYTHON_SYNTAX_CHECKER, file_path],
        content,
    )
    .await?;
    if result.code == Some(0) {
        return None;
    }
    let parsed = parse_json(
        result
            .stdout
            .trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r')),
    )
    .ok()?;
    if !matches!(parsed, JsValue::Object(_) | JsValue::Array(_)) {
        return None;
    }
    // `positiveInteger(value) ?? 1`.
    let positive = |key: &str| {
        parsed
            .get(key)
            .and_then(JsValue::as_number)
            .filter(|value| value.is_finite())
            .map(f64::floor)
            .filter(|value| *value > 0.0)
            .unwrap_or(1.0)
    };
    let message = parsed
        .get("message")
        .and_then(JsValue::as_string)
        .filter(|message| !message.is_empty())
        .map_or_else(
            || "Invalid Python syntax.".to_owned(),
            |message| String::from_utf16_lossy(message.units()),
        );
    Some(SyntaxDiagnostic {
        line: positive("line"),
        column: positive("column"),
        message: trim_diagnostic_message(&message),
    })
}

/// `collectBashDiagnostics` / `diagnosticFromBashStderr`.
async fn bash_diagnostics(content: &str) -> Vec<SyntaxDiagnostic> {
    static SYNTAX_ERROR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)syntax error").expect("valid pattern"));
    static LINE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bline\s+(\d+)\b").expect("valid pattern"));
    static BASH_PREFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^bash:\s*").expect("valid pattern"));
    static LINE_PREFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^line\s+\d+:\s*").expect("valid pattern"));
    let Some(result) = run_checker("bash", &["-n", "-s"], content).await else {
        return Vec::new();
    };
    if result.code == Some(0) {
        return Vec::new();
    }
    let stderr = result.stderr.trim_matches(js_space);
    if stderr.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = stderr
        .split('\n')
        .map(|line| line.trim_matches(js_space))
        .filter(|line| !line.is_empty())
        .collect();
    let primary = lines
        .iter()
        .rev()
        .find(|line| SYNTAX_ERROR.is_match(line))
        .or(lines.last())
        .copied()
        .unwrap_or(stderr);
    let line = LINE
        .captures(primary)
        .or_else(|| LINE.captures(stderr))
        .and_then(|found| found[1].parse::<usize>().ok())
        .unwrap_or(1);
    let message = BASH_PREFIX.replace(primary, "");
    let message = LINE_PREFIX.replace(&message, "");
    vec![diagnostic(line, 1, trim_diagnostic_message(&message))]
}

/// `collectYamlDiagnostics` (and the front-matter variant, which shifts the
/// line by one for the opening `---` and prefixes the message).
fn yaml_diagnostics(content: &str, line_offset: usize, prefix: &str) -> Vec<SyntaxDiagnostic> {
    let document = pilotdeck_yaml::parse_document(
        &JsString::from(content),
        pilotdeck_yaml::Options {
            pretty_errors: false,
            ..Default::default()
        },
    );
    document
        .errors
        .iter()
        .map(|error| {
            let position = document.line_counter.line_pos(error.pos[0]);
            diagnostic(
                position.line + line_offset,
                position.col,
                trim_diagnostic_message(&format!(
                    "{prefix}{}",
                    String::from_utf16_lossy(error.message.units())
                )),
            )
        })
        .collect()
}

/// `content.split(/\r?\n/u)`.
fn split_lines(content: &str) -> Vec<&str> {
    content
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            // The `\r` belongs to the separator, so the last piece keeps it.
            if index + 1 < content.split('\n').count() {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                line
            }
        })
        .collect()
}

/// `collectMarkdownFrontmatterDiagnostics`.
fn markdown_frontmatter_diagnostics(content: &str) -> Vec<SyntaxDiagnostic> {
    static KEY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+\s*:").expect("valid pattern"));
    let lines = split_lines(content);
    let trimmed = |line: &str| line.trim_matches(js_space).to_owned();
    if lines.first().map(|line| trimmed(line)).as_deref() != Some("---") {
        return Vec::new();
    }
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| matches!(trimmed(line).as_str(), "---" | "..."))
        .map(|(index, _)| index);
    let Some(closing) = closing else {
        let looks_like_frontmatter = lines
            .iter()
            .take(40)
            .skip(1)
            .any(|line| KEY.is_match(&trimmed(line)));
        if !looks_like_frontmatter {
            return Vec::new();
        }
        return vec![diagnostic(
            1,
            1,
            "YAML frontmatter is opened with --- but no closing delimiter was found.".into(),
        )];
    };
    let frontmatter = lines[1..closing].join("\n");
    if frontmatter.chars().all(js_space) {
        return Vec::new();
    }
    yaml_diagnostics(&frontmatter, 1, "YAML frontmatter: ")
}

/// `collectMarkdownFenceDiagnostics`: the first fence that never closes.
fn markdown_fence_diagnostics(content: &str) -> Vec<SyntaxDiagnostic> {
    static OPEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^( {0,3})(`{3,}|~{3,})(.*)$").expect("valid pattern"));
    static CLOSE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^( {0,3})(`{3,}|~{3,})\s*$").expect("valid pattern"));
    // (marker, length, line, column)
    let mut open: Option<(char, usize, usize, usize)> = None;
    for (index, line) in split_lines(content).into_iter().enumerate() {
        match open {
            None => {
                let Some(found) = OPEN.captures(line) else {
                    continue;
                };
                let fence = &found[2];
                if fence.starts_with('`') && found[3].contains('`') {
                    continue;
                }
                open = Some((
                    fence.chars().next().unwrap_or('`'),
                    fence.len(),
                    index + 1,
                    found[1].len() + 1,
                ));
            }
            Some((marker, length, _, _)) => {
                if let Some(found) = CLOSE.captures(line)
                    && found[2].starts_with(marker)
                    && found[2].len() >= length
                {
                    open = None;
                }
            }
        }
    }
    match open {
        Some((_, _, line, column)) => vec![diagnostic(
            line,
            column,
            "Unclosed Markdown code fence.".into(),
        )],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_and_name_follow_node_path() {
        assert_eq!(extension_and_name("d/a.JSON").0, ".json");
        assert_eq!(extension_and_name("a.tar.gz").0, ".gz");
        assert_eq!(extension_and_name(".bashrc"), ("".into(), ".bashrc".into()));
        assert_eq!(extension_and_name("d/Makefile").0, "");
    }

    #[test]
    fn diagnostic_messages_are_compacted_and_capped() {
        assert_eq!(trim_diagnostic_message("  a \n\t b  "), "a b");
        let trimmed = trim_diagnostic_message(&"x".repeat(230));
        assert_eq!(trimmed.len(), 220);
        assert!(trimmed.ends_with("..."));
    }

    #[test]
    fn markdown_fences_and_front_matter() {
        assert!(markdown_fence_diagnostics("```js\nx\n```\n").is_empty());
        let open = markdown_fence_diagnostics("# T\n\n  ~~~\nx\n```\n");
        assert_eq!((open[0].line, open[0].column), (3.0, 3.0));
        assert_eq!(open[0].message, "Unclosed Markdown code fence.");
        // An inline run of backticks after the fence is not a fence.
        assert!(markdown_fence_diagnostics("``` a ` b\n").is_empty());
        let unclosed = markdown_frontmatter_diagnostics("---\ntitle: x\nbody\n");
        assert_eq!(
            unclosed[0].message,
            "YAML frontmatter is opened with --- but no closing delimiter was found."
        );
        assert!(markdown_frontmatter_diagnostics("---\njust prose\n").is_empty());
        assert!(markdown_frontmatter_diagnostics("---\ntitle: x\n---\nbody\n").is_empty());
    }

    #[test]
    fn json_errors_carry_v8_locations() {
        assert!(json_diagnostics("{\"a\": 1}\n").is_empty());
        let bad = json_diagnostics("{\"a\": 1,}\n");
        assert_eq!((bad[0].line, bad[0].column), (1.0, 9.0));
    }
}
