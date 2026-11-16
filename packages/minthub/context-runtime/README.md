# pilotdeck-rs/context-runtime (`pilotdeck-context-runtime`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-context-runtime`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 上下文运行时抽取包：组装提示词、历史与制品端口、指令发现、缓存规划、记忆消息投影、工具预算与压缩编排。

汇集上下文运行时抽取模块：`compaction`、`history`、`instructions`、`memory`、`artifact`、`cache`、`operations`、`prepare`、`prompt`、`tool_budget`、`boundary_codec` 等。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/context-runtime@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-context-runtime`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/context-core` (`=0.1.0`)
- `pilotdeck-rs/agent` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)
- `pilotdeck-rs/model-core` (`=0.1.0`)

## 源码边界与补充说明

抽取演进中的运行时组件；具体摘要引擎、authoritative history 提交与宿主适配器需由上层组合根（如 `pilotdeck-iface`）接入并验证。

注：`src/pilotdeck-context-runtime/Cargo.toml` 使用显式 `version = "0.1.0"` 与 `edition = "2024"`（继承 `pilotdeck-rs` AGPL-3.0-only 许可声明）。
