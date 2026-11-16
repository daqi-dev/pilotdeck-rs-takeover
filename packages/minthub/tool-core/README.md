# pilotdeck-rs/tool-core (`pilotdeck-tool-core`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-tool-core`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。包版本为 `0.1.2`。

## 职责与定位

PilotDeck 工具产品逻辑核心（`#![forbid(unsafe_code)]`），划分为七个纯/有界模块化子模块（统一从 `lib.rs` 再导出）：

- `time`（`src/time.rs`）：`get_current_time` 纯产品定义与执行逻辑（`definition()`、`timezone()`、`execute()`、`days_from_civil()`、`TimeOutput`），通过 `pilotdeck-transport::system_clock` 做时间格式投影。
- `schema`（`src/schema.rs`）：纯 JSON Schema 输入校验器（`validate_tool_input()`、`validate_schema_shape()`、`SchemaValidationError`），支持 `object` / `array` / `string` / `number` / `integer` / `boolean` / `null`、`required`、`additionalProperties`、`enum`、数值/长度/数组边界（`minimum` / `maximum` / `minLength` / `maxLength` / `minItems` / `maxItems`），以及嵌套 `properties` 与数组 `items` 递归校验。
- `catalog`（`src/catalog.rs`）：纯工具目录元数据与并发批划分（`ToolConcurrencyPolicy`、`ToolCatalogEntry`、`ToolCallCandidate`、`ToolBatchSlice`、`partition_tool_batch()`），将连续 `ConcurrentReadOnly` 且 `read_only_call == true` 的调用聚为并行切片，非只读或未知工具逐一隔离为 `concurrent: false` 串行切片。
- `todo`（`src/todo.rs`）：`todo_write` 纯产品定义与状态更新（`todo_definition()`、`parse_todo_markdown()`、`apply_todo_update()`、`parse_todo_write_request()`、`TodoItem`、`TodoWriteRequest`、`TodoStatus`），支持枚举校验、`merge` 按 `id` 合并/追加、替换及 markdown 复选框解析。
- `fs_guard`（`src/fs_guard.rs`）：`WorkspaceBoundary` 与 `ResolvedWorkspacePath` 严格规范根包含守卫，要求绝对且可规范化的工作区根目录，拒绝 NUL 字节与 `..` 越界穿越，并对已存在路径与尚不存在的写目标（通过规范化最近已存在祖先目录及悬空符号链接目标检查）拦截符号链接逃逸。
- `file_ops`（`src/file_ops.rs`）：有界工作区文件操作与定义（`read_file` / `read_file_definition()`、`write_file` / `write_file_definition()`、`edit_file` / `edit_file_definition()`）：
  - `read_file`：支持 1-based `offset` / `limit` 分页、输出字节预算与超长行截断标记、二进制/NUL 与非法 UTF-8 拒绝，返回结构化 JSON（`content`、`totalLines`、`startLine`、`endLine`、`truncated`、`sha256`、`byteSize`）；
  - `write_file`：在边界内按需创建父目录、可选 `expected_sha256` 乐观锁（CAS）、同目录临时文件 + `fsync` + `rename` 原子写入，返回 `path`、`bytesWritten`、`sha256`、`created`；
  - `edit_file`：精确 `old_string` / `new_string` 替换，支持 `replace_all`（默认 `false` 时要求唯一命中）、可选 `expected_sha256` CAS、原子写回，返回替换次数与结构化 diff/patch。
- `search_ops`（`src/search_ops.rs`）：确定性有界工作区检索与定义（`glob` / `glob_definition()`、`grep` / `grep_definition()`）：
  - `glob`：纯 Rust 迭代匹配 `*` / `**` / `?` / `{a,b}` / `[...]`，拦截或跳过越界符号链接，按工作区相对 POSIX 路径排序并受 `limit` 截断；
  - `grep`：在有界文本文件内执行子串行检索（跳过二进制/NUL 与超大文件），支持可选 `glob` 过滤、`case_insensitive`、`max_matches` 上限、1-based 行号与确定性排序。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/tool-core@0.1.2`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-tool-core`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)

## 源码边界与补充说明

`#![forbid(unsafe_code)]`；宿主时区发现、系统时钟读取、工作区边界授信来源与工具授权执行均由调用方（如 `pilotdeck-tool-iface`）注入。

仅依赖 `pilotdeck-protocol`、`pilotdeck-transport` 与 `sha2`（workspace 锁定的纯 Rust SHA-256），无运行时宿主或网络依赖。
