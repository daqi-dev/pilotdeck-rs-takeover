---
id: H1-WHATSAPP-DESCENDANTS-001
---

## 类别

资源泄漏修复。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`src/adapters/channel/whatsapp/` 下 TS WhatsAppChannel.ts:316-340 只终止直接child。真实stop后忽略TERM的孙进程仍在。

原始隔离mock证据：`_tmp/20261113-h1-next-matrix-whatsapp/` 下 bridge-stress-a/results.json 和 bridge-stress-b/results.json，mode orphan。既有证据本轮重新读取，不将本轮源码审查描述为重新执行。

## Rust 行为

`src/pilotdeck-gateway/src/channel/whatsapp/` 下 bridge.rs Unix独立process group；waitid NOWAIT保留leader所有权，TERM最多5秒后KILL，最后一次组信号后reap。

## 范围

本期Linux/macOS由本实例启动的bridge组；不终止不属于本实例的进程。Windows同等保证尚未实现/验收。

## 机器判据

channel::whatsapp::process_tests::{stop_kills_owned_descendants_before_reaping_the_group_anchor,ignored_term_is_killed_and_joined_after_the_five_second_grace,dropping_start_during_readiness_does_not_lose_the_child_owner}。模块测试与 `tests/fixtures/h1-whatsapp/capture.mjs` 正式native同驱动对比必须运行并保存固定SHA、二进制/场景hash及原始结果；compare self-test及真实错误回复负控须拒绝错误。Rust当前候选尚未集中编译/运行，本条不能自动计为通过。

## 批准记录

用户2026-11-13 `docs/plan/backend-completion.md` 明确授权修复崩溃、挂起、丢数据和安全问题；本会话主协调于2026-11-13明确分配WhatsApp候选修复，并授权“三项已知安全/挂起/泄漏窄差异新条目”，引用该用户批准bug修复范围。授权只覆盖本条明确现象，不能代替Rust验收，也不覆盖普通start失败的日志/返回值差异。
