---
id: A7-SDK-TASK-BUDGET-IO-001
---

## 类别

bug修复：原账本先修改内存runId去重/费用，再append。append失败后同run重试可因内存去重而正常返回，未落盘费用在重启后消失，形成假成功/丢账风险。原生生产owner在IO失败后拒绝继续对预算操作宣称成功。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，`src/cli/ProjectRuntimeRegistry.ts:1375-1407` 的recordTaskBudgetSpendForSdk先加入settledRunIds、增加spentUsd，再调用append；`:1515-1538` 同步mkdir/append/compact可以抛IO异常。同run重试在`:1390`附近的settledRunIds检查提前返回。clearTaskBudgetLedger在`:1506-1509`先删除内存条目再append。

2026-11-13根授权轻量槽运行两个真实TS模块与实际JSONL IO，两轮cmp一致。`_tmp/20261113-sdk-task-budget/oracle-{first,second}.json` 明确记录首次append抛EISDIR、同run重复调用resolved、snapshot spentUsd=0.5。隔离临时根被精确替换为<ROOT>，不忽略其它错误信息；预算方法和fs没有替换。身份在同目录identity.json，验证器正控及double-charge/hide-false-success两种故障注入均按预期拒绝。已有Rust TaskBudgetStore明确保留内存先变后写的基线逻辑，本修正不改该存储实现或旧特征标准。

## Rust 行为

新 `native_process/task_budget.rs` 的NativeTaskBudget复用唯一TaskBudgetStore。任何IO写失败都会留下该owner的明确失败状态；预算snapshot和后续settle/configure/clear继续返回错误，不能被原store的同run去重掩盖。正常SDK项目total/retention冲突不把owner判为IO失败。

session删除路径先要求同owner的预算clear成功，再清MCP/SDK缓存；失败返回错误，不再吞掉预算写失败并返回ok。临时run快照也仅在clear成功后删除。原有更早的会话文件删除顺序保持，因此IO失败可留下明确报告的部分删除；本条不承诺跨会话文件与账本的事务原子性。

## 范围

只适用原生Gateway的SDK taskBudget预算存储IO失败路径。正常费用计算、session/project键、runId幂等、project retention、冲突错误、ledger JSONL格式均由原TaskBudgetStore保持。IO错误外显实现覆盖owner、live delete_session与O1已接入Result聚合的ephemeral cleanup；后者修后运行仍待验证，本条不豁免也不宣称该shutdown路径已验收。该owner失败状态不会伪造已持久化费用；重启只重读实际磁盘，**不声称恢复从未落盘的费用**。未配置taskBudget的普通submit_turn快照预检不因预算owner写失败而额外拒绝；需要写账的configure/clear清理操作仍明确报错。

## 机器判据

`native_process::task_budget::tests::failed_append_cannot_turn_duplicate_run_into_false_success`：把已加载账本文件替换为目录，首次settle失败，同run再次settle仍失败，budget snapshot失败；不能返回假成功。

`failed_clear_preserves_sdk_and_captured_turn_cache`：同类真实IO失败下，clear返回错误，SDK配置与run快照仍保留，后续预算snapshot明确失败。其余scope/concurrent/restart/cancel测试保护正常语义。

本条登记时新Rust测试尚未编译。复算由统一Cargo槽执行 `cargo test -p pilotdeck-gateway --lib native_process::task_budget::tests`，不是因本条存在而判绿。

## 批准记录

用户2026-11-13要求执行 `docs/plan/backend-completion.md`，其中“已明确的用户决定”允许修复明确的崩溃、挂起、丢数据和安全缺陷。根协调者在当前会话明确授权“IO失败owner fail closed可以实现，记录明确错误，不称重启恢复未落盘账；不要冒然变store格式”，并要求同run重试不假成功、失败不得清缓存误称成功。本条落实该窄授权，不扩大到正常账本或协议差异。
