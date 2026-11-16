# pilotdeck-rs/context-core (`pilotdeck-context-core`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-context-core`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 纯上下文裁剪算法核心：Snip 裁剪、Micro 旧工具结果压缩、工具调用配对完整性、Token 预算快照与 canonical wire 投影。

从 Gateway 提取的无状态裁剪与预算核心（`SnipEngine`、`MicroCompactionEngine`、`budget::snapshot_from_tokens`、`tool_pair`、`wire`），通过调用方注入 token 计数与投影函数。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/context-core@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-context-core`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)

## 源码边界与补充说明

不依赖 Gateway、Kernel、Tokio、模型、文件系统或会话权限状态；模型驱动摘要、日志落盘与历史提交仍在宿主侧编排。

验证命令：`cargo test -p pilotdeck-context-core` 与 `cargo test -p pilotdeck-gateway --test compaction_snip_micro`；详细边界见源码仓说明。
