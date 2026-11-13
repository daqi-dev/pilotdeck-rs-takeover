---
id: H1-OBSERVER-SUPPLEMENT-001
---

## 类别

用户明确批准的精确兼容与派生值判据补充；不修改产品调度，不将整个场景或事件数组豁免。同 ID 关系的 producer/slot/projection 适配另属既有归一化实现修复，不新增全局 UUID 许可。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。`agent/sub/OneShotSubagentPort.ts:184-245,353-410`、`cli/ProjectSessionRuntimeBundle.ts:445-457`、`agent/sub/BackgroundSubagentRuntime.ts:45-82` 在 worker 完成后注册后台 observer，不等待 observer 请求结束。原实际捕获均为 parent-after→observer，而 Rust 为反向；未观察到同 exact TS 反向，不以“随机已证”作为批准依据。

`context/budget/TokenAccountingRuntime.ts:184-205,372-390` 按实际 canonical 请求、memory 分隔块及 usage 校准生成预算。原六份实际 raw、完整逐侧来源及旧 DIFF226/166/115 见 `_tmp/20261113-e0-resume/e287-observer-651/`、`e287-final-triage-inputs/triplet-slot-2/run/raw/` 与 `_tmp/20261113-observer-supplement-proposal/`；原件不改。

## Rust 行为

`native_process/agent_observer.rs` 和 `runtime/background.rs` 注册 owned 后台 observer；父模型请求与 observer 请求独立继续。真实 UUID/耗时报告导致 token 数及窗口归属的派生数值变化，产品不为匹配单次捕获增加 sleep 或跨支 latch。

## 范围

仅 fork SHA `9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca` 与 memory SHA `d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a`：完整 tag/count/每请求 body、同 worker/reviewer/digest 验证后，provider 对象仅允许相邻 parent-after/observer 两项反序。其余 key/order/header/body 不变。

只对独立真实重算并跨 WS/durable/registry/history/projection/stream 绑定的八个 context 数值叶及最终 usage 三叶，保各侧实际原路径逐叶处理；包含明确新增的 memory 分隔块计数支持。两个 parent run 的 lastSeq=N、revision=N+1、两项 history sequence、registry 单行 skeleton 仅在完整实际成员、终态、producer、原始字节骨架及所有非目标内容/键序证明后准入。raw skeleton 不能脱离这些依赖单独忽略。

`observer-id-links-policy.mjs` 仅逐已证 ID 叶关联完整 producer、既有 title slot 和 projection，不能将159 UUID 表全局替换。memory tree 键序仍未批准，继续 DIFF；未知类型/键/数组长度/字段与任何未证明引用不接受。

## 机器判据

`normalization.mjs` 仅提取原规则；`observer-provenance.mjs` 重算 canonical/tokenizer/calibration 及完整引用来源；`observer-supplement-policy.mjs` 与 `observer-id-links-policy.mjs` 内部从 raw 生成实际 views/aliases 后输出精确 Map。不存在 caller passed/map 或动态 eval。

对应 `.test.mjs` 可用显式 `BBX_OBSERVER_ACTUAL_MANIFEST`（每份 raw SHA）及 `BBX_BASELINE` 执行真实正负控；缺 child/run/producer、错模型正文、未执行/重复事件、错初始预算/summary、错误引用、scope 外顺序均拒。既有 comparator selftest61和提取前后等价证明保存于 `_tmp/20261113-bbx-public-integration/`。新正式公共接线须独审、固定后另记离线重判，不把私有 SAME 当正式成绩。

## 批准记录

2026-11-13 用户明确回答“批准 A 组精确范围（推荐）”，持久批准锚 `state/decisions-20261113.md:59`，固定候选 tail-scope-fixed-ready SHA `6989d8300623bf0fa3d24e1b4bb6c3c09144e3057760fc4ee6bc5f1ad5f59373`，说明 SHA `840cf64375241ffbd092029ba568bbb79a01507928ff55f2948389b79bb5cfcf`。原44控制与额外 memory 拒绝、scope 首败、C1独立16控制均保留。仅本条明确范围获准；watch、parent、cron、Original53 与 tree 排序不在内。冻结 aggregate/FROZEN 不改。
