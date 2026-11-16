---
name: tokenizer
description: PilotDeck 不可变 o200k_base 普通文本 token 计数器：忠实保留 js-tiktoken 1.0.21 缩写正则，无文件、网络或宿主权限。
---

# tokenizer

## 能力

提供 `count_tokens(&JsString)` 与精确的 `SOURCE_PATTERN`，为上下文裁剪、预算统计与 `pilotdeck-agent` 提供与 TS 基线一致的 token 计数。

## 先读什么

- `Cargo.toml`、`src/lib.rs`、`src/implementation.rs`。

## 边界

- 仅做离线普通文本计数；不读写磁盘配置、不发起远程计数请求、不持有预算账本。
