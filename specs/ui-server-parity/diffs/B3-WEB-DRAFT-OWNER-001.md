---
id: B3-WEB-DRAFT-OWNER-001
---

## 类别

bug修复：固定TS网页切换会话时可能将旧会话草稿覆盖到新会话，导致未发送正文丢失。属于数据丢失修复，不新增功能。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85的 `ui/src/components/chat/hooks/useChatComposerState.ts:1656–1689` 在scope effect先发布activeDraftStorageKeyRef，却只在setInput updater中更新inputValueRef。实际Web12同入口：General A输入多行草稿，整页导航项目B输入不同草稿，sidebar回A后composer为B且刷新仍B。首件 `_tmp/20261114-web-full-integration/cua/s06-first-switch.{txt,png}`；带诊断原实现再次复现，`draft-diagnostic-switch-logs.json` 的28887.7读取savedA、28888.7仍currentB、28893.9以ownerA/currentB flush、29047.2重建读取B，证明覆盖窗口。无原localStorage快照，不推断未采内容。

## Rust 行为

Rust后端不变。现有TS网页仅通过 `scripts/backend-web-integration/patches.json` 的composer overlay修补：旧scope保存及上传取消回调完成后，同步相邻发布新owner与savedInput，再调用纯setInput(savedInput)；不依赖有副作用updater执行时机。固定基线只读。

## 范围

只覆盖现有chat composer会话/project scope切换时的草稿owner/value发布，保留既有key、storage格式、防抖、导航flush、上传取消与正常发送清空。无草稿迁移、全局清理、后端协议或其他页面行为变化。

## 机器判据

同批CUA `draft-fixed-switch.txt`、`draft-fixed-reload.txt`、`draft-fixed-sent.txt`、`draft-fixed-b-retained.txt/png` 已由root实际记录：A多行草稿切回及刷新保留，发送后唯一完成且input清空，B草稿仍在。provider/持久独立核验与正式无诊断组合回归另由root汇总，不以本条代替。私有 `walkthrough/draft-control/src/components/chat/hooks/useChatComposerState.commands.test.tsx` 为真实hook+Composer可重跑控制，命令见draft-triage.md；旧版与v2均14/14通过，未击中race，不冒称杀错负控。实际旧首FAIL与相同CUA序列候选复验是本次对照，原件不覆盖。判分器、旧账本与分母不变。

## 批准记录

依据本会话用户授权修复现有网页集成bug、数据丢失修复默认范围，root于2026-11-15明确授权正式apply无诊断v2及新增精确差异条目；非用户逐字批准具体实现。非作者已审v2 owner/value同步发布与私有manifest静态PASS，实际候选页面复验通过；正式无诊断构建与最小回归尚待root执行。本条不扩大行为豁免。详见 `_tmp/20261114-web-full-integration/walkthrough/draft-triage.md`，当前最终身份以 `state/backend-handoff.md` 为准。
