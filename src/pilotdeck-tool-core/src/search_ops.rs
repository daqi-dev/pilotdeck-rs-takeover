//! Deterministic bounded workspace search (`glob` and `grep`)
//! enforced by [`crate::fs_guard::WorkspaceBoundary`].

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};

use crate::fs_guard::{BoundaryError, BoundaryErrorKind, WorkspaceBoundary};

pub const GLOB_TOOL_NAME: &str = "glob";
pub const GREP_TOOL_NAME: &str = "grep";

pub const DEFAULT_MAX_GLOB_RESULTS: usize = 1_000;
pub const MAX_GLOB_RESULTS_CAP: usize = 10_000;
pub const DEFAULT_MAX_GREP_MATCHES: usize = 250;
pub const MAX_GREP_MATCHES_CAP: usize = 5_000;
pub const DEFAULT_MAX_GREP_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_GREP_LINE_BYTES: usize = 2_048;

const MAX_WALK_DEPTH: usize = 64;
const MAX_SCANNED_ENTRIES: usize = 100_000;
const MAX_PATTERN_BYTES: usize = 4_096;
const MAX_BRACE_DEPTH: usize = 16;
const MAX_BRACE_EXPANSIONS: usize = 64;

const GLOB_DESCRIPTION: &str = "Deterministic bounded file pattern matching inside the current workspace.\n\nUsage:\n- Supports `*`, `**`, `?`, `{a,b}`, and `[...]` character classes.\n- Rejects escaping symlinks and `..` traversal.\n- Returns sorted workspace-relative POSIX paths capped by `limit`.";

const GLOB_INPUT_SCHEMA: &str = r#"{"type":"object","required":["pattern"],"additionalProperties":false,"properties":{"pattern":{"type":"string","minLength":1,"description":"Glob pattern to match files against (supports *, **, ?, {a,b})."},"path":{"type":"string","minLength":1,"description":"Optional directory inside the workspace to search in (defaults to workspace root)."},"limit":{"type":"integer","minimum":1,"description":"Maximum number of matched relative file paths to return (defaults to 1000)."}}}"#;

const GREP_DESCRIPTION: &str = "Deterministic bounded substring/line search across UTF-8 workspace files.\n\nUsage:\n- Searches workspace-contained text files, skipping binary/NUL files and escaping symlinks.\n- Supports optional `glob` file filter, `case_insensitive`, and `max_matches` cap.\n- Returns deterministic ordered matches with file path, 1-based lineNumber, and line preview.";

const GREP_INPUT_SCHEMA: &str = r#"{"type":"object","required":["pattern"],"additionalProperties":false,"properties":{"pattern":{"type":"string","minLength":1,"description":"Non-empty substring pattern to search for in line contents."},"path":{"type":"string","minLength":1,"description":"Optional file or directory inside the workspace to search in (defaults to workspace root)."},"glob":{"type":"string","minLength":1,"description":"Optional glob filter for candidate files (e.g. '*.rs' or 'src/**/*.{rs,toml}')."},"case_insensitive":{"type":"boolean","default":false,"description":"When true, perform case-insensitive substring matching."},"max_matches":{"type":"integer","minimum":1,"description":"Maximum number of line matches to return (defaults to 250)."}}}"#;

/// Machine-readable error category for search operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SearchOpErrorKind {
    Boundary(BoundaryErrorKind),
    InvalidInput,
    InvalidPattern,
    NotFound,
    NotADirectory,
    Io,
}

/// Structured error returned by [`glob`] and [`grep`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOpError {
    pub kind: SearchOpErrorKind,
    pub code: String,
    pub message: String,
}

impl SearchOpError {
    pub fn new(
        kind: SearchOpErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
        }
    }
}

impl From<BoundaryError> for SearchOpError {
    fn from(err: BoundaryError) -> Self {
        let code = match err.kind {
            BoundaryErrorKind::OutsideWorkspace => "path_not_allowed",
            BoundaryErrorKind::SymlinkEscape => "symlink_escape",
            BoundaryErrorKind::InvalidPath | BoundaryErrorKind::InvalidRoot => "invalid_input",
            BoundaryErrorKind::NotFound => "file_not_found",
            BoundaryErrorKind::NotADirectory => "file_conflict",
            BoundaryErrorKind::Io => "tool_execution_failed",
        };
        let kind = match err.kind {
            BoundaryErrorKind::NotFound => SearchOpErrorKind::NotFound,
            BoundaryErrorKind::NotADirectory => SearchOpErrorKind::NotADirectory,
            other => SearchOpErrorKind::Boundary(other),
        };
        Self::new(kind, code, err.message)
    }
}

impl fmt::Display for SearchOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SearchOpError {}

fn object<const N: usize>(entries: [(&str, JsValue); N]) -> JsObject {
    let mut result = JsObject::new();
    for (key, value) in entries {
        result.insert(key.into(), value);
    }
    result
}

fn string(value: impl Into<JsString>) -> JsValue {
    JsValue::String(value.into())
}

fn schema(json: &str) -> JsObject {
    match parse_json(json).as_ref() {
        Ok(JsValue::Object(object)) => object.clone(),
        _ => panic!("builtin search_ops input schema must be a valid JSON object"),
    }
}

/// Return the canonical `glob` tool definition.
pub fn glob_definition() -> JsObject {
    object([
        ("name", string(GLOB_TOOL_NAME)),
        ("aliases", JsValue::Array(vec![string("Glob")])),
        ("description", string(GLOB_DESCRIPTION)),
        ("kind", string("filesystem")),
        ("inputSchema", JsValue::Object(schema(GLOB_INPUT_SCHEMA))),
    ])
}

/// Return the canonical `grep` tool definition.
pub fn grep_definition() -> JsObject {
    object([
        ("name", string(GREP_TOOL_NAME)),
        ("aliases", JsValue::Array(vec![string("Grep")])),
        ("description", string(GREP_DESCRIPTION)),
        ("kind", string("filesystem")),
        ("inputSchema", JsValue::Object(schema(GREP_INPUT_SCHEMA))),
    ])
}

/// Parameters for [`glob`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub limit: usize,
}

impl GlobRequest {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            path: None,
            limit: DEFAULT_MAX_GLOB_RESULTS,
        }
    }
}

/// Structured output produced by [`glob`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobResult {
    pub files: Vec<String>,
    pub count: usize,
    pub truncated: bool,
}

impl GlobResult {
    pub fn to_js_value(&self) -> JsValue {
        JsValue::Object(object([
            (
                "files",
                JsValue::Array(self.files.iter().map(|p| string(p.as_str())).collect()),
            ),
            ("count", JsValue::Number(self.count as f64)),
            ("truncated", JsValue::Bool(self.truncated)),
        ]))
    }
}

/// Decode a validated `glob` JSON input object into a [`GlobRequest`].
pub fn parse_glob_request(input: &JsValue) -> Result<GlobRequest, SearchOpError> {
    let obj = input.as_object().ok_or_else(|| {
        SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            "glob input must be a JSON object",
        )
    })?;
    let pattern = required_utf8_string(obj, "pattern")?;
    let path = optional_utf8_string(obj, "path")?;
    let limit = match obj.get("limit") {
        None | Some(JsValue::Null | JsValue::Undefined) => DEFAULT_MAX_GLOB_RESULTS,
        Some(val) => parse_positive_usize(val, "limit", 1)?.min(MAX_GLOB_RESULTS_CAP),
    };
    Ok(GlobRequest {
        pattern,
        path,
        limit,
    })
}

/// Match workspace-contained files against `request.pattern` and return sorted relative POSIX paths.
pub fn glob(
    boundary: &WorkspaceBoundary,
    request: &GlobRequest,
) -> Result<GlobResult, SearchOpError> {
    if request.limit == 0 {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            "limit must be >= 1",
        ));
    }
    let cap = request.limit.min(MAX_GLOB_RESULTS_CAP);
    let matcher = GlobMatcher::compile(&request.pattern)?;
    let root = boundary.resolve_search_root(request.path.as_deref())?;
    let root_meta = fs::metadata(&root.canonical_path).map_err(|err| {
        SearchOpError::new(
            SearchOpErrorKind::Io,
            "tool_execution_failed",
            format!("cannot inspect search root '{}': {err}", root.relative_path),
        )
    })?;
    if !root_meta.is_dir() {
        return Err(SearchOpError::new(
            SearchOpErrorKind::NotADirectory,
            "file_conflict",
            format!("search path '{}' is not a directory", root.relative_path),
        ));
    }

    let walk = collect_workspace_files(boundary, &root.canonical_path)?;
    let mut matched = Vec::new();
    for file in walk.files {
        if matcher.matches(&file.search_rel) {
            matched.push(file.workspace_rel);
        }
    }
    matched.sort();
    matched.dedup();

    let total_found = matched.len();
    let cap_truncated = total_found > cap;
    if cap_truncated {
        matched.truncate(cap);
    }

    Ok(GlobResult {
        count: total_found,
        files: matched,
        truncated: cap_truncated || walk.scan_truncated,
    })
}

/// Parameters for [`grep`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepRequest {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub case_insensitive: bool,
    pub max_matches: usize,
    pub max_file_bytes: usize,
    pub max_line_bytes: usize,
}

impl GrepRequest {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            path: None,
            glob: None,
            case_insensitive: false,
            max_matches: DEFAULT_MAX_GREP_MATCHES,
            max_file_bytes: DEFAULT_MAX_GREP_FILE_BYTES,
            max_line_bytes: DEFAULT_MAX_GREP_LINE_BYTES,
        }
    }
}

/// One deterministic line match returned by [`grep`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

impl GrepMatch {
    pub fn to_js_value(&self) -> JsValue {
        JsValue::Object(object([
            ("path", string(self.path.as_str())),
            ("lineNumber", JsValue::Number(self.line_number as f64)),
            ("line", string(self.line.as_str())),
        ]))
    }
}

/// Structured output produced by [`grep`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub files: Vec<String>,
    pub count: usize,
    pub truncated: bool,
}

impl GrepResult {
    pub fn to_js_value(&self) -> JsValue {
        JsValue::Object(object([
            (
                "matches",
                JsValue::Array(self.matches.iter().map(GrepMatch::to_js_value).collect()),
            ),
            (
                "files",
                JsValue::Array(self.files.iter().map(|f| string(f.as_str())).collect()),
            ),
            ("count", JsValue::Number(self.count as f64)),
            ("truncated", JsValue::Bool(self.truncated)),
        ]))
    }
}

/// Decode a validated `grep` JSON input object into a [`GrepRequest`].
pub fn parse_grep_request(input: &JsValue) -> Result<GrepRequest, SearchOpError> {
    let obj = input.as_object().ok_or_else(|| {
        SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            "grep input must be a JSON object",
        )
    })?;
    let pattern = required_utf8_string(obj, "pattern")?;
    let path = optional_utf8_string(obj, "path")?;
    let glob = optional_utf8_string(obj, "glob")?;
    let case_insensitive = obj
        .get("case_insensitive")
        .and_then(JsValue::as_bool)
        .unwrap_or(false);
    let max_matches = match obj.get("max_matches") {
        None | Some(JsValue::Null | JsValue::Undefined) => DEFAULT_MAX_GREP_MATCHES,
        Some(val) => parse_positive_usize(val, "max_matches", 1)?.min(MAX_GREP_MATCHES_CAP),
    };
    Ok(GrepRequest {
        pattern,
        path,
        glob,
        case_insensitive,
        max_matches,
        max_file_bytes: DEFAULT_MAX_GREP_FILE_BYTES,
        max_line_bytes: DEFAULT_MAX_GREP_LINE_BYTES,
    })
}

/// Search bounded UTF-8 text files within `boundary` in deterministic sorted path order.
pub fn grep(
    boundary: &WorkspaceBoundary,
    request: &GrepRequest,
) -> Result<GrepResult, SearchOpError> {
    if request.pattern.is_empty() || request.pattern.contains('\0') {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            "pattern must be a non-empty string without NUL bytes",
        ));
    }
    if request.max_matches == 0 {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            "max_matches must be >= 1",
        ));
    }

    let glob_filter = request
        .glob
        .as_deref()
        .map(GlobMatcher::compile)
        .transpose()?;

    let root = boundary.resolve_search_root(request.path.as_deref())?;
    let root_meta = fs::metadata(&root.canonical_path).map_err(|err| {
        SearchOpError::new(
            SearchOpErrorKind::Io,
            "tool_execution_failed",
            format!("cannot inspect search root '{}': {err}", root.relative_path),
        )
    })?;

    let (mut candidates, scan_truncated) = if root_meta.is_file() {
        let name = root
            .canonical_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.relative_path.clone());
        (
            vec![WalkedFile {
                canonical_path: root.canonical_path,
                workspace_rel: root.relative_path,
                search_rel: name,
            }],
            false,
        )
    } else if root_meta.is_dir() {
        let walk = collect_workspace_files(boundary, &root.canonical_path)?;
        (walk.files, walk.scan_truncated)
    } else {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "file_conflict",
            format!(
                "search target '{}' is neither a regular file nor a directory",
                root.relative_path
            ),
        ));
    };

    candidates.sort_by(|a, b| a.workspace_rel.cmp(&b.workspace_rel));
    candidates.dedup_by(|a, b| a.workspace_rel == b.workspace_rel);

    let cap = request.max_matches.min(MAX_GREP_MATCHES_CAP);
    let pattern_lower = request.case_insensitive.then(|| {
        if request.pattern.is_ascii() {
            String::new()
        } else {
            request.pattern.to_lowercase()
        }
    });

    let mut matches = Vec::new();
    let mut matched_files = BTreeSet::new();
    let mut cap_truncated = false;

    'files: for candidate in candidates {
        if let Some(filter) = &glob_filter
            && !filter.matches_filter(&candidate.search_rel)
        {
            continue;
        }
        let Ok(meta) = fs::metadata(&candidate.canonical_path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > request.max_file_bytes as u64 {
            continue;
        }
        let Ok(bytes) = fs::read(&candidate.canonical_path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);

        for (idx, line) in split_search_lines(text).into_iter().enumerate() {
            let hit = if request.case_insensitive {
                if request.pattern.is_ascii() {
                    contains_ascii_ignore_case(line, &request.pattern)
                } else {
                    line.to_lowercase()
                        .contains(pattern_lower.as_deref().unwrap_or_default())
                }
            } else {
                line.contains(&request.pattern)
            };
            if hit {
                if matches.len() >= cap {
                    cap_truncated = true;
                    break 'files;
                }
                matched_files.insert(candidate.workspace_rel.clone());
                matches.push(GrepMatch {
                    path: candidate.workspace_rel.clone(),
                    line_number: idx + 1,
                    line: truncate_line_preview(line, request.max_line_bytes),
                });
            }
        }
    }

    let count = matches.len();
    Ok(GrepResult {
        matches,
        files: matched_files.into_iter().collect(),
        count,
        truncated: cap_truncated || scan_truncated,
    })
}

#[derive(Clone, Debug)]
struct WalkedFile {
    canonical_path: PathBuf,
    workspace_rel: String,
    search_rel: String,
}

struct WalkCollect {
    files: Vec<WalkedFile>,
    scan_truncated: bool,
}

fn collect_workspace_files(
    boundary: &WorkspaceBoundary,
    search_root_canonical: &Path,
) -> Result<WalkCollect, SearchOpError> {
    let mut files = Vec::new();
    let mut visited_dirs = BTreeSet::new();
    visited_dirs.insert(search_root_canonical.to_path_buf());

    let mut stack: Vec<(PathBuf, String, usize)> =
        vec![(search_root_canonical.to_path_buf(), String::new(), 0)];
    let mut scanned = 0usize;
    let mut scan_truncated = false;

    'walk: while let Some((dir_path, search_rel_prefix, depth)) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir_path) else {
            continue;
        };
        let mut children: Vec<(String, PathBuf)> = Vec::new();
        for entry_res in read_dir {
            let Ok(entry) = entry_res else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_ignored_dir_name(&name) {
                continue;
            }
            children.push((name, entry.path()));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));

        let mut subdirs = Vec::new();
        for (name, path) in children {
            scanned += 1;
            if scanned > MAX_SCANNED_ENTRIES {
                scan_truncated = true;
                break 'walk;
            }
            let Some((resolved, is_dir)) = boundary.inspect_walk_entry(&path)? else {
                // Broken symlink or symlink escaping workspace boundary -> never follow or emit.
                continue;
            };
            let child_search_rel = if search_rel_prefix.is_empty() {
                name
            } else {
                format!("{search_rel_prefix}/{name}")
            };

            if is_dir {
                if depth + 1 > MAX_WALK_DEPTH {
                    scan_truncated = true;
                } else if visited_dirs.insert(resolved.canonical_path.clone()) {
                    subdirs.push((resolved.canonical_path, child_search_rel, depth + 1));
                }
            } else {
                files.push(WalkedFile {
                    canonical_path: resolved.canonical_path,
                    workspace_rel: resolved.relative_path,
                    search_rel: child_search_rel,
                });
            }
        }

        // Push in reverse order so DFS pops children in ascending lexicographical order.
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }

    Ok(WalkCollect {
        files,
        scan_truncated,
    })
}

fn is_ignored_dir_name(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "dist")
}

fn split_search_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    trimmed
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn contains_ascii_ignore_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn truncate_line_preview(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_owned();
    }
    let marker = "... [truncated]";
    if max_bytes <= marker.len() {
        let mut end = max_bytes;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        return line[..end].to_owned();
    }
    let mut end = max_bytes - marker.len();
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &line[..end])
}

/// Compiled glob pattern supporting `*`, `**`, `?`, `{a,b}`, and `[...]` classes.
#[derive(Clone, Debug)]
struct GlobMatcher {
    variants: Vec<Vec<String>>,
    has_slash: bool,
}

impl GlobMatcher {
    fn compile(pattern: &str) -> Result<Self, SearchOpError> {
        if pattern.is_empty() {
            return Err(SearchOpError::new(
                SearchOpErrorKind::InvalidPattern,
                "invalid_input",
                "glob pattern must not be empty",
            ));
        }
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(SearchOpError::new(
                SearchOpErrorKind::InvalidPattern,
                "invalid_input",
                format!("glob pattern exceeds {MAX_PATTERN_BYTES} bytes"),
            ));
        }
        if pattern.contains('\0') {
            return Err(SearchOpError::new(
                SearchOpErrorKind::InvalidPattern,
                "invalid_input",
                "glob pattern must not contain NUL bytes",
            ));
        }
        let normalized = pattern.replace('\\', "/");
        if normalized.split('/').any(|seg| seg == "..") {
            return Err(SearchOpError::new(
                SearchOpErrorKind::InvalidPattern,
                "path_not_allowed",
                format!("glob pattern '{pattern}' must not contain '..' traversal"),
            ));
        }
        let has_slash = normalized.contains('/');
        let expanded = expand_braces(&normalized, 0)?;
        let mut variants = Vec::with_capacity(expanded.len());
        for raw_variant in expanded {
            let segs: Vec<String> = raw_variant
                .split('/')
                .filter(|seg| !seg.is_empty() && *seg != ".")
                .map(str::to_owned)
                .collect();
            variants.push(segs);
        }
        Ok(Self {
            variants,
            has_slash,
        })
    }

    fn matches(&self, relative_path: &str) -> bool {
        let path_segs: Vec<&str> = relative_path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        self.variants
            .iter()
            .any(|pat_segs| match_segments(pat_segs, &path_segs))
    }

    /// For `grep`'s `glob` filter: patterns without `/` match the file's
    /// basename anywhere in the search subtree (e.g. `*.rs`), while patterns
    /// containing `/` match the relative path from search root (`src/**/*.rs`).
    fn matches_filter(&self, search_rel_path: &str) -> bool {
        if self.has_slash {
            return self.matches(search_rel_path);
        }
        let basename = search_rel_path
            .rsplit('/')
            .next()
            .unwrap_or(search_rel_path);
        self.matches(basename) || self.matches(search_rel_path)
    }
}

fn expand_braces(pattern: &str, depth: usize) -> Result<Vec<String>, SearchOpError> {
    if depth > MAX_BRACE_DEPTH {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidPattern,
            "invalid_input",
            "glob brace nesting exceeds maximum depth",
        ));
    }
    let bytes = pattern.as_bytes();
    let mut open_idx = None;
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'{' {
            open_idx = Some(idx);
            break;
        }
    }
    let Some(open) = open_idx else {
        return Ok(vec![pattern.to_owned()]);
    };

    let mut brace_depth = 0usize;
    let mut close_idx = None;
    let mut comma_positions = Vec::new();
    for (idx, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => brace_depth += 1,
            b'}' => {
                brace_depth -= 1;
                if brace_depth == 0 {
                    close_idx = Some(idx);
                    break;
                }
            }
            b',' if brace_depth == 1 => {
                comma_positions.push(idx);
            }
            _ => {}
        }
    }

    let Some(close) = close_idx else {
        // Unclosed `{` -> treat literally.
        return Ok(vec![pattern.to_owned()]);
    };
    if comma_positions.is_empty() {
        // `{single}` without commas -> keep literal braces if not an alternation, or strip?
        // Standard brace alternation requires at least one comma; if none, keep literal.
        let rest_expanded = expand_braces(&pattern[close + 1..], depth + 1)?;
        let prefix = &pattern[..=close];
        let mut out = Vec::with_capacity(rest_expanded.len());
        for tail in rest_expanded {
            out.push(format!("{prefix}{tail}"));
        }
        return Ok(out);
    }

    let prefix = &pattern[..open];
    let suffix_variants = expand_braces(&pattern[close + 1..], depth + 1)?;

    let mut alternatives = Vec::new();
    let mut start = open + 1;
    for comma in comma_positions {
        alternatives.push(&pattern[start..comma]);
        start = comma + 1;
    }
    alternatives.push(&pattern[start..close]);

    let mut combined = Vec::new();
    for alt in alternatives {
        let inner_variants = expand_braces(alt, depth + 1)?;
        for inner in inner_variants {
            for tail in &suffix_variants {
                if combined.len() >= MAX_BRACE_EXPANSIONS {
                    return Err(SearchOpError::new(
                        SearchOpErrorKind::InvalidPattern,
                        "invalid_input",
                        format!("glob brace expansion exceeds {MAX_BRACE_EXPANSIONS} alternatives"),
                    ));
                }
                combined.push(format!("{prefix}{inner}{tail}"));
            }
        }
    }
    Ok(combined)
}

/// Iterative O(P * N) glob segment matcher supporting `**` multi-segment wildcards.
fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si = 0usize;

    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == "**" {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && match_segment(&pattern[pi], path[si]) {
            pi += 1;
            si += 1;
        } else if let Some(saved_pi) = star_pi {
            pi = saved_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == "**" {
        pi += 1;
    }
    pi == pattern.len()
}

/// Iterative O(P * N) single-segment glob matcher supporting `*`, `?`, and `[...]`.
fn match_segment(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_si = 0usize;

    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == '?' {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '[' {
            if let Some((matched, next_pi)) = match_char_class(&p[pi..], s[si]) {
                if matched {
                    pi += next_pi;
                    si += 1;
                } else if let Some(saved_pi) = star_pi {
                    pi = saved_pi + 1;
                    star_si += 1;
                    si = star_si;
                } else {
                    return false;
                }
            } else if s[si] == '[' {
                pi += 1;
                si += 1;
            } else if let Some(saved_pi) = star_pi {
                pi = saved_pi + 1;
                star_si += 1;
                si = star_si;
            } else {
                return false;
            }
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(saved_pi) = star_pi {
            pi = saved_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn match_char_class(pattern: &[char], candidate: char) -> Option<(bool, usize)> {
    let close = pattern.iter().skip(1).position(|c| *c == ']')? + 1;
    let mut body = &pattern[1..close];
    let negated = matches!(body.first(), Some('!') | Some('^'));
    if negated {
        body = &body[1..];
    }
    if body.is_empty() {
        return None;
    }
    let mut hit = false;
    let mut idx = 0usize;
    while idx < body.len() {
        if idx + 2 < body.len() && body[idx + 1] == '-' {
            if body[idx] <= candidate && candidate <= body[idx + 2] {
                hit = true;
            }
            idx += 3;
        } else {
            if body[idx] == candidate {
                hit = true;
            }
            idx += 1;
        }
    }
    Some((hit != negated, close + 1))
}

fn required_utf8_string(obj: &JsObject, key: &str) -> Result<String, SearchOpError> {
    let Some(val) = obj.get(key) else {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            format!("missing required string field '{key}'"),
        ));
    };
    let Some(s) = val.as_string() else {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' must be a string"),
        ));
    };
    let text = s.to_utf8().map_err(|_| {
        SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' contains unpaired UTF-16 surrogates"),
        )
    })?;
    if text.is_empty() {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' must not be empty"),
        ));
    }
    Ok(text)
}

fn optional_utf8_string(obj: &JsObject, key: &str) -> Result<Option<String>, SearchOpError> {
    match obj.get(key) {
        None | Some(JsValue::Null | JsValue::Undefined) => Ok(None),
        Some(val) => {
            let Some(s) = val.as_string() else {
                return Err(SearchOpError::new(
                    SearchOpErrorKind::InvalidInput,
                    "invalid_input",
                    format!("'{key}' must be a string"),
                ));
            };
            let text = s.to_utf8().map_err(|_| {
                SearchOpError::new(
                    SearchOpErrorKind::InvalidInput,
                    "invalid_input",
                    format!("'{key}' contains unpaired UTF-16 surrogates"),
                )
            })?;
            if text.is_empty() {
                return Err(SearchOpError::new(
                    SearchOpErrorKind::InvalidInput,
                    "invalid_input",
                    format!("'{key}' must not be empty"),
                ));
            }
            Ok(Some(text))
        }
    }
}

fn parse_positive_usize(val: &JsValue, key: &str, min: usize) -> Result<usize, SearchOpError> {
    let Some(n) = val.as_number() else {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' must be an integer"),
        ));
    };
    if !n.is_finite() || n.fract() != 0.0 || n < (min as f64) || n > (usize::MAX as f64) {
        return Err(SearchOpError::new(
            SearchOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' must be an integer >= {min}"),
        ));
    }
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::validate_tool_input;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempWorkspace {
        dir: PathBuf,
    }

    impl TempWorkspace {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir().join(format!(
                "pilotdeck-tool-core-search-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            Self { dir: base }
        }

        fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn definitions_and_glob_matcher_cover_wildcards_braces_and_bounds() {
        for def in [glob_definition(), grep_definition()] {
            let schema = def.get("inputSchema").unwrap();
            assert!(crate::schema::validate_schema_shape(schema).is_ok());
        }
        let glob_schema = glob_definition().get("inputSchema").unwrap().clone();
        assert!(
            validate_tool_input(
                &glob_schema,
                &parse_json(r#"{"pattern":"**/*.{rs,toml}","limit":50}"#).unwrap()
            )
            .is_ok()
        );
        assert!(
            validate_tool_input(
                &glob_schema,
                &parse_json(r#"{"pattern":"*.rs","limit":0}"#).unwrap()
            )
            .is_err()
        );

        let m = GlobMatcher::compile("src/**/*.{rs,toml}").unwrap();
        assert!(m.matches("src/lib.rs"));
        assert!(m.matches("src/a/b/mod.rs"));
        assert!(m.matches("src/Cargo.toml"));
        assert!(!m.matches("src/lib.txt"));

        let q = GlobMatcher::compile("doc?.md").unwrap();
        assert!(q.matches("doc1.md"));
        assert!(!q.matches("doc12.md"));
        assert!(!q.matches("sub/doc1.md"));
    }

    #[test]
    fn glob_is_deterministic_capped_and_rejects_or_skips_symlink_escapes() {
        let ws = TempWorkspace::new("glob");
        let outside = TempWorkspace::new("glob-outside");
        fs::write(outside.path().join("leak.rs"), b"secret").unwrap();

        fs::create_dir_all(ws.path().join("src/nested")).unwrap();
        fs::create_dir_all(ws.path().join(".git")).unwrap();
        fs::write(ws.path().join(".git/ignored.rs"), b"skip").unwrap();
        fs::write(ws.path().join("z.rs"), b"z").unwrap();
        fs::write(ws.path().join("a.rs"), b"a").unwrap();
        fs::write(ws.path().join("src/b.rs"), b"b").unwrap();
        fs::write(ws.path().join("src/nested/c.toml"), b"c").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), ws.path().join("ext_dir")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("leak.rs"),
                ws.path().join("ext_file.rs"),
            )
            .unwrap();
        }

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        let all = glob(&boundary, &GlobRequest::new("**/*.{rs,toml}")).unwrap();
        assert_eq!(
            all.files,
            vec!["a.rs", "src/b.rs", "src/nested/c.toml", "z.rs"]
        );
        assert_eq!(all.count, 4);
        assert!(!all.truncated);

        // Max result cap keeps sorted prefix and marks truncated=true.
        let capped = glob(
            &boundary,
            &GlobRequest {
                pattern: "**/*.{rs,toml}".into(),
                path: None,
                limit: 2,
            },
        )
        .unwrap();
        assert_eq!(capped.files, vec!["a.rs", "src/b.rs"]);
        assert_eq!(capped.count, 4);
        assert!(capped.truncated);

        // Explicit search root targeting escaping symlink fails closed.
        #[cfg(unix)]
        {
            let err = glob(
                &boundary,
                &GlobRequest {
                    pattern: "*".into(),
                    path: Some("ext_dir".into()),
                    limit: 10,
                },
            )
            .unwrap_err();
            assert_eq!(err.code, "symlink_escape");
        }
    }

    #[test]
    fn grep_filters_case_insensitive_caps_skips_binary_and_orders_deterministically() {
        let ws = TempWorkspace::new("grep");
        let outside = TempWorkspace::new("grep-outside");
        fs::write(outside.path().join("secret.txt"), b"Needle inside outside").unwrap();

        fs::create_dir_all(ws.path().join("src")).unwrap();
        fs::write(ws.path().join("b.txt"), b"first\nneedle line\nNEEDLE two\n").unwrap();
        fs::write(ws.path().join("src/a.rs"), b"fn needle() {}\n// Needle\n").unwrap();
        fs::write(ws.path().join("src/other.md"), b"Needle in markdown\n").unwrap();
        fs::write(ws.path().join("bin.dat"), b"Needle\x00binary").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            ws.path().join("leak.txt"),
        )
        .unwrap();

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        let cs = grep(&boundary, &GrepRequest::new("Needle")).unwrap();
        assert_eq!(
            cs.matches,
            vec![
                GrepMatch {
                    path: "src/a.rs".into(),
                    line_number: 2,
                    line: "// Needle".into(),
                },
                GrepMatch {
                    path: "src/other.md".into(),
                    line_number: 1,
                    line: "Needle in markdown".into(),
                },
            ]
        );
        assert!(!cs.truncated);

        let ci_filtered = grep(
            &boundary,
            &GrepRequest {
                pattern: "needle".into(),
                path: None,
                glob: Some("*.{rs,txt}".into()),
                case_insensitive: true,
                max_matches: 3,
                max_file_bytes: DEFAULT_MAX_GREP_FILE_BYTES,
                max_line_bytes: DEFAULT_MAX_GREP_LINE_BYTES,
            },
        )
        .unwrap();
        assert_eq!(
            ci_filtered.matches,
            vec![
                GrepMatch {
                    path: "b.txt".into(),
                    line_number: 2,
                    line: "needle line".into(),
                },
                GrepMatch {
                    path: "b.txt".into(),
                    line_number: 3,
                    line: "NEEDLE two".into(),
                },
                GrepMatch {
                    path: "src/a.rs".into(),
                    line_number: 1,
                    line: "fn needle() {}".into(),
                },
            ]
        );
        assert!(ci_filtered.truncated);
    }

    #[test]
    fn glob_matches_single_segment_nested_braces_char_classes_and_subdirectory_scope() {
        let ws = TempWorkspace::new("glob-scope");
        fs::create_dir_all(ws.path().join("src/deep")).unwrap();
        fs::write(ws.path().join("top.rs"), b"").unwrap();
        fs::write(ws.path().join("src/a1.rs"), b"").unwrap();
        fs::write(ws.path().join("src/a2.md"), b"").unwrap();
        fs::write(ws.path().join("src/deep/b.rs"), b"").unwrap();

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        // Single-segment `*.rs` does not match `src/a1.rs`.
        let top_only = glob(&boundary, &GlobRequest::new("*.rs")).unwrap();
        assert_eq!(top_only.files, vec!["top.rs"]);

        // Subdirectory `path` scopes pattern while returning workspace-relative POSIX paths.
        let sub_scoped = glob(
            &boundary,
            &GlobRequest {
                pattern: "a[0-9].{rs,{md,txt}}".into(),
                path: Some("src".into()),
                limit: 20,
            },
        )
        .unwrap();
        assert_eq!(sub_scoped.files, vec!["src/a1.rs", "src/a2.md"]);

        // Rejects traversal in glob pattern and non-directory search path.
        assert_eq!(
            glob(&boundary, &GlobRequest::new("../**/*.rs"))
                .unwrap_err()
                .code,
            "path_not_allowed"
        );
        assert_eq!(
            glob(
                &boundary,
                &GlobRequest {
                    pattern: "*".into(),
                    path: Some("top.rs".into()),
                    limit: 10,
                },
            )
            .unwrap_err()
            .kind,
            SearchOpErrorKind::NotADirectory
        );
    }

    #[test]
    fn grep_supports_single_file_exact_cap_no_false_truncation_and_line_preview_bounds() {
        let ws = TempWorkspace::new("grep-bounds");
        fs::create_dir_all(ws.path().join("a")).unwrap();
        fs::write(ws.path().join("a-file.txt"), b"hit one\n").unwrap();
        fs::write(ws.path().join("a/file.txt"), b"hit two\n").unwrap();
        fs::write(
            ws.path().join("long.txt"),
            format!("prefix-hit-{}\n", "x".repeat(200)).as_bytes(),
        )
        .unwrap();
        fs::write(ws.path().join("huge.txt"), "hit\n".repeat(500)).unwrap();

        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        // Exact cap without extra match keeps truncated=false, and orders `a-file.txt` before `a/file.txt`.
        let exact = grep(
            &boundary,
            &GrepRequest {
                pattern: "hit".into(),
                path: None,
                glob: Some("a*.txt".into()),
                case_insensitive: false,
                max_matches: 1,
                max_file_bytes: DEFAULT_MAX_GREP_FILE_BYTES,
                max_line_bytes: DEFAULT_MAX_GREP_LINE_BYTES,
            },
        )
        .unwrap();
        assert_eq!(exact.count, 1);
        assert!(!exact.truncated);
        assert_eq!(exact.matches[0].path, "a-file.txt");

        let both = grep(
            &boundary,
            &GrepRequest {
                pattern: "hit".into(),
                path: Some("a".into()),
                glob: None,
                case_insensitive: false,
                max_matches: 5,
                max_file_bytes: DEFAULT_MAX_GREP_FILE_BYTES,
                max_line_bytes: DEFAULT_MAX_GREP_LINE_BYTES,
            },
        )
        .unwrap();
        assert_eq!(both.matches[0].path, "a/file.txt");

        // Single-file search + line preview truncation + max_file_bytes skip.
        let single = grep(
            &boundary,
            &GrepRequest {
                pattern: "hit".into(),
                path: Some("long.txt".into()),
                glob: None,
                case_insensitive: false,
                max_matches: 10,
                max_file_bytes: 512,
                max_line_bytes: 40,
            },
        )
        .unwrap();
        assert_eq!(single.count, 1);
        assert!(single.matches[0].line.len() <= 40);
        assert!(single.matches[0].line.ends_with("... [truncated]"));

        // File exceeding max_file_bytes is skipped cleanly.
        let skipped = grep(
            &boundary,
            &GrepRequest {
                pattern: "hit".into(),
                path: Some("huge.txt".into()),
                glob: None,
                case_insensitive: false,
                max_matches: 10,
                max_file_bytes: 100,
                max_line_bytes: 100,
            },
        )
        .unwrap();
        assert_eq!(skipped.count, 0);
    }
}
