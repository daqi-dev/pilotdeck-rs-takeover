---
name: tool-core
description: PilotDeck 纯工具产品逻辑核心（0.1.1）：get_current_time、todo_write、纯 JSON Schema 校验与工具目录并发批划分。
---

# tool-core

## 能力

- `time`（`src/time.rs`）：`get_current_time` 纯产品定义与格式化输出（`definition()`、`timezone()`、`execute()`、`days_from_civil()`、`TimeOutput`）。
- `schema`（`src/schema.rs`）：`validate_tool_input()` / `validate_schema_shape()` / `SchemaValidationError` 纯 JSON Schema 校验器。
- `catalog`（`src/catalog.rs`）：`ToolConcurrencyPolicy`、`ToolCatalogEntry`、`ToolCallCandidate`、`ToolBatchSlice` 与 `partition_tool_batch()` 并发/串行批切片算法。
- `todo`（`src/todo.rs`）：`todo_definition()`、`parse_todo_markdown()`、`apply_todo_update()` 与 `TodoItem` / `TodoWriteRequest` 状态更新逻辑。

## 先读什么

- `src/lib.rs`
- `src/time.rs`
- `src/schema.rs`
- `src/catalog.rs`
- `src/todo.rs`

## 边界

- 纯逻辑包；宿主时区发现、系统时钟读取、解析器所有权与执行权限裁决均由调用方（如 `pilotdeck-tool-iface`）提供。
