---
name: context-core
description: PilotDeck 纯上下文裁剪算法核心：Snip 裁剪、Micro 旧工具结果压缩、工具调用配对完整性、Token 预算快照与 canonical wire 投影。
---

# context-core

## 能力

从 Gateway 提取的无状态裁剪与预算核心（`SnipEngine`、`MicroCompactionEngine`、`budget::snapshot_from_tokens`、`tool_pair`、`wire`），通过调用方注入 token 计数与投影函数。

## 先读什么

- `src/lib.rs`、`src/snip.rs`、`src/micro.rs`、`src/budget.rs`、`src/tool_pair.rs`。

## 边界

- 不依赖 Gateway、Kernel、Tokio、模型、文件系统或会话权限状态；模型驱动摘要、日志落盘与历史提交仍在宿主侧编排。
