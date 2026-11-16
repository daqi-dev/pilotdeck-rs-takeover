---
name: regex-proto
description: 基于 rquickjs（QuickJS-NG）的有界 JS RegExp 原型引擎（#1265）：默认关闭特性，仅按 UTF-16 码元与执行预算执行正则匹配。
---

# regex-proto

## 能力

把 JS RegExp 原型隔离在非默认 `regex-proto` feature 后，确保默认 workspace 构建无需拉取或编译 `rquickjs`；启用特性后提供带内存与步骤预算的 `RegexEngine`。

## 先读什么

- `Cargo.toml`、`src/lib.rs`、`src/engine.rs`（启用 `regex-proto` 时）。

## 边界

- 原型实验包，不暴露任意 JS 求值、宿主 I/O 或模块加载；默认构建下仅提供 `engine_disabled_reason()`。
