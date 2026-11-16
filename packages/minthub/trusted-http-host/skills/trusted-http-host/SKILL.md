---
name: trusted-http-host
description: PilotDeck 经 Kernel egress 策略与审计门控的受信模型 HTTP 宿主实现：作用域连接池、两段请求头预检、凭据隔离与重定向核验。
---

# trusted-http-host

## 能力

把 `pilotdeck-http-transport` 与 Kernel `env`/`policy`/`types` 门控绑定：提供 `TrustedHttpRuntime`、`TrustedProjectLease`、`ApprovedOrigins`、`JobHeaders` 与 `ScopedLifecycle`，强制每次请求与重定向跳都经 Kernel 拨号/策略核验。

## 先读什么

- `src/lib.rs`、`src/runtime.rs`、`src/prepared.rs`、`src/headers.rs`、`src/credentials.rs`、`src/chain.rs`。

## 边界

- 工作区集成中的受信模型 HTTP 宿主层：无默认凭据或网络放行，要求显式 `ApprovedOrigins` 与受限租约，敏感凭据投影全程 fail-closed 脱敏。
