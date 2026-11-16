---
name: transport
description: PilotDeck Module 2.0 传输适配与受控循环刺激器：stdio/TCP 帧收发、默认工厂、宿主操作编解码与 tz-rs 系统时钟时区。
---

# transport

## 能力

实现 Module 2.0 `server`/`stdio`/`tcp` 传输适配、`default_factory`、`execute`、`loop_stimulus`、`system_clock`（基于 `tz-rs`）以及 `host_operations` 载荷映射。

## 先读什么

- `src/lib.rs`、`src/execute.rs`、`src/server.rs`、`src/system_clock.rs`、`build.rs`。

## 边界

- 严格区分 Module 2.0 传输与 Gateway 1.2 及宿主策略；`build.rs` 在无 `.git` 的解包目录构建时回落为 `PILOTDECK_SOURCE_COMMIT=unregistered-build`。
