---
id: D1-SDK-MEMORY-DERIVED-001
---

## 类别

已批准的精确派生值评分差异。将既有独立运行 duration/worker UUID 的报告字节数重算合同扩至唯一固定 memory 场景，不修改产品输出、原始 capture 或 observer 结构顺序。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 实际原件 `_tmp/20261113-e0-resume/7bef-ffbb-batch2/agent03-sdk-memory-lifecycle/pair/agent03-sdk-memory-lifecycle.ts.json`，SHA256 `7c34e78259377ebed355e5c41d0b54ea1b219518c926bdbf0549029326f6d532`：worker 报告 durationMs=105、resultBytes=669；parent-after/check 请求 content-length=41710/41806。cb7a Rust 原件报告 durationMs=1436、resultBytes=670，请求长度41711/41807。逐侧 JSON.stringify(body) 的 UTF-8 长度与实际记录的 content-length 相等；仅替换已验证报告中的 duration 和等长关联 worker UUID 后，两侧完整请求 serialized 字节相同。原件及完整结构差异保留，不把本条回写为原案 SAME。

## Rust 行为

`scripts/bbx/observer-memory-policy.mjs::observerMemoryPaths` 仅在 scenario 为 `agent03-sdk-fork-lifecycle-probe`、scene SHA 为 `d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a`、固定 TS 身份且无失败时执行补充合同。沿用完整 worker 业务报告校验，绑定唯一 ag03-agent、WS preview/data/bytes、durable result/message/raw、registry 相同事件及 owning turn；对两份完整 parent 请求逐字节比较，仅在已验证 worker 报告的 duration/UUID 替换后相同，才返回实际差额叶。

## 范围

仅两个 `provider.parent-after/check[0].headers.content-length` 与同一 worker 完成事件在 WS 和 registry 的两个 `resultBytes`。路径下标及归一后的 registry path 必须相同，不替 observer 重新对齐或排序。其他 body、schema、header 值、token/usage、fileSize、history、seq、UUID 时间轴与生命周期差异不在本条范围。非 memory case 不新增任何例外；fork 保留原已有规则。

## 机器判据

`node --test scripts/bbx/observer-memory-policy.test.mjs` 的 14 个无本机绝对路径合成测试覆盖两组精确合同；本条负控要求拒绝伪 reportBytes、provider 正文变化后重算真长度、错误调用关联、registry 假字节。正常 4 叶输出及原输入不变有断言。真实原件一次验证收据 `_tmp/20261113-observer-memory-policy-application/controls.json` 对同一实际 pair 再执行 14 个正负控制，不启动产品，不执行全案比较，不宣称全案 SAME。既有首失败和原 DIFF 保留。

## 批准记录

2026-11-13，本会话用户明确批准三组精确边界；主协调者随即授权正式应用已独审 memory/header 候选。此条仅登记其中 memory 4 个派生字节叶；用户批准前的最终私有 candidate SHA256 为 `a2f8a53c7ea610e373486901a1f4a90ba33774249f87986b6a08d1e5ede5dee8`，SDK 独审最终绑定补充无阻断。实施记录见 `_tmp/20261113-observer-memory-policy-application/application.json`。此授权不扩大为任意 observer 顺序或 header/正文宽容，也不表示真实全案重判已通过。
