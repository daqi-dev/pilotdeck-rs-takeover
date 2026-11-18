//! `documents.*` — 受控文档预览（t013，S3）。
//!
//! 合同正本是 `state/cli-v1-operations.json` 的 `documents.capabilities` /
//! `documents.preview` 条目（行 36389–36600）。TS 基线（cc7c4e6
//! `ui/server/services/officePreview.js`）是语义参考：LibreOffice（soffice）
//! 转换 + 120s 超时 + 平台候选路径探测；本文件不复制其代码，只对齐行为。
//!
//! 能力矩阵（`documents.capabilities` 显式 available=false+原因，不空白成功）：
//! - `text`（txt/md/代码/日志）→ `text`：原生 UTF-8 解码，恒可用；
//! - `csv` → `spreadsheetManifest`/`spreadsheetSheet`/`text`：原生解析；
//! - `spreadsheet`（xlsx）→ `spreadsheetManifest`/`spreadsheetSheet`：
//!   最小 zip+XML 读取器（flate2 解压 + 严格扫描；解析失败 →
//!   MALFORMED_DOCUMENT，不当空文档成功）；
//! - `pdf` → `text`：MuPDF 组件（`crate::pdf`，缺组件显式不可用）；
//! - `office`（doc/docx/ppt/pptx/ods/rtf）→ `pdf`/`text`/`html`：LibreOffice
//!   探测缺失 → DEPENDENCY_UNAVAILABLE。
//!
//! `documents.preview` 是 `execution=async`：受理即返回公共 operation
//! receipt；转换在后台任务（隔离 profile、超时 120s、不执行宏、限输入字节/
//! 页数/单元格），completionResult{sourceRevision, artifacts[],
//! manifestSchemaId, truncated, nextRange} 落 `async_operations`（uploads.rs
//! 的 receipt 数据面）。**缓存键 = 源内容 revision + 输出形态 + 分页参数 +
//! 转换器版本**（验收：旧版本预览不可冒充最新）。
//!
//! 分页：`pageStart/pageCount`（1 起页）仅用于 pdf→text 的按页抽取；
//! `rowStart/rowCount`（0 起）仅用于 spreadsheetSheet。其它路径携带对应
//! 参数 → INVALID_ARGUMENT（明确拒绝而非静默忽略）。`sheetId` 为 sheet 的
//! 0 起索引字符串。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::errors::{
    CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use base64::Engine;
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::public_v1::domains::artifacts;
use crate::public_v1::domains::files::{decode_byte_path, sha256_hex};
use crate::public_v1::domains::{DomainRegistry, uploads};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::V1Store;

pub(crate) const CODE_UNSUPPORTED_FORMAT: &str = "UNSUPPORTED_FORMAT";
pub(crate) const CODE_DEPENDENCY_UNAVAILABLE: &str = "DEPENDENCY_UNAVAILABLE";
pub(crate) const CODE_MALFORMED_DOCUMENT: &str = "MALFORMED_DOCUMENT";
pub(crate) const CODE_CONVERSION_TIMEOUT: &str = "CONVERSION_TIMEOUT";

/// 转换器版本（缓存键成分；行为变化时递增）。
const CONVERTER_VERSION: &str = "1";
/// LibreOffice 转换超时（TS officePreview.js:15 的 120s）。
const SOFFICE_TIMEOUT: Duration = Duration::from_secs(120);
/// 输入字节上限（pdf 组件 MAX_INPUT 同值）。
const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;
/// pdf→text 的按页抽取页数上限。
const MAX_PAGES: u64 = 500;
/// sheet 解析单元格上限。
const MAX_CELLS: u64 = 100_000;
/// spreadsheetSheet 默认行页大小（schema 未给默认值；与 100 条页大小一致）。
const DEFAULT_ROW_COUNT: u64 = 100;

/// 识别的输入格式与输出能力（capabilities/preview 共用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InputFormat {
    Text,
    Csv,
    Xlsx,
    Pdf,
    Office,
}

impl InputFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Csv => "csv",
            Self::Xlsx => "spreadsheet",
            Self::Pdf => "pdf",
            Self::Office => "office",
        }
    }
}

fn detect_format(media_type: &str, display_name: &str) -> Option<InputFormat> {
    let lower = media_type.to_ascii_lowercase();
    let name = display_name.to_ascii_lowercase();
    let ext = name.rsplit('.').next().unwrap_or_default();
    if lower.starts_with("text/")
        || matches!(
            lower.as_str(),
            "application/json" | "application/xml" | "application/yaml" | "application/x-yaml"
        )
    {
        return Some(InputFormat::Text);
    }
    if lower == "text/csv" || ext == "csv" || ext == "tsv" {
        return Some(InputFormat::Csv);
    }
    if lower.contains("spreadsheetml") || ext == "xlsx" {
        return Some(InputFormat::Xlsx);
    }
    if lower == "application/pdf" || ext == "pdf" {
        return Some(InputFormat::Pdf);
    }
    if matches!(
        ext,
        "doc" | "docx" | "ppt" | "pptx" | "ods" | "odt" | "odp" | "rtf"
    ) || lower.contains("officedocument")
        || lower.contains("msword")
        || lower.contains("ms-powerpoint")
        || lower == "application/rtf"
    {
        return Some(InputFormat::Office);
    }
    None
}

// ---------------------------------------------------------------------------
// 依赖探测（soffice / MuPDF 组件；进程内缓存）
// ---------------------------------------------------------------------------

/// 平台常见安装路径（对齐 TS officePreview 的探测语义；环境变量优先）。
fn soffice_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for variable in ["SOFFICE_PATH", "LIBREOFFICE_PATH"] {
        if let Some(path) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(path));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            candidates.push(dir.join("soffice"));
            candidates.push(dir.join("libreoffice"));
        }
    }
    candidates.extend_from_slice(&[
        PathBuf::from("/usr/bin/soffice"),
        PathBuf::from("/usr/bin/libreoffice"),
        PathBuf::from("/usr/local/bin/soffice"),
        PathBuf::from("/snap/bin/libreoffice"),
        PathBuf::from("/Applications/LibreOffice.app/Contents/MacOS/soffice"),
        PathBuf::from("/opt/homebrew/bin/soffice"),
    ]);
    candidates
}

/// 探测 LibreOffice（`soffice --version`，5s 上限），进程内缓存。
fn soffice_binary() -> Option<&'static PathBuf> {
    static PROBE: OnceLock<Option<PathBuf>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            for candidate in soffice_candidates() {
                if !candidate.is_file() {
                    continue;
                }
                let output = std::process::Command::new(&candidate)
                    .arg("--version")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output();
                if let Ok(output) = output
                    && output.status.success()
                {
                    return Some(candidate);
                }
            }
            None
        })
        .as_ref()
}

/// MuPDF 组件可用性（`native_component::tool_path` 的解析结果落盘才可用；
/// 裸名回退表示未安装）。
fn pdf_component_available() -> bool {
    let resolved =
        crate::native_component::tool_path("PILOTDECK_PDF_TOOL", "mupdf", "pilotdeck-pdf");
    let bare = PathBuf::from("pilotdeck-pdf");
    if resolved == bare {
        // 裸名回退：查 PATH 目录。
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| dir.join("pilotdeck-pdf").is_file()))
            .unwrap_or(false)
    } else {
        resolved.is_file()
    }
}

// ---------------------------------------------------------------------------
// 最小 zip 读取器（xlsx 容器；zip64 不支持 → MALFORMED_DOCUMENT）
// ---------------------------------------------------------------------------

struct ZipEntry {
    name: String,
    method: u16,
    compressed_size: usize,
    data_offset: usize,
}

fn u16le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn u32le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn malformed(message: &str) -> ContractError {
    ContractError::new(
        CODE_MALFORMED_DOCUMENT,
        message.to_owned(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn parse_zip(bytes: &[u8]) -> Result<Vec<ZipEntry>, ContractError> {
    if bytes.len() < 22 {
        return Err(malformed("xlsx container is too small to be a zip archive"));
    }
    // EOCD（结束目录记录）从尾部回扫。
    let scan_start = bytes.len().saturating_sub(66_000);
    let mut eocd = None;
    let mut i = bytes.len() - 22;
    loop {
        if &bytes[i..i + 4] == b"PK\x05\x06" {
            eocd = Some(i);
            break;
        }
        if i <= scan_start {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.ok_or_else(|| malformed("xlsx zip end-of-central-directory not found"))?;
    let count = u16le(&bytes[eocd + 10..]) as usize;
    let cd_offset = u32le(&bytes[eocd + 16..]) as usize;
    let mut entries = Vec::with_capacity(count);
    let mut p = cd_offset;
    for _ in 0..count {
        if p + 46 > bytes.len() || &bytes[p..p + 4] != b"PK\x01\x02" {
            return Err(malformed("xlsx zip central directory is truncated"));
        }
        let method = u16le(&bytes[p + 10..]);
        let compressed_size = u32le(&bytes[p + 20..]) as usize;
        let name_len = u16le(&bytes[p + 28..]) as usize;
        let extra_len = u16le(&bytes[p + 30..]) as usize;
        let comment_len = u16le(&bytes[p + 32..]) as usize;
        let local_offset = u32le(&bytes[p + 42..]) as usize;
        if p + 46 + name_len > bytes.len() {
            return Err(malformed("xlsx zip entry name is truncated"));
        }
        let name = String::from_utf8_lossy(&bytes[p + 46..p + 46 + name_len]).into_owned();
        entries.push(ZipEntry {
            name,
            method,
            compressed_size,
            data_offset: local_offset,
        });
        p += 46 + name_len + extra_len + comment_len;
    }
    // 本地头解析数据偏移。
    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        let local = entry.data_offset;
        if local + 30 > bytes.len() || &bytes[local..local + 4] != b"PK\x03\x04" {
            return Err(malformed("xlsx zip local header is invalid"));
        }
        let name_len = u16le(&bytes[local + 26..]) as usize;
        let extra_len = u16le(&bytes[local + 28..]) as usize;
        resolved.push(ZipEntry {
            data_offset: local + 30 + name_len + extra_len,
            ..entry
        });
    }
    Ok(resolved)
}

fn zip_read(bytes: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, ContractError> {
    let end = entry
        .data_offset
        .checked_add(entry.compressed_size)
        .ok_or_else(|| malformed("xlsx zip entry size overflow"))?;
    if end > bytes.len() {
        return Err(malformed("xlsx zip entry data is truncated"));
    }
    let data = &bytes[entry.data_offset..end];
    match entry.method {
        0 => Ok(data.to_vec()),
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(data)
                .read_to_end(&mut out)
                .map_err(|_| malformed("xlsx zip entry failed to inflate"))?;
            Ok(out)
        }
        method => Err(malformed(&format!(
            "xlsx zip entry uses unsupported compression method {method}"
        ))),
    }
}

fn zip_find<'a>(entries: &'a [ZipEntry], name: &str) -> Option<&'a ZipEntry> {
    entries.iter().find(|entry| entry.name == name)
}

// ---------------------------------------------------------------------------
// 最小 XML/CSV 工具
// ---------------------------------------------------------------------------

fn xml_unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
}

/// 提取标签属性 `name="value"`（单双引号都接受）。
fn xml_attr(tag: &str, attr: &str) -> Option<String> {
    let mut probe = format!("{attr}=\"");
    if let Some(start) = tag.find(&probe) {
        let value = &tag[start + probe.len()..];
        let end = value.find('"')?;
        return Some(xml_unescape(&value[..end]));
    }
    probe = format!("{attr}='");
    if let Some(start) = tag.find(&probe) {
        let value = &tag[start + probe.len()..];
        let end = value.find('\'')?;
        return Some(xml_unescape(&value[..end]));
    }
    None
}

/// 收集 `container` 内所有 `<tag …>text</tag>` 的 text（不含嵌套同名）。
/// 标签边界要求 `<tag` 后是空白/`>`/`/`（避免 `<t` 误配 `<table>`）。
fn xml_collect_texts(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open).filter(|start| {
        rest[*start + open.len()..]
            .chars()
            .next()
            .is_some_and(|ch| ch == ' ' || ch == '>' || ch == '/' || ch == '\t' || ch == '\n')
    }) {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let opening = &after[..open_end];
        let body = &after[open_end + 1..];
        if let Some(end) = body.find(&close) {
            let inner = &body[..end];
            // 自闭合（无内容）不计条目。
            if !opening.ends_with('/') {
                out.push(xml_unescape(inner.trim()));
            }
            rest = &body[end + close.len()..];
        } else {
            break;
        }
    }
    out
}

/// CSV 解析（带引号转义与嵌入换行）。
fn parse_csv(text: &str, max_cells: u64) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut cells = 0u64;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if cells >= max_cells {
            break;
        }
        if in_quotes {
            match ch {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(ch),
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            ',' => {
                row.push(std::mem::take(&mut field));
                cells += 1;
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                cells += 1;
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

// ---------------------------------------------------------------------------
// 电子表格模型
// ---------------------------------------------------------------------------

struct Sheet {
    sheet_id: usize,
    name: String,
    rows: Vec<Vec<String>>,
}

impl Sheet {
    fn column_count(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
}

fn column_index(reference: &str) -> Option<usize> {
    let letters: String = reference
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return None;
    }
    let mut index = 0usize;
    for ch in letters.chars() {
        index = index
            .checked_mul(26)?
            .checked_add((ch as u8 - b'A' + 1) as usize)?;
        if index > 16_384 {
            return None;
        }
    }
    Some(index - 1)
}

fn parse_xlsx(bytes: &[u8], max_cells: u64) -> Result<Vec<Sheet>, ContractError> {
    let entries = parse_zip(bytes)?;
    let workbook_entry = zip_find(&entries, "xl/workbook.xml")
        .ok_or_else(|| malformed("xlsx workbook.xml is missing"))?;
    let workbook = String::from_utf8_lossy(&zip_read(bytes, workbook_entry)?).into_owned();

    // sheet 顺序：workbook.xml 里 <sheet name=… r:id="rIdN"/>。
    let mut sheet_refs: Vec<(String, String)> = Vec::new();
    let mut rest = workbook.as_str();
    while let Some(start) = rest.find("<sheet ") {
        let after = &rest[start..];
        let Some(end) = after.find('>') else {
            break;
        };
        let tag = &after[..end];
        let name = xml_attr(tag, "name").unwrap_or_default();
        let rid = xml_attr(tag, "r:id")
            .or_else(|| xml_attr(tag, "id"))
            .unwrap_or_default();
        sheet_refs.push((name, rid));
        rest = &after[end..];
    }
    if sheet_refs.is_empty() {
        return Err(malformed("xlsx workbook declares no sheets"));
    }

    // rId → target（rel 文件）。
    let mut targets: HashMap<String, String> = HashMap::new();
    if let Some(rels) = zip_find(&entries, "xl/_rels/workbook.xml.rels") {
        let rels_xml = String::from_utf8_lossy(&zip_read(bytes, rels)?).into_owned();
        let mut scan = rels_xml.as_str();
        while let Some(start) = scan.find("<Relationship ") {
            let after = &scan[start..];
            let Some(end) = after.find('>') else {
                break;
            };
            let tag = &after[..end];
            let id = xml_attr(tag, "Id").unwrap_or_default();
            let target = xml_attr(tag, "Target").unwrap_or_default();
            if !id.is_empty() && !target.is_empty() {
                targets.insert(id, target);
            }
            scan = &after[end..];
        }
    }

    // sharedStrings（可选）。
    let mut shared: Vec<String> = Vec::new();
    if let Some(entry) = zip_find(&entries, "xl/sharedStrings.xml") {
        let xml = String::from_utf8_lossy(&zip_read(bytes, entry)?).into_owned();
        // 每个 <si> 的文本 = 其内所有 <t> 拼接。
        let mut rest = xml.as_str();
        while let Some(start) = rest.find("<si>") {
            let after = &rest[start..];
            let Some(end) = after.find("</si>") else {
                break;
            };
            let inner = &after[..end];
            shared.push(xml_collect_texts(inner, "t").join(""));
            rest = &after[end..];
        }
    }

    let mut sheets = Vec::new();
    let mut cells = 0u64;
    for (index, (name, rid)) in sheet_refs.iter().enumerate() {
        let target = targets.get(rid).cloned().unwrap_or_default();
        let sheet_path = if target.starts_with("/xl/") {
            target.trim_start_matches('/').to_owned()
        } else if target.starts_with("xl/") {
            target.clone()
        } else {
            format!("xl/{target}")
        };
        let rows = match zip_find(&entries, &sheet_path) {
            Some(entry) => parse_sheet_xml(
                &String::from_utf8_lossy(&zip_read(bytes, entry)?),
                &shared,
                &mut cells,
                max_cells,
            )?,
            None => Vec::new(),
        };
        sheets.push(Sheet {
            sheet_id: index,
            name: name.clone(),
            rows,
        });
    }
    Ok(sheets)
}

fn parse_sheet_xml(
    xml: &str,
    shared: &[String],
    cells: &mut u64,
    max_cells: u64,
) -> Result<Vec<Vec<String>>, ContractError> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut rest = xml;
    loop {
        // `<row `（带属性）与 `<row>`（无属性）都接受。
        let Some(start) = rest.find("<row").filter(|start| {
            rest[*start + 4..]
                .chars()
                .next()
                .is_some_and(|ch| ch == ' ' || ch == '>')
        }) else {
            break;
        };
        let after = &rest[start..];
        let Some(row_open_end) = after.find('>') else {
            break;
        };
        let row_tag = &after[..row_open_end];
        let row_number = xml_attr(row_tag, "r").and_then(|v| v.parse::<usize>().ok());
        let Some(body_end) = after[row_open_end + 1..]
            .find("</row>")
            .map(|e| row_open_end + 1 + e)
        else {
            break;
        };
        let row_body = &after[row_open_end + 1..body_end];
        let mut row: Vec<String> = Vec::new();
        let mut scan = row_body;
        while let Some(cell_start) = scan.find("<c ") {
            let cell_after = &scan[cell_start..];
            let Some(cell_open_end) = cell_after.find('>') else {
                break;
            };
            let cell_tag = &cell_after[..cell_open_end];
            let cell_body = &cell_after[cell_open_end + 1..];
            let (inner, next_scan) = if cell_tag.ends_with('/') {
                // 自闭合 <c .../>：无内容。
                ("", cell_body)
            } else if let Some(end) = cell_body.find("</c>") {
                (&cell_body[..end], &cell_body[end + 4..])
            } else {
                break;
            };
            let reference = xml_attr(cell_tag, "r").unwrap_or_default();
            let cell_type = xml_attr(cell_tag, "t").unwrap_or_default();
            let value = if cell_type == "inlineStr" {
                xml_collect_texts(inner, "t").join("")
            } else if let Some(vstart) = inner.find("<v>") {
                let vbody = &inner[vstart + 3..];
                let vend = vbody.find("</v>").unwrap_or(vbody.len());
                xml_unescape(vbody[..vend].trim())
            } else {
                String::new()
            };
            let text = if cell_type == "s" {
                value
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| shared.get(index).cloned())
                    .unwrap_or_default()
            } else {
                value
            };
            if let Some(col) = column_index(&reference) {
                while row.len() <= col {
                    row.push(String::new());
                }
                row[col] = text;
                *cells += 1;
                if *cells >= max_cells {
                    // 达到单元格上限：截断但保序。
                    return Ok(rows);
                }
            }
            scan = next_scan;
        }
        // 稀疏行号 → 稠密定位。
        let slot = row_number.unwrap_or(rows.len() + 1).saturating_sub(1);
        while rows.len() < slot {
            rows.push(Vec::new());
        }
        if slot == rows.len() {
            rows.push(row);
        } else {
            rows[slot] = row;
        }
        rest = &after[body_end + 6..];
    }
    Ok(rows)
}

/// HTML → 纯文本（标签剥离 + 基本实体解码；脚本/样式块整体丢弃，天然
/// 不执行任何脚本）。
fn html_to_text(html: &str) -> String {
    let mut cut = html.to_owned();
    for (open, close) in [("<script", "</script>"), ("<style", "</style>")] {
        loop {
            let lower_cut = cut.to_ascii_lowercase();
            let Some(start) = lower_cut.find(open) else {
                break;
            };
            let Some(end) = lower_cut[start..].find(close) else {
                break;
            };
            let end_abs = start + end + close.len();
            cut.replace_range(start..end_abs, "");
        }
    }
    let mut out = String::new();
    let mut in_tag = false;
    for ch in cut.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    // 块级标签换行痕迹已随标签剥离；压缩 3+ 连续空白。
    let mut collapsed = String::new();
    let mut last_space = false;
    for ch in xml_unescape(&out).chars() {
        if ch.is_whitespace() {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(ch);
            last_space = false;
        }
    }
    collapsed.trim().to_owned()
}

// ---------------------------------------------------------------------------
// LibreOffice 转换（隔离 profile + 超时杀进程）
// ---------------------------------------------------------------------------

async fn soffice_convert(
    binary: &Path,
    input: &Path,
    workdir: &Path,
    to_format: &str,
) -> Result<PathBuf, ContractError> {
    let profile = workdir.join(".lo-profile");
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--headless")
        .arg("--norestore")
        .arg("--nolockcheck")
        .arg("--nodefault")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            profile.display()
        ))
        .arg("--convert-to")
        .arg(to_format)
        .arg("--outdir")
        .arg(workdir)
        .arg(input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let converted = tokio::time::timeout(SOFFICE_TIMEOUT, command.output()).await;
    match converted {
        Err(_) => Err(ContractError::new(
            CODE_CONVERSION_TIMEOUT,
            format!(
                "LibreOffice conversion exceeded {}s",
                SOFFICE_TIMEOUT.as_secs()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        Ok(Err(err)) => Err(ContractError::new(
            CODE_DEPENDENCY_UNAVAILABLE,
            format!("LibreOffice could not be executed: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        Ok(Ok(output)) => {
            if !output.status.success() {
                return Err(malformed(&format!(
                    "LibreOffice conversion failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                )));
            }
            let stem = input
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("document");
            let produced = match to_format {
                "pdf" => workdir.join(format!("{stem}.pdf")),
                "txt:Text" | "txt" => workdir.join(format!("{stem}.txt")),
                "html" | "html:HTML" => workdir.join(format!("{stem}.html")),
                _ => workdir.join(format!("{stem}.out")),
            };
            if produced.is_file() {
                Ok(produced)
            } else {
                Err(malformed("LibreOffice produced no output file"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 预览缓存
// ---------------------------------------------------------------------------

fn previews_cache_dir(store: &V1Store) -> PathBuf {
    store.home().join("v1").join("document-previews")
}

fn cache_entry_path(store: &V1Store, key: &str) -> PathBuf {
    previews_cache_dir(store).join(format!("{key}.json"))
}

fn read_cache_entry(store: &V1Store, key: &str) -> Option<Value> {
    let raw = std::fs::read_to_string(cache_entry_path(store, key)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache_entry(store: &V1Store, key: &str, entry: &Value) {
    let dir = previews_cache_dir(store);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let temp = dir.join(format!(".{key}.tmp"));
    if std::fs::write(&temp, serde_json::to_string(entry).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&temp, cache_entry_path(store, key));
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

pub(super) fn register(reg: &mut DomainRegistry) {
    reg.handler("documents.capabilities", |rt, input, _key, _rev| {
        Box::pin(documents_capabilities(rt, input))
    });
    reg.handler("documents.preview", |rt, input, key, _rev| {
        Box::pin(documents_preview(rt, input, key))
    });
}

async fn documents_capabilities(
    _rt: Arc<PublicV1Runtime>,
    _input: Value,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let office_available = soffice_binary().is_some();
    let pdf_available = pdf_component_available();
    let mut formats = vec![
        json!({
            "format": "text",
            "available": true,
            "outputs": ["text"],
        }),
        json!({
            "format": "csv",
            "available": true,
            "outputs": ["spreadsheetManifest", "spreadsheetSheet", "text"],
        }),
        json!({
            "format": "spreadsheet",
            "available": true,
            "outputs": ["spreadsheetManifest", "spreadsheetSheet"],
        }),
    ];
    formats.push(if pdf_available {
        json!({
            "format": "pdf",
            "available": true,
            "outputs": ["text"],
        })
    } else {
        json!({
            "format": "pdf",
            "available": false,
            "outputs": ["text"],
            "unavailableReason": "PDF text extraction requires the pilotdeck-pdf (MuPDF) component; install it or set PILOTDECK_PDF_TOOL",
        })
    });
    formats.push(if office_available {
        json!({
            "format": "office",
            "available": true,
            "outputs": ["pdf", "text", "html"],
        })
    } else {
        json!({
            "format": "office",
            "available": false,
            "outputs": ["pdf", "text", "html"],
            "unavailableReason": "Office conversion requires LibreOffice (soffice); install it or set SOFFICE_PATH",
        })
    });
    Ok((
        ResponseOutcome::Succeeded,
        json!({
            "formats": formats,
            "maxInputBytes": MAX_INPUT_BYTES,
            "maxPages": MAX_PAGES,
            "maxCells": MAX_CELLS,
        }),
    ))
}

/// 预览输入解析结果：字节 + 源 revision + 格式识别。
struct PreviewSource {
    bytes: Vec<u8>,
    revision: String,
    format: InputFormat,
    display_name: String,
    media_type: String,
    project_id: Option<String>,
}

fn resolve_preview_source(
    store: &Arc<V1Store>,
    source: &Value,
) -> Result<PreviewSource, ContractError> {
    if let Some(artifact_id) = source.get("artifactId").and_then(Value::as_str) {
        let artifact = artifacts::artifact_json(store, artifact_id)?;
        let bytes = artifacts::read_artifact_bytes(store, artifact_id)?;
        let media_type = artifact["mediaType"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let display_name = artifact["displayName"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let revision = artifact["revision"].as_str().unwrap_or_default().to_owned();
        let format = detect_format(&media_type, &display_name)
            .ok_or_else(|| unsupported(&media_type, &display_name))?;
        if bytes.len() as u64 > MAX_INPUT_BYTES {
            return Err(input_too_large(bytes.len() as u64));
        }
        return Ok(PreviewSource {
            bytes,
            revision,
            format,
            display_name,
            media_type,
            project_id: artifact["projectId"].as_str().map(ToOwned::to_owned),
        });
    }
    let project_id = source
        .get("path")
        .and_then(|p| p.get("projectId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let relative = source
        .get("path")
        .and_then(|p| p.get("relativePath"))
        .map(|byte_path| decode_byte_path(byte_path))
        .transpose()?
        .unwrap_or_default();
    let project = crate::public_v1::domains::files::resolve_project(store, &project_id)
        .map_err(ContractError::from)?;
    let target = crate::public_v1::domains::files::request_target(&project, &relative)?;
    let canonical = std::fs::canonicalize(&target).map_err(|_| {
        ContractError::new(
            CODE_NOT_FOUND,
            format!("preview source `{}` not found", target.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    crate::public_v1::domains::files::require_within(&project.workspace, &canonical)?;
    let meta = std::fs::symlink_metadata(&canonical).map_err(|_| {
        ContractError::new(
            CODE_NOT_FOUND,
            format!("preview source `{}` not found", canonical.display()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    if !meta.is_file() {
        return Err(ContractError::new(
            CODE_INVALID_ARGUMENT,
            "preview source is not a regular file",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if meta.len() > MAX_INPUT_BYTES {
        return Err(input_too_large(meta.len()));
    }
    let bytes = std::fs::read(&canonical).map_err(|_| {
        ContractError::new(
            CODE_UNAVAILABLE,
            "preview source could not be read",
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        )
    })?;
    let content_sha = sha256_hex(&bytes);
    let revision =
        crate::public_v1::domains::files::file_revision_of(&content_sha, bytes.len() as u64);
    let display_name =
        String::from_utf8_lossy(&file_name_bytes(canonical.file_name())).into_owned();
    let media_type = guess_media_type(&display_name);
    let format = detect_format(&media_type, &display_name)
        .ok_or_else(|| unsupported(&media_type, &display_name))?;
    Ok(PreviewSource {
        bytes,
        revision,
        format,
        display_name,
        media_type,
        project_id: Some(project_id),
    })
}

#[cfg(unix)]
fn file_name_bytes(name: Option<&std::ffi::OsStr>) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.map(|n| n.as_bytes().to_vec()).unwrap_or_default()
}

#[cfg(not(unix))]
fn file_name_bytes(name: Option<&std::ffi::OsStr>) -> Vec<u8> {
    name.map(|n| n.to_string_lossy().into_owned().into_bytes())
        .unwrap_or_default()
}

fn unsupported(media_type: &str, display_name: &str) -> ContractError {
    ContractError::new(
        CODE_UNSUPPORTED_FORMAT,
        format!(
            "preview source `{display_name}` (mediaType `{media_type}`) has no supported preview format"
        ),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    )
}

fn input_too_large(size: u64) -> ContractError {
    let mut err = ContractError::new(
        agentcpm_pd_protocol::errors::CODE_RESOURCE_LIMIT,
        format!("preview source is {size} bytes; the limit is {MAX_INPUT_BYTES}"),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    );
    err.details = Some(serde_json::Map::from_iter([(
        "maxInputBytes".to_owned(),
        json!(MAX_INPUT_BYTES),
    )]));
    err
}

fn guess_media_type(display_name: &str) -> String {
    let ext = display_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf".to_owned(),
        "csv" | "tsv" => "text/csv".to_owned(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_owned(),
        "doc" => "application/msword".to_owned(),
        "docx" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_owned()
        }
        "ppt" => "application/vnd.ms-powerpoint".to_owned(),
        "pptx" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_owned()
        }
        "ods" => "application/vnd.oasis.opendocument.spreadsheet".to_owned(),
        "odt" => "application/vnd.oasis.opendocument.text".to_owned(),
        "odp" => "application/vnd.oasis.opendocument.presentation".to_owned(),
        "rtf" => "application/rtf".to_owned(),
        "html" | "htm" => "text/html".to_owned(),
        "md" | "txt" | "json" | "xml" | "yaml" | "yml" | "log" | "" => "text/plain".to_owned(),
        other => format!("application/x-{other}"),
    }
}

async fn documents_preview(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    _idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let source = input.get("source").cloned().unwrap_or(Value::Null);
    let expected_revision = input
        .get("expectedRevision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let output = input
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let sheet_id = input
        .get("sheetId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let row_start = input.get("rowStart").and_then(Value::as_u64);
    let row_count = input.get("rowCount").and_then(Value::as_u64);
    let page_start = input.get("pageStart").and_then(Value::as_u64);
    let page_count = input.get("pageCount").and_then(Value::as_u64);

    // 预受理校验（同步失败：未受理、零副作用）。
    let preview = {
        let store = store.clone();
        let source = source.clone();
        tokio::task::spawn_blocking(move || resolve_preview_source(&store, &source))
            .await
            .map_err(join_err)??
    };
    if preview.revision != expected_revision {
        return Err(DispatchError::Application(ContractError::new(
            agentcpm_pd_protocol::errors::CODE_REVISION_CONFLICT,
            format!(
                "preview source revision `{}` does not match expectedRevision `{expected_revision}`",
                preview.revision
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        )));
    }

    // 分页参数适用面（其它路径携带 → 明确拒绝，不静默忽略）。pageStart/
    // pageCount（pdf→text 按页抽取）在当前 pilotdeck-pdf 组件（`text FILE`
    // 不带页参，components/mupdf/pilotdeck-pdf.c usage）下不可实现，显式拒绝
    // 并说明，而不是返回整文冒充分页（报告登记为转换器限制）。
    match output.as_str() {
        "spreadsheetSheet" => {
            if sheet_id.is_none() {
                return Err(DispatchError::Application(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "output=spreadsheetSheet requires sheetId",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
            if page_start.is_some() || page_count.is_some() {
                return Err(DispatchError::Application(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "pageStart/pageCount are not supported by this build's PDF converter; \
                     use rowStart/rowCount for sheets",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
        }
        "pdf" | "html" | "text" => {
            if row_start.is_some() || row_count.is_some() || sheet_id.is_some() {
                return Err(DispatchError::Application(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "rowStart/rowCount/sheetId apply to spreadsheetSheet output only",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
            if page_start.is_some() || page_count.is_some() {
                return Err(DispatchError::Application(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "pageStart/pageCount are not supported by this build's PDF converter \
                     (pilotdeck-pdf `text` extracts whole documents)",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
        }
        "spreadsheetManifest" => {
            if row_start.is_some()
                || row_count.is_some()
                || sheet_id.is_some()
                || page_start.is_some()
                || page_count.is_some()
            {
                return Err(DispatchError::Application(ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "spreadsheetManifest output takes no paging parameters",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
        }
        other => {
            return Err(DispatchError::Application(ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("unsupported preview output `{other}`"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
    }

    // 输出可支持性 + 依赖可用性（同步）。
    let needs = |format: InputFormat, output: &str| -> bool {
        match (format, output) {
            (InputFormat::Text, "text") => true,
            (InputFormat::Csv, "text")
            | (InputFormat::Csv, "spreadsheetManifest")
            | (InputFormat::Csv, "spreadsheetSheet") => true,
            (InputFormat::Xlsx, "spreadsheetManifest")
            | (InputFormat::Xlsx, "spreadsheetSheet") => true,
            (InputFormat::Pdf, "text") => true,
            (InputFormat::Office, "pdf")
            | (InputFormat::Office, "text")
            | (InputFormat::Office, "html") => true,
            _ => false,
        }
    };
    if !needs(preview.format, &output) {
        return Err(DispatchError::Application(ContractError::new(
            CODE_UNSUPPORTED_FORMAT,
            format!(
                "preview output `{output}` is not supported for input format `{}`",
                preview.format.label()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )));
    }
    match preview.format {
        InputFormat::Office if soffice_binary().is_none() => {
            return Err(DispatchError::Application(ContractError::new(
                CODE_DEPENDENCY_UNAVAILABLE,
                "Office conversion requires LibreOffice (soffice); install it or set SOFFICE_PATH",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
        InputFormat::Pdf if !pdf_component_available() => {
            return Err(DispatchError::Application(ContractError::new(
                CODE_DEPENDENCY_UNAVAILABLE,
                "PDF text extraction requires the pilotdeck-pdf (MuPDF) component; install it or set PILOTDECK_PDF_TOOL",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
        _ => {}
    }

    // 受理：公共 operation receipt + 后台转换。
    let operation_id = store.next_id();
    let accepted_revision = {
        let conn = store.conn.lock().expect("sqlite mutex");
        uploads::create_async_operation(&conn, "documents.preview", "documents", &operation_id)?
    };

    let driver = PreviewJob {
        store: store.clone(),
        operation_id: operation_id.clone(),
        source: preview,
        output,
        sheet_id,
        row_start: row_start.unwrap_or(0),
        row_count: row_count.unwrap_or(DEFAULT_ROW_COUNT),
    };
    tokio::task::spawn(async move { driver.run().await });

    Ok((
        ResponseOutcome::Accepted,
        json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": accepted_revision,
            "deduplicated": false,
            "state": "running",
        }),
    ))
}

fn join_err(err: tokio::task::JoinError) -> DispatchError {
    DispatchError::Rpc {
        code: -32603,
        message: format!("documents worker task failed: {err}"),
        data: None,
    }
}

struct PreviewJob {
    store: Arc<V1Store>,
    operation_id: String,
    source: PreviewSource,
    output: String,
    sheet_id: Option<String>,
    row_start: u64,
    row_count: u64,
}

struct PreviewOutput {
    artifacts: Vec<Value>,
    manifest_schema_id: Option<&'static str>,
    truncated: bool,
    next_range: Option<String>,
}

impl PreviewJob {
    async fn run(self) {
        // 缓存键：源 revision + 输出形态 + 分页 + 转换器版本。
        let cache_key = {
            let mut hasher = Sha256::new();
            hasher.update(b"documents.preview/");
            hasher.update(CONVERTER_VERSION.as_bytes());
            hasher.update(self.source.revision.as_bytes());
            hasher.update(self.output.as_bytes());
            hasher.update(self.sheet_id.clone().unwrap_or_default().as_bytes());
            hasher.update(self.row_start.to_le_bytes());
            hasher.update(self.row_count.to_le_bytes());
            format!("{:x}", hasher.finalize())
        };
        if let Some(cached) = read_cache_entry(&self.store, &cache_key)
            && cached["sourceRevision"] == json!(self.source.revision)
        {
            let completion = json!({
                "sourceRevision": self.source.revision,
                "artifacts": cached["artifacts"].clone(),
                "manifestSchemaId": cached["manifestSchemaId"].clone(),
                "truncated": cached["truncated"].as_bool().unwrap_or(false),
                "nextRange": cached["nextRange"].clone(),
            });
            let _ = uploads::finish_async_operation(
                &self.store,
                &self.operation_id,
                "succeeded",
                Some(&completion),
                None,
            );
            return;
        }

        let result = self.convert().await;
        match result {
            Ok(outcome) => {
                let completion = json!({
                    "sourceRevision": self.source.revision,
                    "artifacts": outcome.artifacts,
                    "manifestSchemaId": outcome.manifest_schema_id,
                    "truncated": outcome.truncated,
                    "nextRange": outcome.next_range,
                });
                write_cache_entry(
                    &self.store,
                    &cache_key,
                    &json!({
                        "sourceRevision": self.source.revision,
                        "artifacts": completion["artifacts"].clone(),
                        "manifestSchemaId": completion["manifestSchemaId"].clone(),
                        "truncated": completion["truncated"].clone(),
                        "nextRange": completion["nextRange"].clone(),
                    }),
                );
                let _ = uploads::finish_async_operation(
                    &self.store,
                    &self.operation_id,
                    "succeeded",
                    Some(&completion),
                    None,
                );
            }
            Err((code, message)) => {
                let _ = uploads::finish_async_operation(
                    &self.store,
                    &self.operation_id,
                    "failed",
                    None,
                    Some((code.as_str(), message.as_str())),
                );
            }
        }
    }

    async fn convert(&self) -> Result<PreviewOutput, (String, String)> {
        let fail =
            |err: ContractError| -> (String, String) { (err.code.clone(), err.message.clone()) };
        let project = self
            .source
            .project_id
            .clone()
            .unwrap_or_else(|| "documents".to_owned());
        match (self.source.format, self.output.as_str()) {
            (InputFormat::Text, "text") => {
                let text = String::from_utf8(self.source.bytes.clone())
                    .map_err(|_| malformed("text preview requires valid UTF-8 bytes"))
                    .map_err(fail)?;
                let artifact = artifacts::publish_artifact(
                    &self.store,
                    &project,
                    "text/plain; charset=utf-8",
                    Some(&self.source.display_name),
                    text.as_bytes(),
                    "preview",
                )
                .map_err(fail)?;
                self.note_artifact_event(&artifact);
                Ok(PreviewOutput {
                    artifacts: vec![artifact],
                    manifest_schema_id: None,
                    truncated: false,
                    next_range: None,
                })
            }
            (InputFormat::Csv, "text") => {
                let text = String::from_utf8_lossy(&self.source.bytes).into_owned();
                let artifact = artifacts::publish_artifact(
                    &self.store,
                    &project,
                    "text/plain; charset=utf-8",
                    Some(&self.source.display_name),
                    text.as_bytes(),
                    "preview",
                )
                .map_err(fail)?;
                self.note_artifact_event(&artifact);
                Ok(PreviewOutput {
                    artifacts: vec![artifact],
                    manifest_schema_id: None,
                    truncated: false,
                    next_range: None,
                })
            }
            (InputFormat::Csv, "spreadsheetManifest") | (InputFormat::Csv, "spreadsheetSheet") => {
                let text = String::from_utf8_lossy(&self.source.bytes).into_owned();
                let sheets = vec![Sheet {
                    sheet_id: 0,
                    name: self.source.display_name.clone(),
                    rows: parse_csv(&text, MAX_CELLS),
                }];
                if self.output == "spreadsheetManifest" {
                    self.manifest_output(&project, &sheets).map_err(fail)
                } else {
                    self.sheet_output(&project, &sheets).map_err(fail)
                }
            }
            (InputFormat::Xlsx, "spreadsheetManifest")
            | (InputFormat::Xlsx, "spreadsheetSheet") => {
                let sheets = parse_xlsx(&self.source.bytes, MAX_CELLS).map_err(fail)?;
                if self.output == "spreadsheetManifest" {
                    self.manifest_output(&project, &sheets).map_err(fail)
                } else {
                    self.sheet_output(&project, &sheets).map_err(fail)
                }
            }
            (InputFormat::Pdf, "text") => {
                // 整文档抽取（pilotdeck-pdf 的 `text` 模式不带页参——
                // components/mupdf/pilotdeck-pdf.c usage；按页分页在受理面已
                // 显式拒绝，见 documents_preview 的参数校验）。
                let document = crate::pdf::Document::from_bytes(&self.source.bytes)
                    .map_err(pdf_error)
                    .map_err(fail)?;
                let text = document.text().await.map_err(pdf_error).map_err(fail)?;
                let artifact = artifacts::publish_artifact(
                    &self.store,
                    &project,
                    "text/plain; charset=utf-8",
                    Some(&self.source.display_name),
                    text.as_bytes(),
                    "preview",
                )
                .map_err(fail)?;
                self.note_artifact_event(&artifact);
                Ok(PreviewOutput {
                    artifacts: vec![artifact],
                    manifest_schema_id: None,
                    truncated: false,
                    next_range: None,
                })
            }
            (InputFormat::Office, "pdf")
            | (InputFormat::Office, "text")
            | (InputFormat::Office, "html") => {
                let binary = soffice_binary().cloned().ok_or_else(|| {
                    (
                        CODE_DEPENDENCY_UNAVAILABLE.to_owned(),
                        "LibreOffice (soffice) is not available".to_owned(),
                    )
                })?;
                let workdir =
                    previews_cache_dir(&self.store).join(format!(".work-{}", self.store.next_id()));
                std::fs::create_dir_all(&workdir).map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("preview workdir failed: {err}"),
                    )
                })?;
                let input_path = workdir.join("input.bin");
                std::fs::write(&input_path, &self.source.bytes).map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("preview staging failed: {err}"),
                    )
                })?;
                // soffice 按扩展名识别输入格式；用识别出的扩展名重命名暂存。
                let ext = self.source.display_name.rsplit('.').next().unwrap_or("bin");
                let staged_input = workdir.join(format!("input.{ext}"));
                let _ = std::fs::rename(&input_path, &staged_input);
                let to_format = match self.output.as_str() {
                    "pdf" => "pdf",
                    "text" => "txt:Text",
                    _ => "html",
                };
                let converted = soffice_convert(&binary, &staged_input, &workdir, to_format).await;
                let produced = match converted {
                    Ok(path) => path,
                    Err(err) => {
                        let _ = std::fs::remove_dir_all(&workdir);
                        return Err((err.code, err.message));
                    }
                };
                let bytes = std::fs::read(&produced).map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("converted output unreadable: {err}"),
                    )
                })?;
                let _ = std::fs::remove_dir_all(&workdir);
                let media_type = match self.output.as_str() {
                    "pdf" => "application/pdf",
                    "text" => "text/plain; charset=utf-8",
                    _ => "text/html; charset=utf-8",
                };
                let artifact = artifacts::publish_artifact(
                    &self.store,
                    &project,
                    media_type,
                    Some(&self.source.display_name),
                    &bytes,
                    "preview",
                )
                .map_err(fail)?;
                self.note_artifact_event(&artifact);
                Ok(PreviewOutput {
                    artifacts: vec![artifact],
                    manifest_schema_id: None,
                    truncated: false,
                    next_range: None,
                })
            }
            _ => Err((
                CODE_UNSUPPORTED_FORMAT.to_owned(),
                format!(
                    "preview output `{}` is not supported for input format `{}`",
                    self.output,
                    self.source.format.label()
                ),
            )),
        }
    }

    fn note_artifact_event(&self, artifact: &Value) {
        if let Some(artifact_id) = artifact["artifactId"].as_str() {
            let _ = uploads::append_domain_event(
                &self.store,
                artifact_id,
                "artifacts.created",
                "agentcpm-pd.artifact.event/1",
                &json!({"artifactId": artifact_id, "origin": "preview"}),
            );
        }
    }

    fn manifest_output(
        &self,
        project: &str,
        sheets: &[Sheet],
    ) -> Result<PreviewOutput, ContractError> {
        let manifest = json!({
            "schemaId": "documents.spreadsheetManifest.v1",
            "sheets": sheets
                .iter()
                .map(|sheet| {
                    json!({
                        "sheetId": sheet.sheet_id.to_string(),
                        "name": sheet.name,
                        "rowCount": sheet.rows.len(),
                        "columnCount": sheet.column_count(),
                    })
                })
                .collect::<Vec<_>>(),
        });
        let artifact = artifacts::publish_artifact(
            &self.store,
            project,
            "application/json",
            Some(&self.source.display_name),
            serde_json::to_vec(&manifest).unwrap_or_default().as_slice(),
            "preview",
        )?;
        self.note_artifact_event(&artifact);
        Ok(PreviewOutput {
            artifacts: vec![artifact],
            manifest_schema_id: Some("documents.spreadsheetManifest.v1"),
            truncated: false,
            next_range: None,
        })
    }

    fn sheet_output(
        &self,
        project: &str,
        sheets: &[Sheet],
    ) -> Result<PreviewOutput, ContractError> {
        let wanted = self.sheet_id.as_deref().unwrap_or("0");
        let parsed_id: usize = wanted.parse().map_err(|_| {
            ContractError::new(
                CODE_INVALID_ARGUMENT,
                format!("sheetId `{wanted}` is not a sheet index"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let sheet = sheets.get(parsed_id).ok_or_else(|| {
            ContractError::new(
                CODE_NOT_FOUND,
                format!(
                    "sheetId {parsed_id} not found (manifest declares {} sheets)",
                    sheets.len()
                ),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let total_rows = sheet.rows.len() as u64;
        let start = self.row_start.min(total_rows);
        let end = (start + self.row_count).min(total_rows);
        let rows: Vec<Value> = sheet.rows[start as usize..end as usize]
            .iter()
            .map(|row| json!(row))
            .collect();
        let truncated = end < total_rows;
        let payload = json!({
            "schemaId": "documents.spreadsheetSheet.v1",
            "sheetId": sheet.sheet_id.to_string(),
            "name": sheet.name,
            "rowStart": start,
            "rowCount": rows.len(),
            "columnCount": sheet.column_count(),
            "rows": rows,
        });
        let artifact = artifacts::publish_artifact(
            &self.store,
            project,
            "application/json",
            Some(&self.source.display_name),
            serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
            "preview",
        )?;
        self.note_artifact_event(&artifact);
        Ok(PreviewOutput {
            artifacts: vec![artifact],
            manifest_schema_id: Some("documents.spreadsheetSheet.v1"),
            truncated,
            next_range: if truncated {
                Some(format!("rowStart={end}"))
            } else {
                None
            },
        })
    }
}

fn pdf_error(err: &crate::pdf::Error) -> ContractError {
    match err {
        crate::pdf::Error::Unavailable => ContractError::new(
            CODE_DEPENDENCY_UNAVAILABLE,
            crate::pdf::UNAVAILABLE,
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        crate::pdf::Error::ParseFailed(message) | crate::pdf::Error::Failed(message) => {
            malformed(message)
        }
        crate::pdf::Error::Timeout => ContractError::new(
            CODE_CONVERSION_TIMEOUT,
            "PDF text extraction exceeded its time limit",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
        crate::pdf::Error::TooLarge => ContractError::new(
            agentcpm_pd_protocol::errors::CODE_RESOURCE_LIMIT,
            "PDF extraction exceeded its output size limit",
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public_v1::bootstrap::{initialize_instance, load_local_instance_and_token};
    use crate::public_v1::runtime::PublicV1Runtime;
    use crate::public_v1::store::InstanceMeta;
    use crate::public_v1::test_support::TempDir;
    use agentcpm_pd_protocol::registry::OperationRegistry;

    fn open_runtime(root_dir: &std::path::Path) -> (Arc<PublicV1Runtime>, String) {
        let (file, _token) = load_local_instance_and_token(root_dir, None).unwrap();
        let meta = InstanceMeta {
            instance_id: file.instance_id.clone(),
            owner_id: file.owner_id.clone(),
            product: file.product.clone(),
            protocol_version: file.protocol_version.clone(),
            node_id: file.node_id.clone(),
            generation: "gen-test-1".to_owned(),
            server_revision: "srv-test-1".to_owned(),
            initialization_revision: file.initialization_revision.clone(),
        };
        let store = V1Store::open_and_recover(root_dir, meta).unwrap();
        let rt = PublicV1Runtime::new(store);
        (rt, file.instance_id.clone())
    }

    async fn wait_preview_terminal(rt: &Arc<PublicV1Runtime>, operation_id: &str) -> Value {
        for _ in 0..200 {
            let row: Option<(String, Option<String>)> = {
                let store = rt.store();
                let conn = store.conn.lock().expect("sqlite mutex");
                conn.query_row(
                    "SELECT state, result_json FROM async_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok()
            };
            if let Some((state, result_json)) = row {
                if state != "running" {
                    return json!({"state": state, "result": result_json
                        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                        .unwrap_or(Value::Null)});
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("documents.preview did not terminalize in time");
    }

    /// 构造一个最小 xlsx（stored + deflate 两种条目），验证读取器与 manifest。
    fn build_minimal_xlsx() -> Vec<u8> {
        fn local_entry(name: &str, data: &[u8], method: u16) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&crc32(data).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            out
        }
        fn crc32(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (i, slot) in table.iter_mut().enumerate() {
                let mut value = i as u32;
                for _ in 0..8 {
                    value = if value & 1 != 0 {
                        0xEDB8_8320 ^ (value >> 1)
                    } else {
                        value >> 1
                    };
                }
                *slot = value;
            }
            let mut crc = 0xFFFF_FFFFu32;
            for byte in data {
                crc = table[((crc ^ *byte as u32) & 0xFF) as usize] ^ (crc >> 8);
            }
            crc ^ 0xFFFF_FFFF
        }
        let workbook = br#"<?xml version="1.0"?><workbook><sheets><sheet name="Alpha" sheetId="1" r:id="rId1"/><sheet name="Beta" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let rels = br#"<?xml version="1.0"?><Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let shared =
            br#"<?xml version="1.0"?><sst><si><t>hello</t></si><si><t>world</t></si></sst>"#;
        let sheet1 = br#"<?xml version="1.0"?><worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2"><v>42</v></c></row></sheetData></worksheet>"#;
        let sheet2 = br#"<?xml version="1.0"?><worksheet><sheetData><row r="1"><c r="A1"><v>7</v></c></row></sheetData></worksheet>"#;
        let entries: Vec<(&str, &[u8], u16)> = vec![
            ("xl/workbook.xml", workbook, 0),
            ("xl/_rels/workbook.xml.rels", rels, 0),
            ("xl/sharedStrings.xml", shared, 0),
            ("xl/worksheets/sheet1.xml", sheet1, 8),
            ("xl/worksheets/sheet2.xml", sheet2, 0),
        ];
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut offset = 0u32;
        for (name, data, method) in &entries {
            let body = match *method {
                8 => {
                    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new());
                    use std::io::Write;
                    encoder.write_all(data).unwrap();
                    encoder.finish().unwrap()
                }
                _ => data.to_vec(),
            };
            out.extend_from_slice(&local_entry(name, &body, *method));
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc32(&body).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(*data).len().to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
            offset += 30 + name.len() as u32 + body.len() as u32;
        }
        let cd_offset = out.len();
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&(cd_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[tokio::test]
    async fn capabilities_report_dependencies_explicitly() {
        let tmp = TempDir::new("docs-caps");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, _iid) = open_runtime(&root_dir);
        let (_, caps) = rt
            .dispatch("documents.capabilities", json!({}), None, None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("documents.capabilities", &caps)
            .unwrap();
        let formats = caps["formats"].as_array().unwrap();
        let by_format: HashMap<String, Value> = formats
            .iter()
            .map(|f| (f["format"].as_str().unwrap().to_owned(), f.clone()))
            .collect();
        // 缺依赖的格式必须显式 available=false + 原因，不能空白成功。
        for format in ["pdf", "office"] {
            let entry = &by_format[format];
            if entry["available"] == json!(false) {
                assert!(
                    entry["unavailableReason"]
                        .as_str()
                        .is_some_and(|r| !r.is_empty()),
                    "{format} unavailable entry must carry a reason: {entry}"
                );
            }
        }
        // 文本/表格原生可用。
        assert_eq!(by_format["text"]["available"], json!(true));
        assert_eq!(by_format["csv"]["available"], json!(true));
        assert_eq!(by_format["spreadsheet"]["available"], json!(true));
    }

    #[tokio::test]
    async fn preview_manifest_and_sheet_from_csv_and_xlsx() {
        let tmp = TempDir::new("docs-sheets");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, _iid) = open_runtime(&root_dir);
        let store = rt.store();

        // CSV → manifest + sheet 分页。
        let csv = "a,b\n1,2\n3,4\n5,6\n".to_owned();
        let csv_sha = sha256_hex(csv.as_bytes());
        let csv_artifact = artifacts::publish_artifact(
            &store,
            "proj-docs",
            "text/csv",
            Some("grid.csv"),
            csv.as_bytes(),
            "upload",
        )
        .unwrap();
        let csv_artifact_id = csv_artifact["artifactId"].as_str().unwrap().to_owned();
        let csv_revision = csv_artifact["revision"].as_str().unwrap().to_owned();

        let (_, receipt) = rt
            .dispatch(
                json!({
                    "source": {"artifactId": csv_artifact_id},
                    "expectedRevision": csv_revision,
                    "output": "spreadsheetManifest",
                }),
                Some("key-docs-manifest"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(receipt["kind"], json!("operation"));
        let terminal = wait_preview_terminal(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(terminal["state"], json!("succeeded"));
        let completion = &terminal["result"];
        assert_eq!(completion["sourceRevision"], json!(csv_revision));
        assert_eq!(
            completion["manifestSchemaId"],
            json!("documents.spreadsheetManifest.v1")
        );
        assert_eq!(completion["truncated"], json!(false));
        let manifest_artifact_id = completion["artifacts"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, manifest_read) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": manifest_artifact_id, "maxBytes": 4096}),
                None,
                None,
            )
            .await
            .unwrap();
        let manifest_bytes = base64::engine::general_purpose::STANDARD
            .decode(manifest_read["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest["sheets"][0]["rowCount"], json!(4));
        assert_eq!(manifest["sheets"][0]["columnCount"], json!(2));

        // sheet 分页：rowStart=1, rowCount=2。
        let (_, sheet_receipt) = rt
            .dispatch(
                json!({
                    "source": {"artifactId": csv_artifact_id},
                    "expectedRevision": csv_revision,
                    "output": "spreadsheetSheet",
                    "sheetId": "0",
                    "rowStart": 1,
                    "rowCount": 2,
                }),
                Some("key-docs-sheet"),
                None,
            )
            .await
            .unwrap();
        let terminal =
            wait_preview_terminal(&rt, sheet_receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(terminal["state"], json!("succeeded"));
        let completion = &terminal["result"];
        assert_eq!(
            completion["manifestSchemaId"],
            json!("documents.spreadsheetSheet.v1")
        );
        assert_eq!(completion["truncated"], json!(false));
        let sheet_artifact_id = completion["artifacts"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, sheet_read) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": sheet_artifact_id, "maxBytes": 4096}),
                None,
                None,
            )
            .await
            .unwrap();
        let sheet_bytes = base64::engine::general_purpose::STANDARD
            .decode(sheet_read["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        let sheet: Value = serde_json::from_slice(&sheet_bytes).unwrap();
        assert_eq!(sheet["rows"], json!([["1", "2"], ["3", "4"]]));

        // 过期 revision → REVISION_CONFLICT（缓存不得冒充最新）。
        let err = rt
            .dispatch(
                json!({
                    "source": {"artifactId": csv_artifact_id},
                    "expectedRevision": "arev-stale",
                    "output": "spreadsheetManifest",
                }),
                Some("key-docs-stale"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, "REVISION_CONFLICT"),
            other => panic!("expected application error, got {other:?}"),
        }

        // xlsx → manifest（两个 sheet，shared string 解析）。
        let xlsx = build_minimal_xlsx();
        let xlsx_artifact = artifacts::publish_artifact(
            &store,
            "proj-docs",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("book.xlsx"),
            &xlsx,
            "upload",
        )
        .unwrap();
        let xlsx_id = xlsx_artifact["artifactId"].as_str().unwrap().to_owned();
        let xlsx_revision = xlsx_artifact["revision"].as_str().unwrap().to_owned();
        let (_, receipt) = rt
            .dispatch(
                json!({
                    "source": {"artifactId": xlsx_id},
                    "expectedRevision": xlsx_revision,
                    "output": "spreadsheetManifest",
                }),
                Some("key-docs-xlsx"),
                None,
            )
            .await
            .unwrap();
        let terminal = wait_preview_terminal(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(terminal["state"], json!("succeeded"));
        let xlsx_manifest_id = terminal["result"]["artifacts"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, xlsx_manifest_read) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": xlsx_manifest_id, "maxBytes": 4096}),
                None,
                None,
            )
            .await
            .unwrap();
        let manifest_bytes = base64::engine::general_purpose::STANDARD
            .decode(xlsx_manifest_read["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest["sheets"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["sheets"][0]["name"], json!("Alpha"));
        assert_eq!(manifest["sheets"][0]["rowCount"], json!(2));

        // sheet1 内容：shared string + 数字。
        let (_, sheet_receipt) = rt
            .dispatch(
                json!({
                    "source": {"artifactId": xlsx_id},
                    "expectedRevision": xlsx_revision,
                    "output": "spreadsheetSheet",
                    "sheetId": "0",
                }),
                Some("key-docs-xlsx-sheet"),
                None,
            )
            .await
            .unwrap();
        let terminal =
            wait_preview_terminal(&rt, sheet_receipt["operationId"].as_str().unwrap()).await;
        let sheet_artifact_id = terminal["result"]["artifacts"][0]["artifactId"]
            .as_str()
            .unwrap()
            .to_owned();
        let (_, sheet_read) = rt
            .dispatch(
                "artifacts.read",
                json!({"artifactId": sheet_artifact_id, "maxBytes": 4096}),
                None,
                None,
            )
            .await
            .unwrap();
        let sheet_bytes = base64::engine::general_purpose::STANDARD
            .decode(sheet_read["chunk"]["bytesBase64"].as_str().unwrap())
            .unwrap();
        let sheet: Value = serde_json::from_slice(&sheet_bytes).unwrap();
        assert_eq!(sheet["rows"][0], json!(["hello", "world"]));
        assert_eq!(sheet["rows"][1], json!(["42"]));

        // 不支持的组合 → UNSUPPORTED_FORMAT（xlsx → text）。
        let err = rt
            .dispatch(
                json!({
                    "source": {"artifactId": xlsx_id},
                    "expectedRevision": xlsx_revision,
                    "output": "text",
                }),
                Some("key-docs-unsupported"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, "UNSUPPORTED_FORMAT"),
            other => panic!("expected application error, got {other:?}"),
        }
        // 损坏 xlsx → MALFORMED_DOCUMENT（不当空文档成功）。
        let broken = artifacts::publish_artifact(
            &store,
            "proj-docs",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("broken.xlsx"),
            b"this is not a zip at all, definitely not",
            "upload",
        )
        .unwrap();
        let err = rt
            .dispatch(
                json!({
                    "source": {"artifactId": broken["artifactId"].as_str().unwrap()},
                    "expectedRevision": broken["revision"].as_str().unwrap(),
                    "output": "spreadsheetManifest",
                }),
                Some("key-docs-broken"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, "MALFORMED_DOCUMENT"),
            other => panic!("expected application error, got {other:?}"),
        }
    }
}
