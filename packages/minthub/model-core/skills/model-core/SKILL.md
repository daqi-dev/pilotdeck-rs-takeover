---
name: model-core
description: PilotDeck 模型协议纯核心：UTF-16 工具参数 jsonrepair、四类 provider 请求/事件编解码、重试与流检查点决策、请求媒体检查。
---

# model-core

## 能力

提供 Anthropic、OpenAI Chat Completions / Responses 与 Google 四类协议的请求构造、有状态 SSE 事件累积、`jsonrepair`、`driver` 重试/检查点决策和 `request::inspect_request_media`。

## 先读什么

- `src/lib.rs`、`src/codec/`、`src/driver/`、`src/jsonrepair.rs`、`src/request.rs`。

## 边界

- 不执行 HTTP/SSE 网络传输、不读取凭据或环境变量、不持有会话状态；鉴权头注入仅通过受信调用方的 `*_with_auth` 闭包完成。
