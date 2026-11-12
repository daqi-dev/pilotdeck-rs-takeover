---
id: D1-SDK-QUERY-ERROR-DISPOSE-001
---

## 类别

明确挂起 bug 修复。SDK query 初始化或迭代已经失败、没有在途 abort 时，CLI 保留 query socket 导致错误输出后不能自然退出。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85：src/cli/sdkCli.ts:43-86 的 finally 只关闭 control；packages/sdk/src/client.ts:1247-1256 的 next() catch 未关闭 query；:2001-2011 规定显式 model 必须有 projectKey。

本轮 macOS/Node22.22.0 正式 dist/src/cli/pilotdeck.js 入口，两轮各十格，产物 _tmp/20261113-c1-sdk/ts-run-1/summary.json 与 ts-run-2/summary.json。run/resume × missing-project、query-reject、model-ambiguous、stream-reject 共八格输出原业务错误后 query 仍存活，3500ms 观察边界未自然退出；run/resume success 两格 exit0、无存活连接。两轮可观察结果一致。原始 stderr、RPC、UUID、退出与自有进程清理记录保留，没有以归一化覆盖原件。另 ts-hello-1/summary.json 的 run/resume query hello 在10s超时输出错误后至13500ms仍未退出。

## Rust 行为

src/pilotdeck-cli/src/sdk/turn.rs 的普通 query 错误路径保留校验与 RPC 顺序，移除信号监听、先关闭 control，再有界关闭 query，返回原错误，由 main 仅打印一次，CLI 自然 exit1。无 project 的 --model 仍在 query resume 后报 projectKey is required when options.model is set.，不发送 model_catalog_list 或 submit_turn。不添加默认项目或新协议。

## 范围

仅没有在途 abort 的普通 query 初始化/迭代失败。成功、failed/result_unknown、显式信号取消及 RetainAborts 的既有有界 deadline 不变。旧100格 signal 矩阵受影响的精确 ID 为：

- run--query-hello-pending
- resume--query-hello-pending
- run--query-reject-cleanup
- resume--query-reject-cleanup
- run--response-error-cleanup
- resume--response-error-cleanup

这六格从 OBSERVED_TIMEOUT/清理SIGKILL/query1006 改为 OBSERVED_EXIT/natural exit1/query1005 noncleanup；仍须两连接、零abort、原stdout及业务stderr、原RPC序列，退出前均释放。原oracle字节保持，不能对其他格全局放宽timeout、退出码或close。

## 机器判据

src/pilotdeck-cli/tests/c1_sdk_lifecycle.rs：run_and_resume_release_failed_queries（8格）、run_and_resume_preserve_success_and_failed_results（6格）、run_and_resume_release_queries_after_hello_timeout（2格），随机端口mock驱动正式CLI，要求确切输出/退出/RPC、两连接显式close和无额外落盘。sdk_run_exit.rs::run_model_without_project_preserves_control_error 保证更早的control错误不被model校验掩盖。旧signal比较器仅按精确六格验证本差异，原格外保持严格，新增负控验证错误退出/缺close/范围扩张必失败。

Rust执行结果引用 docs/backend-delivery/C1.md 的固定候选验证记录；登记本身不声称测试已通过，不改变冻结C/A/B场景或分母。

## 批准记录

2026-11-13 用户本轮任务说明 docs/plan/backend-completion.md「已明确的用户决定」明确允许修复崩溃、挂起、丢数据和安全缺陷；C1独立任务再次明确授权 QueryOutcome::Retain 已知挂起修复。本条按该明确授权收敛，普通参数/输出不扩展。授权与实际验证分别记录，不以批准代替测试。
