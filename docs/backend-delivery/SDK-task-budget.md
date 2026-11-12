# SDK taskBudget 原生生产接线

状态：进行中。新owner、admission、turn options、terminal settle及clear进入第六候选。首check exit101发现测试helper的format大括号错误，已按根窄授权改为同顺序JsObject，SHA572fc27bb12c4a8620a84137910a7e6ac370a33448e9d9b01fcfeeddbbf325ae；**尚未完成修后编译或新测试**。作者未自行运行Cargo。固定TS基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。

## 单一所有权与接口

`native_process/task_budget.rs` 新NativeTaskBudget持有一个Mutex<TaskBudgetStore>、共享SessionOverrides、Options与时钟。原store继续负责所有JSONL持久化、scope键、project retention、runId幂等；没有第二套费用账本或新持久格式。

O1在assembly构造一次Arc，SDK配置、admission、Requests删除和ephemeral清理共用该Arc。Email唯一修改sdk_config：gate允许taskBudget，所有候选校验完成后、发布SDK配置前调用configure_sdk，取消false不发布，已有JSON相等早退位置不变。

- `configure_sdk(session,project,config,completion)`：按固定TS配置project总额/retention；project根取本次set配置的project或默认项目，不能错误沿用上一配置的项目。
- `SubmitTaskBudgetSnapshot::snapshot_for_run`：取精确admission snapshot并绑定completion身份；无预算返回Undefined。
- `turn_options(session,runId)`：返回同run冻结的taskBudgetUsd/initialTaskBudgetSpentUsd。prepare不重新读取其他session可能已改变的项目费用。
- `observe_event(input,event)`：只对真实raw turn_completed.result.budget中taskBudgetUsd已定义的事件，按turnId与turnSpentUsd结算。不是从客户端声明或Gateway投影usage猜费用。
- `release_run`：finish_pump清理临时快照；缓存另保留completion并在下一次snapshot/options读取时清除已结束项，覆盖尚未进入pump的取消/预检早退。
- `clear_sdk`：只清session账本，project累计费用不因单session删除而消失；持久clear成功后才删临时run缓存。Requests随后才清MCP/SDK缓存。

根已明确授权本作者只改runtime/turn/admission.rs和submit_host.rs的预算trait、snapshot/options、observe/release窄块及methods/session_dir.rs的预算clear块；没有修改Protocol负责的host_factory/live插件切片。

## 固定基线

- `src/cli/ProjectRuntimeRegistry.ts:1058-1073`：SDK snapshot从session配置取scope，项目取已配置total，费用取ledger。
- `:1375-1407`：真实turn费用按runId只结算一次；同一project多个session共享费用，session scope分开。
- `:1409-1453`：project总额/retention由首次配置固定，仅在后续project配置时按不活跃窗口过期。
- `src/gateway/client/InProcessGateway.ts:1133-1181`：预检拒绝已耗尽预算。
- `:1555-1557`：传入taskBudgetUsd和initialTaskBudgetSpentUsd。
- `:1650-1656`：仅raw turn_completed.result.budget标记存在时记录turnSpentUsd。

不同session并发可获得相同initialSpent，基线没有项目预算reservation，本迁移不新增预扣费协议。真实取消若产生带budget的终态仍结算；没有budget的session_aborted不会凭空收费。被新fence替代或超时后旧事件的既有丢弃规则不在本子包擅改。

## 错误与风险

原store有先内存修改后append的基线行为，IO失败后同run重试会被内存去重变成假成功。NativeTaskBudget对写IO错误fail closed，不改store格式；预算操作持续明确失败，普通无预算submit_turn的快照预检不额外受阻；需要写账的configure/clear仍明确报错。session删除clear失败保留SDK/cache并返回错误。差异与边界登记 `specs/gateway-parity/diffs/A7-SDK-TASK-BUDGET-IO-001.md`。

不承诺重启恢复未落盘费用，不承诺跨会话文件删除/账本append事务原子性，不把错误状态当作成功预算结算。

## 待执行验证

新owner六项测试：scope与冻结snapshot、8线程同run幂等/项目隔离、重启retention、取消/带真实预算的错误终态、真实append IO失败重复run不假成功、clear IO失败保留配置/缓存。

```sh
cd src
cargo test -p pilotdeck-gateway --lib native_process::task_budget::tests
```

真实固定TS模块+实际JSONL IO短oracle已准备在 `_tmp/20261113-sdk-task-budget/oracle.cjs`，全src冻结期间未移动到fixture；已获根轻量槽2执行两轮并cmp一致，槽已归还；原始oracle-first/second.json与identity.json同目录。真实TS证实IO首错EISDIR、重复同run resolved且内存spent=0.5；该原始缺陷未被比较器抹掉。verify.py完整正控PASS，double-charge与hide-false-success两种注入实际exit1，日志同目录。仅runtime.resolve为provider seam，预算方法和fs均不替换。正式SDK入口的实际定价模型usage→terminal预算→append→下一轮拒绝与重启组合仍待补验，不能用owner函数测试替代。

第七检查后按根窄窗口已追加正式核心测试 `production_priced_usage_settles_and_restart_admission_reads_the_same_ledger`，并将已执行TS oracle/verify同字节移入 `tests/fixtures/h1-sdk-task-budget/`。该测试使用真实LocalGateway、本地HTTP模型usage和明确美元费率，检查settled文件、下一次拒绝且无新模型请求、重启读取同账。代码SHA80729e4200be56b93e912d8782db59b567ffa6b8ff16e8e092874912fe6edd67，进入第八源码冻结，尚待执行。

Protocol独审发现的两个缺口已进入修后候选：等待owner锁期间取消需要锁后重查（下述因果测试已落）；ephemeral清理此前忽略clear错误，O1现将helper改为Result并纳入shutdown cleanup聚合。两项均待实际新测试，不再把旧包装误说成已验收。不改变原文件先删顺序，不引入跨文件/账本+SDK事务，不回滚同步write内已发生的基线副作用。

第八types/check已通过，clippy报本文件3项样式诊断（其余作者诊断另由根协调）。根给精确窗口后仅修改task_budget.rs：增加锁后completion重查与 `cancelled_while_waiting_for_budget_owner_does_not_write_configuration` 的线程局部one-shot因果测试，并修3项clippy；rustfmt/diff-check通过。新SHA145054ac92b551403fea3ebb021ee7756630e8499ca99c6930488eb34508b111，已交根最终lib编译。预算测试现共8项（6 owner、真实定价组合、锁等待取消），新修尚未执行，未自行Cargo。
