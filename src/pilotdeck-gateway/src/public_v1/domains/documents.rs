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

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentcpm_pd_protocol::contract::ResponseOutcome;
use agentcpm_pd_protocol::envelope::compute_input_digest;
use agentcpm_pd_protocol::errors::{
    CODE_IDEMPOTENCY_CONFLICT, CODE_INVALID_ARGUMENT, CODE_NOT_FOUND, CODE_RESOURCE_LIMIT,
    CODE_REVISION_CONFLICT, CODE_UNAVAILABLE, ContractError, EffectStatus, RetrySpec,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::process_group;
use crate::public_v1::domains::artifacts;
use crate::public_v1::domains::files::{decode_byte_path, sha256_hex};
use crate::public_v1::domains::{DomainRegistry, uploads};
use crate::public_v1::runtime::{DispatchError, PublicV1Runtime};
use crate::public_v1::store::{V1Store, now_rfc3339};

pub(crate) const CODE_UNSUPPORTED_FORMAT: &str = "UNSUPPORTED_FORMAT";
pub(crate) const CODE_DEPENDENCY_UNAVAILABLE: &str = "DEPENDENCY_UNAVAILABLE";
pub(crate) const CODE_MALFORMED_DOCUMENT: &str = "MALFORMED_DOCUMENT";
pub(crate) const CODE_CONVERSION_TIMEOUT: &str = "CONVERSION_TIMEOUT";

/// 转换器版本（缓存键成分；行为变化时递增）。
const CONVERTER_VERSION: &str = "1";
/// LibreOffice 探测超时。
const SOFFICE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// LibreOffice 转换超时（TS officePreview.js:15 的 120s）。
const SOFFICE_TIMEOUT: Duration = Duration::from_secs(120);
/// 输入字节上限（pdf 组件 MAX_INPUT 同值）。
const MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;
/// XLSX 单条目及整包解压上限（防 Zip Bomb OOM DoS）。
const MAX_XLSX_ENTRY_UNCOMPRESSED: u64 = 64 * 1024 * 1024;
const MAX_XLSX_TOTAL_UNCOMPRESSED: u64 = 128 * 1024 * 1024;
/// XLSX OOXML 最大合法行号（Excel 硬顶 1,048,576）。
const MAX_SHEET_ROW_NUMBER: usize = 1_048_576;
/// LibreOffice stdio/输出大小安全上限。
const MAX_SOFFICE_STDIO_BYTES: usize = 4 * 1024 * 1024;
/// 缓存 JSON 最大读取字节上限。
const MAX_CACHE_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
/// 幂等记录保留天数（与 search.rs/runs 同源）。
const PREVIEW_IDEMPOTENCY_RETENTION_DAYS: i64 = 7;
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
    // CSV/TSV carry tabular structure: they must be classified before the
    // generic `text/` prefix — `text/csv` would otherwise fall into the
    // plain-text bucket and every spreadsheet preview would be rejected
    // with UNSUPPORTED_FORMAT.
    if lower == "text/csv" || ext == "csv" || ext == "tsv" {
        return Some(InputFormat::Csv);
    }
    if lower.starts_with("text/")
        || matches!(
            lower.as_str(),
            "application/json" | "application/xml" | "application/yaml" | "application/x-yaml"
        )
    {
        return Some(InputFormat::Text);
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

async fn read_stream_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    keep_bytes: usize,
    max_total_bytes: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok((kept, false));
        }
        total = total.saturating_add(n);
        if total > max_total_bytes {
            return Ok((kept, true));
        }
        if kept.len() < keep_bytes {
            let take = (keep_bytes - kept.len()).min(n);
            kept.extend_from_slice(&buf[..take]);
        }
    }
}

async fn probe_soffice_candidate(candidate: &Path, timeout: Duration) -> bool {
    if !candidate.is_file() {
        return false;
    }
    let mut command = tokio::process::Command::new(candidate);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        pilotdeck_child_signals::install_child_detach_session(command.as_std_mut());
        pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let mut group = process_group::GroupGuard(
        child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    let stdout = child.stdout.take();
    let wait_res = tokio::time::timeout(timeout, async {
        let drain = async {
            if let Some(out) = stdout {
                let _ = read_stream_capped(out, 1024, 64 * 1024).await;
            }
            Ok::<(), std::io::Error>(())
        };
        let exit = async {
            if let Some(pid) = group.0 {
                process_group::wait_exit_unreaped(pid).await
            } else {
                Ok(())
            }
        };
        tokio::try_join!(drain, exit)
    })
    .await;
    group.signal(rustix::process::Signal::KILL);
    let status = child.wait().await;
    group.disarm();
    matches!(wait_res, Ok(Ok(_))) && status.is_ok_and(|s| s.success())
}

/// 异步探测 LibreOffice（`soffice --version`，真实 5s 超时 + 进程组清理），进程内缓存。
async fn soffice_binary() -> Option<&'static PathBuf> {
    static PROBE: tokio::sync::OnceCell<Option<PathBuf>> = tokio::sync::OnceCell::const_new();
    PROBE
        .get_or_init(|| async {
            for candidate in soffice_candidates() {
                if probe_soffice_candidate(&candidate, SOFFICE_PROBE_TIMEOUT).await {
                    return Some(candidate);
                }
            }
            None
        })
        .await
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
    let mut entries = Vec::with_capacity(count.min(1024));
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
        p = p
            .checked_add(46)
            .and_then(|v| v.checked_add(name_len))
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(comment_len))
            .ok_or_else(|| malformed("xlsx zip central directory offset overflow"))?;
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
        let data_offset = local
            .checked_add(30)
            .and_then(|v| v.checked_add(name_len))
            .and_then(|v| v.checked_add(extra_len))
            .ok_or_else(|| malformed("xlsx zip local header offset overflow"))?;
        resolved.push(ZipEntry {
            data_offset,
            ..entry
        });
    }
    Ok(resolved)
}

fn zip_read(
    bytes: &[u8],
    entry: &ZipEntry,
    total_uncompressed: &mut u64,
) -> Result<Vec<u8>, ContractError> {
    let end = entry
        .data_offset
        .checked_add(entry.compressed_size)
        .ok_or_else(|| malformed("xlsx zip entry size overflow"))?;
    if end > bytes.len() {
        return Err(malformed("xlsx zip entry data is truncated"));
    }
    let remaining_total = MAX_XLSX_TOTAL_UNCOMPRESSED.saturating_sub(*total_uncompressed);
    let entry_cap = MAX_XLSX_ENTRY_UNCOMPRESSED.min(remaining_total);
    let limit_err = || {
        ContractError::new(
            CODE_RESOURCE_LIMIT,
            format!(
                "xlsx decompressed content exceeds limit ({MAX_XLSX_ENTRY_UNCOMPRESSED}B/entry, {MAX_XLSX_TOTAL_UNCOMPRESSED}B/workbook)"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    };
    let data = &bytes[entry.data_offset..end];
    match entry.method {
        0 => {
            if data.len() as u64 > entry_cap {
                return Err(limit_err());
            }
            *total_uncompressed = total_uncompressed.saturating_add(data.len() as u64);
            Ok(data.to_vec())
        }
        8 => {
            let mut out = Vec::new();
            flate2::read::DeflateDecoder::new(data)
                .take(entry_cap.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(|_| malformed("xlsx zip entry failed to inflate"))?;
            if out.len() as u64 > entry_cap {
                return Err(limit_err());
            }
            *total_uncompressed = total_uncompressed.saturating_add(out.len() as u64);
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
    rows: BTreeMap<usize, Vec<String>>,
    row_count: usize,
    column_count: usize,
}

impl Sheet {
    fn from_dense(sheet_id: usize, name: String, dense: Vec<Vec<String>>) -> Self {
        let row_count = dense.len();
        let mut column_count = 0usize;
        let mut rows = BTreeMap::new();
        for (idx, row) in dense.into_iter().enumerate() {
            column_count = column_count.max(row.len());
            if !row.is_empty() {
                rows.insert(idx, row);
            }
        }
        Self {
            sheet_id,
            name,
            rows,
            row_count,
            column_count,
        }
    }

    fn column_count(&self) -> usize {
        self.column_count
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
    let mut total_uncompressed = 0u64;
    let workbook_entry = zip_find(&entries, "xl/workbook.xml")
        .ok_or_else(|| malformed("xlsx workbook.xml is missing"))?;
    let workbook =
        String::from_utf8_lossy(&zip_read(bytes, workbook_entry, &mut total_uncompressed)?)
            .into_owned();

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
        let rels_xml =
            String::from_utf8_lossy(&zip_read(bytes, rels, &mut total_uncompressed)?).into_owned();
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
        let xml =
            String::from_utf8_lossy(&zip_read(bytes, entry, &mut total_uncompressed)?).into_owned();
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
        let (rows, row_count, column_count) = match zip_find(&entries, &sheet_path) {
            Some(entry) => parse_sheet_xml(
                &String::from_utf8_lossy(&zip_read(bytes, entry, &mut total_uncompressed)?),
                &shared,
                &mut cells,
                max_cells,
            )?,
            None => (BTreeMap::new(), 0, 0),
        };
        sheets.push(Sheet {
            sheet_id: index,
            name: name.clone(),
            rows,
            row_count,
            column_count,
        });
    }
    Ok(sheets)
}

fn parse_sheet_xml(
    xml: &str,
    shared: &[String],
    cells: &mut u64,
    max_cells: u64,
) -> Result<(BTreeMap<usize, Vec<String>>, usize, usize), ContractError> {
    let mut rows: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut row_count: usize = 0;
    let mut column_count: usize = 0;
    let mut next_seq_row: usize = 0;
    let mut row_events: u64 = 0;
    let max_row_events = max_cells.saturating_mul(2);
    let mut rest = xml;
    // `<row `、`<row>` 与自闭合 `<row .../>` 都支持。
    while let Some(start) = rest.find("<row").filter(|start| {
        rest[*start + 4..]
            .chars()
            .next()
            .is_some_and(|ch| ch == ' ' || ch == '>' || ch == '/')
    }) {
        row_events = row_events.saturating_add(1);
        if row_events > max_row_events {
            return Err(ContractError::new(
                CODE_RESOURCE_LIMIT,
                "xlsx sheet exceeds maximum row/cell budget",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        let after = &rest[start..];
        let Some(row_open_end) = after.find('>') else {
            break;
        };
        let row_tag = &after[..row_open_end];
        let slot = if let Some(r_raw) = xml_attr(row_tag, "r") {
            let parsed = r_raw.trim().parse::<usize>().map_err(|_| {
                ContractError::new(
                    CODE_RESOURCE_LIMIT,
                    format!("xlsx row number `{r_raw}` exceeds supported range"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
            if parsed == 0 {
                return Err(malformed("xlsx row number 0 is invalid"));
            }
            if parsed > MAX_SHEET_ROW_NUMBER {
                return Err(ContractError::new(
                    CODE_RESOURCE_LIMIT,
                    format!(
                        "xlsx row number {parsed} exceeds maximum supported row limit {MAX_SHEET_ROW_NUMBER}"
                    ),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            parsed - 1
        } else {
            next_seq_row
        };
        next_seq_row = slot.saturating_add(1);
        let (row_body, next_rest) = if row_tag.ends_with('/') {
            ("", &after[row_open_end + 1..])
        } else {
            let Some(body_end) = after[row_open_end + 1..]
                .find("</row>")
                .map(|e| row_open_end + 1 + e)
            else {
                break;
            };
            (&after[row_open_end + 1..body_end], &after[body_end + 6..])
        };
        let mut row: Vec<String> = Vec::new();
        let mut scan = row_body;
        let mut hit_cell_limit = false;
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
                    hit_cell_limit = true;
                    break;
                }
            }
            scan = next_scan;
        }
        column_count = column_count.max(row.len());
        row_count = row_count.max(slot + 1);
        if !row.is_empty() {
            rows.insert(slot, row);
        }
        if hit_cell_limit {
            return Ok((rows, row_count, column_count));
        }
        rest = next_rest;
    }
    Ok((rows, row_count, column_count))
}

/// HTML → 纯文本（标签剥离 + 基本实体解码；脚本/样式块整体丢弃，天然
/// 不执行任何脚本）。
#[allow(dead_code)] // office->html preview path (soffice html output)
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
// LibreOffice 转换（隔离 profile + 禁宏 + 进程组 killpg + 有界读/写 + RAII workdir）
// ---------------------------------------------------------------------------

struct WorkdirGuard(PathBuf);

impl Drop for WorkdirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prepare_libreoffice_profile(profile: &Path) -> Result<(), ContractError> {
    let user_dir = profile.join("user");
    std::fs::create_dir_all(&user_dir).map_err(|err| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("LibreOffice profile dir failed: {err}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        )
    })?;
    let xcu = r#"<?xml version="1.0" encoding="UTF-8"?>
<oor:items xmlns:oor="http://openoffice.org/2001/registry" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="DisableMacrosExecution" oor:op="fuse"><value>true</value></prop></item>
  <item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="MacroSecurityLevel" oor:op="fuse"><value>3</value></prop></item>
  <item oor:path="/org.openoffice.Office.Common/Security/Scripting"><prop oor:name="BlockUntrustedRefererLinks" oor:op="fuse"><value>true</value></prop></item>
  <item oor:path="/org.openoffice.Office.Calc/Content/Update"><prop oor:name="Link" oor:op="fuse"><value>2</value></prop></item>
  <item oor:path="/org.openoffice.Office.Writer/Content/Update"><prop oor:name="Link" oor:op="fuse"><value>2</value></prop></item>
</oor:items>
"#;
    std::fs::write(user_dir.join("registrymodifications.xcu"), xcu.as_bytes()).map_err(|err| {
        ContractError::new(
            CODE_UNAVAILABLE,
            format!("LibreOffice security config failed: {err}"),
            RetrySpec::backoff_read(Some(250)),
            EffectStatus::NotApplied,
        )
    })?;
    Ok(())
}

fn whitelist_office_ext(display_name: &str) -> &'static str {
    let ext = display_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "doc" => "doc",
        "docx" => "docx",
        "ppt" => "ppt",
        "pptx" => "pptx",
        "ods" => "ods",
        "odt" => "odt",
        "odp" => "odp",
        "rtf" => "rtf",
        _ => "bin",
    }
}

async fn soffice_convert_with_timeout(
    binary: &Path,
    input: &Path,
    workdir: &Path,
    to_format: &str,
    timeout: Duration,
) -> Result<PathBuf, ContractError> {
    let profile = workdir.join(".lo-profile");
    let profile_for_prep = profile.clone();
    tokio::task::spawn_blocking(move || prepare_libreoffice_profile(&profile_for_prep))
        .await
        .map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("preview profile task failed: {err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })??;
    let abs_profile = if profile.is_absolute() {
        profile.clone()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&profile))
            .unwrap_or_else(|_| profile.clone())
    };
    let profile_uri = url::Url::from_directory_path(&abs_profile)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", abs_profile.display()));

    let mut command = tokio::process::Command::new(binary);
    command
        .arg("--headless")
        .arg("--invisible")
        .arg("--nologo")
        .arg("--nofirststartwizard")
        .arg("--norestore")
        .arg("--nolockcheck")
        .arg("--nodefault")
        .arg(format!("-env:UserInstallation={profile_uri}"))
        .arg("--convert-to")
        .arg(to_format)
        .arg("--outdir")
        .arg(workdir)
        .arg(input)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        pilotdeck_child_signals::install_child_detach_session(command.as_std_mut());
        pilotdeck_child_signals::install_child_signal_reset(command.as_std_mut());
    }
    let mut child = command.spawn().map_err(|err| {
        ContractError::new(
            CODE_DEPENDENCY_UNAVAILABLE,
            format!("LibreOffice could not be executed: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )
    })?;
    let mut group = process_group::GroupGuard(
        child
            .id()
            .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
    );
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    enum WaitOutcome {
        Done {
            stdout_over: bool,
            stderr_over: bool,
            stderr_bytes: Vec<u8>,
        },
        TimedOut,
        IoErr(std::io::Error),
    }

    let wait_outcome = match tokio::time::timeout(timeout, async {
        let wait_exit = async {
            if let Some(pid) = group.0 {
                process_group::wait_exit_unreaped(pid).await
            } else {
                Ok(())
            }
        };
        tokio::try_join!(
            read_stream_capped(stdout, 1024, MAX_SOFFICE_STDIO_BYTES),
            read_stream_capped(stderr, 4096, MAX_SOFFICE_STDIO_BYTES),
            wait_exit,
        )
    })
    .await
    {
        Err(_) => WaitOutcome::TimedOut,
        Ok(Err(err)) => WaitOutcome::IoErr(err),
        Ok(Ok(((_, stdout_over), (stderr_bytes, stderr_over), _))) => WaitOutcome::Done {
            stdout_over,
            stderr_over,
            stderr_bytes,
        },
    };

    // 无论超时、失败还是正常退出，先杀整组（oosplash/soffice.bin 等派生子进程）再 reap。
    group.signal(rustix::process::Signal::KILL);
    let status = child.wait().await;
    group.disarm();

    match wait_outcome {
        WaitOutcome::TimedOut => Err(ContractError::new(
            CODE_CONVERSION_TIMEOUT,
            format!("LibreOffice conversion exceeded {}s", timeout.as_secs()),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        WaitOutcome::IoErr(err) => Err(ContractError::new(
            CODE_DEPENDENCY_UNAVAILABLE,
            format!("LibreOffice execution failed: {err}"),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
        WaitOutcome::Done {
            stdout_over,
            stderr_over,
            stderr_bytes,
        } => {
            if stdout_over || stderr_over {
                return Err(ContractError::new(
                    CODE_RESOURCE_LIMIT,
                    "LibreOffice stdio output exceeded safety limit",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                ));
            }
            let status = status.map_err(|err| {
                ContractError::new(
                    CODE_DEPENDENCY_UNAVAILABLE,
                    format!("LibreOffice wait failed: {err}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
            if !status.success() {
                return Err(malformed(&format!(
                    "LibreOffice conversion failed: {}",
                    String::from_utf8_lossy(&stderr_bytes)
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

async fn soffice_convert(
    binary: &Path,
    input: &Path,
    workdir: &Path,
    to_format: &str,
) -> Result<PathBuf, ContractError> {
    soffice_convert_with_timeout(binary, input, workdir, to_format, SOFFICE_TIMEOUT).await
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ContractError> {
    #[cfg(unix)]
    {
        let Some(parent) = path.parent() else {
            return Err(malformed("output path has no parent"));
        };
        let Some(leaf) = path.file_name() else {
            return Err(malformed("output path has no leaf"));
        };
        let parent_fd = std::fs::File::open(parent).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("converted output dir unreadable: {err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        crate::public_v1::domains::files::read_file_bounded_at(&parent_fd, leaf, max_bytes)
    }
    #[cfg(not(unix))]
    {
        let meta = std::fs::symlink_metadata(path).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("converted output unreadable: {err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(ContractError::new(
                CODE_INVALID_ARGUMENT,
                "converted output is not a regular file",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            ));
        }
        if meta.len() > max_bytes {
            return Err(input_too_large(meta.len()));
        }
        let mut file = std::fs::File::open(path).map_err(|err| {
            ContractError::new(
                CODE_UNAVAILABLE,
                format!("converted output unreadable: {err}"),
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )
        })?;
        let mut bytes = Vec::with_capacity(meta.len().min(max_bytes) as usize);
        file.by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|err| {
                ContractError::new(
                    CODE_UNAVAILABLE,
                    format!("converted output unreadable: {err}"),
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(input_too_large(bytes.len() as u64));
        }
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// 预览缓存（NOFOLLOW + 唯一临时文件 + sync_all + 原子 rename）
// ---------------------------------------------------------------------------

fn previews_cache_dir(store: &V1Store) -> PathBuf {
    store.home().join("v1").join("document-previews")
}

fn valid_cache_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn read_cache_entry(store: &V1Store, key: &str) -> Option<Value> {
    if !valid_cache_key(key) {
        return None;
    }
    let dir = previews_cache_dir(store);
    let leaf = format!("{key}.json");
    #[cfg(unix)]
    {
        let dir_fd = crate::project_files::open_directory_beneath(store.home(), &dir).ok()?;
        let bytes = crate::public_v1::domains::files::read_file_bounded_at(
            &dir_fd,
            std::ffi::OsStr::new(&leaf),
            MAX_CACHE_ENTRY_BYTES,
        )
        .ok()?;
        serde_json::from_slice(&bytes).ok()
    }
    #[cfg(not(unix))]
    {
        let raw = std::fs::read_to_string(dir.join(leaf)).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

fn write_cache_entry(store: &V1Store, key: &str, entry: &Value) {
    if !valid_cache_key(key) {
        return;
    }
    let dir = previews_cache_dir(store);
    let bytes = serde_json::to_vec(entry).unwrap_or_default();
    let final_name = format!("{key}.json");
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
        if crate::project_files::create_directories_beneath(store.home(), &dir).is_err() {
            return;
        }
        let Ok(dir_fd) = crate::project_files::open_directory_beneath(store.home(), &dir) else {
            return;
        };
        let temp_name = format!(".{key}.{}.tmp", uuid::Uuid::new_v4());
        struct CacheTempGuard<'a> {
            dir: &'a std::fs::File,
            name: &'a str,
            armed: bool,
        }
        impl Drop for CacheTempGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    let _ = unlinkat(self.dir, self.name, AtFlags::empty());
                }
            }
        }
        let mut guard = CacheTempGuard {
            dir: &dir_fd,
            name: &temp_name,
            armed: false,
        };
        let Ok(fd) = openat(
            &dir_fd,
            &temp_name,
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        ) else {
            return;
        };
        guard.armed = true;
        let mut file = std::fs::File::from(fd);
        if file.write_all(&bytes).is_err() || file.sync_all().is_err() {
            return;
        }
        drop(file);
        if renameat(&dir_fd, &temp_name, &dir_fd, &final_name).is_ok() {
            guard.armed = false;
            if let Ok(rd) = crate::public_v1::domains::files::open_dir_readable_nofollow(
                &dir_fd,
                std::ffi::OsStr::new("."),
            ) {
                let _ = rd.sync_all();
            }
        }
    }
    #[cfg(not(unix))]
    {
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let temp = dir.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4()));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            && f.write_all(&bytes).is_ok()
            && f.sync_all().is_ok()
        {
            let _ = std::fs::rename(&temp, dir.join(final_name));
        } else {
            let _ = std::fs::remove_file(&temp);
        }
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
    let (office_available, pdf_available) =
        tokio::join!(async { soffice_binary().await.is_some() }, async {
            tokio::task::spawn_blocking(pdf_component_available)
                .await
                .unwrap_or(false)
        });
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
    #[allow(dead_code)] // kept for preview cache keys
    media_type: String,
    project_id: Option<String>,
}

fn resolve_preview_source(
    store: &Arc<V1Store>,
    source: &Value,
) -> Result<PreviewSource, ContractError> {
    if let Some(artifact_id) = source.get("artifactId").and_then(Value::as_str) {
        let artifact = artifacts::artifact_json(store, artifact_id)?;
        let byte_len = artifact["byteLength"].as_u64().unwrap_or(0);
        if byte_len > MAX_INPUT_BYTES {
            return Err(input_too_large(byte_len));
        }
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
        let bytes = artifacts::read_artifact_bytes_bounded(store, artifact_id, MAX_INPUT_BYTES)
            .map_err(|err| {
                if err.code == CODE_RESOURCE_LIMIT {
                    input_too_large(byte_len.max(MAX_INPUT_BYTES + 1))
                } else {
                    err
                }
            })?;
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
        .map(decode_byte_path)
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

    let mut file = crate::public_v1::domains::files::open_file_canonical(&project, &canonical)
        .map_err(|err| {
            if err.code == crate::public_v1::domains::files::CODE_NOT_A_FILE {
                ContractError::new(
                    CODE_INVALID_ARGUMENT,
                    "preview source is not a regular file",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )
            } else {
                err
            }
        })?;
    let meta = file.metadata().map_err(|_| {
        ContractError::new(
            CODE_UNAVAILABLE,
            "preview source metadata could not be read",
            RetrySpec::backoff_read(Some(250)),
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
    let mut bytes = Vec::with_capacity(meta.len().min(MAX_INPUT_BYTES) as usize);
    file.by_ref()
        .take(MAX_INPUT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ContractError::new(
                CODE_UNAVAILABLE,
                "preview source could not be read",
                RetrySpec::backoff_read(Some(250)),
                EffectStatus::NotApplied,
            )
        })?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(input_too_large(bytes.len() as u64));
    }
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

fn sql_err(err: rusqlite::Error) -> ContractError {
    ContractError::new(
        CODE_UNAVAILABLE,
        format!("sqlite storage error: {err}"),
        RetrySpec::backoff_read(Some(250)),
        EffectStatus::Unknown,
    )
}

fn check_preview_idempotency_in_conn(
    conn: &rusqlite::Connection,
    key: &str,
    digest: &str,
) -> Result<Option<Value>, ContractError> {
    let existing: Option<(String, String, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT input_digest, request_state, receipt_json, rejection_code
             FROM idempotency_records
             WHERE idempotency_key = ?1 AND method_id = 'documents.preview'",
            params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((stored_digest, req_state, receipt_json, rejection_code)) = existing else {
        return Ok(None);
    };
    if stored_digest != digest {
        return Err(ContractError::new(
            CODE_IDEMPOTENCY_CONFLICT,
            format!(
                "idempotencyKey `{key}` already used for documents.preview with different input"
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        ));
    }
    if req_state == "accepted"
        && let Some(raw) = receipt_json
        && let Ok(mut receipt) = serde_json::from_str::<Value>(&raw)
    {
        if let Some(obj) = receipt.as_object_mut() {
            obj.insert("deduplicated".into(), json!(true));
        }
        return Ok(Some(receipt));
    }
    Err(ContractError::new(
        rejection_code.unwrap_or_else(|| CODE_INVALID_ARGUMENT.to_owned()),
        "request previously rejected definitively; fix the input and use a new key".to_owned(),
        RetrySpec::never(),
        EffectStatus::NotApplied,
    ))
}

fn record_preview_rejection_if_needed(
    store: &V1Store,
    idempotency_key: Option<&str>,
    digest: &str,
    err: ContractError,
) -> ContractError {
    let Some(key) = idempotency_key else {
        return err;
    };
    if !matches!(
        err.code.as_str(),
        CODE_INVALID_ARGUMENT | CODE_UNSUPPORTED_FORMAT | CODE_REVISION_CONFLICT
    ) {
        return err;
    }
    let now = now_rfc3339();
    let retained_until = (chrono::Utc::now()
        + chrono::Duration::days(PREVIEW_IDEMPOTENCY_RETENTION_DAYS))
    .to_rfc3339();
    let mut conn = store.conn.lock().expect("sqlite mutex");
    if let Ok(tx) = conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        let _ = tx.execute(
            "INSERT OR IGNORE INTO idempotency_records
             (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
              receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
             VALUES (?1, 'documents.preview', ?2, 'notAccepted', 'definitivelyRejected', NULL,
                     NULL, ?3, ?4, ?5, 'correctRejectedInputWithNewKey')",
            params![key, digest, err.code, now, retained_until],
        );
        let _ = tx.commit();
    }
    err
}

async fn documents_preview(
    rt: Arc<PublicV1Runtime>,
    input: Value,
    idempotency_key: Option<String>,
) -> Result<(ResponseOutcome, Value), DispatchError> {
    let store = rt.store().clone();
    let digest = compute_input_digest("documents.preview", &input);

    // 1. 幂等预检（run.submit / search.start 同序：已接受直接重放、冲突报 IDEMPOTENCY_CONFLICT）。
    if let Some(ref key) = idempotency_key {
        let store_check = store.clone();
        let key_check = key.clone();
        let digest_check = digest.clone();
        let replay = tokio::task::spawn_blocking(move || {
            let conn = store_check.conn.lock().expect("sqlite mutex");
            check_preview_idempotency_in_conn(&conn, &key_check, &digest_check)
        })
        .await
        .map_err(join_err)??;
        if let Some(receipt) = replay {
            return Ok((ResponseOutcome::Accepted, receipt));
        }
    }

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

    let reject_in_blocking = |code: &str, message: String| {
        let err = ContractError::new(code, message, RetrySpec::never(), EffectStatus::NotApplied);
        let store_rej = store.clone();
        let key_clone = idempotency_key.clone();
        let digest_clone = digest.clone();
        async move {
            let err = tokio::task::spawn_blocking(move || {
                record_preview_rejection_if_needed(
                    &store_rej,
                    key_clone.as_deref(),
                    &digest_clone,
                    err,
                )
            })
            .await
            .map_err(join_err)?;
            Err::<(ResponseOutcome, Value), DispatchError>(DispatchError::Application(err))
        }
    };

    // 2. 分页参数与输出类型校验（先于读取潜在 128 MiB 源文件）。
    match output.as_str() {
        "spreadsheetSheet" => {
            if sheet_id.is_none() {
                return reject_in_blocking(
                    CODE_INVALID_ARGUMENT,
                    "output=spreadsheetSheet requires sheetId".to_owned(),
                )
                .await;
            }
            if matches!(row_count, Some(0)) {
                return reject_in_blocking(
                    CODE_INVALID_ARGUMENT,
                    "rowCount must be at least 1".to_owned(),
                )
                .await;
            }
            if page_start.is_some() || page_count.is_some() {
                return reject_in_blocking(
                    CODE_INVALID_ARGUMENT,
                    "pageStart/pageCount are not supported by this build's PDF converter; \
                     use rowStart/rowCount for sheets"
                        .to_owned(),
                )
                .await;
            }
        }
        "pdf" | "html" | "text" => {
            if row_start.is_some() || row_count.is_some() || sheet_id.is_some() {
                return reject_in_blocking(
                    CODE_INVALID_ARGUMENT,
                    "rowStart/rowCount/sheetId apply to spreadsheetSheet output only".to_owned(),
                )
                .await;
            }
            if page_start.is_some() || page_count.is_some() {
                return reject_in_blocking(
                    CODE_INVALID_ARGUMENT,
                    "pageStart/pageCount are not supported by this build's PDF converter \
                     (pilotdeck-pdf `text` extracts whole documents)"
                        .to_owned(),
                )
                .await;
            }
        }
        "spreadsheetManifest" => {
            if row_start.is_some()
                || row_count.is_some()
                || sheet_id.is_some()
                || page_start.is_some()
                || page_count.is_some()
            {
                return reject_in_blocking(
                    CODE_INVALID_ARGUMENT,
                    "spreadsheetManifest output takes no paging parameters".to_owned(),
                )
                .await;
            }
        }
        other => {
            return reject_in_blocking(
                CODE_INVALID_ARGUMENT,
                format!("unsupported preview output `{other}`"),
            )
            .await;
        }
    }

    // 3. 源解析（带 128 MiB 预检与 NOFOLLOW/NONBLOCK 读）及 revision/格式/依赖核查。
    let preview = {
        let store_src = store.clone();
        let source_val = source.clone();
        let key_clone = idempotency_key.clone();
        let digest_clone = digest.clone();
        tokio::task::spawn_blocking(move || {
            resolve_preview_source(&store_src, &source_val).map_err(|err| {
                record_preview_rejection_if_needed(
                    &store_src,
                    key_clone.as_deref(),
                    &digest_clone,
                    err,
                )
            })
        })
        .await
        .map_err(join_err)??
    };
    if preview.revision != expected_revision {
        let err = ContractError::new(
            CODE_REVISION_CONFLICT,
            format!(
                "preview source revision `{}` does not match expectedRevision `{expected_revision}`",
                preview.revision
            ),
            RetrySpec::after_reconcile(),
            EffectStatus::NotApplied,
        );
        let store_rej = store.clone();
        let key_clone = idempotency_key.clone();
        let digest_clone = digest.clone();
        let err = tokio::task::spawn_blocking(move || {
            record_preview_rejection_if_needed(&store_rej, key_clone.as_deref(), &digest_clone, err)
        })
        .await
        .map_err(join_err)?;
        return Err(DispatchError::Application(err));
    }

    let needs = |format: InputFormat, output: &str| -> bool {
        matches!(
            (format, output),
            (InputFormat::Text, "text")
                | (InputFormat::Csv, "text")
                | (InputFormat::Csv, "spreadsheetManifest")
                | (InputFormat::Csv, "spreadsheetSheet")
                | (InputFormat::Xlsx, "spreadsheetManifest")
                | (InputFormat::Xlsx, "spreadsheetSheet")
                | (InputFormat::Pdf, "text")
                | (InputFormat::Office, "pdf")
                | (InputFormat::Office, "text")
                | (InputFormat::Office, "html")
        )
    };
    if !needs(preview.format, &output) {
        return reject_in_blocking(
            CODE_UNSUPPORTED_FORMAT,
            format!(
                "preview output `{output}` is not supported for input format `{}`",
                preview.format.label()
            ),
        )
        .await;
    }
    match preview.format {
        InputFormat::Office if soffice_binary().await.is_none() => {
            return Err(DispatchError::Application(ContractError::new(
                CODE_DEPENDENCY_UNAVAILABLE,
                "Office conversion requires LibreOffice (soffice); install it or set SOFFICE_PATH",
                RetrySpec::never(),
                EffectStatus::NotApplied,
            )));
        }
        InputFormat::Pdf => {
            let available = tokio::task::spawn_blocking(pdf_component_available)
                .await
                .map_err(join_err)?;
            if !available {
                return Err(DispatchError::Application(ContractError::new(
                    CODE_DEPENDENCY_UNAVAILABLE,
                    "PDF text extraction requires the pilotdeck-pdf (MuPDF) component; install it or set PILOTDECK_PDF_TOOL",
                    RetrySpec::never(),
                    EffectStatus::NotApplied,
                )));
            }
        }
        _ => {}
    }

    // 4. 原子受理事务（TransactionBehavior::Immediate + 幂等复验 + async_operations + idempotency_records）。
    let store_accept = store.clone();
    let idemp_accept = idempotency_key.clone();
    let digest_accept = digest.clone();
    let (operation_id, receipt, deduplicated) = tokio::task::spawn_blocking(move || {
        let mut conn = store_accept.conn.lock().expect("sqlite mutex");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        if let Some(ref key) = idemp_accept
            && let Some(replay) = check_preview_idempotency_in_conn(&tx, key, &digest_accept)?
        {
            tx.commit().map_err(sql_err)?;
            let op_id = replay["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            return Ok::<_, ContractError>((op_id, replay, true));
        }
        let operation_id = store_accept.next_id();
        let accepted_revision =
            uploads::create_async_operation(&tx, "documents.preview", "documents", &operation_id)?;
        let receipt = json!({
            "kind": "operation",
            "operationId": operation_id,
            "durable": true,
            "acceptedRevision": accepted_revision,
            "deduplicated": false,
            "state": "running",
        });
        if let Some(ref key) = idemp_accept {
            let now = now_rfc3339();
            let retained_until = (chrono::Utc::now()
                + chrono::Duration::days(PREVIEW_IDEMPOTENCY_RETENTION_DAYS))
            .to_rfc3339();
            tx.execute(
                "INSERT INTO idempotency_records
                 (idempotency_key, method_id, input_digest, request_state, reason, resource_json,
                  receipt_json, rejection_code, observed_at, retained_until, retry_disposition)
                 VALUES (?1, 'documents.preview', ?2, 'accepted', 'receiptFound', ?3, ?4, NULL,
                         ?5, ?6, 'doNotReplay')",
                params![
                    key,
                    digest_accept,
                    serde_json::to_string(
                        &json!({"kind": "operation", "operationId": operation_id})
                    )
                    .unwrap_or_default(),
                    serde_json::to_string(&receipt).unwrap_or_default(),
                    now,
                    retained_until,
                ],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok((operation_id, receipt, false))
    })
    .await
    .map_err(join_err)??;

    if deduplicated {
        return Ok((ResponseOutcome::Accepted, receipt));
    }

    let driver = PreviewJob {
        store: store.clone(),
        operation_id: operation_id.clone(),
        source: preview,
        output,
        sheet_id,
        row_start: row_start.unwrap_or(0),
        row_count: row_count.unwrap_or(DEFAULT_ROW_COUNT),
    };
    let guard_store = store.clone();
    let guard_op = operation_id;
    tokio::task::spawn(async move {
        if let Err(err) = tokio::task::spawn(driver.run()).await {
            let msg = format!("documents.preview worker panicked: {err}");
            let _ = tokio::task::spawn_blocking(move || {
                let _ = uploads::finish_async_operation(
                    &guard_store,
                    &guard_op,
                    "failed",
                    None,
                    Some((CODE_UNAVAILABLE, msg.as_str())),
                );
            })
            .await;
        }
    });

    Ok((ResponseOutcome::Accepted, receipt))
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

fn build_preview_completion(
    source_revision: &str,
    artifacts: Value,
    manifest_schema_id: Option<&str>,
    truncated: bool,
    next_range: Option<&str>,
) -> Value {
    let mut out = json!({
        "sourceRevision": source_revision,
        "artifacts": artifacts,
        "truncated": truncated,
    });
    if let Some(schema) = manifest_schema_id {
        out["manifestSchemaId"] = json!(schema);
    }
    if let Some(next) = next_range {
        out["nextRange"] = json!(next);
    }
    out
}

impl PreviewJob {
    fn finish_sync(&self, completion: Option<&Value>, error: Option<(&str, &str)>) {
        let state = if completion.is_some() {
            "succeeded"
        } else {
            "failed"
        };
        let _ = uploads::finish_async_operation(
            &self.store,
            &self.operation_id,
            state,
            completion,
            error,
        );
    }

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

        // 缓存读与命中终态化全部跑在 spawn_blocking（不在 async worker 读盘或锁 SQLite）。
        let store_cache = self.store.clone();
        let key_cache = cache_key.clone();
        let op_cache = self.operation_id.clone();
        let rev_cache = self.source.revision.clone();
        let cache_hit = tokio::task::spawn_blocking(move || {
            if let Some(cached) = read_cache_entry(&store_cache, &key_cache)
                && cached["sourceRevision"] == json!(rev_cache)
            {
                let completion = build_preview_completion(
                    &rev_cache,
                    cached["artifacts"].clone(),
                    cached["manifestSchemaId"].as_str(),
                    cached["truncated"].as_bool().unwrap_or(false),
                    cached["nextRange"].as_str(),
                );
                let _ = uploads::finish_async_operation(
                    &store_cache,
                    &op_cache,
                    "succeeded",
                    Some(&completion),
                    None,
                );
                return true;
            }
            false
        })
        .await
        .unwrap_or(false);
        if cache_hit {
            return;
        }

        let job = Arc::new(self);
        let result = job.convert().await;
        let job_fin = Arc::clone(&job);
        let key_write = cache_key;
        let _ = tokio::task::spawn_blocking(move || match result {
            Ok(outcome) => {
                let completion = build_preview_completion(
                    &job_fin.source.revision,
                    json!(outcome.artifacts),
                    outcome.manifest_schema_id,
                    outcome.truncated,
                    outcome.next_range.as_deref(),
                );
                write_cache_entry(&job_fin.store, &key_write, &completion);
                job_fin.finish_sync(Some(&completion), None);
            }
            Err((code, message)) => {
                job_fin.finish_sync(None, Some((code.as_str(), message.as_str())));
            }
        })
        .await;
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
            (InputFormat::Text, "text")
            | (InputFormat::Csv, "text")
            | (InputFormat::Csv, "spreadsheetManifest")
            | (InputFormat::Csv, "spreadsheetSheet")
            | (InputFormat::Xlsx, "spreadsheetManifest")
            | (InputFormat::Xlsx, "spreadsheetSheet") => {
                let store = self.store.clone();
                let source_bytes = self.source.bytes.clone();
                let display_name = self.source.display_name.clone();
                let format = self.source.format;
                let output = self.output.clone();
                let sheet_id = self.sheet_id.clone();
                let row_start = self.row_start;
                let row_count = self.row_count;
                let project_id = project.clone();
                tokio::task::spawn_blocking(move || {
                    convert_sync_document(
                        &store,
                        &project_id,
                        format,
                        &output,
                        &display_name,
                        &source_bytes,
                        sheet_id.as_deref(),
                        row_start,
                        row_count,
                    )
                    .map_err(fail)
                })
                .await
                .map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("worker panicked: {err}"),
                    )
                })?
            }
            (InputFormat::Pdf, "text") => {
                let document = crate::pdf::Document::from_bytes(&self.source.bytes)
                    .map_err(|err| pdf_error(&err))
                    .map_err(fail)?;
                let text = document
                    .text()
                    .await
                    .map_err(|err| pdf_error(&err))
                    .map_err(fail)?;
                let store = self.store.clone();
                let display_name = self.source.display_name.clone();
                tokio::task::spawn_blocking(move || {
                    let artifact = artifacts::publish_artifact(
                        &store,
                        &project,
                        "text/plain; charset=utf-8",
                        Some(&display_name),
                        text.as_bytes(),
                        "preview",
                    )?;
                    note_artifact_event_sync(&store, &artifact);
                    Ok::<_, ContractError>(PreviewOutput {
                        artifacts: vec![artifact],
                        manifest_schema_id: None,
                        truncated: false,
                        next_range: None,
                    })
                })
                .await
                .map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("worker panicked: {err}"),
                    )
                })?
                .map_err(fail)
            }
            (InputFormat::Office, "pdf")
            | (InputFormat::Office, "text")
            | (InputFormat::Office, "html") => {
                let binary = soffice_binary().await.cloned().ok_or_else(|| {
                    (
                        CODE_DEPENDENCY_UNAVAILABLE.to_owned(),
                        "LibreOffice (soffice) is not available".to_owned(),
                    )
                })?;
                let store_prep = self.store.clone();
                let bytes_prep = self.source.bytes.clone();
                let display_prep = self.source.display_name.clone();
                let (workdir_guard, staged_input) = tokio::task::spawn_blocking(move || {
                    let workdir = previews_cache_dir(&store_prep)
                        .join(format!(".work-{}", store_prep.next_id()));
                    std::fs::create_dir_all(&workdir).map_err(|err| {
                        (
                            CODE_UNAVAILABLE.to_owned(),
                            format!("preview workdir failed: {err}"),
                        )
                    })?;
                    let guard = WorkdirGuard(workdir.clone());
                    let ext = whitelist_office_ext(&display_prep);
                    let staged_input = workdir.join(format!("input.{ext}"));
                    std::fs::write(&staged_input, &bytes_prep).map_err(|err| {
                        (
                            CODE_UNAVAILABLE.to_owned(),
                            format!("preview staging failed: {err}"),
                        )
                    })?;
                    Ok::<_, (String, String)>((guard, staged_input))
                })
                .await
                .map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("worker panicked: {err}"),
                    )
                })??;

                let to_format = match self.output.as_str() {
                    "pdf" => "pdf",
                    "text" => "txt:Text",
                    _ => "html",
                };
                let produced = soffice_convert(&binary, &staged_input, &workdir_guard.0, to_format)
                    .await
                    .map_err(fail)?;
                let store_fin = self.store.clone();
                let display_fin = self.source.display_name.clone();
                let media_type = match self.output.as_str() {
                    "pdf" => "application/pdf",
                    "text" => "text/plain; charset=utf-8",
                    _ => "text/html; charset=utf-8",
                };
                tokio::task::spawn_blocking(move || {
                    let _guard = workdir_guard;
                    let bytes = read_bounded_regular_file(&produced, MAX_INPUT_BYTES)?;
                    let artifact = artifacts::publish_artifact(
                        &store_fin,
                        &project,
                        media_type,
                        Some(&display_fin),
                        &bytes,
                        "preview",
                    )?;
                    note_artifact_event_sync(&store_fin, &artifact);
                    Ok::<_, ContractError>(PreviewOutput {
                        artifacts: vec![artifact],
                        manifest_schema_id: None,
                        truncated: false,
                        next_range: None,
                    })
                })
                .await
                .map_err(|err| {
                    (
                        CODE_UNAVAILABLE.to_owned(),
                        format!("worker panicked: {err}"),
                    )
                })?
                .map_err(fail)
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
}

fn note_artifact_event_sync(store: &V1Store, artifact: &Value) {
    if let Some(artifact_id) = artifact["artifactId"].as_str() {
        let _ = uploads::append_domain_event(
            store,
            artifact_id,
            "artifacts.created",
            "agentcpm-pd.artifact.event/1",
            &json!({"artifactId": artifact_id, "origin": "preview"}),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn convert_sync_document(
    store: &V1Store,
    project: &str,
    format: InputFormat,
    output: &str,
    display_name: &str,
    source_bytes: &[u8],
    sheet_id: Option<&str>,
    row_start: u64,
    row_count: u64,
) -> Result<PreviewOutput, ContractError> {
    match (format, output) {
        (InputFormat::Text, "text") => {
            let text = std::str::from_utf8(source_bytes)
                .map_err(|_| malformed("text preview requires valid UTF-8 bytes"))?;
            let artifact = artifacts::publish_artifact(
                store,
                project,
                "text/plain; charset=utf-8",
                Some(display_name),
                text.as_bytes(),
                "preview",
            )?;
            note_artifact_event_sync(store, &artifact);
            Ok(PreviewOutput {
                artifacts: vec![artifact],
                manifest_schema_id: None,
                truncated: false,
                next_range: None,
            })
        }
        (InputFormat::Csv, "text") => {
            let text = String::from_utf8_lossy(source_bytes);
            let artifact = artifacts::publish_artifact(
                store,
                project,
                "text/plain; charset=utf-8",
                Some(display_name),
                text.as_bytes(),
                "preview",
            )?;
            note_artifact_event_sync(store, &artifact);
            Ok(PreviewOutput {
                artifacts: vec![artifact],
                manifest_schema_id: None,
                truncated: false,
                next_range: None,
            })
        }
        (InputFormat::Csv, "spreadsheetManifest") | (InputFormat::Csv, "spreadsheetSheet") => {
            let text = String::from_utf8_lossy(source_bytes);
            let dense = parse_csv(&text, MAX_CELLS);
            let sheets = vec![Sheet::from_dense(0, display_name.to_owned(), dense)];
            if output == "spreadsheetManifest" {
                manifest_output(store, project, display_name, &sheets)
            } else {
                sheet_output(
                    store,
                    project,
                    display_name,
                    &sheets,
                    sheet_id,
                    row_start,
                    row_count,
                )
            }
        }
        (InputFormat::Xlsx, "spreadsheetManifest") | (InputFormat::Xlsx, "spreadsheetSheet") => {
            let sheets = parse_xlsx(source_bytes, MAX_CELLS)?;
            if output == "spreadsheetManifest" {
                manifest_output(store, project, display_name, &sheets)
            } else {
                sheet_output(
                    store,
                    project,
                    display_name,
                    &sheets,
                    sheet_id,
                    row_start,
                    row_count,
                )
            }
        }
        _ => Err(ContractError::new(
            CODE_UNSUPPORTED_FORMAT,
            format!(
                "preview output `{output}` is not supported for format `{}`",
                format.label()
            ),
            RetrySpec::never(),
            EffectStatus::NotApplied,
        )),
    }
}

fn manifest_output(
    store: &V1Store,
    project: &str,
    display_name: &str,
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
                    "rowCount": sheet.row_count,
                    "columnCount": sheet.column_count(),
                })
            })
            .collect::<Vec<_>>(),
    });
    let artifact = artifacts::publish_artifact(
        store,
        project,
        "application/json",
        Some(display_name),
        serde_json::to_vec(&manifest).unwrap_or_default().as_slice(),
        "preview",
    )?;
    note_artifact_event_sync(store, &artifact);
    Ok(PreviewOutput {
        artifacts: vec![artifact],
        manifest_schema_id: Some("documents.spreadsheetManifest.v1"),
        truncated: false,
        next_range: None,
    })
}

fn sheet_output(
    store: &V1Store,
    project: &str,
    display_name: &str,
    sheets: &[Sheet],
    sheet_id: Option<&str>,
    row_start: u64,
    row_count: u64,
) -> Result<PreviewOutput, ContractError> {
    let wanted = sheet_id.unwrap_or("0");
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
    let total_rows = sheet.row_count as u64;
    let start = row_start.min(total_rows);
    let page_limit = row_count.clamp(1, MAX_CELLS);
    let end = start.saturating_add(page_limit).min(total_rows);
    let mut rows: Vec<Value> = Vec::with_capacity((end - start).min(4096) as usize);
    for idx in (start as usize)..(end as usize) {
        if let Some(row) = sheet.rows.get(&idx) {
            rows.push(json!(row));
        } else {
            rows.push(json!([]));
        }
    }
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
        store,
        project,
        "application/json",
        Some(display_name),
        serde_json::to_vec(&payload).unwrap_or_default().as_slice(),
        "preview",
    )?;
    note_artifact_event_sync(store, &artifact);
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
    use base64::Engine;
    use rusqlite::params;

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
            let row: Option<(String, Option<String>, Option<String>)> = {
                let store = rt.store();
                let conn = store.conn.lock().expect("sqlite mutex");
                conn.query_row(
                    "SELECT state, result_json, error_code FROM async_operations WHERE operation_id = ?1",
                    params![operation_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok()
            };
            if let Some((state, result_json, error_code)) = row
                && state != "running"
            {
                return json!({
                    "state": state,
                    "result": result_json
                        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                        .unwrap_or(Value::Null),
                    "errorCode": error_code,
                });
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
                    let mut encoder = flate2::write::DeflateEncoder::new(
                        Vec::new(),
                        flate2::Compression::default(),
                    );
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
            central.extend_from_slice(&((*data).len() as u32).to_le_bytes());
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
        let _csv_sha = sha256_hex(csv.as_bytes());
        let csv_artifact = artifacts::publish_artifact(
            store,
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
                "documents.preview",
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
                "documents.preview",
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
        // 4 行的表取 rowStart=1 rowCount=2：页后还有第 3 行 → truncated，
        // nextRange 指向续页（truncated 的语义是「页后仍有行」，与
        // nextRange 的续读令牌配套）。
        assert_eq!(completion["truncated"], json!(true));
        assert_eq!(completion["nextRange"], json!("rowStart=3"));
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
                "documents.preview",
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
            store,
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
                "documents.preview",
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
                "documents.preview",
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
                "documents.preview",
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
            store,
            "proj-docs",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Some("broken.xlsx"),
            b"this is not a zip at all, definitely not",
            "upload",
        )
        .unwrap();
        // 损坏 xlsx 的受理是异步失败：受理面只验源存在/大小/格式，zip
        // 完整性在后台任务里验（受理回执 running → 终态 failed +
        // MALFORMED_DOCUMENT，绝不当空文档成功）。
        let (_, broken_receipt) = rt
            .dispatch(
                "documents.preview",
                json!({
                    "source": {"artifactId": broken["artifactId"].as_str().unwrap()},
                    "expectedRevision": broken["revision"].as_str().unwrap(),
                    "output": "spreadsheetManifest",
                }),
                Some("key-docs-broken"),
                None,
            )
            .await
            .unwrap();
        let broken_terminal =
            wait_preview_terminal(&rt, broken_receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(broken_terminal["state"], json!("failed"));
        assert_eq!(broken_terminal["errorCode"], json!("MALFORMED_DOCUMENT"));
    }

    fn build_custom_xlsx(sheet1_xml: &[u8], sheet1_method: u16) -> Vec<u8> {
        let workbook = br#"<?xml version="1.0"?><workbook><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = br#"<?xml version="1.0"?><Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let entries: Vec<(&str, &[u8], u16)> = vec![
            ("xl/workbook.xml", workbook, 0),
            ("xl/_rels/workbook.xml.rels", rels, 0),
            ("xl/worksheets/sheet1.xml", sheet1_xml, sheet1_method),
        ];
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut offset = 0u32;
        for (name, data, method) in &entries {
            let body = match *method {
                8 => {
                    let mut encoder =
                        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                    use std::io::Write;
                    encoder.write_all(data).unwrap();
                    encoder.finish().unwrap()
                }
                _ => data.to_vec(),
            };
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);

            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
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

    #[test]
    fn xlsx_zip_bomb_and_sparse_row_oom_are_rejected() {
        // 1) Deflate Zip Bomb：>64 MiB 解压负载必须被 zip_read 截断报 RESOURCE_LIMIT。
        let huge = vec![b'A'; 65 * 1024 * 1024];
        let bomb = build_custom_xlsx(&huge, 8);
        assert!(bomb.len() < 512 * 1024, "compressed zip bomb stays tiny");
        let err = parse_xlsx(&bomb, MAX_CELLS)
            .err()
            .expect("zip bomb rejected");
        assert_eq!(err.code, CODE_RESOURCE_LIMIT);

        // 2) 稀疏极端行号 <row r="1000000000"></row> 不得分配 10 亿 Vec。
        let sparse_extreme =
            br#"<?xml version="1.0"?><worksheet><sheetData><row r="1000000000"></row></sheetData></worksheet>"#;
        let sparse_zip = build_custom_xlsx(sparse_extreme, 0);
        let err = parse_xlsx(&sparse_zip, MAX_CELLS)
            .err()
            .expect("extreme sparse row rejected");
        assert_eq!(err.code, CODE_RESOURCE_LIMIT);

        // 3) 合法大稀疏行（如 r="50000"）只存非空行，零空行 Vec 分配。
        let valid_sparse =
            br#"<?xml version="1.0"?><worksheet><sheetData><row r="40000"/><row r="50000"><c r="A50000" t="inlineStr"><is><t>leaf</t></is></c></row></sheetData></worksheet>"#;
        let valid_zip = build_custom_xlsx(valid_sparse, 8);
        let sheets = parse_xlsx(&valid_zip, MAX_CELLS).unwrap();
        assert_eq!(sheets[0].row_count, 50_000);
        assert_eq!(sheets[0].rows.len(), 1);
        assert_eq!(
            sheets[0].rows.get(&49_999).unwrap(),
            &vec!["leaf".to_owned()]
        );
    }

    #[tokio::test]
    async fn preview_source_size_limit_and_symlink_fifo_rejected() {
        let tmp = TempDir::new("docs-p0-source");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, _iid) = open_runtime(&root_dir);
        let store = rt.store();

        // 1) Artifact 行声明 >128 MiB 时预检直接报 RESOURCE_LIMIT（不读入内存）。
        let art = artifacts::publish_artifact(
            store,
            "proj-docs",
            "text/plain",
            Some("big.txt"),
            b"small",
            "upload",
        )
        .unwrap();
        let art_id = art["artifactId"].as_str().unwrap().to_owned();
        let rev = art["revision"].as_str().unwrap().to_owned();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE artifacts SET byte_length = 200000000 WHERE artifact_id = ?1",
                params![art_id],
            )
            .unwrap();
        }
        let err = rt
            .dispatch(
                "documents.preview",
                json!({
                    "source": {"artifactId": art_id},
                    "expectedRevision": rev,
                    "output": "text",
                }),
                Some("key-docs-oversize"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, CODE_RESOURCE_LIMIT),
            other => panic!("expected RESOURCE_LIMIT, got {other:?}"),
        }

        // 2) 项目文件 FIFO 与越界符号链接在 resolve_preview_source 必须 fail-closed。
        #[cfg(unix)]
        {
            let declared_root = root_dir.parent().unwrap().join("authorized");
            let workspace = declared_root.join("ws");
            std::fs::create_dir_all(&workspace).unwrap();
            {
                let conn = store.conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO authorized_roots (root_id, instance_id, absolute_path, created_at)
                     VALUES ('root-docs', ?1, ?2, ?3)",
                    params![
                        store.meta().instance_id,
                        declared_root.display().to_string(),
                        now_rfc3339(),
                    ],
                )
                .unwrap();
            }
            store
                .register_project(&json!({
                    "projectId": "proj-sec",
                    "name": "Docs Sec",
                    "environmentId": "local",
                    "root": {"instanceId": store.meta().instance_id, "rootId": "root-docs", "relativePath": "ws"},
                }))
                .unwrap();

            let outside = declared_root.parent().unwrap().join("outside.txt");
            std::fs::write(&outside, b"secret").unwrap();
            std::os::unix::fs::symlink(&outside, workspace.join("leak.txt")).unwrap();
            let err = resolve_preview_source(
                store,
                &json!({"path": {"projectId": "proj-sec", "relativePath": {"encoding": "utf8", "value": "leak.txt"}}}),
            )
            .err()
            .expect("escaping symlink refused");
            assert_eq!(err.code, crate::public_v1::domains::files::CODE_PATH_ESCAPE);

            let fifo_path = workspace.join("pipe.txt");
            let st = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("mkfifo");
            assert!(st.success());
            let err = resolve_preview_source(
                store,
                &json!({"path": {"projectId": "proj-sec", "relativePath": {"encoding": "utf8", "value": "pipe.txt"}}}),
            )
            .err()
            .expect("FIFO refused without hanging");
            assert_eq!(err.code, CODE_INVALID_ARGUMENT);
        }
    }

    #[tokio::test]
    async fn sheet_pagination_u64_max_does_not_overflow_or_panic() {
        let tmp = TempDir::new("docs-u64-max");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, _iid) = open_runtime(&root_dir);
        let csv_artifact = artifacts::publish_artifact(
            rt.store(),
            "proj-docs",
            "text/csv",
            Some("grid.csv"),
            b"a,b\n1,2\n3,4\n5,6\n",
            "upload",
        )
        .unwrap();
        let (_, receipt) = rt
            .dispatch(
                "documents.preview",
                json!({
                    "source": {"artifactId": csv_artifact["artifactId"]},
                    "expectedRevision": csv_artifact["revision"],
                    "output": "spreadsheetSheet",
                    "sheetId": "0",
                    "rowStart": 1,
                    "rowCount": u64::MAX,
                }),
                Some("key-docs-u64max"),
                None,
            )
            .await
            .unwrap();
        let terminal = wait_preview_terminal(&rt, receipt["operationId"].as_str().unwrap()).await;
        assert_eq!(terminal["state"], json!("succeeded"));
        assert_eq!(terminal["result"]["truncated"], json!(false));
        assert!(terminal["result"].get("nextRange").is_none());
    }

    #[tokio::test]
    async fn soffice_hardening_timeout_profile_and_workdir_cleanup() {
        let tmp = TempDir::new("docs-lo-harden");
        let profile = tmp.0.join("space dir").join(".lo-profile");
        prepare_libreoffice_profile(&profile).unwrap();
        let xcu = std::fs::read_to_string(profile.join("user/registrymodifications.xcu")).unwrap();
        assert!(xcu.contains("DisableMacrosExecution"));
        assert!(xcu.contains("MacroSecurityLevel"));
        assert!(xcu.contains("BlockUntrustedRefererLinks"));
        assert!(xcu.contains("org.openoffice.Office.Calc/Content/Update"));
        assert_eq!(whitelist_office_ext("a/../evil.DOCX"), "docx");
        assert_eq!(whitelist_office_ext("a/../../payload"), "bin");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = tmp.0.join("hang-soffice.sh");
            std::fs::write(&script, b"#!/bin/sh\nsleep 10 &\nwait\n").unwrap();
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();

            assert!(!probe_soffice_candidate(&script, Duration::from_millis(150)).await);

            let workdir = tmp.0.join("workdir-test");
            std::fs::create_dir_all(&workdir).unwrap();
            let err = {
                let guard = WorkdirGuard(workdir.clone());
                let input = guard.0.join("input.docx");
                std::fs::write(&input, b"dummy").unwrap();
                soffice_convert_with_timeout(
                    &script,
                    &input,
                    &guard.0,
                    "pdf",
                    Duration::from_millis(200),
                )
                .await
                .unwrap_err()
            };
            assert_eq!(err.code, CODE_CONVERSION_TIMEOUT);
            assert!(
                !workdir.exists(),
                "WorkdirGuard must delete workdir on drop"
            );
        }
    }

    #[tokio::test]
    async fn preview_idempotency_requests_lookup_and_completion_schema_omit_nulls() {
        let tmp = TempDir::new("docs-idemp");
        let root_dir = tmp.0.join("pd-home");
        initialize_instance(&root_dir, None, Some("local"), Some(true)).unwrap();
        let (rt, _iid) = open_runtime(&root_dir);
        let art = artifacts::publish_artifact(
            rt.store(),
            "proj-docs",
            "text/plain",
            Some("hello.txt"),
            b"hello world",
            "upload",
        )
        .unwrap();

        let input = json!({
            "source": {"artifactId": art["artifactId"]},
            "expectedRevision": art["revision"],
            "output": "text",
        });
        let (_, first) = rt
            .dispatch(
                "documents.preview",
                input.clone(),
                Some("key-docs-idemp"),
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("documents.preview", &first)
            .unwrap();
        assert_eq!(first["deduplicated"], json!(false));

        // 重放同 key + 同参数 → deduplicated=true 且同 operationId。
        let (_, replay) = rt
            .dispatch("documents.preview", input, Some("key-docs-idemp"), None)
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("documents.preview", &replay)
            .unwrap();
        assert_eq!(replay["deduplicated"], json!(true));
        assert_eq!(replay["operationId"], first["operationId"]);

        // requests.lookup 可查到 {"kind":"operation","operationId":...}。
        let (_, looked_up) = rt
            .dispatch(
                "requests.lookup",
                json!({"methodId": "documents.preview", "idempotencyKey": "key-docs-idemp"}),
                None,
                None,
            )
            .await
            .unwrap();
        OperationRegistry::global()
            .validate_result("requests.lookup", &looked_up)
            .unwrap();
        assert_eq!(
            looked_up["resource"],
            json!({"kind": "operation", "operationId": first["operationId"]})
        );

        // 同 key 不同输入 → IDEMPOTENCY_CONFLICT。
        let err = rt
            .dispatch(
                "documents.preview",
                json!({
                    "source": {"artifactId": art["artifactId"]},
                    "expectedRevision": "arev-wrong",
                    "output": "text",
                }),
                Some("key-docs-idemp"),
                None,
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::Application(e) => assert_eq!(e.code, CODE_IDEMPOTENCY_CONFLICT),
            other => panic!("expected IDEMPOTENCY_CONFLICT, got {other:?}"),
        }

        // completionResult 不得含 manifestSchemaId: null / nextRange: null。
        let terminal = wait_preview_terminal(&rt, first["operationId"].as_str().unwrap()).await;
        assert_eq!(terminal["state"], json!("succeeded"));
        let res = &terminal["result"];
        assert!(res.get("manifestSchemaId").is_none());
        assert!(res.get("nextRange").is_none());
        assert_eq!(res["sourceRevision"], art["revision"]);
        assert_eq!(res["truncated"], json!(false));
    }
}
