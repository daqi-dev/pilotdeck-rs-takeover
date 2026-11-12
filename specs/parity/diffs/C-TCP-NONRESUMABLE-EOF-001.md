---
id: C-TCP-NONRESUMABLE-EOF-001
---

## 类别

挂起修复：没有协商流恢复的 TCP peer 已关闭，sidecar 仍等待只有该 peer 才能答复的 module callback，执行不能自然结束。修复只终止失去传输所有权的原执行，不重放模型或工具操作，不把结果不明改成成功。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/cli/pilotdeck-agent-loop-sidecar.ts:26-38` 使用默认能力，TCP 同样为 resume=false/ack=false。`agent/modules/transport/agentLoopSidecarServer.ts:194-197` 在 EOF 只 detach connection，然后等待 active executions；`:440-467` 保留已发 callback Promise，只有原绑定连接答复或显式 abort 才能结束。已关闭 peer 无法再答复，而此默认服务拒绝 resume。

2026-11-13，Node 22.22.0、macOS ARM64 实际正式 CLI 双自比证据为 `_tmp/20261113-R2/orphan-self-2/`。真实默认 factory 已进入 model.prepare callback 后关闭其 TCP socket；另一个 socket 连续三次 status 仍为 running，同 execute 为 OPERATION_ALREADY_ACTIVE。原始 callback、状态和连接退出均存于 `ts.json` / `ts-self.json` / `*-process.json`，manifest 为 passed=true、artifactsUnchanged=true。这里的持续挂起判断同时依据实际状态和源码中无人能完成的 Promise；没有把短时间采样本身称为无限等待的实验证明。

## Rust 行为

`pilotdeck-transport/src/tcp/mod.rs` 的 detach 在 stream_resume=false 时关闭该 peer 仍在途执行的原 Engine，取消并回收 reader/writer/worker。`tcp/requests.rs` 对已退休但没有已知终态的执行返回 status resolving/result_unknown；再次提交完全相同 execute 返回 final RESULT_UNKNOWN，不再次调用 factory，也不返回一个永远不会继续的 accepted。

## 范围

仅未协商恢复、物理连接失效且执行尚无已知终态的路径。默认能力继续明确为 resume=false/ack=false。显式 `TcpServerOptions.stream_resume=true` 的连接丢失保留原执行及精确 pending callback；其恢复由旧 binding、同实例和游标检查控制。已知成功终态、可恢复连接、正常取消和其他 CLI 输出不因本条改变。

## 机器判据

`scripts/transport-validation/tcp-sidecar.mjs --baseline <固定TS checkout> --rust <本候选sidecar> --out <新目录> --self --orphaned-call` 对两侧分别断言明确状态：TS running / OPERATION_ALREADY_ACTIVE；Rust resolving + result_unknown / final RESULT_UNKNOWN。两侧都必须只有原始一次 callback，不能再次 execute 取得成功。本条不进入全局归一化，也不改变冻结分母。

`tcp::tests::tcp_nonresumable_disconnect_retires_workers_and_never_reaccepts_dead_execution` 还要求原 `ExecutionServer::serve` 全部线程完成、incomplete=0、factory 计数恰为 1。`tcp_resume_replays_exact_callback_once_and_revokes_old_socket_authority` 保证显式可恢复配置仍保留原引擎和 callback，不把该修复扩展到恢复路径。上述 Rust 机器证据由主协调者在当前候选集中运行，正式结果追加到交付报告；本条本身不视为测试通过。

## 批准记录

用户 2026-11-13 执行计划明确允许修复挂起，且要求区分 stdio 进程与 TCP 连接所有权、同实例恢复和副作用不重复。此处具备固定基线实际入口及源码的挂起证据，按该授权实施；不新增普通协议差异许可、不宣称 Node 原始行为已一致、不修改冻结标准。
