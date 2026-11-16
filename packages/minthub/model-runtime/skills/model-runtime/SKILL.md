---
name: model-runtime
description: PilotDeck 模型执行运行时基础（无环境网络权限）：异步网络取消/定时器仲裁、重定向策略、请求头与 ModelStreamError/ModelHttpTransport。
---

# model-runtime

## 能力

共享异步模型驱动与网络仲裁基础：`network`（`ExchangeFuture` / `AbortFlag`）、`error::ModelStreamError`、`transport`（`ModelHttpTransport` / `ModelBody`）、`fetch_redirect`、`headers`、`http` 与 `services`。

## 先读什么

- `src/lib.rs`、`src/network.rs`、`src/transport.rs`、`src/fetch_redirect.rs`、`src/error.rs`。

## 边界

- 不包含 Hyper client、DNS、socket、TLS、代理、CA 文件、凭据读取或审计落盘；不向 ifACE 协议暴露 Node 私有事件分类。
