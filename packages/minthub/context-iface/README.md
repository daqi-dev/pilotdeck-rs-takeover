# pilotdeck-rs/context-iface (`pilotdeck-context-iface`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-context-iface`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 面向 Kernel ifACE 公开契约（`context` / `store` / `types`）的上下文适配包：

- `PilotDeckTokenCounter`：实现 `agentcpm_iface::context::TokenCounter`，复用 `pilotdeck-tokenizer`（精确 `o200k_base`）、无损容错字节计数（非 UTF-8 不失败）与固定每条消息开销（`MESSAGES_PER_TURN_OVERHEAD = 4`）。
- `AuthoritativeContextLedger`：单一权威版本化上下文状态账本（覆盖全部 5 个上下文操作），强制单调 `revision` 乐观并发（`RevisionMismatch`）、幂等 `transaction_id` + SHA-256 请求摘要校验（`IdempotencyConflict`）、输入原子消费、`pilotdeck_context_core::tool_pair` 配对完整性 fail-closed 校验及 `pilotdeck_context_runtime::history::History` 桥接。
- `PilotDeckContextContract`：实现 `agentcpm_iface::context::ContextContract`，在只读读取期间不修改权威状态并应用配置的 `MicroCompactionEngine` / `SnipEngine` 投影（输出 `application/vnd.pilotdeck.context-snapshot+json`），重建能力清单，并将 `trace_input` 原子先写落盘到注入的 `StoreContract`。
- `PilotDeckCompactionPersistence`：实现 `agentcpm_iface::context::CompactionPersistenceContract`，通过 `StoreContract` 持久化压缩边界与压缩后消息并执行域分隔 SHA-256 与长度 fail-closed 校验。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/context-iface@0.1.1`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-context-iface`
- **需要契约接口（`[requires].interfaces`）**：`context`, `store`, `types`
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/tokenizer` (`=0.1.0`)
- `pilotdeck-rs/context-core` (`=0.1.0`)
- `pilotdeck-rs/context-runtime` (`=0.1.0`)
- `pilotdeck-rs/model-core` (`=0.1.0`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `iface/context` (`>=0.1.5, <0.2.0`)
- `iface/store` (`>=0.1.3, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 安装与验证

```bash
# 安装本包（含依赖闭包；registry 以实际部署为准）
mint --registry https://mint-hub.ea.modelbest.co add -y pilotdeck-rs/context-iface@0.1.1

# 在 pilotdeck-rs 仓库内运行本包单测（src/ 为 Cargo workspace 根）
cargo nextest run -p pilotdeck-context-iface --profile ci --config-file ../.config/nextest.toml

# 端到端装配验证（MintHub 拉包 + 编译 + 跨包 E2E；详见仓库 docs/iface-integration/minthub-stitched-verification.md）
bash scripts/verify-minthub-stitched-assembly.sh --mint "$(command -v mint)" --jobs 6 --out-report-dir /tmp/pd-stitched
```

## 源码边界与诚实说明

`#![forbid(unsafe_code)]`；本包负责 `agentcpm_iface::context`、`StoreContract` 桥接与单权威版本化上下文状态账本，而端到端非 Recovery 的 Gateway Context 替换与完整五操作宿主接线仍在 `pilotdeck-gateway` 中。
