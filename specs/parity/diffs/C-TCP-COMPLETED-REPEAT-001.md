---
id: C-TCP-COMPLETED-REPEAT-001
---

## 类别

防止重复执行的修复：同一模块实例收到字节相同的已完成 execute 时，固定 TS 会再次运行原执行并再次调用模型 provider。Rust 在有界终态保留期内返回原终态，不重复模型或工具效果。本条单独声明该可观察差异，不以现有 Rust 缓存实现作为兼容依据。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/agent/modules/transport/agentLoopSidecarServer.ts:209-228` 只拒绝仍 active 的 operation；其完成后重收原 envelope 会再次创建 controller 和 stream，再调用 factory。`src/agent/modules/transport/moduleRuntime.ts:27-35` 对已存在 operation 返回既有记录，不拒绝已完成 request。

2026-11-13 正式 CLI 实测 `_tmp/20261113-R2/completed-repeat-1/` 使用真实默认 factory、隔离 HOME 和确定性宿主模型。原 execute 完成并查询 completed 后，经另一个 TCP socket 再发完全相同 envelope。TS 双自比均收到新的 accepted/stream、第二组 model.prepare/model.stream 以及成功终态。`ts-provider-dispatch.jsonl` 与 `ts-self-provider-dispatch.jsonl` 各记录两次实际 model.stream dispatch；原始 frame 和执行身份留存在对应 JSON，不以短暂等待未见调用作为证据。模型派发可能产生外部费用；该实测没有声称真实调用了外部账户或实际工具。

## Rust 行为

`pilotdeck-transport/src/tcp/requests.rs` 在确认请求与已缓存 boundary 完全一致、且已知 final_event 时返回原终态，包括原 stream/sequence/result。不会创建新的 Engine，也不会再次进入 factory 或调用模型/工具。原执行仍 active 时严格按 TS 返回 OPERATION_ALREADY_ACTIVE；不同 run 返回 OPERATION_IDENTITY_MISMATCH，不回一个缓存的 accepted。

## 范围

仅同一服务实例、有界保留期内、完整 envelope 相同的已知终态请求。终态保留期默认 30 秒，受配置、服务关闭和显式缓存回收影响；这不是跨进程或永久幂等存储。过期缓存、新请求身份、不同参数及未知执行结果不在本条授权范围。显式 resume 继续通过旧 binding 和游标恢复原 stream，不使用再次 execute 模拟恢复。

## 机器判据

`scripts/transport-validation/tcp-sidecar.mjs --baseline <固定TS checkout> --rust <候选sidecar> --out <新目录> --self --completed-repeat` 精确要求 TS 两次模型派发且获得新 stream，Rust 一次派发且第二次回复与原 final 深度相等。两侧都必须先完成实际默认循环；差异仅在命名场景逐侧断言，不进入全局归一化或改变冻结分母。上述 `completed-repeat-1` 为开发产物实测通过，manifest 记录二进制 `9d7482957cdf397afc9ad5c08f119dd42e52cfe74cc2cc39ba86d17d4569a9eb`、源码快照和 artifactsUnchanged=true。

`tcp::tests::tcp_active_repeat_is_rejected_and_completed_repeat_never_reexecutes_effect` 另用真实 sockets 和声明 SideEffect 的宿主 callback 计数验证：两连接 active 重复均拒绝，异 run 拒绝，完成后精确 terminal 缓存且 factory/effect 调用仅一次；该新增测试等待主协调者集中运行，正式结果记录到 R2 交付报告。`--active-repeat` 正式对拍同时防止把本差异扩展到 active 状态，其首次旧 Rust 错回 accepted 的失败保留于 `active-repeat-1`。

## 批准记录

用户 2026-11-13 执行计划明确要求 TCP 管理同实例恢复，不能重启新模块或重放副作用冒充恢复；工作包 R2 明列「副作用不重复」。本条按该既定授权，将已知完成、相同执行身份的重发限制为读取原结果，并补固定基线实际证据。它不授权扩大缓存期限、放宽身份匹配或跳过正式行为验证，不宣称与 TS 原始重复执行行为一致。
