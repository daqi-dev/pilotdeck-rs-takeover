---
id: A7-SESSION-FORK-LIFECYCLE-001-R1
original: A7-SESSION-FORK-LIFECYCLE-001
---

## 类别

基线 bug 修复的范围澄清与回归修复；保留原条目和首败记录。

## 基线依据

固定 cc7c4e6 `resources/PilotDeck/src/web/server/forkSession.ts:164` 对截点后的 session_metadata 原本不携带。会话标题/收尾元数据可在完成尾部出现，它不是新的模型、工具或交互事件。Rust60实际 Linux `host_owned_storage_management` 三个既有正常入口因这些记录被新后缀检查误拒，原件 `_tmp/20261114-fork-recovery/linux/candidate60/affected.stderr.log`；不把该回归称为基线必需行为。

## Rust 行为

`closed_fork_prefix` 检查完整同turn后缀时允许越过 session_metadata，但沿原selector省略它，不加入子历史。缺parent仍只沿已验证真实来源祖先修复；metadata后的真实新step、模型、工具、交互或上下文事件仍拒绝。原有step/终态/正文/附件与来源验证不放宽。

## 范围

仅澄清原条目中的后缀许可描述：session_metadata是可扫描越过、但不携带的非语义记录。不是允许任意事件或忽略元数据前后的未来业务语义。既有坏历史离线恢复工具与队列完全不变。

## 机器判据

`session_fork_lifecycle_tests::assistant_fork_omits_nonsemantic_metadata_before_and_after_terminal` 核元数据省略、真实闭合与artifact保留、parent来源及原历史不变；`middle_assistant_rejects_later_step_and_response_without_mutating_source` 加入元数据后继续拒绝未来模型步骤。三个首败host storage入口须在相同新候选实际复验；当前待运行，不预称PASS。另一个首败是合成Node fixture缺createdAt被真实reader过滤，已补字段及诊断/完整读回前置，不改产品reader。

## 批准记录

主协调依据用户当前直接修复及项目内自主授权，批准这项精确回归修复与说明补充，持久授权见 `state/decisions-20261114.md` 的“分叉续聊及已有坏历史恢复”。原60的25PASS/4FAIL保持，不回写原条目或判分器；新候选仍需双平台受影响及F01–F09实证。
