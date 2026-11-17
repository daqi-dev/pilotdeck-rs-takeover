# pilotdeck-rs/tool-iface (`pilotdeck-tool-iface`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-tool-iface`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。包版本为 `0.1.3`。

## 职责与定位

PilotDeck 工具的公开 Kernel ifACE 适配器，在保持既有 `get_current_time` 与目录治理导出的基础上，深化工作区文件/检索/清单内置工具契约装配：

- `lib.rs`：`TimeToolContract`、`Clock`、`ZoneResolver`、`ZoneError`、`TOOL_NAME`、`OUTPUT_PROFILE`（`"pilotdeck.time-tool-output.v1"`）。
- `catalog_contract.rs`：`CatalogToolContract` 同时实现 `agentcpm_iface::tool::ToolContract` 与 `agentcpm_iface::tool::ToolStaging`：
  - 使用 `ToolEffect::{ReadOnly, SideEffect, Unknown}`，对未注册工具与 stub-only 工具 fail-closed 返回 `ToolEffect::Unknown` 与 `-32033`；
  - `ToolStaging` 支持原子 `register_stub`、幂等 `remove_stub` 与 `contains`，`register(spec)` 校验 JSON Schema 字节并拒绝 stub 或冲突 spec；支持 `register_tools_atomic` 整批原子注册与 `invoke_finalized` 预授权摘要核验；
  - 在派发 `CatalogToolHandler` 前基于 `pilotdeck_tool_core::schema::validate_tool_input` 校验入参，向处理器传递 `VerifiedToolContext { meta, environment, finalized_args }`。
- `governance.rs`（`TC-01` & `TC-04`）：
  - `VerifiedToolEnvironment`：严格从 `ToolCall.environment` 构造，绝不从模型参数解析，并拦截模型 JSON 中伪造保留宿主字段及工作区根字段（含 `workspace_root` / `workspaceRoot` 等）的尝试；
  - `PreAuthArgumentFinalizer` 与 `FinalizedToolCall`：在 Kernel 最终授权前完成 PreToolUse 参数修改与 SHA-256 摘要锁定；
  - `NestedToolInvoker` 与 `GovernedNestedToolInvoker`：要求嵌套工具调用必须构造携带已验证 `environment` 的新 `ToolCall::new(...)` 并经 `Arc<dyn ToolContract>`（Kernel 授权边界）重入，拒绝缺失父环境或未注册工具。
- `workspace_tools.rs`：将 `read_file`、`write_file`、`edit_file`、`glob`、`grep`、`todo_write` 整批原子注册到 `CatalogToolContract`（`register_workspace_tools` / `workspace_tool_contract` / `WorkspaceToolConfig` / `WorkspaceTodoStore`）：
  - 诚实声明 `ToolEffect`：`read_file` / `glob` / `grep` 为 `ToolEffect::ReadOnly`，`write_file` / `edit_file` / `todo_write` 为 `ToolEffect::SideEffect`；
  - 处理器执行前强制完成 TC-01 环境归属检查、TC-04 `FinalizedToolCall` 摘要核验与精确 JSON Schema 校验；
  - 工作区边界 `WorkspaceBoundary` 仅从受信任的调用方配置与已验证 `VerifiedToolEnvironment` 解析，永不信任原始工具参数中的工作区路径覆盖。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/tool-iface@0.1.3`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-tool-iface`
- **需要契约接口（`[requires].interfaces`）**：`tool`, `types`
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)
- `pilotdeck-rs/tool-core` (`=0.1.3`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `iface/tool` (`>=0.1.3, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 源码边界与补充说明

`#![forbid(unsafe_code)]`；注入的本地时钟/时区端口、`WorkspaceToolConfig` 边界绑定与工具处理器必须遵守 Kernel `ToolContract` / `ToolStaging` 契约，不接受工具参数传入会话身份、执行环境、工作区根或服务句柄。

因 `[requires].interfaces` 声明了 `tool`，`mint pack` / `mint publish` 会自动执行 tool 契约清单层 TCK 校验并生成合规证据。
