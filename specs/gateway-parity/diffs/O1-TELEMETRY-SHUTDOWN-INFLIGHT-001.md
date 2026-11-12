---
id: O1-TELEMETRY-SHUTDOWN-INFLIGHT-001
---

## 类别

bug 修复：固定 TS shutdown 可在 HTTP 批次未确认时返回并遗漏该批次的落盘，属于丢数据；也可能在旧磁盘队列已发送后留下重复恢复记录。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`src/telemetry/sender.ts:80,111-119,167-182`。实际 collector/sender → loopback held HTTP headers 场景：shutdown 返回时 response 仍 open，JSONL 仅有 queued 而无 inflight；释放响应后两个事件发送成功，但磁盘仍有 queued。原始 `_tmp/20261113-o1-telemetry/module-second/shutdown-inflight-baseline/raw.json`，金样 `src/pilotdeck-gateway/tests/fixtures/o1-telemetry/module-oracle.json` 中 shutdown-inflight-baseline；复算 driver 为同目录 capture.mjs。

## Rust 行为

`src/pilotdeck-gateway/src/telemetry/sender.rs` 的 TelemetrySender::shutdown 同步停止 timer，复用已有 shutdown receipt；owned worker 等待当前发送 receipt 确定成功、失败或超时后 persist。OwnedBatch 持有批次及完成责任；外部 shutdown future 被取消不取消 worker，也不遗失批次。成功发送的记录不落为待发送事件，失败后按既有重试规则保留的记录落盘。尚未执行 Rust 验证，不将预期写成已通过事实。

## 范围

仅 telemetry sender shutdown 与正在发送批次的生命周期及相应 JSONL。普通 flush 在已有发送时仍立即返回；事件字段、失败批次逐项前插逆序、maxRetries、队列上限、恢复规则、collector enabled 共享配置及 timer 别名行为保持固定基线。没有增加新外部协议。

## 机器判据

`telemetry::tests::enqueue_claims_batch_synchronously_before_next_cap_check_and_shutdown_waiter_can_be_dropped` 正向断言同步摘批、普通 flush 即返及取消 shutdown 等待后仍发送完成；`telemetry::tests::shutdown_uses_existing_timeout_then_persists_the_owned_failed_batch` 正向断言等待既有 timeout 后持久化失败批次。集中运行 `cargo test -p pilotdeck-gateway telemetry::tests`，构建由 c1_validation 调度。真实 socket 成功 headers、503、timeout、取消 shutdown waiter 及重启恢复的差异回放仍待验证；不能把现有 mock 测试视为真实 socket 验收。基线 HTTP 503→200 负控由 capture.mjs 的 `--mutate unexpected-success` 捕获。完整命令及隔离约束见 fixture README。

## 批准记录

已批准修复该类别缺陷：用户提供并作为本轮唯一计划的 `docs/plan/backend-completion.md:29` 明确“正常协议、参数、输出和持久化行为保持兼容；明确的崩溃、挂起、丢数据和安全缺陷可以修复”；AGENTS.md 行为约束同时要求修复后登记接受差异。2026-11-13 本轮根协调者明确授权本条窄差异：仅修 shutdown 等待并保存 owned 在途批次，不扩大普通 flush 和 timer 行为。此批准不代表尚未运行的 Rust 行为已验证；正式通过仍依赖上述正向判据。
