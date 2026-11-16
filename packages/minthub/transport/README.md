# pilotdeck-rs/transport (`pilotdeck-transport`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-transport`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck Module 2.0 传输适配与受控循环刺激器：stdio/TCP 帧收发、默认工厂、宿主操作编解码与 tz-rs 系统时钟时区。

实现 Module 2.0 `server`/`stdio`/`tcp` 传输适配、`default_factory`、`execute`、`loop_stimulus`、`system_clock`（基于 `tz-rs`）以及 `host_operations` 载荷映射。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/transport@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-transport`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/agent` (`=0.1.0`)
- `pilotdeck-rs/turn-preparation` (`=0.1.0`)

## 源码边界与补充说明

严格区分 Module 2.0 传输与 Gateway 1.2 及宿主策略；`build.rs` 在无 `.git` 的解包目录构建时回落为 `PILOTDECK_SOURCE_COMMIT=unregistered-build`。

注意：`build.rs` 仅在构建期读取 Git HEAD 注入 `PILOTDECK_SOURCE_COMMIT`，在脱离 Git 仓库的源码包环境中会安全回落为 `unregistered-build`。
