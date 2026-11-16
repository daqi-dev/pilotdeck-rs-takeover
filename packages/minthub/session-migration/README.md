# pilotdeck-rs/session-migration (`pilotdeck-session-migration`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-session-migration`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `edition.workspace = true`、`rust-version.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 面向 P7 迁移与退役阶段的会话归属与静止导入包：

- `ownership`：`BackendOwner`（`LegacyRuntime` / `KernelScopedResident` / `KernelFullLate`）、`SessionOwnershipRecord` 与基于 temp+fsync+rename 及 SHA-256 账本校验的 `SessionOwnershipStore`。新会话路由到可配置 `default_owner`，既有会话严格固定至既有归属；`handle_kernel_turn_failure` 返回真实结构化失败且绝不静默回退或跨后端重放到 `LegacyRuntime`。
- `stationary`：`LiveSessionActivity`、`StationaryAudit` 与 `audit_jsonl_transcript`。在导入前严格 fail-closed 拒绝活动写租约、待处理权限/提问、未终结回合/步骤、在途未配对工具调用、`result_unknown` 操作、空/不一致 `sessionId` 以及损坏或截断的 `.jsonl` 行。
- `importer`：`import_stationary_session` 与 `import_for_rollback`。以确定性幂等键与 `boundary_codec` 无损编码将已验证的静止 `.jsonl` 转录导入 `pilotdeck_context_iface::AuthoritativeContextLedger`（同时以 `ContextStateAuthority` 别名导出），保留消息顺序、工具配对及单代理 UTF-16 字符；回滚路径始终创建显式新目标会话 ID 而不修改原转录或原会话归属记录。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/session-migration@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-session-migration`
- **需要契约接口（`[requires].interfaces`）**：`context`, `types`
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/context-core` (`=0.1.0`)
- `pilotdeck-rs/context-iface` (`=0.1.0`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `iface/context` (`>=0.1.5, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 源码边界与诚实说明

`#![forbid(unsafe_code)]`；本包负责会话归属持久化账本、Kernel 失败不回退约束、静止转录审计以及向 `AuthoritativeContextLedger` 的幂等导入/回滚逻辑；Gateway / CLI / 渠道入口的默认切换与旧执行运行时退役接线仍在 `pilotdeck-gateway` 与 `pilotdeck-cli` 中。
