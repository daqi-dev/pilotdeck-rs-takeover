---
id: H1-OBSERVER-LIFECYCLE-WINDOWS-001
---

## 类别

用户已批准的精确生命周期兼容与评分差异。仅后台 observer 的 started 与完成三事件允许两个严格关联的窗口；不是任意事件重排、通用 UUID 忽略或整数组豁免，不改变产品调度。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：`OneShotSubagentPort.ts:190-230,353-444` 完成 worker 及 durable teardown 后只注册后台 observer；`ProjectSessionRuntimeBundle.ts:445-466` 与 `BackgroundSubagentRuntime.ts:45-81` 不等待 observer 结束；`agent/protocol/events.ts:158-165` 接收缓冲，`AgentLoop.ts:677,1985,2860-2940` 在运行窗口排出。

原 TS fork `_tmp/20261113-e0-resume/7bef-ffbb-batch1/fork/pair/agent03-sdk-fork.ts.json` 与 memory `7bef-ffbb-batch2/agent03-sdk-memory-lifecycle/pair/agent03-sdk-memory-lifecycle.ts.json` 中 observer 在 parent 首轮 terminal 后 5/6ms 完成，三事件由下一轮真实 q4 排出。新 cb7a 实际 observer 提前完成，三事件由 q1 排出。sourcefix 后两侧完整摘要、报告、业务参数一致。原证与根因表 `global-delivery-review/cb7a-formal-triage.json` 保留。未找到同 exact TS 输入的反向自比；批准不以“已证明同输入随机”为前提。

## Rust 行为

`native_process/agent_observer.rs` 经后台任务注册，`InteractionEvents::emit_forked` 排入活跃 parent turn 或下轮 carried buffer。`subagents.rs` 完成 child 和 sidechain teardown 后记录 completed。正式产品不为模仿一次 TS 捕获增加 sleep 或父轮屏障。

## 范围

只限两个固定原场景 SHA：fork `9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca`；memory `d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a`。

先验证唯一 worker/reviewer 与 parent session、完整正文/工具参数、durable entry 链及 projection、真实 wire/registry/history 一致和 run 内 seq，再分别处理：reviewer started 紧邻地在 agent tool_result 前，或在 tool_result/step_completed 后；reviewer text_delta、assistant_block、completed 同 producer、有序且各一次，位于 W0 或同 session 紧接的 W1。对应 seq/位置与持久引用仅逐叶接受；其他数组仍按实际顺序比较。

同场景的额外数组记录只在已证一一关联根处展开为类型、完整键集/序、长度及叶值；未知或未证明叶继续报告，不整记录豁免。仅六类强实体 UUID 用于目标配对，不接受其他首次出现别名连锁。provider 键序、model_selection 键序、headers、token/byte/hash/trace 值不属本条；memory 两子组另记独立条目。

## 机器判据

`scripts/bbx/observer-lifecycle-policy.mjs` 实施输入门与有限逐叶 Map；`observer-lifecycle-policy.test.mjs` 使用明确合成结构 fixture 覆盖缺失、重复、错 child/session/run、producer block、正文、工具 schema、越过未许可事件、局部/非目标乱序及缺见证，并实际 spawn diff 验证未知尾记录字段继续 DIFF、未知目标字段 INVALID。原完整两场 21 控制另记 `_tmp/20261113-observer-final-policy-proposal/lifecycle/formal-real-controls.log`；旧 8a/ccc 四组真实失败复用在 `old-real-failure-gates.json`，均在专属四次 assistant 活动门以 `3 !== 4` 拒绝。没有重采或改写原件。

非生命周期字段继续原比较；正式冻结后按新 harness 重判，不能把合成/私有测试当作整体 SAME。`evidence.mjs` 对正式模块保存 SHA。

## 批准记录

2026-11-13，本会话用户明确批准审批提案的三组精确边界；主协调者授权本条的 observer started/completion 两窗口正式集成，其他差异保留。SDK 已独审私有候选与 21 控制无阻断，原候选 SHA `7a6d343e023e20785e23b96a9cc22a4b273066237f72e5dddabbf0fdf6c21342`。本条只登记其中生命周期组，不自动批准全部 1107 处原差异。实现、负控及后续正式独审记录位于 `_tmp/20261113-observer-final-policy-proposal/`；聚合 accepted-diffs 与 FROZEN 不修改。
