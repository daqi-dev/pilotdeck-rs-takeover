---
id: S1-RESTORE-NO-REPLACE-001
---

## 类别

数据安全修复：归档恢复发布完整临时文件时拒绝覆盖竞争者已创建的目标，已有目录或软链接也视为目标占用。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 `createLocalGateway.ts:1323-1365`先检查普通transcript存在，再写`.restore.tmp`，最后rename到最终路径。正式TS恢复RPC的EIO/持流中断和成功重启证据在`_tmp/20261113-s1-session-restore/ts-1/observed.json`；本平台`filesystem-atomic-proof.json`实际证明rename覆盖已存在目标，而同目录hard_link返回存在冲突。TS先检查再发布间没有跨进程的无覆盖承诺。

## Rust 行为

`native_process/session_archive.rs`复用原TranscriptWriter在同目录唯一临时文件写完整JSONL并sync_all，用hard_link原子发布而不替换最终名字，随后只清理自己拥有的临时名字。目录、普通文件、活软链或悬空软链均返回SESSION_RESTORE_CONFLICT；发布时外部creator抢占也返回该冲突，既有目标字节不变。其他操作系统错误保留其真实阶段和错误码。

## 范围

仅本地restore_session_transcript最终目标的占用/竞争安全边界。不改变归档校验、正常JSONL字段顺序、标题/消息内容或export结果；普通写入中段失败不产生最终文件是对齐TS原子恢复行为，不是本条新增豁免。请求取消仍等待blocking工作保有fence和writer lease；进程SIGKILL可能留下独立临时名，下一次用新stage重试，不扫描删除其他写入者文件。取消与最终发布竞争允许观察完整成功提交，绝不允许半导入。

## 机器判据

`native_process/session_archive_s1_tests.rs`覆盖阶段中外部目标抢占、已有文件/目录/两类软链不覆盖、同文件别名和writer lease、失败/请求取消后的重试。正式`tests/fixtures/s1-session-restore/capture.mjs` safe模式要求中段EIO无最终文件、同key重试成功、正常有/无title导入、重启export一致、中段SIGKILL后无最终文件且可重试；错误必须按来源核对，不统一成EIO。

截至登记Rust修后候选未编译/执行。修前固定binary SHA256为7ba4eabc0738974f7c80a2d37062ff56f90c740e66eab69d942472552fa38f59，已真实复现半导入但未定位其source-build回执；仅为固定旧binary证据，不冒充当前HEAD。修后须用固定源码和构建身份重新运行。

## 批准记录

用户2026-11-13执行计划授权数据安全修复；主协调明确要求restore原子提交、失败可重试且既有目标不可覆盖，并授权复用writer与同目录临时文件。本条记录hard_link相对TS rename的精确竞争边界，不作为未执行测试的通过证明。

### 2026-11-13 验证进展追加

第四轮gateway全部targets编译检查已exit0，证据 `_build/20261113-043855-h1-fourth-check`；首10测试为3 passed/7 failed/0 ignored（`_build/20261113-044959-s1-first`），不得称通过。7失败均为测试将导入字段顺序当作export顺序；固定TS两轮的首次/重启四个原始RPC帧都支持产品的`schemaVersion,format,messages,title`。仅新增独立有序expected_export，保留完整JsValue严格断言；产品hash不变，修后测试待重跑。最新正式TS证据为`ts-verified1/2`七段、三次进程，另实际确认目录EISDIR、活软链文件冲突、悬空/目录软链被TS替换和竞争者被TS rename覆盖；本条只对这些占用/竞争差异授权。
