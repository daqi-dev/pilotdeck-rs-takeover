---
name: iface
description: PilotDeck 接入公开 ifACE Scheduler SDK 的 AgentLoop 外壳：原生/恢复启动入口、资源多路编解码与 Kernel 客户端桥接。
---

# iface

## 能力

通过 `agentcpm_iface::loop::LoopContract` 与 `agentcpm_scheduler_protocol` 暴露 PilotDeck 准入的 `AgentLoop`，提供 `AdmittedTurn`、`CallAssembly`、`native_loop_launch`、`production_entry`、`recovery_launch`、`resource_mux`、`scoped_native_entry`，以及 `pilotdeck-context-recovery-provider` 与 `pilotdeck-native-loop` 二进制入口。

## 先读什么

- `Cargo.toml`、`src/lib.rs`、`src/production_entry.rs`、`src/native_loop_launch.rs`、`src/recovery_composition.rs`、`src/scoped_native_entry.rs`、`src/bin/pilotdeck_native_loop.rs`。

## 边界

- `#![forbid(unsafe_code)]`；不实例化 Gateway 或第二套执行运行时，所有模型、工具与上下文宿主效应均经调用绑定的 Kernel 客户端完成。
