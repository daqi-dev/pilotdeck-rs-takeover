# R1 恢复回归修复

- 任务：恢复核心后端交付的 native 宿主回归与四个失效负控；StaffDeck 延期。
- 起点：`e8618392e7e4eb24c227eedd28e14c84237ca8a4`；交付提交待根协调者集成。
- 负责人：native_regression 子任务；根协调者审阅，E0 统一编译及变异执行。
- 正式入口：production session adapter → `TurnHostFactory`；native profile 启动与 title provider。

## 根因及修改

`production_adapter_dispatches_to_host` 原先断言 native 尚未实现的错误，并将 ports 闭包设为 panic。native 已升级为真实进程内执行，实际调用 ports 后触发旧装配 panic，属于过时测试而非该路径的产品失败。本次在该场景安装实际宿主支持的 generation lease supplier：记录 `host.acquire` 并返回独有失败 `HOST_DISPATCH_LEASE_BOOM`。断言要求 ProductionAdapter 输出准确的宿主错误，且宿主 acquire 恰好一次；如果未分派、吞错、错误伪成功或重复调用均失败。生命周期 harness 无需制造模型或工具结果；产品实现未改变。

旧 `configured-hooks` 拒绝已被顶层 hooks 忽略行为替代，旧 `configured-modules` 拒绝已被 disabled skills 接受行为替代。保留两个负控名字，注入错误拒绝，绑定当前正向启动断言并恢复 `negative_assert!` nonce 证据。对应行为依据沿用当前测试所引固定基线 `loadPilotConfig.ts:343-375`，本批没有重新运行 TS，不能将旧依据冒充本次实测。

两个 title 等待负控因新增 suggestion provider 复用了相同 select 分支，旧锚各命中两次。现在包含 title 分支实际缩进，要求唯一匹配；仍注入取消/超时后不等待 provider 的同一故障，保留原等待断言及 nonce，不改变验收条件。

另按根授权将 Google complete 的 thoughtSignature 插入改为 `if let` chain，处理 E0 在固定起点报告的 `collapsible_if`；表达式和插入内容不变。

## 修改范围及接口

仅测试装配、两处现有测试断言、native-process 负控锚和 Google clippy 等价整理；无公共接口变更，无运行协议变更。

## 状态及验证

| 层次 | 状态 | 证据或未闭合项 |
| --- | --- | --- |
| 接口 | 待集成 | 无接口变化，host supplier 使用既有公开接口 |
| 实现 | 待验证 | 修改已落盘，未本机编译 |
| 接线 | 待验证 | 实际 adapter 的宿主 acquire 及失败传播断言待 E0 执行 |
| 验证 | 待验证 | 静态 66 项锚及断言绑定通过；编译、正控与四项实际变异执行待 E0 |
| 交付 | 待集成 | 需固定新集成 SHA 验证与审阅 |

本机运行 `python3 src/pilotdeck-gateway/tests/fixtures/native-process/negative_controls.py --check`：`#1071 66 unique anchors and assertion bindings: OK`。`git diff --check` 通过。本任务未调用 Cargo，遵守共享构建槽限制。静态锚通过不能证明实际变异被杀死。

下一步：E0 在固定新提交运行 `production_adapter_dispatches_to_host`、`configured_hooks_section_is_not_refused`、`production_profile_and_registration`、`title_waits_for_provider_after_abort`，再运行 `configured-hooks configured-modules title-cancel-wait title-timeout-wait` 四项负控，保存编译、nonce失败和恢复通过证据；根统一重跑 clippy。全量旧首次失败证据继续保留。
