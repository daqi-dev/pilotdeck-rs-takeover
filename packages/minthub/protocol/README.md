# pilotdeck-rs/protocol (`pilotdeck-protocol`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-protocol`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 边界协议与 UTF-16 JsValue 核心（Module 2.0 / Gateway 1.2）：无宿主权限或 I/O 依赖，提供统一 JSON/JS 数值语义、快照与模型事件类型。

定义 `JsValue`/`JsString`（保留孤立代理码元）、`MODULE_PROTOCOL_VERSION` (2.0)、`GATEWAY_PROTOCOL_VERSION` (1.2)、模型事件与线缆编解码，是整个 PilotDeck Rust 依赖图的无 I/O 基座。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/protocol@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-protocol`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- 无内部 `pilotdeck-rs/*` 或 Kernel 包依赖。

## 源码边界与补充说明

纯类型与确定性编解码；不持有文件句柄、网络套接字、模型凭据或宿主会话状态。

本包仅依赖 `serde_json` 与 `ryu-js`，不依赖任何内部或 Kernel 包；`Cargo.toml` 通过 `version.workspace = true` 与 `license-file.workspace = true` 继承 `pilotdeck-rs/src/Cargo.toml` 元数据。
