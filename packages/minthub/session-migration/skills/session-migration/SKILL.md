---
name: session-migration
description: PilotDeck P7 会话归属固定、无静默回退故障处理、静止 JSONL 审计与幂等上下文导入/回滚包。
---

# session-migration

## 能力

提供 `SessionOwnershipStore`（原子落盘 + SHA-256 账本校验）、`handle_kernel_turn_failure`（禁止静默回退 `LegacyRuntime`）、`audit_jsonl_transcript`（静止态与工具配对 fail-closed 审计）、`import_stationary_session`（幂等无损导入 `AuthoritativeContextLedger`）与 `import_for_rollback`（生成显式新会话 ID 完成回滚恢复）。

## 先读什么

- `src/lib.rs`、`src/error.rs`、`src/ownership.rs`、`src/stationary.rs`、`src/importer.rs`。

## 边界

- `#![forbid(unsafe_code)]`；已固定会话严禁跨后端混合运行态；导入前必须满足静止条件；回滚必须使用全新 `target_session_id` 并保持原 `.jsonl` 与原会话归属记录只读不变。
