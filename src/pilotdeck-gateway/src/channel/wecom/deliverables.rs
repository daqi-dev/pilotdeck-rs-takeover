use super::*;
use regex::Regex;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::LazyLock,
};
const EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "svg", "mp4", "mov", "avi", "mkv", "webm",
    "mp3", "wav", "ogg", "opus", "m4a", "flac", "amr", "pdf", "docx", "doc", "odt", "rtf", "txt",
    "md", "xlsx", "xls", "csv", "tsv", "json", "xml", "yaml", "yml", "pptx", "ppt", "odp", "zip",
    "tar", "gz", "tgz", "bz2", "7z", "html", "htm",
];
static PROTECTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.*?```|`[^`\n]+`|(?m:^>.*$)").unwrap());
static MEDIA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)["'`]?MEDIA:\s*(?:`([^`\n]+)`|"([^"\n]+)"|'([^'\n]+)'|((?:~/|/)\S+))["'`]?"#)
        .unwrap()
});
static BARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|[\s(:：])((?:~/|/)[^\s`"',;:)\]}]+\.[a-zA-Z0-9]+)(?:$|[\s`"',;:)\]}])"#)
        .unwrap()
});
fn denied(path: &Path) -> bool {
    let base = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if [
        ".env",
        "auth.json",
        "credentials",
        "server-token",
        "pilotdeck.yaml",
    ]
    .contains(&base.as_str())
    {
        return true;
    }
    path.components()
        .filter_map(|v| v.as_os_str().to_str())
        .any(|v| {
            [".git", ".ssh", "node_modules", "mcp-tokens", "pairing"]
                .contains(&v.to_lowercase().as_str())
        })
}
fn normalize(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
async fn validate(raw: &str) -> Result<PathBuf, String> {
    let raw = raw
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim_end_matches(['.', ',', ';', ':', ')', ']', '}']);
    if raw.is_empty() {
        return Err("附件路径为空。".into());
    }
    let path = if raw.starts_with("file:") {
        url::Url::parse(raw)
            .ok()
            .and_then(|v| v.to_file_path().ok())
            .ok_or_else(|| "企业微信附件投递仅支持本地文件路径。".to_owned())?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
    } else {
        if raw.contains("://") {
            return Err("企业微信附件投递仅支持本地文件路径。".into());
        }
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        return Err("附件路径必须是绝对路径或 ~/ 路径。".into());
    }
    let path = normalize(path);
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !EXTENSIONS.contains(&ext.as_str()) {
        return Err(format!(
            "不支持的附件类型 {}。",
            if ext.is_empty() {
                "(无扩展名)".to_owned()
            } else {
                format!(".{ext}")
            }
        ));
    }
    if denied(&path) {
        return Err("该路径位于敏感目录或敏感配置文件中。".into());
    }
    let info = tokio::fs::metadata(&path)
        .await
        .map_err(|_| "附件文件不存在。".to_owned())?;
    if !info.is_file() {
        return Err("附件路径不是普通文件。".into());
    }
    let real = tokio::fs::canonicalize(&path).await.unwrap_or(path);
    if denied(&real) {
        return Err("该路径位于敏感目录或敏感配置文件中。".into());
    }
    Ok(real)
}
pub(super) async fn extract(text: &JsString) -> (Vec<PathBuf>, Vec<JsString>) {
    let text = String::from_utf16_lossy(text.units());
    let mut bytes = text.as_bytes().to_vec();
    for span in PROTECTED.find_iter(&text) {
        let prefix = &text[..span.start()];
        if span.as_str().starts_with('`')
            && prefix.trim_end().to_ascii_lowercase().ends_with("media:")
        {
            continue;
        }
        for byte in &mut bytes[span.start()..span.end()] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let masked = String::from_utf8(bytes).unwrap();
    let mut candidates = Vec::new();
    let mut masked_bare = masked.as_bytes().to_vec();
    for matched in MEDIA.captures_iter(&masked) {
        let whole = matched.get(0).unwrap();
        let path = (1..=4).find_map(|i| matched.get(i)).unwrap();
        candidates.push((whole.start(), path.as_str().to_owned()));
        for byte in &mut masked_bare[whole.start()..whole.end()] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let masked_bare = String::from_utf8(masked_bare).unwrap();
    for matched in BARE.captures_iter(&masked_bare) {
        let path = matched.get(1).unwrap();
        let ext = Path::new(path.as_str())
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if EXTENSIONS.contains(&ext.as_str()) {
            candidates.push((path.start(), path.as_str().to_owned()));
        }
    }
    candidates.sort_by_key(|(position, _)| *position);
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut seen_warnings = HashSet::new();
    for (_, raw) in candidates {
        match validate(&raw).await {
            Ok(path) => {
                if seen.insert(path.clone()) {
                    files.push(path);
                }
            }
            Err(reason) => {
                let warning = format!("附件未发送：{reason}");
                if seen_warnings.insert(warning.clone()) {
                    warnings.push(warning.into());
                }
            }
        }
    }
    (files, warnings)
}
