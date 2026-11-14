---
id: B3-WEB-ATTACHMENT-DIAGNOSTIC-001
---

## 类别

bug修复：含图片和文本附件的历史轮次可能显示原始附件路径和正文而不显示文件卡片。

## 基线依据

固定cc7c4e6 `ui/src/components/chat/utils/attachmentNotes.ts:169` 的生成诊断解析器不识别实际 `- PilotDeck does not resize images; original bytes forwarded (intentional_difference §4.5).`，导致末尾诊断未剥离，前置附件块因尾随文本不被接收。隔离父会话 `web:s_9b26f0aa-74bc-4a9b-87f6-8eb2760bcc41` 与子会话实际历史HTTP响应均有该行及附件块，`cua/attachments-empty-historical-reloaded.txt` 显示正文而非文件卡。未证明分叉改变父数据。

## Rust 行为

后端不改。现有网页 overlay 只将该精确已知诊断行加入尾部诊断识别；原有连续尾块边界及上传元数据恢复逻辑不变。

## 范围

只涉及该精确诊断与历史附件块恢复，不识别任意类似文本，不改变上传、文件内容、租约期限或服务端协议；元数据缺失时保留既有降级。

## 机器判据

真实父HTTP正文驱动 `files/history-control-old/results.json` 旧版首项失败；`files/history-control-new-r2/results.json` 候选4/4通过，包含真实附件ID、近似诊断、尾随任意文字和普通用户文字控制。均相对 `_tmp/20261114-web-full-integration/`。正式组合及父子历史页面复验待执行。

## 批准记录

用户授权修复现有网页集成问题；root授权采用本项窄差异，非作者独审PASS；正式组合的实际页面复验尚未完成。
