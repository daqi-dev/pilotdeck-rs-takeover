//! Pure PilotDeck tool logic: `get_current_time`, JSON Schema validation,
//! catalog concurrency batching, and `todo_write` checklist updates.
//! Host timezone discovery, clock access, and execution authority belong to callers.

pub mod catalog;
pub mod schema;
pub mod time;
pub mod todo;

pub use catalog::{
    ToolBatchSlice, ToolCallCandidate, ToolCatalogEntry, ToolConcurrencyPolicy,
    partition_tool_batch,
};
pub use schema::{
    SchemaValidationError, SchemaValidationIssue, validate_schema_shape, validate_tool_input,
};
pub use time::*;
pub use todo::{
    TODO_TOOL_NAME, TodoItem, TodoStatus, TodoWriteRequest, VALID_TODO_STATUSES, apply_todo_update,
    parse_todo_markdown, parse_todo_write_request, todo_definition,
};
