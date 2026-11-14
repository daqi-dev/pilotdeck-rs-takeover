---
id: B3-WEB-ATTACHMENT-IDENTITY-001
---

## 类别

bug修复：同名不同文件上传显示成功后发送时丢失内容，属于数据丢失修复。

## 基线依据

固定cc7c4e6 `ui/src/components/chat/hooks/useChatComposerState.ts:265` 对每个文件按路径或文件名查找第一个上传结果；`ui/src/components/chat/utils/attachmentUpload.ts:134` 为每个文件生成唯一clientFileId；`src/gateway/dialog/UploadStore.ts:170` 将其用作attachmentId。实页两同名不同大小上传完成后发送，持久accepted_input仅含A，不含C；原件 `cua/attachments-distinct-same-name.txt` 及隔离会话 `web:s_16e71044-b162-4572-9c6c-84e04b4e5340`。

## Rust 行为

Rust后端不改。网页overlay保留每个文件的clientFileId，验证已完成上传返回的ID恰好构成该批次集合，并用ID映射原文件，避免同名和乱序错误。异常ID拒绝整批，不依靠名称、大小或顺序猜测。

## 范围

仅聊天附件上传映射；不改变上传协议、文件内容、租约期限、文件树或下载。客户端本来按名称、大小和时间去重的同指纹选择仍按原行为。

## 机器判据

私有实际hook控制 `files/control-old/results.json` 旧版将同名同大小不同时间文件错误映射成[id-b,id-b]；`files/control-candidate/results.json` 候选返回[id-a,id-b]。`files/control-upload/results.json` 实际上传函数乱序、缺失、重复、外来ID四控通过；`files/control-retry-r2/results.json` 实际hook共享批取消后重传保留项通过。生产构建 `files/build-candidate/build-result.json` 成功。真实网页首败同序列复验待执行；以上files均相对 `_tmp/20261114-web-full-integration/`。

## 批准记录

用户授权本会话修复集成数据丢失；root已正式采用该授权并批准apply；实际页面复验待完成。未完成复验前不声称缺陷关闭。
