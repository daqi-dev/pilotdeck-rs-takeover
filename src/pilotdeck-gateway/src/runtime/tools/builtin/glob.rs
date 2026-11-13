//! `glob` (baseline `tool/builtin/glob.ts` + `filesystem/ripgrepFiles.ts`).
//! The TS tool shells out to the bundled ripgrep with
//! `--files --hidden --no-ignore --glob <pattern> --sort=modified`; this port
//! runs the same bundled engine through the `grep` tool's ripgrep runner, so
//! matching, ordering (equal modification times included) and spawn errors
//! are ripgrep's own.
use super::{
    basename, dirname, is_absolute, object, resolve_workspace_path, schema, string, text,
    tool_error,
};
use crate::runtime::tools::sandbox::Sandbox;
use crate::{
    host::{HostError, modules::capability::ToolDefinition},
    runtime::tools::{ExecutableTool, ToolError, ToolFuture, ToolOutput, ToolRunContext},
};
use pilotdeck_protocol::{JsObject, JsString, JsValue};
use std::path::Path;

const DEFAULT_LIMIT: usize = 1_000;

pub struct GlobTool {
    fields: JsObject,
    sandbox: Sandbox,
}

impl GlobTool {
    pub fn new() -> Self {
        Self {
            fields: definition(),
            sandbox: Sandbox::default(),
        }
    }

    /// Spawn ripgrep through the execution world's sandbox.
    pub fn with_sandbox(mut self, sandbox: Sandbox) -> Self {
        self.sandbox = sandbox;
        self
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolDefinition for GlobTool {
    fn fields(&self) -> &JsObject {
        &self.fields
    }
    fn is_read_only(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
    fn is_concurrency_safe(&self, _input: &JsValue) -> Result<JsValue, HostError> {
        Ok(JsValue::Bool(true))
    }
}

impl ExecutableTool for GlobTool {
    fn execute(&self, input: JsValue, context: ToolRunContext) -> ToolFuture<ToolOutput> {
        let sandbox = self.sandbox;
        Box::pin(async move { execute(&input, &context, sandbox).await })
    }
}

fn definition() -> JsObject {
    object([
        ("name", string("glob")),
        ("aliases", JsValue::Array(vec![string("Glob")])),
        (
            "description",
            string(
                "Fast file pattern matching tool scoped to the workspace.\n\nUsage:\n- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\".\n- Use this tool when you need to find files by name patterns.\n- Provide the optional path parameter to restrict the search to a subdirectory inside the workspace.\n- Returns matching file paths in stable sorted order.\n- Use this tool to narrow down candidate files before reading or editing them.",
            ),
        ),
        ("kind", string("filesystem")),
        (
            "inputSchema",
            JsValue::Object(schema(
                r#"{"type":"object","required":["pattern"],"additionalProperties":false,"properties":{"pattern":{"type":"string","description":"The glob pattern to match files against. May be workspace-relative, path-relative, or an absolute glob that resolves inside the workspace."},"path":{"type":"string","description":"The directory to search in. If not specified, the workspace root will be used. Omit this field to use the default directory. Must resolve to a directory inside the workspace if provided."},"limit":{"type":"integer","description":"Maximum number of file paths to return. This is a PilotDeck-specific output cap; defaults to 1000. Results remain stable and sorted before truncation."}}}"#,
            )),
        ),
        ("maxResultBytes", JsValue::Number(200_000.0)),
    ])
}

async fn execute(
    input: &JsValue,
    context: &ToolRunContext,
    sandbox: Sandbox,
) -> Result<ToolOutput, ToolError> {
    let pattern = input
        .get("pattern")
        .and_then(JsValue::as_string)
        .cloned()
        .ok_or_else(|| tool_error("invalid_tool_input", "pattern must be a string."))?;
    let mut search_path = input
        .get("path")
        .and_then(JsValue::as_string)
        .cloned()
        .unwrap_or_else(|| JsString::from("."));
    let mut search_pattern = pattern.clone();
    if is_absolute(&pattern) {
        let (base, relative) = extract_glob_base_directory(&pattern);
        if !base.is_empty() {
            search_path = base;
            search_pattern = relative;
        }
    }
    let resolved = resolve_workspace_path(&search_path, context, false, false)?;
    let prefix = if resolved.relative.equals_str(".") {
        String::new()
    } else {
        format!("{}/", text(&resolved.relative))
    };
    let files = ripgrep_files(&resolved.absolute, &search_pattern, context, sandbox).await?;
    let limit = input
        .get("limit")
        .and_then(JsValue::as_number)
        .unwrap_or(DEFAULT_LIMIT as f64);
    let selected = slice_limit(&files, limit);
    let truncated = selected.len() < files.len();
    let workspace_files: Vec<String> = selected
        .iter()
        .map(|file| format!("{prefix}{file}"))
        .collect();
    let limit_text = if input.get("limit").is_some() {
        format!(" limit={}", js_number_string(limit))
    } else {
        String::new()
    };
    let mut output_lines = if workspace_files.is_empty() {
        vec!["[No files matched]".to_owned()]
    } else {
        workspace_files.clone()
    };
    output_lines.push(String::new());
    output_lines.push(format!(
        "[glob pagination] returned={} total={} truncated={}{limit_text}",
        workspace_files.len(),
        files.len(),
        if truncated { "true" } else { "false" }
    ));
    if truncated {
        output_lines.push(
            "More files are available. Narrow the pattern/path or call glob again with a higher limit if you need the full list."
                .to_owned(),
        );
    }
    let data_files = JsValue::Array(
        workspace_files
            .into_iter()
            .map(|file| JsValue::String(JsString::from(file)))
            .collect(),
    );
    Ok(ToolOutput {
        content: vec![JsValue::Object(object([
            ("type", string("text")),
            ("text", string(output_lines.join("\n"))),
        ]))],
        supplemental_messages: None,
        data: Some(JsValue::Object(object([
            ("files", data_files),
            ("count", JsValue::Number(files.len() as f64)),
            ("truncated", JsValue::Bool(truncated)),
        ]))),
        metadata: Some(object([("truncated", JsValue::Bool(truncated))])),
    })
}

fn js_number_string(value: f64) -> String {
    crate::js_value::number_string(value)
}

/// JS `files.slice(0, limit)` including negative and non-finite limits.
fn slice_limit(files: &[String], limit: f64) -> Vec<String> {
    if limit.is_nan() {
        return Vec::new();
    }
    let length = files.len() as i64;
    let end = if limit.is_infinite() {
        if limit > 0.0 { length } else { 0 }
    } else {
        let value = limit.trunc();
        // ECMAScript `ToIntegerOrInfinity(-0)` is `+0`; `slice(0, -0)` is
        // empty, not "all files". `-0.5` truncates to `-0` for the same reason.
        if value < 0.0 {
            (length as f64 + value).max(0.0) as i64
        } else {
            value.min(length as f64) as i64
        }
    };
    let start = 0;
    let end = end.clamp(0, length);
    files[start..end as usize].to_vec()
}

/// `ripgrepFiles` (filesystem/ripgrepFiles.ts:52-85): the bundled ripgrep
/// lists the files (`--files --hidden --no-ignore --glob <pattern>
/// --sort=modified .`) and its output order is kept, so equal modification
/// times stay in ripgrep's walk order. Spawn failures map as `runRipgrep`
/// does: a missing `cwd` is the spawn's ENOENT, reported like a missing
/// binary; a `cwd` that is a file is `spawn ENOTDIR`.
async fn ripgrep_files(
    cwd: &JsString,
    pattern: &JsString,
    context: &ToolRunContext,
    sandbox: Sandbox,
) -> Result<Vec<String>, ToolError> {
    let glob = normalize_ripgrep_glob_pattern(&text(pattern), &super::os_path(cwd));
    let args: Vec<String> = [
        "--files",
        "--hidden",
        "--no-ignore",
        "--glob",
        glob.as_str(),
        "--sort=modified",
        ".",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    let stdout = super::grep::run_ripgrep_for("glob", &args, cwd, context, sandbox).await?;
    Ok(super::grep::split_ripgrep_lines(&stdout)
        .iter()
        .map(|line| super::grep::normalize_relative_path(line))
        .filter(|line| !super::grep::is_ignored_path(line))
        .collect())
}

const GLOB_ESCAPE_CHARS: [char; 7] = ['*', '?', '[', ']', '{', '}', '\\'];

/// `normalizeRipgrepGlobPattern`: a backslash is a path separator unless it
/// escapes a glob metacharacter of a literal segment.
fn normalize_ripgrep_glob_pattern(pattern: &str, cwd: &Path) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut normalized = String::new();
    for (index, &character) in chars.iter().enumerate() {
        if character != '\\' {
            normalized.push(character);
            continue;
        }
        let separator = backslash_is_separator(&chars, index, &normalized, cwd);
        normalized.push(if separator { '/' } else { '\\' });
    }
    normalized
}

fn backslash_is_separator(chars: &[char], index: usize, prefix: &str, cwd: &Path) -> bool {
    let Some(&next) = chars.get(index + 1) else {
        return false;
    };
    if !GLOB_ESCAPE_CHARS.contains(&next) {
        return true;
    }
    // `startsGlobstarPathSegment(pattern, index + 1)`.
    if next == '*'
        && chars.get(index + 2) == Some(&'*')
        && matches!(chars.get(index + 3), None | Some('\\') | Some('/'))
    {
        return true;
    }
    let last_segment = prefix.rsplit('/').next().unwrap_or(prefix);
    if has_unescaped_glob_special_chars(last_segment) {
        return true;
    }
    // `staticPrefixIsDirectory(prefix, cwd)`.
    !prefix.is_empty()
        && !has_unescaped_glob_special_chars(prefix)
        && std::fs::metadata(cwd.join(prefix)).is_ok_and(|metadata| metadata.is_dir())
}

fn has_unescaped_glob_special_chars(value: &str) -> bool {
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' | '{' => return true,
            _ => {}
        }
    }
    false
}

fn extract_glob_base_directory(pattern: &JsString) -> (JsString, JsString) {
    let units = pattern.units();
    let Some(index) = units
        .iter()
        .position(|unit| matches!(*unit, 0x2a | 0x3f | 0x5b | 0x7b))
    else {
        return (dirname(pattern), basename(pattern));
    };
    let static_prefix = &units[..index];
    let Some(last_separator) = static_prefix.iter().rposition(|unit| *unit == b'/' as u16) else {
        return (JsString::from(""), pattern.clone());
    };
    let base = JsString::from_utf16(units[..last_separator].to_vec());
    let relative = JsString::from_utf16(units[last_separator + 1..].to_vec());
    let base = if base.is_empty() && last_separator == 0 {
        JsString::from("/")
    } else {
        base
    };
    (base, relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        ["a", "b", "c"]
            .iter()
            .map(|value| (*value).to_owned())
            .collect()
    }

    #[test]
    fn slice_limit_matches_js_negative_zero_and_negative_ranges() {
        let files = files();
        assert!(slice_limit(&files, -0.0).is_empty());
        assert!(slice_limit(&files, -0.5).is_empty());
        assert!(slice_limit(&files, 0.0).is_empty());
        assert_eq!(slice_limit(&files, 2.0), vec!["a", "b"]);
        assert_eq!(slice_limit(&files, -1.0), vec!["a", "b"]);
        assert!(slice_limit(&files, -5.0).is_empty());
        assert!(slice_limit(&files, f64::NAN).is_empty());
        assert_eq!(slice_limit(&files, f64::INFINITY), files);
        assert!(slice_limit(&files, f64::NEG_INFINITY).is_empty());
    }
}
