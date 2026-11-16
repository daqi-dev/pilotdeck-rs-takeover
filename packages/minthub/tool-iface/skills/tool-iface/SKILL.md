---
name: tool-iface
description: PilotDeck `get_current_time` 工具的公开 Kernel ifACE 适配器：实现 ToolContract（tool / types）与 ReadOnly 宿主端口绑定。
---

# tool-iface

## 能力

提供 `TimeToolContract`、`Clock` 与 `ZoneResolver` 只读端口，通过 `agentcpm_iface::tool::ToolContract` 暴露精确的 `get_current_time`（`OUTPUT_PROFILE = "pilotdeck.time-tool-output.v1"`）。

## 先读什么

- `src/lib.rs`。

## 边界

- `#![forbid(unsafe_code)]`；注入的本地时钟/时区端口必须短小、有界且无阻塞，不接受工具参数传入会话身份或服务句柄。
