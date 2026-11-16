# pilotdeck-rs/tool-iface (`pilotdeck-tool-iface`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-tool-iface`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck `get_current_time` 工具的公开 Kernel ifACE 适配器：实现 ToolContract（tool / types）与 ReadOnly 宿主端口绑定。

提供 `TimeToolContract`、`Clock` 与 `ZoneResolver` 只读端口，通过 `agentcpm_iface::tool::ToolContract` 暴露精确的 `get_current_time`（`OUTPUT_PROFILE = "pilotdeck.time-tool-output.v1"`）。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/tool-iface@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-tool-iface`
- **需要契约接口（`[requires].interfaces`）**：`tool`, `types`
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)
- `pilotdeck-rs/tool-core` (`=0.1.0`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `iface/tool` (`>=0.1.3, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 源码边界与补充说明

`#![forbid(unsafe_code)]`；注入的本地时钟/时区端口必须短小、有界且无阻塞，不接受工具参数传入会话身份或服务句柄。

因 `[requires].interfaces` 声明了 `tool`，`mint pack` / `mint publish` 会自动执行 tool 契约清单层 TCK 校验并生成合规证据。
