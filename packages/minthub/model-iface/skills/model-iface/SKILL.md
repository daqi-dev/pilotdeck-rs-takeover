---
name: model-iface
description: PilotDeck 面向 Kernel ifACE 公开契约（model / types）的受信模型会话适配器：PilotDeckModelFactory 与流式/拉取桥接。
---

# model-iface

## 能力

实现 Kernel `iface/model` 与 `iface/types` 契约面，提供 `PilotDeckModelFactory`、`SharedPilotDeckModelFactory`、`ModelJobServices` 与基于 SSE/pull 的模型会话适配。

## 先读什么

- `src/lib.rs`、`src/session.rs`、`src/event.rs`、`src/pull.rs`、`src/sse.rs`。

## 边界

- `#![forbid(unsafe_code)]`；仅依赖契约接口与 `model-core`/`model-runtime`，不直接依赖 `http-transport` 或环境网络权限。
