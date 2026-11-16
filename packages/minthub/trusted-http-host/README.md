# pilotdeck-rs/trusted-http-host (`pilotdeck-trusted-http-host`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-trusted-http-host`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 经 Kernel egress 策略与审计门控的受信模型 HTTP 宿主实现：作用域连接池、两段请求头预检、凭据隔离与重定向核验。

把 `pilotdeck-http-transport` 与 Kernel `env`/`policy`/`types` 门控绑定：提供 `TrustedHttpRuntime`、`TrustedProjectLease`、`ApprovedOrigins`、`JobHeaders` 与 `ScopedLifecycle`，强制每次请求与重定向跳都经 Kernel 拨号/策略核验。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/trusted-http-host@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-trusted-http-host`
- **需要契约接口（`[requires].interfaces`）**：`env`, `policy`, `types`
- **声明能力（`[capabilities]`）**：`net = ['http://127.0.0.1', 'https://*']`（Outbound model HTTP transport strictly mediated through Kernel egress policy and audit gates）

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/model-core` (`=0.1.0`)
- `pilotdeck-rs/model-runtime` (`=0.1.0`)
- `pilotdeck-rs/http-transport` (`=0.1.0`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `agentcpm/core` (`>=0.1.2, <0.2.0`)
- `agentcpm/host` (`>=0.1.6, <0.2.0`)
- `iface/env` (`>=0.1.7, <0.2.0`)
- `iface/policy` (`>=0.1.11, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 源码边界与补充说明

工作区集成中的受信模型 HTTP 宿主层：无默认凭据或网络放行，要求显式 `ApprovedOrigins` 与受限租约，敏感凭据投影全程 fail-closed 脱敏。

声明 `net` 能力（`http://127.0.0.1` 与 `https://*`）并附带对应 `[capabilities.reason]`，外发模型连接必须经 Kernel 出口策略与审计闸门核准；详尽安全不变量见源码仓内 `README.md` 历史设计说明。
