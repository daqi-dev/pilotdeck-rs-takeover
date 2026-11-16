---
name: protocol
description: PilotDeck 边界协议与 UTF-16 JsValue 核心（Module 2.0 / Gateway 1.2）：无宿主权限或 I/O 依赖，提供统一 JSON/JS 数值语义、快照与模型事件类型。
---

# protocol

## 能力

定义 `JsValue`/`JsString`（保留孤立代理码元）、`MODULE_PROTOCOL_VERSION` (2.0)、`GATEWAY_PROTOCOL_VERSION` (1.2)、模型事件与线缆编解码，是整个 PilotDeck Rust 依赖图的无 I/O 基座。

## 先读什么

- `src/lib.rs`、`src/model.rs`、`src/wire.rs`、`src/admission.rs`。

## 边界

- 纯类型与确定性编解码；不持有文件句柄、网络套接字、模型凭据或宿主会话状态。
