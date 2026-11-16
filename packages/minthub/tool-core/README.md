# pilotdeck-rs/tool-core (`pilotdeck-tool-core`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-tool-core`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。包版本为 `0.1.1`。

## 职责与定位

PilotDeck 纯工具产品逻辑核心，划分为四个纯模块化子模块（统一从 `lib.rs` 再导出）：

- `time`（`src/time.rs`）：`get_current_time` 纯产品定义与执行逻辑（`definition()`、`timezone()`、`execute()`、`days_from_civil()`、`TimeOutput`），通过 `pilotdeck-transport::system_clock` 做时间格式投影。
- `schema`（`src/schema.rs`）：纯 JSON Schema 输入校验器（`validate_tool_input()`、`validate_schema_shape()`、`SchemaValidationError`），支持 `object` / `array` / `string` / `number` / `integer` / `boolean` / `null`、`required`、`additionalProperties: false`、`enum`、以及嵌套 `properties` 与数组 `items` 递归校验。
- `catalog`（`src/catalog.rs`）：纯工具目录元数据与并发批划分（`ToolConcurrencyPolicy`、`ToolCatalogEntry`、`ToolCallCandidate`、`ToolBatchSlice`、`partition_tool_batch()`），将连续 `ConcurrentReadOnly` 且 `read_only_call == true` 的调用聚为并行切片，非只读或未知工具逐一隔离为 `concurrent: false` 串行切片。
- `todo`（`src/todo.rs`）：`todo_write` 纯产品定义与状态更新（`todo_definition()`、`parse_todo_markdown()`、`apply_todo_update()`、`TodoItem`、`TodoWriteRequest`、`TodoStatus`），支持枚举校验、`merge` 按 `id` 合并/追加、替换及 markdown 复选框解析。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/tool-core@0.1.1`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-tool-core`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)

## 源码边界与补充说明

纯逻辑包；宿主时区发现、系统时钟读取、解析器所有权与工具授权执行均由调用方（如 `pilotdeck-tool-iface`）注入。

仅依赖 `pilotdeck-protocol` 与 `pilotdeck-transport`，无运行时宿主或网络依赖。
