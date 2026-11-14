---
id: A7-SESSION-FORK-LIFECYCLE-001
---

## 类别

基线 bug 修复：修复 fork 历史无法 fresh restore，以及 `resumeAt` 的 accepted-input 切点带入未来回答。对于无法在不携带切点以后语义的前提下闭合的中间 assistant 切点，明确拒绝；这是一项有界的可观察差异，不是允许忽略 fork 对拍的通用豁免。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：`resources/PilotDeck/src/cli/GatewaySessionHistoryBundle.ts:150` 调用 fork provider；`resources/PilotDeck/src/web/server/forkSession.ts:100` 选择切点、`:164` 在保留切点时还保留同 turn 未来的 `turn_result` 和 `file_artifacts`，`:359` 过滤并建立新 fork；`resources/PilotDeck/src/session/storage/ProjectSessionStorage.ts:337` fresh restore 加载并验证事件；`resources/PilotDeck/src/session/events/SessionDomainValidation.ts:326` 拒绝尚有 open step 的 turn result。SDK `resources/PilotDeck/packages/sdk/src/client.ts:1088` 将 `resumeSessionAt` 作为 `fork_session` 的 `resumeAt: true` 调用。

真实基线 oracle：`_tmp/20261114-fork-recovery/oracle/run-02/result.json`、`source-entries.json`、`assistant-fork.jsonl`、`user-resume-at-fork.json`，运行方式和边界见 `_tmp/20261114-fork-recovery/oracle/README.md`。两轮合成有效源 fresh restore 成功；第一轮 assistant 切点创建出的 fork 漏掉原有 `step_completed`，fresh restore 报 `turn_has_open_step`；第二轮 `accepted_input` 切点 `resumeAt=true` 的 fork 虽能 restore，却包含原 sequence 14 `turn_result.finalMessage` 中切点之后的 `synthetic answer 2`。对照 `resumeAt=false` 不带第二轮结果并返回原输入 prefill。移除合法源的 step 完成事件后，validator 负控确实拒绝。该 oracle 使用真实 session history/provider 与持久恢复组件，未运行网络 Gateway、真实 LLM 或页面；不将它表述为端到端 Gateway/页面验收。

## Rust 行为

产品位于 `src/pilotdeck-gateway/src/native_process/session_fork.rs` 的 `closed_fork_prefix`：对有真实生命周期的 assistant 截点核验完整同 turn 后缀，仅允许原 `step_completed`、匹配选中消息的 `turn_result` 与该已完成 turn 的 `file_artifacts`；未来工具、模型、交互或上下文语义会明确返回 `fork_cutoff_not_closed`，无真实终态也拒绝。允许真实收尾在终态后产生的 turn artifacts，不虚构逐工具外键。SDK `resumeAt` 的 accepted-input 截点只保留真实前缀，不追未来回复终态；仅用不落盘的实际domain验证探针核验无悬置step/交互。

受用源前缀经过实际 Recorder.restore；显式parent须引用更早源记录，否则返回 `fork_invalid_source_history` 或 `fork_invalid_source_chain`。保留合法DAG和可选缺字段，仅对本次过滤造成的缺parent沿真实源祖先链回接。新fork中 `result.finalMessage` 的media引用使用与assistant消息相同的既有路径重定向，使二级分叉仍可严格比较正文。源历史与队列不改；已有坏会话通过独立、有备份的离线维护操作恢复。

## 范围

限定 `fork_session` / SDK `resumeSessionAt` 的本条生命周期与截断行为，及与之直接相关的 fresh restore。只接受真实源记录支持的闭合；不因本差异笼统拒绝合法多 step、已完成工具/附件、二代 fork 或合法 legacy 历史，不放弃其他正常 fork、协议字段、结果、排序或恢复判定。`fork_cutoff_not_closed` 仅在切点确实不能在不引入未来语义的条件下闭合时适用；具体支持边界由上述实现及正负控约束，禁止用拒绝全部复杂历史冒充修复。已有坏会话修复、队列 Continue 和页面验证须各自保留独立实证，不在此条替代验收。

## 机器判据

基线 oracle 命令（须选不存在的隔离输出目录、已有固定 TS 依赖）：`sh _tmp/20261114-fork-recovery/oracle/run.sh /Users/da/ws/pilotdeck-rs/resources/PilotDeck /Users/da/ws/pilotdeck-rs-cc2c/_tmp/20261114-fork-recovery/oracle/<新目录>`；已有 `run-02` 结果是实际证据，不覆盖或把新一次运行预称通过。

Rust 候选测试 `src/pilotdeck-gateway/src/native_process/session_fork_lifecycle_tests.rs`：`assistant_fork_closes_source_step_and_fresh_recorder_continues`、`product_node_fork_writes_restorable_history_and_fresh_next_turn`、`accepted_input_resume_preserves_baseline_prefix_semantics`；危险切点负控 `middle_assistant_rejects_later_step_and_response_without_mutating_source`、`assistant_fork_rejects_future_final_message_without_later_message_event`；合法范围控制 `assistant_fork_keeps_completed_tool_steps_and_terminal_artifact_references`、`assistant_fork_second_generation_keeps_exact_body_and_closure`、`legacy_assistant_without_domain_lifecycle_remains_restorable`。19项新增定点控制以及实际离线Recorder门已准备；另需正式入口与现有正常行为回归，检查 source 未改、无未来答案/副作用、fork 成功后 fresh restore 和新 turn，以及错误切点不创建损坏 fork。由统一构建者在 `src/` 以fresh nextest选取 `native_process::session_fork::lifecycle_tests`、`host_owned_storage_management` 及 `offline_recorder_restore`，另执行gateway fmt/clippy/doc与实际CLI release；实际命令、首轮 stdout/stderr、退出码、测试计数和源 SHA 尚待补入。上述为拟验收判据，**尚未据此声称 Rust 测试通过**。

## 批准记录

用户本轮直接授权原话：“按这个计划继续推进，先修复分叉续聊和已有坏会话恢复，保留我的历史和排队消息，完成首批9项验证。继续用 Sol 和 Terra 并行，Astra 审关键恢复逻辑；先可控测试，再少量真实 LLM，按根因和严重度汇总，不重复无影响测试。项目内决策你自行处理。”授权原文与root具体决定已持久记录在 `state/decisions-20261114.md` 的“分叉续聊及已有坏历史恢复”段。主协调据此批准本条精确bug修复范围；实际测试结果仍待固定候选执行，不因批准标PASS。本登记不修改冻结判分器、不豁免其他差异，旧基线失败原件保持。已有数据维护工具与F01–F09网页/正式入口验收各自独立核验，不能以本条登记替代。
