# pilotdeck-rs/model-iface (`pilotdeck-model-iface`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-model-iface`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 面向 Kernel ifACE 公开契约（model / types）的受信模型会话适配器：PilotDeckModelFactory 与流式/拉取桥接。

实现 Kernel `iface/model` 与 `iface/types` 契约面，提供 `PilotDeckModelFactory`、`SharedPilotDeckModelFactory`、`ModelJobServices` 与基于 SSE/pull 的模型会话适配。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/model-iface@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-model-iface`
- **需要契约接口（`[requires].interfaces`）**：`model`, `types`
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/model-core` (`=0.1.0`)
- `pilotdeck-rs/model-runtime` (`=0.1.0`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `iface/model` (`>=0.1.4, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 源码边界与补充说明

`#![forbid(unsafe_code)]`；仅依赖契约接口与 `model-core`/`model-runtime`，不直接依赖 `http-transport` 或环境网络权限。

依赖 Kernel 契约载体 `agentcpm/iface` 及身份包 `iface/model`、`iface/types`（采用 `>=..., <0.2.0` 范围以同时兼容独立安装与 `agentcpm/host` 闭包）。
