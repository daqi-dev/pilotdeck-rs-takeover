//! Pure/bounded workspace file operations (`read_file`, `write_file`, `edit_file`)
//! enforced by [`crate::fs_guard::WorkspaceBoundary`].

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use pilotdeck_protocol::{JsObject, JsString, JsValue, parse_json};
use sha2::{Digest, Sha256};

use crate::fs_guard::{BoundaryError, BoundaryErrorKind, WorkspaceBoundary};

pub const READ_FILE_TOOL_NAME: &str = "read_file";
pub const WRITE_FILE_TOOL_NAME: &str = "write_file";
pub const EDIT_FILE_TOOL_NAME: &str = "edit_file";

pub const DEFAULT_MAX_READ_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_READ_LINE_BYTES: usize = 20_000;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const OUTPUT_TRUNCATED_MARKER: &str = "\n... [truncated]";
const LINE_TRUNCATED_MARKER: &str = "... [line truncated]";
const TEMP_FILE_RETRIES: usize = 64;
const DIFF_CONTEXT_LINES: usize = 3;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

const READ_DESCRIPTION: &str = "Reads a UTF-8 text file inside the current workspace.\n\nUsage:\n- The file_path parameter may be relative to the workspace or an absolute path inside the workspace.\n- Optional 1-based offset and limit paginate large files.\n- Output uses numbered lines (`1|...`), enforces a byte budget, and rejects binary/NUL files.\n- Returns structured JSON including content, totalLines, startLine, endLine, truncated, sha256, and byteSize.";

const READ_INPUT_SCHEMA: &str = r#"{"type":"object","required":["file_path"],"additionalProperties":false,"properties":{"file_path":{"type":"string","minLength":1,"description":"Relative or absolute path to the file to read inside the workspace."},"offset":{"type":"integer","minimum":1,"description":"The 1-based line number to start reading from (defaults to 1)."},"limit":{"type":"integer","minimum":0,"description":"Maximum number of lines to read."}}}"#;

const WRITE_DESCRIPTION: &str = "Writes a UTF-8 text file inside the current workspace.\n\nUsage:\n- Creates missing parent directories strictly inside the workspace boundary.\n- Supports optional expected_sha256 optimistic CAS lock.\n- Writes atomically via a temporary file in the parent directory, fsync, and rename.";

const WRITE_INPUT_SCHEMA: &str = r#"{"type":"object","required":["file_path","content"],"additionalProperties":false,"properties":{"file_path":{"type":"string","minLength":1,"description":"Relative or absolute path to the file to write inside the workspace."},"content":{"type":"string","description":"The UTF-8 text content to write to the file."},"expected_sha256":{"type":"string","minLength":1,"description":"Optional expected SHA-256 hex digest of the existing file for optimistic CAS locking."}}}"#;

const EDIT_DESCRIPTION: &str = "Edits an existing UTF-8 text file inside the current workspace by exact string replacement.\n\nUsage:\n- Fails if old_string occurs 0 times or more than once unless replace_all is true.\n- Supports optional expected_sha256 optimistic CAS lock.\n- Writes atomically via a temporary file in the parent directory, fsync, and rename.";

const EDIT_INPUT_SCHEMA: &str = r#"{"type":"object","required":["file_path","old_string","new_string"],"additionalProperties":false,"properties":{"file_path":{"type":"string","minLength":1,"description":"Relative or absolute path to the file to edit inside the workspace."},"old_string":{"type":"string","minLength":1,"description":"Exact non-empty substring to find and replace."},"new_string":{"type":"string","description":"Replacement text."},"replace_all":{"type":"boolean","default":false,"description":"When true, replace all non-overlapping occurrences; defaults to false (requires unique match)."},"expected_sha256":{"type":"string","minLength":1,"description":"Optional expected SHA-256 hex digest of the existing file for optimistic CAS locking."}}}"#;

/// Machine-readable error category for workspace file operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileOpErrorKind {
    Boundary(BoundaryErrorKind),
    InvalidInput,
    NotFound,
    NotAFile,
    BinaryFile,
    CasMismatch,
    StringNotFound,
    AmbiguousMatch,
    Io,
}

/// Structured error returned by [`read_file`], [`write_file`], and [`edit_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOpError {
    pub kind: FileOpErrorKind,
    pub code: String,
    pub message: String,
}

impl FileOpError {
    pub fn new(kind: FileOpErrorKind, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
        }
    }

    fn from_io(err: std::io::Error, path_hint: &str) -> Self {
        let (kind, code) = match err.kind() {
            ErrorKind::NotFound => (FileOpErrorKind::NotFound, "file_not_found"),
            _ => (FileOpErrorKind::Io, "tool_execution_failed"),
        };
        Self::new(kind, code, format!("I/O error on '{path_hint}': {err}"))
    }
}

impl From<BoundaryError> for FileOpError {
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
            BoundaryErrorKind::NotFound => FileOpErrorKind::NotFound,
            other => FileOpErrorKind::Boundary(other),
        };
        Self::new(kind, code, err.message)
    }
}

impl fmt::Display for FileOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FileOpError {}

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
        _ => panic!("builtin file_ops input schema must be a valid JSON object"),
    }
}

/// Return the canonical `read_file` tool definition.
pub fn read_file_definition() -> JsObject {
    object([
        ("name", string(READ_FILE_TOOL_NAME)),
        ("aliases", JsValue::Array(vec![string("Read")])),
        ("description", string(READ_DESCRIPTION)),
        ("kind", string("filesystem")),
        ("inputSchema", JsValue::Object(schema(READ_INPUT_SCHEMA))),
    ])
}

/// Return the canonical `write_file` tool definition.
pub fn write_file_definition() -> JsObject {
    object([
        ("name", string(WRITE_FILE_TOOL_NAME)),
        ("aliases", JsValue::Array(vec![string("Write")])),
        ("description", string(WRITE_DESCRIPTION)),
        ("kind", string("filesystem")),
        ("inputSchema", JsValue::Object(schema(WRITE_INPUT_SCHEMA))),
    ])
}

/// Return the canonical `edit_file` tool definition.
pub fn edit_file_definition() -> JsObject {
    object([
        ("name", string(EDIT_FILE_TOOL_NAME)),
        ("aliases", JsValue::Array(vec![string("Edit")])),
        ("description", string(EDIT_DESCRIPTION)),
        ("kind", string("filesystem")),
        ("inputSchema", JsValue::Object(schema(EDIT_INPUT_SCHEMA))),
    ])
}

/// Compute lowercase hexadecimal SHA-256 digest of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Parameters for [`read_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFileRequest {
    pub file_path: String,
    pub offset: usize,
    pub limit: Option<usize>,
    pub max_output_bytes: usize,
    pub max_line_bytes: usize,
}

impl ReadFileRequest {
    pub fn new(file_path: impl Into<String>) -> Self {
        Self {
            file_path: file_path.into(),
            offset: 1,
            limit: None,
            max_output_bytes: DEFAULT_MAX_READ_OUTPUT_BYTES,
            max_line_bytes: DEFAULT_MAX_READ_LINE_BYTES,
        }
    }
}

/// Structured output produced by [`read_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFileResult {
    pub path: String,
    pub content: String,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub truncated: bool,
    pub sha256: String,
    pub byte_size: u64,
}

impl ReadFileResult {
    pub fn to_js_value(&self) -> JsValue {
        JsValue::Object(object([
            ("path", string(self.path.as_str())),
            ("content", string(self.content.as_str())),
            ("totalLines", JsValue::Number(self.total_lines as f64)),
            ("startLine", JsValue::Number(self.start_line as f64)),
            ("endLine", JsValue::Number(self.end_line as f64)),
            ("truncated", JsValue::Bool(self.truncated)),
            ("sha256", string(self.sha256.as_str())),
            ("byteSize", JsValue::Number(self.byte_size as f64)),
        ]))
    }
}

/// Decode a validated `read_file` JSON input object into a [`ReadFileRequest`].
pub fn parse_read_file_request(input: &JsValue) -> Result<ReadFileRequest, FileOpError> {
    let obj = input.as_object().ok_or_else(|| {
        FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "read_file input must be a JSON object",
        )
    })?;
    let file_path = required_utf8_string(obj, "file_path")?;
    let offset = match obj.get("offset") {
        None | Some(JsValue::Null | JsValue::Undefined) => 1usize,
        Some(val) => parse_positive_usize(val, "offset", 1)?,
    };
    let limit = match obj.get("limit") {
        None | Some(JsValue::Null | JsValue::Undefined) => None,
        Some(val) => Some(parse_positive_usize(val, "limit", 0)?),
    };
    Ok(ReadFileRequest {
        file_path,
        offset,
        limit,
        max_output_bytes: DEFAULT_MAX_READ_OUTPUT_BYTES,
        max_line_bytes: DEFAULT_MAX_READ_LINE_BYTES,
    })
}

/// Read a bounded UTF-8 text file range within `boundary`, rejecting binary/NUL files.
pub fn read_file(
    boundary: &WorkspaceBoundary,
    request: &ReadFileRequest,
) -> Result<ReadFileResult, FileOpError> {
    if request.offset < 1 {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "offset must be a 1-based line number (>= 1)",
        ));
    }
    let resolved = boundary.resolve_existing(&request.file_path)?;
    let file = File::open(&resolved.canonical_path)
        .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    let meta = file
        .metadata()
        .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    if !meta.is_file() {
        return Err(FileOpError::new(
            FileOpErrorKind::NotAFile,
            "file_conflict",
            format!("'{}' is not a regular file", resolved.relative_path),
        ));
    }

    let start_index = request.offset - 1;
    let end_index_exclusive = request
        .limit
        .map(|limit| start_index.saturating_add(limit))
        .unwrap_or(usize::MAX);

    let streamed = stream_text_file(
        file,
        &resolved.relative_path,
        start_index,
        end_index_exclusive,
        request.max_output_bytes,
        request.max_line_bytes,
    )?;

    let (start_line, end_line) = if streamed.emitted_lines > 0 {
        (request.offset, request.offset + streamed.emitted_lines - 1)
    } else {
        let start = request.offset.min(streamed.total_lines + 1);
        (start, start.saturating_sub(1))
    };

    let truncated = streamed.line_or_budget_truncated
        || (streamed.total_lines > 0
            && (start_index > 0 || end_index_exclusive < streamed.total_lines));

    Ok(ReadFileResult {
        path: resolved.relative_path,
        content: streamed.rendered,
        total_lines: streamed.total_lines,
        start_line,
        end_line,
        truncated,
        sha256: streamed.sha256,
        byte_size: streamed.byte_size,
    })
}

struct StreamReadState {
    rendered: String,
    emitted_lines: usize,
    total_lines: usize,
    line_or_budget_truncated: bool,
    sha256: String,
    byte_size: u64,
}

fn stream_text_file(
    mut reader: File,
    rel_path: &str,
    start_index: usize,
    end_index_exclusive: usize,
    max_output_bytes: usize,
    max_line_bytes: usize,
) -> Result<StreamReadState, FileOpError> {
    let mut hasher = Sha256::new();
    let mut byte_size = 0u64;
    let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
    let mut pending: Vec<u8> = Vec::new();
    let mut total_lines = 0usize;
    let mut first_line = true;
    let mut eof = false;

    let mut rendered = String::new();
    let mut emitted_lines = 0usize;
    let mut line_or_budget_truncated = false;
    let mut budget_exhausted = false;

    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|err| FileOpError::from_io(err, rel_path))?;
        if read == 0 {
            eof = true;
        } else {
            let chunk = &buf[..read];
            if chunk.contains(&0) {
                return Err(FileOpError::new(
                    FileOpErrorKind::BinaryFile,
                    "binary_file",
                    format!("'{rel_path}' appears to be a binary file (contains NUL byte)"),
                ));
            }
            hasher.update(chunk);
            byte_size = byte_size.saturating_add(read as u64);
            pending.extend_from_slice(chunk);
        }

        let mut consumed = 0usize;
        let mut cursor = 0usize;
        while cursor < pending.len() {
            match pending[cursor] {
                b'\n' => {
                    process_stream_line(
                        &pending[consumed..cursor],
                        rel_path,
                        start_index,
                        end_index_exclusive,
                        max_output_bytes,
                        max_line_bytes,
                        &mut total_lines,
                        &mut first_line,
                        &mut rendered,
                        &mut emitted_lines,
                        &mut line_or_budget_truncated,
                        &mut budget_exhausted,
                    )?;
                    cursor += 1;
                    consumed = cursor;
                }
                b'\r' => {
                    if cursor + 1 < pending.len() {
                        let sep = if pending[cursor + 1] == b'\n' { 2 } else { 1 };
                        process_stream_line(
                            &pending[consumed..cursor],
                            rel_path,
                            start_index,
                            end_index_exclusive,
                            max_output_bytes,
                            max_line_bytes,
                            &mut total_lines,
                            &mut first_line,
                            &mut rendered,
                            &mut emitted_lines,
                            &mut line_or_budget_truncated,
                            &mut budget_exhausted,
                        )?;
                        cursor += sep;
                        consumed = cursor;
                    } else if eof {
                        process_stream_line(
                            &pending[consumed..cursor],
                            rel_path,
                            start_index,
                            end_index_exclusive,
                            max_output_bytes,
                            max_line_bytes,
                            &mut total_lines,
                            &mut first_line,
                            &mut rendered,
                            &mut emitted_lines,
                            &mut line_or_budget_truncated,
                            &mut budget_exhausted,
                        )?;
                        cursor += 1;
                        consumed = cursor;
                    } else {
                        break;
                    }
                }
                _ => cursor += 1,
            }
        }
        if consumed > 0 {
            pending.drain(..consumed);
        }
        if eof {
            break;
        }
    }

    if !pending.is_empty() {
        process_stream_line(
            &pending,
            rel_path,
            start_index,
            end_index_exclusive,
            max_output_bytes,
            max_line_bytes,
            &mut total_lines,
            &mut first_line,
            &mut rendered,
            &mut emitted_lines,
            &mut line_or_budget_truncated,
            &mut budget_exhausted,
        )?;
    }

    let digest = hasher.finalize();
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut sha256, "{byte:02x}");
    }

    Ok(StreamReadState {
        rendered,
        emitted_lines,
        total_lines,
        line_or_budget_truncated,
        sha256,
        byte_size,
    })
}

#[allow(clippy::too_many_arguments)]
fn process_stream_line(
    raw_bytes: &[u8],
    rel_path: &str,
    start_index: usize,
    end_index_exclusive: usize,
    max_output_bytes: usize,
    max_line_bytes: usize,
    total_lines: &mut usize,
    first_line: &mut bool,
    rendered: &mut String,
    emitted_lines: &mut usize,
    line_or_budget_truncated: &mut bool,
    budget_exhausted: &mut bool,
) -> Result<(), FileOpError> {
    let text = std::str::from_utf8(raw_bytes).map_err(|_| {
        FileOpError::new(
            FileOpErrorKind::BinaryFile,
            "binary_file",
            format!("'{rel_path}' is not valid UTF-8 text"),
        )
    })?;
    let line_idx = *total_lines;
    *total_lines += 1;

    let clean_text = if *first_line {
        *first_line = false;
        text.strip_prefix('\u{FEFF}').unwrap_or(text)
    } else {
        text
    };

    if *budget_exhausted || line_idx < start_index || line_idx >= end_index_exclusive {
        return Ok(());
    }

    let line_num = line_idx + 1;
    let (bounded_line, single_line_truncated) =
        truncate_line_with_marker(clean_text, max_line_bytes);
    if single_line_truncated {
        *line_or_budget_truncated = true;
    }

    let formatted = format!("{line_num}|{bounded_line}");
    if max_output_bytes == 0 {
        *line_or_budget_truncated = true;
        *budget_exhausted = true;
        return Ok(());
    }

    if *emitted_lines == 0 {
        if formatted.len() <= max_output_bytes {
            rendered.push_str(&formatted);
            *emitted_lines += 1;
        } else {
            let truncated_first =
                truncate_utf8_with_marker(&formatted, max_output_bytes, "... [truncated]");
            rendered.push_str(&truncated_first);
            *emitted_lines += 1;
            *line_or_budget_truncated = true;
            *budget_exhausted = true;
        }
        return Ok(());
    }

    if rendered.len() + 1 + formatted.len() <= max_output_bytes {
        rendered.push('\n');
        rendered.push_str(&formatted);
        *emitted_lines += 1;
    } else {
        *line_or_budget_truncated = true;
        *budget_exhausted = true;
        while *emitted_lines > 1
            && rendered.len() + OUTPUT_TRUNCATED_MARKER.len() > max_output_bytes
            && let Some(pos) = rendered.rfind('\n')
        {
            rendered.truncate(pos);
            *emitted_lines -= 1;
        }
        if rendered.len() + OUTPUT_TRUNCATED_MARKER.len() <= max_output_bytes {
            rendered.push_str(OUTPUT_TRUNCATED_MARKER);
        } else if *emitted_lines == 1 && max_output_bytes > "... [truncated]".len() {
            *rendered = truncate_utf8_with_marker(rendered, max_output_bytes, "... [truncated]");
        }
    }
    Ok(())
}

fn truncate_line_with_marker(line: &str, max_bytes: usize) -> (String, bool) {
    if line.len() <= max_bytes {
        return (line.to_owned(), false);
    }
    (
        truncate_utf8_with_marker(line, max_bytes, LINE_TRUNCATED_MARKER),
        true,
    )
}

fn truncate_utf8_with_marker(text: &str, max_bytes: usize, marker: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    if max_bytes <= marker.len() {
        return truncate_utf8_prefix(text, max_bytes).to_owned();
    }
    let prefix = truncate_utf8_prefix(text, max_bytes - marker.len());
    format!("{prefix}{marker}")
}

fn truncate_utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Parameters for [`write_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteFileRequest {
    pub file_path: String,
    pub content: String,
    pub expected_sha256: Option<String>,
}

/// Structured output produced by [`write_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteFileResult {
    pub path: String,
    pub bytes_written: usize,
    pub sha256: String,
    pub created: bool,
}

impl WriteFileResult {
    pub fn to_js_value(&self) -> JsValue {
        JsValue::Object(object([
            ("path", string(self.path.as_str())),
            ("bytesWritten", JsValue::Number(self.bytes_written as f64)),
            ("sha256", string(self.sha256.as_str())),
            ("created", JsValue::Bool(self.created)),
        ]))
    }
}

/// Decode a validated `write_file` JSON input object into a [`WriteFileRequest`].
pub fn parse_write_file_request(input: &JsValue) -> Result<WriteFileRequest, FileOpError> {
    let obj = input.as_object().ok_or_else(|| {
        FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "write_file input must be a JSON object",
        )
    })?;
    let file_path = required_utf8_string(obj, "file_path")?;
    let content = required_utf8_string_allow_empty(obj, "content")?;
    let expected_sha256 = optional_utf8_string(obj, "expected_sha256")?;
    Ok(WriteFileRequest {
        file_path,
        content,
        expected_sha256,
    })
}

/// Write UTF-8 text atomically within `boundary` (creating parent dirs inside
/// the boundary, checking optional `expected_sha256`, temp + fsync + rename).
pub fn write_file(
    boundary: &WorkspaceBoundary,
    request: &WriteFileRequest,
) -> Result<WriteFileResult, FileOpError> {
    let expected_sha = normalize_expected_sha256(request.expected_sha256.as_deref())?;
    let resolved = boundary.resolve_write_target(&request.file_path)?;
    if resolved.relative_path == "." {
        return Err(FileOpError::new(
            FileOpErrorKind::NotAFile,
            "file_conflict",
            "cannot overwrite workspace root directory",
        ));
    }
    let file_name = resolved.canonical_path.file_name().ok_or_else(|| {
        FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "target path has no file name",
        )
    })?;
    let parent = resolved.canonical_path.parent().ok_or_else(|| {
        FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "target path has no parent directory",
        )
    })?;

    let created = match fs::symlink_metadata(&resolved.canonical_path) {
        Ok(sym_meta) => {
            if sym_meta.is_dir() {
                return Err(FileOpError::new(
                    FileOpErrorKind::NotAFile,
                    "file_conflict",
                    format!("'{}' is a directory", resolved.relative_path),
                ));
            }
            let meta = fs::metadata(&resolved.canonical_path)
                .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
            if !meta.is_file() {
                return Err(FileOpError::new(
                    FileOpErrorKind::NotAFile,
                    "file_conflict",
                    format!("'{}' is not a regular file", resolved.relative_path),
                ));
            }
            if let Some(expected) = &expected_sha {
                let actual = hash_existing_file(&resolved.canonical_path, &resolved.relative_path)?;
                if &actual != expected {
                    return Err(FileOpError::new(
                        FileOpErrorKind::CasMismatch,
                        "cas_mismatch",
                        format!(
                            "CAS mismatch for '{}': expected sha256 {expected}, found {actual}",
                            resolved.relative_path
                        ),
                    ));
                }
            }
            false
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if expected_sha.is_some() {
                return Err(FileOpError::new(
                    FileOpErrorKind::CasMismatch,
                    "cas_mismatch",
                    format!(
                        "CAS mismatch for '{}': expected existing file but target does not exist",
                        resolved.relative_path
                    ),
                ));
            }
            true
        }
        Err(err) => return Err(FileOpError::from_io(err, &resolved.relative_path)),
    };

    fs::create_dir_all(parent).map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    if !boundary.contains_canonical(&canonical_parent) {
        return Err(FileOpError::new(
            FileOpErrorKind::Boundary(crate::fs_guard::BoundaryErrorKind::SymlinkEscape),
            "symlink_escape",
            format!(
                "parent directory of '{}' resolves outside workspace boundary",
                resolved.relative_path
            ),
        ));
    }

    let target_path = canonical_parent.join(file_name);
    let content_bytes = request.content.as_bytes();
    atomic_write_in_dir(
        &canonical_parent,
        &target_path,
        content_bytes,
        &resolved.relative_path,
    )?;

    Ok(WriteFileResult {
        path: resolved.relative_path,
        bytes_written: content_bytes.len(),
        sha256: sha256_hex(content_bytes),
        created,
    })
}

/// Parameters for [`edit_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditFileRequest {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
    pub expected_sha256: Option<String>,
}

/// Structured output produced by [`edit_file`].
#[derive(Clone, Debug, PartialEq)]
pub struct EditFileResult {
    pub path: String,
    pub replacements: usize,
    pub sha256: String,
    pub previous_sha256: String,
    pub bytes_written: usize,
    pub changed: bool,
    pub diff: String,
    pub structured_patch: JsValue,
}

impl EditFileResult {
    pub fn to_js_value(&self) -> JsValue {
        JsValue::Object(object([
            ("path", string(self.path.as_str())),
            ("replacements", JsValue::Number(self.replacements as f64)),
            ("sha256", string(self.sha256.as_str())),
            ("previousSha256", string(self.previous_sha256.as_str())),
            ("bytesWritten", JsValue::Number(self.bytes_written as f64)),
            ("changed", JsValue::Bool(self.changed)),
            ("diff", string(self.diff.as_str())),
            ("structuredPatch", self.structured_patch.clone()),
        ]))
    }
}

/// Decode a validated `edit_file` JSON input object into an [`EditFileRequest`].
pub fn parse_edit_file_request(input: &JsValue) -> Result<EditFileRequest, FileOpError> {
    let obj = input.as_object().ok_or_else(|| {
        FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "edit_file input must be a JSON object",
        )
    })?;
    let file_path = required_utf8_string(obj, "file_path")?;
    let old_string = required_utf8_string_allow_empty(obj, "old_string")?;
    let new_string = required_utf8_string_allow_empty(obj, "new_string")?;
    let replace_all = obj
        .get("replace_all")
        .and_then(JsValue::as_bool)
        .unwrap_or(false);
    let expected_sha256 = optional_utf8_string(obj, "expected_sha256")?;
    Ok(EditFileRequest {
        file_path,
        old_string,
        new_string,
        replace_all,
        expected_sha256,
    })
}

/// Perform exact substring replacement on an existing workspace UTF-8 file with
/// optional CAS check and atomic temp+fsync+rename write.
pub fn edit_file(
    boundary: &WorkspaceBoundary,
    request: &EditFileRequest,
) -> Result<EditFileResult, FileOpError> {
    if request.old_string.is_empty() {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "old_string must not be empty",
        ));
    }
    if request.old_string == request.new_string {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            "old_string and new_string must differ",
        ));
    }
    let expected_sha = normalize_expected_sha256(request.expected_sha256.as_deref())?;
    let resolved = boundary.resolve_existing(&request.file_path)?;

    let meta = fs::metadata(&resolved.canonical_path)
        .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    if !meta.is_file() {
        return Err(FileOpError::new(
            FileOpErrorKind::NotAFile,
            "file_conflict",
            format!("'{}' is not a regular file", resolved.relative_path),
        ));
    }

    let raw_bytes = fs::read(&resolved.canonical_path)
        .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    if raw_bytes.contains(&0) {
        return Err(FileOpError::new(
            FileOpErrorKind::BinaryFile,
            "binary_file",
            format!(
                "'{}' appears to be a binary file (contains NUL byte)",
                resolved.relative_path
            ),
        ));
    }
    let previous_sha256 = sha256_hex(&raw_bytes);
    if let Some(expected) = &expected_sha
        && &previous_sha256 != expected
    {
        return Err(FileOpError::new(
            FileOpErrorKind::CasMismatch,
            "cas_mismatch",
            format!(
                "CAS mismatch for '{}': expected sha256 {expected}, found {previous_sha256}",
                resolved.relative_path
            ),
        ));
    }

    let content = std::str::from_utf8(&raw_bytes).map_err(|_| {
        FileOpError::new(
            FileOpErrorKind::BinaryFile,
            "binary_file",
            format!("'{}' is not valid UTF-8 text", resolved.relative_path),
        )
    })?;

    let occurrences = content.match_indices(&request.old_string).count();
    if occurrences == 0 {
        return Err(FileOpError::new(
            FileOpErrorKind::StringNotFound,
            "string_not_found",
            format!("old_string not found in '{}'", resolved.relative_path),
        ));
    }
    if occurrences > 1 && !request.replace_all {
        return Err(FileOpError::new(
            FileOpErrorKind::AmbiguousMatch,
            "ambiguous_match",
            format!(
                "Found {occurrences} matches of old_string in '{}'; set replace_all=true or provide a more specific old_string",
                resolved.relative_path
            ),
        ));
    }

    let (next_content, replacements) = if request.replace_all {
        (
            content.replace(&request.old_string, &request.new_string),
            occurrences,
        )
    } else {
        (
            content.replacen(&request.old_string, &request.new_string, 1),
            1,
        )
    };

    let parent = resolved.canonical_path.parent().ok_or_else(|| {
        FileOpError::new(
            FileOpErrorKind::Io,
            "tool_execution_failed",
            "target path has no parent directory",
        )
    })?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|err| FileOpError::from_io(err, &resolved.relative_path))?;
    if !boundary.contains_canonical(&canonical_parent) {
        return Err(FileOpError::new(
            FileOpErrorKind::Boundary(crate::fs_guard::BoundaryErrorKind::SymlinkEscape),
            "symlink_escape",
            format!(
                "parent directory of '{}' resolves outside workspace boundary",
                resolved.relative_path
            ),
        ));
    }

    let next_bytes = next_content.as_bytes();
    atomic_write_in_dir(
        &canonical_parent,
        &resolved.canonical_path,
        next_bytes,
        &resolved.relative_path,
    )?;

    let new_sha256 = sha256_hex(next_bytes);
    let structured_patch = build_structured_patch(content, &next_content);
    let diff = build_unified_diff(&resolved.relative_path, &structured_patch);

    Ok(EditFileResult {
        path: resolved.relative_path,
        replacements,
        sha256: new_sha256,
        previous_sha256,
        bytes_written: next_bytes.len(),
        changed: next_content != content,
        diff,
        structured_patch,
    })
}

fn hash_existing_file(path: &Path, rel_path: &str) -> Result<String, FileOpError> {
    let mut file = File::open(path).map_err(|err| FileOpError::from_io(err, rel_path))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; STREAM_CHUNK_BYTES];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|err| FileOpError::from_io(err, rel_path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

fn normalize_expected_sha256(raw: Option<&str>) -> Result<Option<String>, FileOpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(FileOpError::new(
            FileOpErrorKind::CasMismatch,
            "cas_mismatch",
            "expected_sha256 must be a 64-character hexadecimal SHA-256 digest",
        ));
    }
    Ok(Some(trimmed.to_ascii_lowercase()))
}

fn atomic_write_in_dir(
    parent_dir: &Path,
    target_path: &Path,
    bytes: &[u8],
    rel_path: &str,
) -> Result<(), FileOpError> {
    let pid = std::process::id();
    let mut temp_path = None;
    let mut temp_file = None;

    for _ in 0..TEMP_FILE_RETRIES {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = parent_dir.join(format!(".pilotdeck-tmp-{pid}-{seq}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temp_path = Some(candidate);
                temp_file = Some(file);
                break;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(FileOpError::from_io(err, rel_path)),
        }
    }

    let Some(temp_path) = temp_path else {
        return Err(FileOpError::new(
            FileOpErrorKind::Io,
            "tool_execution_failed",
            format!("unable to allocate temporary file for '{rel_path}'"),
        ));
    };
    let mut file = temp_file.expect("temp file handle present");

    let write_res = file
        .write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|err| FileOpError::from_io(err, rel_path));
    drop(file);

    if let Err(err) = write_res {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&temp_path, target_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(FileOpError::from_io(err, rel_path));
    }

    if let Ok(dir_handle) = File::open(parent_dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

fn split_diff_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split('\n').collect()
    }
}

fn patch_line(kind: &str, line: &str) -> JsValue {
    JsValue::Object(object([("type", string(kind)), ("text", string(line))]))
}

fn build_structured_patch(old: &str, new: &str) -> JsValue {
    if old == new {
        return JsValue::Array(Vec::new());
    }
    let old_lines = split_diff_lines(old);
    let new_lines = split_diff_lines(new);
    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_changed_end = old_lines.len().saturating_sub(suffix);
    let new_changed_end = new_lines.len().saturating_sub(suffix);
    let old_context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let new_context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let old_context_end = old_changed_end
        .saturating_add(DIFF_CONTEXT_LINES)
        .min(old_lines.len());
    let new_context_end = new_changed_end
        .saturating_add(DIFF_CONTEXT_LINES)
        .min(new_lines.len());

    let mut lines = Vec::new();
    for line in &old_lines[old_context_start..prefix] {
        lines.push(patch_line("context", line));
    }
    for line in &old_lines[prefix..old_changed_end] {
        lines.push(patch_line("delete", line));
    }
    for line in &new_lines[prefix..new_changed_end] {
        lines.push(patch_line("add", line));
    }
    for line in &new_lines[new_changed_end..new_context_end] {
        lines.push(patch_line("context", line));
    }

    JsValue::Array(vec![JsValue::Object(object([
        ("oldStart", JsValue::Number((old_context_start + 1) as f64)),
        (
            "oldLines",
            JsValue::Number((old_context_end - old_context_start) as f64),
        ),
        ("newStart", JsValue::Number((new_context_start + 1) as f64)),
        (
            "newLines",
            JsValue::Number((new_context_end - new_context_start) as f64),
        ),
        ("lines", JsValue::Array(lines)),
    ]))])
}

fn format_range(start: usize, length: usize) -> String {
    if length == 0 {
        format!("{start},0")
    } else if length == 1 {
        format!("{start}")
    } else {
        format!("{start},{length}")
    }
}

fn build_unified_diff(rel_path: &str, hunks: &JsValue) -> String {
    let JsValue::Array(items) = hunks else {
        return String::new();
    };
    if items.is_empty() {
        return String::new();
    }
    let body = items
        .iter()
        .map(|hunk| {
            let old_start = hunk
                .get("oldStart")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0) as usize;
            let old_lines = hunk
                .get("oldLines")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0) as usize;
            let new_start = hunk
                .get("newStart")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0) as usize;
            let new_lines = hunk
                .get("newLines")
                .and_then(JsValue::as_number)
                .unwrap_or(0.0) as usize;
            let mut out = vec![format!(
                "@@ -{} +{} @@",
                format_range(old_start, old_lines),
                format_range(new_start, new_lines)
            )];
            if let Some(JsValue::Array(lines)) = hunk.get("lines") {
                for line in lines {
                    let kind = line
                        .get("type")
                        .and_then(JsValue::as_string)
                        .map(|s| String::from_utf16_lossy(s.units()))
                        .unwrap_or_default();
                    let text = line
                        .get("text")
                        .and_then(JsValue::as_string)
                        .map(|s| String::from_utf16_lossy(s.units()))
                        .unwrap_or_default();
                    let prefix = match kind.as_str() {
                        "delete" => '-',
                        "add" => '+',
                        _ => ' ',
                    };
                    out.push(format!("{prefix}{text}"));
                }
            }
            out.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("--- a/{rel_path}\n+++ b/{rel_path}\n{body}")
}

fn required_utf8_string(obj: &JsObject, key: &str) -> Result<String, FileOpError> {
    let value = required_utf8_string_allow_empty(obj, key)?;
    if value.is_empty() {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            format!("{key} must not be empty"),
        ));
    }
    Ok(value)
}

fn required_utf8_string_allow_empty(obj: &JsObject, key: &str) -> Result<String, FileOpError> {
    let Some(val) = obj.get(key) else {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            format!("missing required string field '{key}'"),
        ));
    };
    let Some(s) = val.as_string() else {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' must be a string"),
        ));
    };
    s.to_utf8().map_err(|_| {
        FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' contains unpaired UTF-16 surrogates"),
        )
    })
}

fn optional_utf8_string(obj: &JsObject, key: &str) -> Result<Option<String>, FileOpError> {
    match obj.get(key) {
        None | Some(JsValue::Null | JsValue::Undefined) => Ok(None),
        Some(val) => {
            let Some(s) = val.as_string() else {
                return Err(FileOpError::new(
                    FileOpErrorKind::InvalidInput,
                    "invalid_input",
                    format!("'{key}' must be a string"),
                ));
            };
            s.to_utf8().map(Some).map_err(|_| {
                FileOpError::new(
                    FileOpErrorKind::InvalidInput,
                    "invalid_input",
                    format!("'{key}' contains unpaired UTF-16 surrogates"),
                )
            })
        }
    }
}

fn parse_positive_usize(val: &JsValue, key: &str, min: usize) -> Result<usize, FileOpError> {
    let Some(n) = val.as_number() else {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
            "invalid_input",
            format!("'{key}' must be an integer"),
        ));
    };
    if !n.is_finite() || n.fract() != 0.0 || n < (min as f64) || n > (usize::MAX as f64) {
        return Err(FileOpError::new(
            FileOpErrorKind::InvalidInput,
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
    use std::path::PathBuf;

    struct TempWorkspace {
        dir: PathBuf,
    }

    impl TempWorkspace {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir().join(format!(
                "pilotdeck-tool-core-fops-{}-{}-{}",
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
    fn definitions_validate_valid_and_invalid_inputs() {
        for def in [
            read_file_definition(),
            write_file_definition(),
            edit_file_definition(),
        ] {
            let schema = def.get("inputSchema").unwrap();
            assert!(crate::schema::validate_schema_shape(schema).is_ok());
        }

        let read_schema = read_file_definition().get("inputSchema").unwrap().clone();
        assert!(
            validate_tool_input(
                &read_schema,
                &parse_json(r#"{"file_path":"a.txt","offset":1,"limit":10}"#).unwrap()
            )
            .is_ok()
        );
        assert!(
            validate_tool_input(
                &read_schema,
                &parse_json(r#"{"file_path":"a.txt","offset":0}"#).unwrap()
            )
            .is_err()
        );
        assert!(
            validate_tool_input(&read_schema, &parse_json(r#"{"file_path":""}"#).unwrap()).is_err()
        );
    }

    #[test]
    fn read_file_paginates_truncates_and_rejects_binary_nul() {
        let ws = TempWorkspace::new("read");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        let text = "\u{FEFF}alpha\r\nbeta\ngamma\ndelta\n";
        fs::write(ws.path().join("notes.txt"), text.as_bytes()).unwrap();

        let full = read_file(&boundary, &ReadFileRequest::new("notes.txt")).unwrap();
        assert_eq!(full.total_lines, 4);
        assert_eq!(full.start_line, 1);
        assert_eq!(full.end_line, 4);
        assert!(!full.truncated);
        assert_eq!(full.byte_size, text.len() as u64);
        assert_eq!(full.sha256, sha256_hex(text.as_bytes()));
        assert_eq!(full.content, "1|alpha\n2|beta\n3|gamma\n4|delta");

        let mut paged_req = ReadFileRequest::new("notes.txt");
        paged_req.offset = 2;
        paged_req.limit = Some(2);
        let paged = read_file(&boundary, &paged_req).unwrap();
        assert_eq!(paged.content, "2|beta\n3|gamma");
        assert_eq!(paged.start_line, 2);
        assert_eq!(paged.end_line, 3);
        assert_eq!(paged.total_lines, 4);
        assert!(paged.truncated);
        assert_eq!(paged.sha256, full.sha256);

        // Line truncation and byte budget truncation markers.
        let long_line = format!("{}\nsecond\n", "x".repeat(120));
        fs::write(ws.path().join("long.txt"), long_line.as_bytes()).unwrap();
        let mut budget_req = ReadFileRequest::new("long.txt");
        budget_req.max_line_bytes = 30;
        budget_req.max_output_bytes = 36;
        let bounded = read_file(&boundary, &budget_req).unwrap();
        assert!(bounded.truncated);
        assert!(bounded.content.len() <= 36);
        assert!(bounded.content.contains("... [line truncated]"));

        // Binary NUL rejection.
        fs::write(ws.path().join("blob.bin"), b"abc\x00def").unwrap();
        let err = read_file(&boundary, &ReadFileRequest::new("blob.bin")).unwrap_err();
        assert_eq!(err.kind, FileOpErrorKind::BinaryFile);
        assert_eq!(err.code, "binary_file");

        // Invalid UTF-8 without NUL rejection.
        fs::write(ws.path().join("badutf8.bin"), [0xff, 0xfe, 0x80]).unwrap();
        let err = read_file(&boundary, &ReadFileRequest::new("badutf8.bin")).unwrap_err();
        assert_eq!(err.kind, FileOpErrorKind::BinaryFile);
    }

    #[test]
    fn write_file_creates_parents_enforces_cas_and_writes_atomically() {
        let ws = TempWorkspace::new("write");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        let created = write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "nested/dir/hello.txt".into(),
                content: "hello v1\n".into(),
                expected_sha256: None,
            },
        )
        .unwrap();
        assert!(created.created);
        assert_eq!(created.path, "nested/dir/hello.txt");
        assert_eq!(created.bytes_written, 9);
        assert_eq!(created.sha256, sha256_hex(b"hello v1\n"));

        // CAS mismatch leaves file untouched.
        let bad_cas = write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "nested/dir/hello.txt".into(),
                content: "should not land".into(),
                expected_sha256: Some("0".repeat(64)),
            },
        )
        .unwrap_err();
        assert_eq!(bad_cas.kind, FileOpErrorKind::CasMismatch);
        assert_eq!(
            fs::read_to_string(ws.path().join("nested/dir/hello.txt")).unwrap(),
            "hello v1\n"
        );

        // CAS match updates atomically and returns created=false.
        let updated = write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "nested/dir/hello.txt".into(),
                content: "hello v2\n".into(),
                expected_sha256: Some(created.sha256),
            },
        )
        .unwrap();
        assert!(!updated.created);
        assert_eq!(updated.sha256, sha256_hex(b"hello v2\n"));
        assert_eq!(
            fs::read_to_string(ws.path().join("nested/dir/hello.txt")).unwrap(),
            "hello v2\n"
        );
    }

    #[test]
    fn edit_file_enforces_uniqueness_replace_all_and_cas() {
        let ws = TempWorkspace::new("edit");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "src/lib.rs".into(),
                content: "fn foo() {}\nfn foo() {}\nfn bar() {}\n".into(),
                expected_sha256: None,
            },
        )
        .unwrap();

        let read = read_file(&boundary, &ReadFileRequest::new("src/lib.rs")).unwrap();

        // Ambiguous match fails when replace_all=false.
        let ambiguous = edit_file(
            &boundary,
            &EditFileRequest {
                file_path: "src/lib.rs".into(),
                old_string: "foo".into(),
                new_string: "baz".into(),
                replace_all: false,
                expected_sha256: Some(read.sha256.clone()),
            },
        )
        .unwrap_err();
        assert_eq!(ambiguous.kind, FileOpErrorKind::AmbiguousMatch);
        assert_eq!(ambiguous.code, "ambiguous_match");

        // Zero match fails.
        let missing = edit_file(
            &boundary,
            &EditFileRequest {
                file_path: "src/lib.rs".into(),
                old_string: "missing_ident".into(),
                new_string: "other".into(),
                replace_all: false,
                expected_sha256: None,
            },
        )
        .unwrap_err();
        assert_eq!(missing.kind, FileOpErrorKind::StringNotFound);

        // Unique match succeeds with diff & replacement count = 1.
        let unique = edit_file(
            &boundary,
            &EditFileRequest {
                file_path: "src/lib.rs".into(),
                old_string: "fn bar() {}".into(),
                new_string: "fn qux() {}".into(),
                replace_all: false,
                expected_sha256: Some(read.sha256),
            },
        )
        .unwrap();
        assert_eq!(unique.replacements, 1);
        assert!(unique.changed);
        assert!(unique.diff.contains("-fn bar() {}"));
        assert!(unique.diff.contains("+fn qux() {}"));

        // replace_all replaces all 2 occurrences.
        let all = edit_file(
            &boundary,
            &EditFileRequest {
                file_path: "src/lib.rs".into(),
                old_string: "foo".into(),
                new_string: "baz".into(),
                replace_all: true,
                expected_sha256: Some(unique.sha256),
            },
        )
        .unwrap();
        assert_eq!(all.replacements, 2);
        assert_eq!(
            fs::read_to_string(ws.path().join("src/lib.rs")).unwrap(),
            "fn baz() {}\nfn baz() {}\nfn qux() {}\n"
        );
    }

    #[test]
    fn read_file_handles_empty_offset_past_eof_limit_zero_and_byte_budget_markers() {
        let ws = TempWorkspace::new("read-edge");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        fs::write(ws.path().join("empty.txt"), b"").unwrap();
        let empty = read_file(&boundary, &ReadFileRequest::new("empty.txt")).unwrap();
        assert_eq!(empty.total_lines, 0);
        assert_eq!(empty.start_line, 1);
        assert_eq!(empty.end_line, 0);
        assert_eq!(empty.byte_size, 0);
        assert!(!empty.truncated);
        assert_eq!(empty.content, "");
        assert_eq!(empty.sha256, sha256_hex(b""));

        fs::write(
            ws.path().join("lines.txt"),
            b"one\ntwo\nthree\nfour\nfive\n",
        )
        .unwrap();
        let mut past_eof = ReadFileRequest::new("lines.txt");
        past_eof.offset = 99;
        let past = read_file(&boundary, &past_eof).unwrap();
        assert_eq!(past.content, "");
        assert_eq!(past.total_lines, 5);
        assert!(past.truncated);

        let mut zero_limit = ReadFileRequest::new("lines.txt");
        zero_limit.limit = Some(0);
        let zero = read_file(&boundary, &zero_limit).unwrap();
        assert_eq!(zero.content, "");
        assert!(zero.truncated);

        let mut tiny_budget = ReadFileRequest::new("lines.txt");
        tiny_budget.max_output_bytes = 32;
        let cut = read_file(&boundary, &tiny_budget).unwrap();
        assert!(cut.truncated);
        assert!(cut.content.len() <= 32);
        assert!(cut.content.contains("... [truncated]"));
        assert_eq!(cut.end_line, 2);
        assert_eq!(cut.total_lines, 5);

        fs::create_dir_all(ws.path().join("subdir")).unwrap();
        assert_eq!(
            read_file(&boundary, &ReadFileRequest::new("subdir"))
                .unwrap_err()
                .kind,
            FileOpErrorKind::NotAFile
        );
    }

    #[test]
    fn write_file_rejects_cas_on_missing_file_bad_hex_dir_and_leaves_no_temp_files() {
        let ws = TempWorkspace::new("write-edge");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        let missing_cas = write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "missing.txt".into(),
                content: "abc".into(),
                expected_sha256: Some("a".repeat(64)),
            },
        )
        .unwrap_err();
        assert_eq!(missing_cas.kind, FileOpErrorKind::CasMismatch);
        assert!(!ws.path().join("missing.txt").exists());

        let bad_hex = write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "badhex.txt".into(),
                content: "abc".into(),
                expected_sha256: Some("not-hex".into()),
            },
        )
        .unwrap_err();
        assert_eq!(bad_hex.kind, FileOpErrorKind::CasMismatch);

        fs::create_dir_all(ws.path().join("folder")).unwrap();
        let dir_err = write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "folder".into(),
                content: "abc".into(),
                expected_sha256: None,
            },
        )
        .unwrap_err();
        assert_eq!(dir_err.kind, FileOpErrorKind::NotAFile);

        write_file(
            &boundary,
            &WriteFileRequest {
                file_path: "clean.txt".into(),
                content: "ok".into(),
                expected_sha256: None,
            },
        )
        .unwrap();
        let leftover_temps: Vec<_> = fs::read_dir(ws.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".pilotdeck-tmp-"))
            .collect();
        assert!(leftover_temps.is_empty());
    }

    #[test]
    fn edit_file_preserves_dollar_signs_crlf_and_rejects_noop_empty_cas_and_binary() {
        let ws = TempWorkspace::new("edit-edge");
        let boundary = WorkspaceBoundary::new(ws.path()).unwrap();

        fs::write(ws.path().join("crlf.txt"), b"alpha\r\nbeta\r\n").unwrap();
        let sha = sha256_hex(b"alpha\r\nbeta\r\n");

        let cas_err = edit_file(
            &boundary,
            &EditFileRequest {
                file_path: "crlf.txt".into(),
                old_string: "beta".into(),
                new_string: "$&-$1".into(),
                replace_all: false,
                expected_sha256: Some("f".repeat(64)),
            },
        )
        .unwrap_err();
        assert_eq!(cas_err.kind, FileOpErrorKind::CasMismatch);

        let res = edit_file(
            &boundary,
            &EditFileRequest {
                file_path: "crlf.txt".into(),
                old_string: "beta".into(),
                new_string: "$&-$1".into(),
                replace_all: false,
                expected_sha256: Some(sha),
            },
        )
        .unwrap();
        assert_eq!(res.replacements, 1);
        assert_eq!(
            fs::read(ws.path().join("crlf.txt")).unwrap(),
            b"alpha\r\n$&-$1\r\n"
        );

        assert_eq!(
            edit_file(
                &boundary,
                &EditFileRequest {
                    file_path: "crlf.txt".into(),
                    old_string: "".into(),
                    new_string: "x".into(),
                    replace_all: false,
                    expected_sha256: None,
                },
            )
            .unwrap_err()
            .kind,
            FileOpErrorKind::InvalidInput
        );
        assert_eq!(
            edit_file(
                &boundary,
                &EditFileRequest {
                    file_path: "crlf.txt".into(),
                    old_string: "alpha".into(),
                    new_string: "alpha".into(),
                    replace_all: false,
                    expected_sha256: None,
                },
            )
            .unwrap_err()
            .kind,
            FileOpErrorKind::InvalidInput
        );

        fs::write(ws.path().join("bin.dat"), b"abc\x00alpha").unwrap();
        assert_eq!(
            edit_file(
                &boundary,
                &EditFileRequest {
                    file_path: "bin.dat".into(),
                    old_string: "alpha".into(),
                    new_string: "beta".into(),
                    replace_all: false,
                    expected_sha256: None,
                },
            )
            .unwrap_err()
            .kind,
            FileOpErrorKind::BinaryFile
        );
    }
}
