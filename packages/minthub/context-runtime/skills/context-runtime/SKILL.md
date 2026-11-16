---
name: context-runtime
description: PilotDeck 上下文运行时抽取包：组装提示词、历史与制品端口、指令发现、缓存规划、记忆消息投影、工具预算与压缩编排。
---

# context-runtime

## 能力

汇集上下文运行时抽取模块：`compaction`、`history`、`instructions`、`memory`、`artifact`、`cache`、`operations`、`prepare`、`prompt`、`tool_budget`、`boundary_codec` 等。

## 先读什么

- `src/lib.rs`、`src/operations.rs`、`src/prepare.rs`、`src/compaction/`、`src/history/`。

## 边界

- 抽取演进中的运行时组件；具体摘要引擎、authoritative history 提交与宿主适配器需由上层组合根（如 `pilotdeck-iface`）接入并验证。
