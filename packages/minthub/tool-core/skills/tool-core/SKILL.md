---
name: tool-core
description: PilotDeck 内置工具 `get_current_time` 纯产品逻辑：输入 schema、时区回落与时间格式化输出投影。
---

# tool-core

## 能力

封装 `get_current_time` 的纯产品定义与执行逻辑（`definition()`、`timezone()`、`execute()`、`TimeOutput`），使用 `pilotdeck-transport::system_clock` 做时间格式投影。

## 先读什么

- `src/lib.rs`。

## 边界

- 纯逻辑包；宿主时区发现、系统时钟读取与解析器所有权均由调用方（如 `pilotdeck-tool-iface`）注入。
