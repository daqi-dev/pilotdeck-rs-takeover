---
name: agent
description: PilotDeck 纯 AgentLoop 算法辅助核心：请求装配、会话状态投影、预算/压缩终止判定、早期流/输出/普通错误恢复与 ICU 排序规则。
---

# agent

## 能力

提供纯 `AgentLoop` 辅助模块：`driver`、`ports`、`request`/`request_materialization`、`session_state`、`budget_terminal`、`compaction_terminal`、`compaction_persistence`、多类恢复策略与 `collation`。

## 先读什么

- `src/lib.rs`、`src/ports.rs`、`src/driver.rs`、`src/session_state.rs`、`src/request.rs`。

## 边界

- 无文件系统、provider 网络、权限裁决或 Session 所有权；所有外部副作用通过 `ports` 抽象注入。
