---
id: H1-WHATSAPP-STDOUT-001
---

## 类别

挂起修复。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`src/adapters/channel/whatsapp/` 下 TS WhatsAppChannel.ts:72-84 配置stdout pipe但不读取；实际4MiB stdout写满后listen无法执行，readiness约15.3秒失败。

原始隔离mock证据：`_tmp/20261113-h1-next-matrix-whatsapp/` 下 bridge-stress-a/results.json 和 bridge-stress-b/results.json，mode stdout。既有证据本轮重新读取，不将本轮源码审查描述为重新执行。

## Rust 行为

`src/pilotdeck-gateway/src/channel/whatsapp/` 下 bridge.rs 同时有界排空stdout和stderr；只记录stderr，不将stdout当业务协议。

## 范围

仅可选外部bridge stdout排空。正常HTTP消息、日志和轮询协议仍按基线。

## 机器判据

channel::whatsapp::process_tests::drains_large_stdout_before_readiness_and_reaps_the_bridge。模块测试与 `tests/fixtures/h1-whatsapp/capture.mjs` 正式native同驱动对比必须运行并保存固定SHA、二进制/场景hash及原始结果；compare self-test及真实错误回复负控须拒绝错误。Rust当前候选尚未集中编译/运行，本条不能自动计为通过。

## 批准记录

用户2026-11-13 `docs/plan/backend-completion.md` 明确授权修复崩溃、挂起、丢数据和安全问题；本会话主协调于2026-11-13明确分配WhatsApp候选修复，并授权“三项已知安全/挂起/泄漏窄差异新条目”，引用该用户批准bug修复范围。授权只覆盖本条明确现象，不能代替Rust验收，也不覆盖普通start失败的日志/返回值差异。
