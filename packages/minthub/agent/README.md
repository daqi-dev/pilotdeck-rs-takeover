# pilotdeck-rs/agent (`pilotdeck-agent`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-agent`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 纯 AgentLoop 算法辅助核心：请求装配、会话状态投影、预算/压缩终止判定、早期流/输出/普通错误恢复与 ICU 排序规则。

提供纯 `AgentLoop` 辅助模块：`driver`、`ports`、`request`/`request_materialization`、`session_state`、`budget_terminal`、`compaction_terminal`、`compaction_persistence`、多类恢复策略与 `collation`。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/agent@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-agent`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/tokenizer` (`=0.1.0`)

## 源码边界与补充说明

无文件系统、provider 网络、权限裁决或 Session 所有权；所有外部副作用通过 `ports` 抽象注入。

本包直接依赖 `pilotdeck-protocol`、`pilotdeck-tokenizer`、`sha2` 与固定版本的 `icu_collator`/`icu_locale_core`，继承 `pilotdeck-rs/src/Cargo.toml` 工作区版本与许可。
