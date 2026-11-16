---
name: context-iface
description: PilotDeck 面向 Kernel ifACE 公开契约（context / store / types）的上下文适配包：TokenCounter、ContextContract、CompactionPersistenceContract 与版本化权威状态账本。
---

# context-iface

## 能力

提供 `PilotDeckTokenCounter`、`AuthoritativeContextLedger`、`PilotDeckContextContract` 与 `PilotDeckCompactionPersistence`，覆盖 `TokenCounter`、`ContextContract`、`CompactionPersistenceContract` 及 `pilotdeck_context_runtime::history::History` 桥接。

## 先读什么

- `src/lib.rs`、`src/token_counter.rs`、`src/state_authority.rs`、`src/contract.rs`。

## 边界

- `#![forbid(unsafe_code)]`；`assemble` 只读投影不修改权威账本；`latest` / `load` 对截断或篡改记录按 `Internal / -32000` fail-closed 拒绝；端到端非 Recovery Gateway Context 替换仍在 `pilotdeck-gateway`。
