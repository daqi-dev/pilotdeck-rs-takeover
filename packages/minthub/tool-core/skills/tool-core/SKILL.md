---
name: tool-core
description: PilotDeck 工具产品逻辑核心（0.1.2）：get_current_time、todo_write、纯 JSON Schema 校验、并发批划分、WorkspaceBoundary 防护与有界文件/检索操作。
---

# tool-core

## 能力

- `time`（`src/time.rs`）：`get_current_time` 纯产品定义与格式化输出（`definition()`、`timezone()`、`execute()`、`days_from_civil()`、`TimeOutput`）。
- `schema`（`src/schema.rs`）：`validate_tool_input()` / `validate_schema_shape()` / `SchemaValidationError` 纯 JSON Schema 校验器（含字符串/数值/数组上下界）。
- `catalog`（`src/catalog.rs`）：`ToolConcurrencyPolicy`、`ToolCatalogEntry`、`ToolCallCandidate`、`ToolBatchSlice` 与 `partition_tool_batch()` 并发/串行批切片算法。
- `todo`（`src/todo.rs`）：`todo_definition()`、`parse_todo_markdown()`、`apply_todo_update()`、`parse_todo_write_request()` 与 `TodoItem` / `TodoWriteRequest` 状态更新逻辑。
- `fs_guard`（`src/fs_guard.rs`）：`WorkspaceBoundary` 严格规范根包含校验（NUL / `..` / 符号链接逃逸与未创建写目标最近祖先校验）。
- `file_ops`（`src/file_ops.rs`）：`read_file`、`write_file`、`edit_file` 与对应 `*_definition()` / `parse_*_request()`（分页、字节预算截断、二进制拒绝、CAS SHA-256 锁、原子临时文件+fsync+rename、结构化 diff）。
- `search_ops`（`src/search_ops.rs`）：`glob`、`grep` 与对应 `*_definition()` / `parse_*_request()`（`*`/`**`/`?`/`{a,b}` 匹配、大小写开关、上限截断、确定性 POSIX 相对路径排序）。

## 先读什么

- `src/lib.rs`
- `src/fs_guard.rs`
- `src/file_ops.rs`
- `src/search_ops.rs`
- `src/schema.rs`
- `src/catalog.rs`
- `src/todo.rs`
- `src/time.rs`

## 边界

- `#![forbid(unsafe_code)]`；工作区边界根路径、宿主时区发现、系统时钟读取与执行权限裁决均由调用方（如 `pilotdeck-tool-iface`）提供与锁定。
