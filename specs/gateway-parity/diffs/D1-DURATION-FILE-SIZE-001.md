---
id: D1-DURATION-FILE-SIZE-001
---

## 类别

用户批准的两项精确 duration 字节长度派生判据，不修正产品的真实 stat，不忽略任意 fileSize。

## 基线依据

固定 cc7c4e6 的实际 e287 原件：gap-loop-long-tool-chain TS/Rust JSONL 为186049/186051字节，末 bash durationMs 十进制位数两处总增2；gap-permissions-json-rules 为629624/629635字节，首 bash durationMs 在11处总增11。每份 canonical JSONL 重建长度/SHA与真实采集 witness相同，其余普通比较原规则一致。完整路径/原SHA/逐位贡献见 `_tmp/20261113-ordinary-tail-triage/review.json`，旧 DIFF1+1 保留。

## Rust 行为

每侧仍返回实际文件长度；durationMs 的原有不稳定许可不应被错误地转化为“产品 stat 错误”。

## 范围

仅 long-tool-chain SHA `67ead0fff8feed4626e11fc217d46e99c063924e5694181744afc1a780c32675` 的 `$.steps[3].frames[0].result.session.fileSize`，以及 permissions SHA `59ea48880977c8dc36802b2dd422e7a4cc30a9a85883a108560fdae34b1e3170` 的 `$.steps[10].frames[0].result.session.fileSize`。

对应步骤必须恰为 read_session_messages/s-gap、frame0成功 response无error，且是唯一 stat。完整 JSONL byte/SHA、连续父链及该侧 stat须吻合；全 capture普通归一后除该路径外完全相同。总字节差必须严格等已列枚举 durationMs 数字的十进制长度贡献，其他大小/正文/排版不在内。

## 机器判据

`duration-file-size-policy.mjs` 从原 raw 内部重建，不接 caller views/map。`duration-file-size-policy.test.mjs` 覆盖两个真实正控/self、假 stat/byte hash/正文，以及两侧一起把唯一size移step0、同改method/session的真实负控。私有初次任意step漏洞及原38证据保留；修正后的44及 Inventory独立scope复核见 `_tmp/20261113-observer-supplement-proposal/` 与 `observer-unified-independent/`。

## 批准记录

用户于2026-11-13明确批准 A 组精确六范围，锚 `state/decisions-20261113.md:59`，其中明确锁定 step3/step10、read_session_messages、s-gap和实际成功 response。无第三个场景、任意位置或任意 fileSize 许可；旧DIFF与冻结 aggregate/FROZEN不改。
