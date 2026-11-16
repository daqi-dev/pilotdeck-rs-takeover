---
name: http-transport
description: PilotDeck 共享 HTTP socket/TLS/SOCKS/代理/响应解码执行器：注入式拨号权限，不直接依赖 Kernel 服务。
---

# http-transport

## 能力

抽取自 Gateway 的底层 HTTP/TLS/SOCKS 传输与内容解压（gzip/zlib/deflate/brotli）实现，通过拨号 trait 供环境连接器与受信 Kernel 宿主共用同一传输逻辑。

## 先读什么

- `src/lib.rs`、`src/executor.rs`、`src/transport.rs`、`src/proxy_socks.rs`。

## 边界

- 拨号权威由调用方注入；普通 `model-runtime` 与 `model-iface` 不依赖本包，不直接持有默认放行策略或凭据。
