//! Shared native session search, used by CLI and channel commands.
use crate::metadata::js_trim;
use crate::{list_session_entries, read_session_bytes, session_chat_dir};
use pilotdeck_protocol::{JsString, JsValue, parse_json, stringify_json};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
mod matcher;
mod title;
#[derive(Debug)]
pub enum Failure {
    Error(String),
    JsError(JsString),
}
fn node_fs_message(error: &std::io::Error, syscall: &str, path: &Path) -> Option<String> {
    crate::node_fs_message(error, syscall, Some(path.to_string_lossy().as_ref()))
}
#[derive(Clone)]
pub struct Text(pub JsString);
impl Serialize for Text {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let json = stringify_json(&JsValue::String(self.0.clone()))
            .unwrap()
            .unwrap();
        serde_json::value::RawValue::from_string(json)
            .unwrap()
            .serialize(serializer)
    }
}
impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf16_lossy(self.0.units()))
    }
}
use matcher::Matcher;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Match {
    pub session_id: Text,
    pub role: &'static str,
    pub text: Text,
    pub snippet: Text,
    pub created_at: Text,
    pub line_number: usize,
    pub session_title: Text,
    pub project_key: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub query: Text,
    pub matches: Vec<Match>,
    pub truncated: bool,
    pub sessions_scanned: usize,
}
struct Target {
    path: PathBuf,
    project: String,
}

pub struct SearchInput<'a> {
    pub home: &'a Path,
    pub cwd: &'a Path,
    pub project: Option<&'a JsString>,
    pub session: Option<&'a JsString>,
    pub query: &'a JsString,
    pub limit: f64,
    pub regex: bool,
    pub sensitive: bool,
    pub role: &'a str,
}
pub fn search(input: SearchInput<'_>) -> Result<SearchResult, Failure> {
    let SearchInput {
        home,
        cwd,
        project,
        session,
        query,
        limit,
        regex,
        sensitive,
        role,
    } = input;
    let matcher = Matcher::new_js(query, regex, sensitive)?;
    let files = collect(home, cwd, project, session);
    let mut result = SearchResult {
        query: Text(query.clone()),
        matches: vec![],
        truncated: false,
        sessions_scanned: files.len(),
    };
    // The TS provider indexes every file before searching or applying the limit.
    let mut titles = HashMap::new();
    for file in &files {
        let bytes = read_session_bytes(&file.path).unwrap_or_default();
        if let Some(title) = title::read(&bytes)? {
            let id = file.path.file_stem().unwrap_or_default().to_string_lossy();
            titles.insert((file.project.clone(), JsString::from(id.as_ref())), title);
        }
    }
    for file in files {
        let bytes = read_session_bytes(&file.path).map_err(|error| {
            Failure::Error(
                node_fs_message(&error, "open", &file.path).unwrap_or_else(|| error.to_string()),
            )
        })?;
        let source = String::from_utf8_lossy(&bytes);
        let mut file_matches = vec![];
        // Node readline treats CR, LF and CRLF as line boundaries.
        for (index, line) in physical_lines(&source).enumerate() {
            let Ok(entry) = parse_json(line) else {
                continue;
            };
            if matches!(entry, JsValue::Null) {
                return Err(Failure::Error(
                    "Cannot read properties of null (reading 'sessionId')".into(),
                ));
            }
            let Some((entry_role, text)) = searchable(&entry) else {
                continue;
            };
            if role != "all" && entry_role != role {
                continue;
            }
            if matcher.find(&text)?.is_none() {
                continue;
            }
            let normalized = normalize(&text);
            let center = matcher.find(&normalized)?.unwrap_or(0);
            let units = normalized.0.units();
            let start = center.saturating_sub(60);
            let end = (center + 60).min(units.len());
            let mut snippet = Vec::new();
            if start > 0 {
                snippet.extend("...".encode_utf16());
            }
            // JS slice clamps both bounds and returns empty when start >= end.
            if start < end {
                snippet.extend_from_slice(&units[start..end]);
            }
            if end < units.len() {
                snippet.extend("...".encode_utf16());
            }
            let snippet = Text(JsString::from_utf16(snippet));
            let session_id = string(&entry, "sessionId")
                .unwrap_or_else(|| Text(file.path.to_string_lossy().as_ref().into()));
            let session_title = titles
                .get(&(file.project.clone(), session_id.0.clone()))
                .cloned()
                .unwrap_or_else(|| session_id.clone());
            file_matches.push(Match {
                session_id,
                role: entry_role,
                text,
                snippet,
                created_at: string(&entry, "createdAt").unwrap_or_else(|| Text("".into())),
                line_number: index + 1,
                session_title,
                project_key: file.project.clone(),
            });
        }
        // TS finishes reading a whole file before applying the result limit.
        for item in file_matches {
            result.matches.push(item);
            // The provider uses >= against the fractional limit, not Array.slice.
            if result.matches.len() as f64 >= limit {
                result.truncated = true;
                break;
            }
        }
        if result.truncated {
            break;
        }
    }
    Ok(result)
}

fn internal(id: &str) -> bool {
    [
        "always-on-discovery",
        "always-on-workspace",
        "always-on-report",
    ]
    .iter()
    .any(|prefix| {
        id.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with([':', '-']))
    })
}
fn collect(
    home: &Path,
    cwd: &Path,
    project: Option<&JsString>,
    session: Option<&JsString>,
) -> Vec<Target> {
    // Resolve the project identity as UTF-16 before disk encoding. Disk .cwd
    // markers are valid UTF-8 and cannot equal an unpaired surrogate that
    // survives lexical resolution; in that case TS falls back to legacy slug.
    let directory = |project: &JsString| {
        let resolved = crate::metadata::resolve_units(&cwd.to_string_lossy(), project);
        let disk = String::from_utf16_lossy(resolved.units());
        let dir = if resolved.to_utf8().is_ok() {
            session_chat_dir(home, cwd, &disk)
        } else {
            home.join("projects")
                .join(crate::metadata::project_id(Path::new(&disk)))
                .join("chats")
        };
        // Storage resolution is absolute, but result.projectKey retains the
        // caller's spelling, as in NodeSessionSearchPort.
        (String::from_utf16_lossy(project.units()), dir)
    };
    if let Some(id) = session.filter(|id| !id.is_empty()) {
        if internal(&String::from_utf16_lossy(id.units())) {
            return vec![];
        }
        let fallback = JsString::from(cwd.to_string_lossy().as_ref());
        let (project, dir) = directory(project.unwrap_or(&fallback));
        let id = crate::storage::sanitize_session_id(id);
        return vec![Target {
            path: dir.join(format!("{id}.jsonl")),
            project,
        }];
    }
    if let Some(project) = project.filter(|p| !p.is_empty()) {
        let (project, dir) = directory(project);
        return list(&dir, &project);
    }
    list_session_entries(&home.join("projects"))
        .unwrap_or_default()
        .into_iter()
        .flat_map(|(id, _)| list(&home.join("projects").join(&id).join("chats"), &id))
        .collect()
}
fn list(dir: &Path, project: &str) -> Vec<Target> {
    list_session_entries(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, _)| {
            let id = name.strip_suffix(".jsonl")?;
            (!internal(id)).then(|| Target {
                path: dir.join(&name),
                project: project.into(),
            })
        })
        .collect()
}
fn string(entry: &JsValue, key: &str) -> Option<Text> {
    entry.get(key)?.as_string().map(|s| Text(s.clone()))
}
fn canonical(messages: &[JsValue]) -> Option<Text> {
    let mut parts = vec![];
    for message in messages {
        let Some(JsValue::Array(content)) = message.get("content") else {
            continue;
        };
        for block in content {
            if !string(block, "type").is_some_and(|s| s.0.equals_str("text")) {
                continue;
            }
            if let Some(text) = string(block, "text") {
                let text = trim(&text);
                if !text.0.is_empty() {
                    parts.push(text);
                }
            }
        }
    }
    (!parts.is_empty()).then(|| {
        let mut units = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                units.push(10);
            }
            units.extend_from_slice(part.0.units());
        }
        Text(JsString::from_utf16(units))
    })
}
fn searchable(entry: &JsValue) -> Option<(&'static str, Text)> {
    match string(entry, "type").map(|s| s.to_string()).as_deref() {
        Some("accepted_input") => {
            let JsValue::Array(messages) = entry.get("messages")? else {
                return None;
            };
            canonical(messages).map(|text| ("user", text))
        }
        Some("assistant_message" | "durable_message") => {
            let message = entry.get("message")?;
            message.as_object()?;
            canonical(std::slice::from_ref(message)).map(|text| ("assistant", text))
        }
        _ => None,
    }
}
fn physical_lines(source: &str) -> impl Iterator<Item = &str> {
    // split_inclusive keeps adjacent empty lines while consuming CRLF once.
    source.split_inclusive('\n').flat_map(|line| {
        line.strip_suffix('\n')
            .unwrap_or(line)
            .strip_suffix('\r')
            .unwrap_or(line.strip_suffix('\n').unwrap_or(line))
            .split('\r')
    })
}
fn space(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(|c| js_trim(&c.to_string()).is_empty())
}
fn trim(text: &Text) -> Text {
    let units = text.0.units();
    let start = units.iter().position(|u| !space(*u)).unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|u| !space(*u))
        .map_or(start, |i| i + 1);
    Text(JsString::from_utf16(units[start..end].to_vec()))
}
fn normalize(text: &Text) -> Text {
    let mut units = Vec::new();
    for part in text
        .0
        .units()
        .split(|u| space(*u))
        .filter(|s| !s.is_empty())
    {
        if !units.is_empty() {
            units.push(32);
        }
        units.extend_from_slice(part);
    }
    Text(JsString::from_utf16(units))
}
