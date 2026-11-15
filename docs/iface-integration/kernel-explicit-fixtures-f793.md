# f793独立追踪：fixture修复与52 graceful实际受测

本批实际受测`f7931007c72f087170589aab1c603a837c37ab01`，no-default-features，Cargo artifact实际features`[]`。新增2项逻辑；52的2项生产修复只在本文件追加f793受测适用性，不修改已冻结52文件当时NOT_RUN事实。未运行Cargo、未写ledger/remote，旧来源SHA全部保持。

| 稳定ID | 逻辑改动 | 分类 |
| --- | --- | --- |
| `iface.change.external-session-explicit-binding-fixtures` | 旧external异步fixture补显式model与endpoint | 测试前提修正（非生产准入放宽） |
| `iface.change.model-admission-local-clippy-fixture-cleanup` | 共同准入adapter局部Clippy注释与unused mut清理 | 局部Clippy门禁和测试清理 |

## iface.change.external-session-explicit-binding-fixtures

aba26项旧异步测试的open请求省略model/endpoint，被新增真实external绑定合同提前拒绝，未进入原目标场景。 仅测试请求补model=scripted及fixture-controlled/fixture-tool endpoint_ref；保持幂等参数差异和原目标断言。handlers_parse准入生产blob不变，显式绑定负控仍实际PASS。

- 状态归属：Kernel测试工厂与异步fixture合法输入。
- 合同：`iface.contract.model`, `iface.contract.context`, `iface.contract.tool`, `iface.contract.assembly`；任务：`iface.P2.model-context`, `iface.P2.tools-auth`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/async_integration_tests.rs:152`（`"endpoint_ref":"fixture-controlled"`；blob `f7d8306d8a4ccbb28897cf26d64c155743971ccc`）；`kernel/host/src/transport/model_session/async_integration_tests/event_stream.rs:55`（`"endpoint_ref":"fixture-controlled"`；blob `d508e08ec4e4fbd50996cb04a014c577a389aa9e`）；`kernel/host/src/transport/model_session/async_integration_tests/finish_order.rs:55`（`"endpoint_ref":"fixture-controlled"`；blob `6a8a9d881caa1de42e826d6b7cefd8ef3f05175f`）；`kernel/host/src/transport/model_session/async_integration_tests/input_ledger.rs:48`（`"endpoint_ref":"fixture-controlled"`；blob `0aab5e13e7e0b58d1108e64cced35c761a8432c9`）；`kernel/host/src/transport/model_session/tool_results/tests/async_path.rs:445`（`"endpoint_ref":"fixture-tool"`；blob `d03321f6ab17caa53341bc5dda7f5ed8baebcfb4`）。
- 限制：完整26项对照来自postflight原表；不能以fixture修正宣传产品默认model/endpoint已被补回或兼容限制取消。
- 限制：原aba234PASS/26FAIL及两Clippy首败不删除、不重标f793；52原source仍保当时未测事实。
- 限制：default矩阵、单独生产cargo build、PilotDeck产品resident/full矩阵本候选未运行；公开token API仍未冻，不纳本批。

## iface.change.model-admission-local-clippy-fixture-cleanup

aba Clippy首败为admit_generation参数8/7和epoch测试unused_mut；不涉及新的授权或模型运行判决。 仅对同一共同准入adapter限定allow(clippy::too_many_arguments)并解释保持共用输入；epoch测试去mut；不改函数签名或运行时检查。

- 状态归属：Kernel局部lint例外与测试绑定可变性。
- 合同：`iface.contract.model`, `iface.contract.packaging`, `iface.contract.assembly`；任务：`iface.P2.model-context`, `iface.P6.contracts`。
- 精确冻结源码：`kernel/host/src/transport/model_session/registry.rs:729`（`This adapter mirrors the shared admission inputs`；blob `61163de37f7a54f709d32cd883de9fee628d645d`）；`kernel/host/src/transport/model_session/execution/http_scope/tests.rs:321`（`fn original_epoch_is_rechecked_during_body_poll()`；blob `17db92ec7142010139854ba976b2edbe28ff4c8f`）。
- 限制：该allow只覆盖此adapter，不把整个模块Clippy关闭；实际no-default all-targets Clippy通过范围按argv保存。
- 限制：原aba234PASS/26FAIL及两Clippy首败不删除、不重标f793；52原source仍保当时未测事实。
- 限制：default矩阵、单独生产cargo build、PilotDeck产品resident/full矩阵本候选未运行；公开token API仍未冻，不纳本批。

## 实际结果与52生产行为的适用性

274次标准执行全部PASS=262 model_session+6 trusted_egress+6 external_backends。http_scope22、原26失败逐项复跑、两新增graceful测试均已包含262，不重复计数。fmt及Host no-default all-targets Clippy -D warnings PASS。所有执行二进制前后及全部检查后的SHA相同。

模型测试名单相对aba260为added3、removed1，净增加2。三个added分别是closing_queue复核、Unknown cleanup、admitted queued generation graceful close；removed是旧queued_commit检查名称，不能把added3称净新增3。

52的queue drain与duplex pending-boundary生产文件到f793逐blob相同；registry整blob因局部Clippy注释不同，但has_drainable_work精确函数体相同。f793真实262测试包括两新增graceful和原duplex回归；受测身份仍f793，不把52当时NOT_RUN涂成当时已测。

旧aba26失败逐项在postflight映射为f793实际ok。handlers_parse生产blob与aba相同，builtin_binding_is_fixed_and_external_binding_must_be_explicit负控也实际PASS；修fixture没有放宽生产门控。旧aba26失败及两Clippy首败继续保留。

| 实际选择 | 结果 | 范围 |
| --- | --- | --- |
| `native-only-trusted-egress` | PASS / executed | f793 trusted_egress6实际PASS，actualfeatures[]；并非default feature矩阵。 |
| `host-model-session` | PASS / executed | f793 native-only model_session262实际全PASS；包含http_scope22及2个新增graceful测试，不另加分母；aba26失败逐项现实际PASS。 |
| `native-only-external-backends` | PASS / executed | f793 external_backends6真实resident测试PASS，actualfeatures[]；生产代码经该真实目标编译，但未另外执行standalone生产cargo build。 |
| `fmt-host` | PASS / build | 仅该f793实际命令/选择范围 |
| `clippy-host-native-only` | PASS / build | f793 Host no-default all-targets Clippy -D warnings实际PASS，aba两诊断失败原件保留。 |

## 未运行及截止线

- default生产、测试和Clippy矩阵未运行。
- 未另执行standalone生产cargo build；生产代码通过真实external_backends目标编译，不冒填独立生产build PASS。
- PilotDeck产品resident/full矩阵没有在f793执行；原e1b+0291等产品结果仍保留原身份。
- token API未冻结，留下一批；本批截止只到f793。
- productionWired与jointlyVerified保持UNKNOWN，局部Kernel Native PASS不能关闭全产品组装与联合验收。

完整UTC、argv、features、SHA、receipt/log依赖、26项对照、名单变更与52源码适用性证明在JSON。root独占主ledger/remote，本任务交付后停写。

## 52两项修复的精确复核范围

队列generation项关联`iface.run.linux-f7931007-native-tests-host-model-session`内以下三个原日志ok，覆盖范围按名字限定，均包含262总数：

- `transport::model_session::execution::commit_guard::tests::closing_queue_rechecks_policy_lease_and_cancel_before_backend` = ok。
- `transport::model_session::execution::commit_guard::tests::graceful_close_attempts_cleanup_for_unknown_generation_without_faking_release` = ok。
- `transport::model_session::execution::commit_guard::tests::graceful_close_drains_already_admitted_queued_generation_before_backend_close` = ok。

duplex项关联同一run内7个既有continuation回归ok，但没有找到专门断言“graceful close + pending_boundaries”的新case；因此只记PARTIAL_EXISTING_CONTINUATION_CASES_PASS，不用262或274整体PASS推出该组合完整通过。7项精确名单与日志SHA在JSON selectedTestEvidence。
