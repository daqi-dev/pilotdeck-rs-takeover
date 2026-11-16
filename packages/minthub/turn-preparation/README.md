# pilotdeck-rs/turn-preparation (`pilotdeck-turn-preparation`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-turn-preparation`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 纯回合输入准入与拥有权准备：权限上下文、计划附注、运行模式准备、初始化与模型准入校验。

负责把原始调用输入准入为类型化 `OriginalTurnPreparation` 与 `PreparationEffects`，提供 `execute`、`initialization`、`model_admission`、`preparation` 与 `AdvertisedMethods` 查询。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/turn-preparation@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-turn-preparation`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/agent` (`=0.1.0`)

## 源码边界与补充说明

纯准入与准备逻辑（`#![forbid(unsafe_code)]`），不创建执行运行时、不持有宿主端口或授权权威。

本包从 `pilotdeck-rs/src/` 抽取，依赖 `pilotdeck-protocol` 与 `pilotdeck-agent`。
