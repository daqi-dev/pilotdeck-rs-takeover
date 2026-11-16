//! Pure PilotDeck tool logic: `get_current_time`, JSON Schema validation,
//! catalog concurrency batching, `todo_write` checklist updates, workspace
//! boundary enforcement (`fs_guard`), bounded file operations (`file_ops`),
//! and deterministic search (`search_ops`).
//! Host timezone discovery, clock access, and execution authority belong to callers.
#![forbid(unsafe_code)]

pub mod catalog;
pub mod file_ops;
pub mod fs_guard;
pub mod schema;
pub mod search_ops;
pub mod time;
pub mod todo;

pub use catalog::{
    ToolBatchSlice, ToolCallCandidate, ToolCatalogEntry, ToolConcurrencyPolicy,
    partition_tool_batch,
};
pub use file_ops::{
    DEFAULT_MAX_READ_LINE_BYTES, DEFAULT_MAX_READ_OUTPUT_BYTES, EDIT_FILE_TOOL_NAME,
    EditFileRequest, EditFileResult, FileOpError, FileOpErrorKind, READ_FILE_TOOL_NAME,
    ReadFileRequest, ReadFileResult, WRITE_FILE_TOOL_NAME, WriteFileRequest, WriteFileResult,
    edit_file, edit_file_definition, parse_edit_file_request, parse_read_file_request,
    parse_write_file_request, read_file, read_file_definition, sha256_hex, write_file,
    write_file_definition,
};
pub use fs_guard::{BoundaryError, BoundaryErrorKind, ResolvedWorkspacePath, WorkspaceBoundary};
pub use schema::{
    SchemaValidationError, SchemaValidationIssue, validate_schema_shape, validate_tool_input,
};
pub use search_ops::{
    DEFAULT_MAX_GLOB_RESULTS, DEFAULT_MAX_GREP_FILE_BYTES, DEFAULT_MAX_GREP_LINE_BYTES,
    DEFAULT_MAX_GREP_MATCHES, GLOB_TOOL_NAME, GREP_TOOL_NAME, GlobRequest, GlobResult, GrepMatch,
    GrepRequest, GrepResult, MAX_GLOB_RESULTS_CAP, MAX_GREP_MATCHES_CAP, SearchOpError,
    SearchOpErrorKind, glob, glob_definition, grep, grep_definition, parse_glob_request,
    parse_grep_request,
};
pub use time::*;
pub use todo::{
    TODO_TOOL_NAME, TodoItem, TodoStatus, TodoWriteRequest, VALID_TODO_STATUSES, apply_todo_update,
    parse_todo_markdown, parse_todo_write_request, todo_definition,
};
