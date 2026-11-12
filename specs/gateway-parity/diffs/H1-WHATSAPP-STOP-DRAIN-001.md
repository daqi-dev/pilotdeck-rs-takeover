---
id: H1-WHATSAPP-STOP-DRAIN-001
---

## 类别

取消及资源泄漏修复。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`src/adapters/channel/whatsapp/` 下 TS WhatsAppChannel.ts:112-123 停止仅abort poll和cleanupChild；正式server禁用后已接受模型turn仍保持SSE。

原始隔离mock证据：`_tmp/20261113-h1-next-matrix-whatsapp/` 下 final-a/results.json 和 final-b/results.json，活动turn禁用场景；cron-active-a/b为早期定位材料。既有证据本轮重新读取，不将本轮源码审查描述为重新执行。

## Rust 行为

`src/pilotdeck-gateway/src/channel/whatsapp/` 下 mod.rs/lifecycle.rs 停止同步关闭入口，准确owned stream cancel_and_drain，独立清理worker不因等待者drop遗失；失败保留stream供重试。

## 范围

仅该WhatsApp实例已接纳工作和出站请求；不能取消同名后继或其他渠道，不放宽正常收发/互动错误为成功。

## 机器判据

channel::whatsapp::tests::{dropped_stop_waiter_keeps_cleanup_owned_and_failed_cancel_is_retryable,cron_sender_is_owned_even_when_caller_drops_and_stop_cancels_http,overlapping_polls_are_all_owned_and_cancelled_during_stop}及正式capture五格。模块测试与 `tests/fixtures/h1-whatsapp/capture.mjs` 正式native同驱动对比必须运行并保存固定SHA、二进制/场景hash及原始结果；compare self-test及真实错误回复负控须拒绝错误。Rust当前候选尚未集中编译/运行，本条不能自动计为通过。

## 批准记录

用户2026-11-13 `docs/plan/backend-completion.md` 明确授权修复崩溃、挂起、丢数据和安全问题；本会话主协调于2026-11-13明确分配WhatsApp候选修复，并授权“三项已知安全/挂起/泄漏窄差异新条目”，引用该用户批准bug修复范围。授权只覆盖本条明确现象，不能代替Rust验收，也不覆盖普通start失败的日志/返回值差异。
