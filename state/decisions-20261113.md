# 2026-11-13 本轮用户决定

本文件保存明确批准的决定；当前进度仍只维护在 `state/backend-handoff.md`。不以 agent 建议代替用户批准。

## D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001：保留 Rust 完整终态

批准来源：本轮用户对 `request_user_input_async` 的明确回答。

问题原文：

> 是否批准这项精确兼容性差异：在已验证的 CLI 信号取消场景中，允许固定 TS 基线漏掉唯一的 result 终态，但要求其他事件、同 run 的落盘终态、恢复及八项协议检查全部通过；Rust 仍必须完整输出终态？具体提案已准备在 _tmp/20261113-sdk-terminal-proposal/D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001.draft.md。

用户回答原文：

> 批准，保留 Rust 完整终态（推荐）

批准的提案 SHA256 为 `1b542e2bb7d2c3c43c2057cd4273ef38286d081313c861734dca5f5ac73de34a`。只覆盖具名 `c1-sdk-cancel-resume` 的实际信号场景：固定 TS 的六个事件完整且逐值对应，只缺唯一 CLI `result sequence=7`；两侧当前 run 的 durable terminal、取消触发、CLI canonical footer、持久恢复、同 binary 八项原始 wire 检查均须成立。Rust 必须保留准确终态。TS self 不享受此差异；任意其他截断、错身份、缺 durable terminal 或 Rust 漏终态仍须拒绝。

这是新增的精确兼容性取舍，不追认它属于旧 crash 修复许可，也不声称基线违反自己的测试或发生持久数据丢失。原始正式 INVALID 与所有原件保留；只有新增差异、公共严格接线和新的正式验证通过后，才能登记新一轮成绩。既有账本与冻结标准不回写。
