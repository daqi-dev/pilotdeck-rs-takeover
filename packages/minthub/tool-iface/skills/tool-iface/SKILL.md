---
name: tool-iface
description: PilotDeck 工具公开 Kernel ifACE 适配器（0.1.2）：TimeToolContract、CatalogToolContract、TC-01/TC-04 治理与内置工作区工具适配。
---

# tool-iface

## 能力

- `TimeToolContract`（`src/lib.rs`）：保持 `get_current_time` 的 `ReadOnly` 宿主端口绑定（`Clock` / `ZoneResolver`）与 `OUTPUT_PROFILE`。
- `CatalogToolContract`（`src/catalog_contract.rs`）：同时实现 `ToolContract` 与 `ToolStaging`，提供 `ToolEffect` fail-closed 分类、JSON Schema 入参预校验、`register_stub` / 幂等 `remove_stub`、`register_tools_atomic`、`invoke_finalized` 与 `VerifiedToolContext` 委派。
- `governance`（`src/governance.rs`）：提供 `VerifiedToolEnvironment`（TC-01 防宿主与工作区根字段伪造）、`PreAuthArgumentFinalizer` / `FinalizedToolCall`（TC-04 SHA-256 授权前锁定）以及 `GovernedNestedToolInvoker`。
- `workspace_tools`（`src/workspace_tools.rs`）：将 `read_file`、`write_file`、`edit_file`、`glob`、`grep`、`todo_write` 以 `ReadOnly` / `SideEffect` 诚实注册到 `CatalogToolContract`，通过 `WorkspaceToolConfig` 从受信任环境与配置绑定 `WorkspaceBoundary`。

## 先读什么

- `src/lib.rs`
- `src/catalog_contract.rs`
- `src/governance.rs`
- `src/workspace_tools.rs`

## 边界

- `#![forbid(unsafe_code)]`；严格从 `ToolCall.environment` 与受信任 `WorkspaceToolConfig` 构造环境与工作区边界归属，拒绝模型 JSON 参数伪造保留宿主/工作区根字段或绕过摘要核验。
