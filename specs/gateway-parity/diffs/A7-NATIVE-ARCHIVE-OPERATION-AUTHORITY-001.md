---
id: A7-NATIVE-ARCHIVE-OPERATION-AUTHORITY-001
---

## 类别

安全 bug 修复。固定 TS 公开 native archive 方法把调用者参数覆盖到授权上下文的保留键 `operation`，允许真实 artifact 方法被伪装成 manifest，从而绕过按操作授权的宿主策略以及文件系统 artifact 保留期。Rust 由实际方法确定这个保留键的唯一权威；不把该缺陷当作必须复刻的普通输入兼容行为。

## 基线依据

固定基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，本次只读核对 Git HEAD 为该完整提交。

- `resources/PilotDeck/src/gateway/client/InProcessGateway.ts:2555–2586` 的三个公开方法先做各自输入校验，再构造授权输入。`:2585` 使用 `{ operation: "artifact", ...input }`，因此调用者额外字段可覆盖真实操作；`:2589–2600` 等待宿主 authorizer，普通错误转 `ARCHIVE_UNAUTHORIZED`，已有 `DialogGatewayError` 保留。文件 SHA256 `bbeabd02733d2cba2020f64fdcff31bcb0defd589aa695aa80a6041900859c7a`。
- `resources/PilotDeck/src/cli/createLocalGateway.ts:998–1017` 的组合策略先等待宿主授权，随后依据同一个 `input.operation` 判断是否执行 artifact filesystem retention。伪装为 manifest 会跳过这一分支。文件 SHA256 `54c2f856936733ef9c04d9c995a32317ed14a0d4070d202a0c2365447491869f`。

真实 oracle 使用完整 `createLocalGateway → 公开 gateway.nativeArchiveArtifact → 原组合 authorizer → 原 artifact provider`，没有直接演示对象 spread 或手写 provider 返回值替代原 callback。实际输入与结果在 `_tmp/20261114-native-archive-policy-oracle/attempt-2/observation.json`：

| 场景 | 正常真实 artifact 请求 | 仅增加 `operation:"manifest"` 后 |
| --- | --- | --- |
| 宿主策略拒绝 artifact、允许其他操作 | `ARCHIVE_UNAUTHORIZED`，message 为 `host policy forbids artifact`，宿主 blob read 为 0 | hook 实际看到 manifest，宿主 blob read 为 1；原 artifact callback 返回 `SYNTHETIC_AUTH_SECRET` 的完整 base64 |
| 文件 mtime=0、now=10000、retentionMs=1000 | `ARCHIVE_ARTIFACT_EXPIRED`，message 为 `Archive artifact expired.txt is outside the configured retention window.` | 原 artifact callback 返回 `SYNTHETIC_EXPIRED_SECRET` 的完整 base64 |

两对对照都是合成数据，除这一额外 operation 字段外保持同一原始请求、同一宿主策略或文件与时钟。总计两个场景、四次公开 API 调用，两个 Gateway 均 dispose，Node22 自然 exit 0，耗时 1.099232912 秒，未超时且已 reap，network guard 记录 0；运行相关 10 项原文件 SHA 前后相同。

原件及身份：

- `_tmp/20261114-native-archive-policy-oracle/attempt-2/receipt.json` SHA256 `34f76ebf0bedc0781cbb4a9b3314736c75195989cd4d64195b795b950ffbdcc6`。
- 同目录 `observation.json` SHA256 `cca0cf7c968ba7e52236d8e9b00060cfc4cc4a71142e8d80c8b10fded71988ae`。
- 同目录 `oracle.mts` SHA256 `3d280f7c28d6939df5bd59a1a63277ce1144d95a2eaf2592c2d93b52eb36ce51`。
- 同目录 `invocation.json` SHA256 `0c9662627ee14aef9bbcf72391f66bcbea2ed4c5fa017e62a90627732bd26b2f`，`cleanup.json` SHA256 `cf18b1f5ce34c6facc8d5199f05b2b773dad4d608a9070704e67ba12b2f501c8`；另保 stdout、stderr 和 owned-process 原件。

认证后的直接 WS 可达性是独立源码合证，未执行真实 WS 攻击重放：`GatewayWsConnection.ts:630–636` 仅验证 request frame/type/id/method，`:315–323` 把原 params 直接交公开方法；SHA256 `cbc27036572bbb7874b195a97ce584fee3f621c48a617f3e48c3fb010e154d0f`。`RemoteGateway.ts:220–222` 同样直接转交 input，SHA256 `9c2168d891747e67549a34d0c97df287cd06564902a6430ffb155f83fcc35a28`。高级 typed SDK 的 `packages/sdk/src/client.ts:3889–3894` 明确挑选字段，不转交 operation，SHA256 `7dc380d0a7e0d3ac1d97210a36b2a18855d27936de9c12ea6cedf74f75e92c2a`；本条不声称该正常 typed SDK 入口受影响，也不声称未认证访问可达。

## Rust 行为

`src/pilotdeck-gateway/src/native_archive/policy.rs:137–161` 的 `authorization_input` 先按实际 method 验证请求并选择 `manifest`、`entries` 或 `artifact`，复制参数后最后覆盖保留键 operation，使调用者无法伪装操作。该实现本次核对 SHA256 `3899ac632fb2c01edf60419a03ec3fc6034e194ac498dfcec205e90b5c4d4524`。

`src/pilotdeck-gateway/src/native_process/methods/metadata.rs:27–37` 在原 provider 请求前 await 策略授权；策略继续先执行宿主 hook，再依据真实操作执行可配置 retention。相同正常/伪装 artifact 请求在上述拒绝策略下都应返回 `ARCHIVE_UNAUTHORIZED` 且 read=0；相同正常/伪装过期文件请求都应返回 `ARCHIVE_ARTIFACT_EXPIRED`。这些是已批准修复的机器期望；本条首次登记时新 Rust policy 控制尚未实际执行，不将静审或批准写成 PASS。

## 范围

只修三个 native archive 方法的授权上下文保留键 operation 及其已实证的两种安全后果。该字段始终由实际方法确定；其他额外字段仍复制保留，不统一删除 unknown 字段，不改变 session/project 归属、能力门、输入验证顺序、异步等待、普通/typed 授权错误、原 provider 读取或编码合同。

filesystem retention 的严格大于、默认无保留期、外部 artifact store 豁免、空/缺失等行为保持固定 TS 合同，不借此改变。`A7-NATIVE-ARCHIVE-U8-BASE64-001` 另行处理内容编码矛盾，与本安全差异互不扩大。原 TS 的拒绝与绕过输出均保留；不改原 judge、原始证据、分母或通用比较规则。

## 机器判据

真实 TS 对照、原源码与执行/清理身份如上。前置一般合同 oracle `_tmp/20261114-native-archive-policy-oracle/attempt-1/` 的 27 个有限控制也保留，receipt SHA256 `af4c4debaf784ea421eff6edbfd7827bb7edd84350df2198f48efb49f03f28a3`，observation SHA256 `1e9276b373e3a73dab4fc0eaa2f6c9cb5c5192b862fb633cf7bab2623b0c0d07`。它证明三个 operation 的 async hook/provider 顺序、普通/typed/non-Error 映射、非法参数在 hook 前拒绝、FS age=1000/1001/-1 的边界、外部 store 豁免与非法 retention 构造错误；不以这些一般控制代替上述安全对照。

Rust 控制入口为 `src/pilotdeck-gateway/tests/native_archive_policy.rs:public_archive_artifact_method_cannot_spoof_authorization_or_retention_operation`：经正式 `LocalGateway::request("native_archive_artifact", ...)`，同一正常与伪装参数都必须按 artifact 授权/保留策略拒绝，宿主 read 不得发生，额外 `policyTrace` 字段仍原样到达宿主 hook。其余六个同文件控制保持三操作、验证顺序、错误、await 和 retention 的对照。当前这些新 policy 控制待 root 集中实际运行，不引用旧 storage/fork 或仅编译通过充作本条修复验收。

## 批准记录

主协调依据本会话用户全部项目决定授权及基线安全 bug 修复规则批准，持久决定为 `state/decisions-20261114.md:153–159`（“Native archive operation 授权权威修复”）。这是明确的项目自主决定，不冒称新增用户逐字审批。

非作者独审原件、来源、公开 IPG 结果及 WS/SDK 可达性边界的收据为 `_tmp/20261114-artifact-hydration-independent/native-archive-operation-security-review.json`，SHA256 `b391d80af75b5bbf1333f4f9996e9e6a66a3c47c14d8286dd4ebc409308e6f7c`。决定只接受这个保留键的权威修复，不放宽其他字段、任意授权、存储或比较器。

本条只新增登记文件；原 src 运行时 checkpoint、历史差异账本、通用 judge 和 FROZEN 未因登记而修改。待新 Rust policy 控制实跑、公共条目机械独审及协调者统一冻结后，才能宣称本条完成修复验收。
