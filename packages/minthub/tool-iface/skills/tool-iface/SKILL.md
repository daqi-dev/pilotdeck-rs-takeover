---
name: tool-iface
description: PilotDeck 工具公开 Kernel ifACE 适配器（0.1.1）：TimeToolContract、多工具 CatalogToolContract（ToolContract / ToolStaging）与 TC-01/TC-04 治理。
---

# tool-iface

## 能力

- `TimeToolContract`（`src/lib.rs`）：保持 `get_current_time` 的 `ReadOnly` 宿主端口绑定（`Clock` / `ZoneResolver`）与 `OUTPUT_PROFILE`。
- `CatalogToolContract`（`src/catalog_contract.rs`）：同时实现 `ToolContract` 与 `ToolStaging`，提供 `ToolEffect` fail-closed 分类、JSON Schema 入参预校验、`register_stub` / 幂等 `remove_stub` 与 `VerifiedToolContext` 委派。
- `governance`（`src/governance.rs`）：提供 `VerifiedToolEnvironment`（TC-01 防宿主字段伪造）、`PreAuthArgumentFinalizer` / `FinalizedToolCall`（TC-04 SHA-256 授权前锁定）以及 `GovernedNestedToolInvoker`（嵌套工具经 `ToolContract` 重入并强制传播父环境）。

## 先读什么

- `src/lib.rs`
- `src/catalog_contract.rs`
- `src/governance.rs`

## 边界

- `#![forbid(unsafe_code)]`；严格从 `ToolCall.environment` 构造执行环境归属，拒绝模型 JSON 参数伪造保留宿主字段或绕过 `ToolContract` 边界直接调用嵌套处理器。
