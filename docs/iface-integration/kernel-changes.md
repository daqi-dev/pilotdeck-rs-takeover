# Kernel/ifACE逐逻辑改动追踪

冻结起点 `ca0e465be3b2788cbfaa078abd0dfc621de69e5a`（ifACE 0.1.7），候选 `41e2f4d16fe369c00d81960ebc0e5f269b263477`（ifACE 0.1.8）。只读这两个Git对象及中间六次提交；未把正在进行的 ModelExecutionContext 纳入已实现。当前本地候选已提交；推送/远端主线合入状态未核实。这里只观察本地远端跟踪引用，未联网刷新，不能据此断言未推送。

本记录区分公开ifACE API、SDK资源面、Kernel生产组装/内部修复、兼容版本登记和测试前提。0cc至093虽沿用0.1.7版本号，均是ca0之后的新改动，不能归称ca0已有能力。源数据是 `state/iface-kernel-changes.json`；本文件不修改主ledger、三表同步器或远端。

| 稳定变更ID | 逻辑改动 | 类别 | 首次修复提交 | 后续修正 | 验证边界 |
| --- | --- | --- | --- | --- | --- |
| iface.change.external-model-injection | 公开完整模型注入及生产异步运行桥 | Kernel生产组装 | `0cc25c1b` | 无 | 有历史局部实测；当前候选无已关联验证记录 |
| iface.change.external-context-injection | Context在默认初始化后注入并保留身份 | Kernel生产组装 | `0cc25c1b` | 无 | 有历史局部实测；当前候选无已关联验证记录 |
| iface.change.external-scheduler-assembly | Native Service/Loop公开生产组装入口 | Kernel生产组装 | `0cc25c1b` | 无 | 实际收据已核：包含执行失败；见candidateObservations；待主ledger关联 |
| iface.change.atomic-scheduler-install | Scheduler绑定与资源快照原子安装 | Kernel内部修复 | `0cc25c1b` | 无 | 实际收据已核：包含执行失败；见candidateObservations；待主ledger关联 |
| iface.change.scheduler-model-resources | SDK受管D96资源与有界事件分页 | SDK资源面 | `0cc25c1b` | `107bb7d9`, `e9f8ab84`, `9ab6bdff`, `09328735` | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.resource-source-fence | 按输入冻结资源来源身份与租约epoch | Kernel内部修复 | `107bb7d9` | `e9f8ab84` | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.continuation-absolute-deadline | 人类续答及存储等待保留原绝对期限 | Kernel内部修复 | `107bb7d9` | 无 | 有历史局部实测；当前候选无已关联验证记录 |
| iface.change.cold-load-settlement-fence | 冷加载和取消收尾保留来源围栏 | Kernel内部修复 | `e9f8ab84` | 无 | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.default-host-execute-build | 默认特性保留普通L-C execute入口 | Kernel内部修复 | `e9f8ab84` | 无 | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.d96-generation-collision | D96生成设置与会话代次字段分离 | Kernel内部修复 | `9ab6bdff` | 无 | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.resource-semantic-reply | 资源返回只封装一次并保留ABI语义 | Kernel内部修复 | `09328735` | 无 | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.public-model-json-profile | 公开富模型JSON profile和结构化事件合同 | 公开ifACE API变更 | `41e2f4d1` | 无 | 实际收据已核：有范围内PASS；相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.profile-host-admission | profile生产协商、输入能力门和wire透传 | Kernel生产组装与内部修复 | `41e2f4d1` | 无 | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.sticky-response-failure | 响应失败粘滞及结束分类不混同 | Kernel内部修复 | `41e2f4d1` | 无 | 实际收据已核：相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.trusted-readonly-tool-effect | 可信工具效果分类与只读资源调用 | 公开ifACE API变更及Kernel内部修复 | `41e2f4d1` | 无 | 实际收据已核：包含执行失败；有范围内PASS；相关host目标编译阻塞未执行；见candidateObservations；待主ledger关联 |
| iface.change.public-managed-http-port | 公开受管HTTP字节流端口及本地取消令牌 | 公开ifACE API变更 | `41e2f4d1` | 无 | 实际收据已核：包含执行失败；有范围内PASS；见candidateObservations；待主ledger关联 |
| iface.change.managed-http-runtime | 受管HTTP生产实现与origin凭据绑定 | Kernel内部实现 | `41e2f4d1` | 无 | 实际收据已核：包含执行失败；见candidateObservations；待主ledger关联 |
| iface.change.stream-secret-redaction | 跨chunk及JSON转义形态密钥脱敏 | Kernel内部修复 | `41e2f4d1` | 无 | 实际收据已核：包含执行失败；有范围内PASS；见candidateObservations；待主ledger关联 |
| iface.change.builtin-usage-optionality | 内置OpenAI usage保留缺失字段 | Kernel内部修复 | `41e2f4d1` | 无 | 实际收据已核：有范围内PASS；见candidateObservations；待主ledger关联 |
| iface.change.iface-version-compatibility-registration | ifACE0.1.8及独立身份包兼容登记 | 兼容版本登记 | `41e2f4d1` | 无 | 实际收据已核：有范围内PASS；见candidateObservations；待主ledger关联 |
| iface.change.abi2-consumer-test-prerequisite | ABI2流消费测试前提与受管fixture修正 | 测试前提修正（非产品fix） | `9ab6bdff` | 无 | 有历史局部实测；当前候选无已关联验证记录 |
| iface.change.atomic-tool-install | 外部工具安装失败回滚绑定与目录 | Kernel内部修复 | `0cc25c1b` | 无 | 当前候选未关联验证记录 |
| iface.change.committed-loop-execution-identity | Loop推进使用已提交轮次与绝对期限 | Kernel内部修复 | `0cc25c1b` | `107bb7d9` | 有历史局部实测；当前候选无已关联验证记录 |

已更新Linux41e2首批及独立clippy实际收据；每项当前候选结果见下方新增记录。历史局部PASS仅针对原run版本/范围。新收据proposedRunId供主ledger唯一维护者关联，本文件不append主ledger。后续修复尚未重跑，首败持续保留。

## iface.change.external-model-injection — 公开完整模型注入及生产异步运行桥

原ExternalBackends仅有session factory，完整ModelContract/Stream与probe缺显式生产注入。 ExternalModelBackend显式注入probe/contract/可选stream；冲突配置在ready前拒绝，缺流/会话不静默回退；经gateway及owned Tokio runtime。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel模型装配与模型调用生命周期。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.assembly`, `iface.contract.model`, `iface.contract.auth`；任务：`iface.P2.embedding`, `iface.P2.model-context`, `iface.P3.model`, `iface.P4.composition`。
- 冻结源码：`kernel/host/src/startup/embed.rs:217`（`pub struct ExternalModelBackend`）；`kernel/host/src/startup/resident/external_backends.rs:12`（`pub(super) fn assemble_model`）；`kernel/host/src/startup/resident/external_backends/model_runtime.rs:11`（`struct Driver`）。
- 历史账本run关联：`iface.run.linux-9ab6bdff-recheck-host-external`, `iface.run.linux-e9f8ab84-rest-host-model-runtime`, `iface.run.linux-e9f8ab84-rest-host-health`。
  - `iface.run.linux-9ab6bdff-recheck-host-external` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-e9f8ab84-rest-host-model-runtime` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-e9f8ab84-rest-host-health` = PASS；仅其actualVersions/command/evidence范围。
- 限制：ExternalBackends是agentcpm-host产品组装API，不是agentcpm-iface新增trait。只支持默认/scripted配置被外部完整模型替换。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.external-context-injection — Context在默认初始化后注入并保留身份

默认store/trace初始化可能覆盖外部Context；基线缺ExternalBackends.context。 就绪前安装显式ContextContract，后续空安装不重建默认实现。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel持有context seam；能力包持有算法状态。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.assembly`, `iface.contract.context`；任务：`iface.P2.embedding`, `iface.P2.model-context`, `iface.P3.context`。
- 冻结源码：`kernel/host/src/startup/embed.rs:195`（`pub context:`）；`kernel/host/src/startup/resident/external_backends.rs:48`（`server.state().context_seam = Some(context)`）。
- 历史账本run关联：`iface.run.linux-e9f8ab84-rest-host-context`。
  - `iface.run.linux-e9f8ab84-rest-host-context` = PASS；仅其actualVersions/command/evidence范围。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.external-scheduler-assembly — Native Service/Loop公开生产组装入口

可信组装入口未能把SchedulerAssembly接到TCP/UDS及stdio共同启动链。 ExternalBackends.scheduler_assembly在ready前安装，权限及资源检查仍归Kernel。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel调度、注册表与执行生命周期。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.assembly`, `iface.contract.native-loop`；任务：`iface.P2.embedding`, `iface.P4.composition`, `iface.P4.loop`。
- 冻结源码：`kernel/host/src/startup/embed.rs:197`（`pub scheduler_assembly:`）；`kernel/host/src/startup/resident/external_backends.rs:51`（`server.install_components(assembly)`）；`kernel/host/src/startup/resident/stdio_face.rs:139`（`external_backends::install`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：只证明生产组装入口存在；不证明PilotDeck Loop已完成整体验收。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.atomic-scheduler-install — Scheduler绑定与资源快照原子安装

逐条register后才安装resource/运行时/OnceLock会在后续失败时残留半激活绑定或Loop条目。 预校验和运行时准备完成后，在同一临界区提交registry+resources，再发布gateway与loop/execution view。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel装配注册事务。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.atomic-install`, `iface.contract.assembly`；任务：`iface.P2.atomic-install`, `iface.P2.embedding`。
- 冻结源码：`kernel/core/src/scheduler/registry.rs:50`（`pub fn install_assembly`）；`kernel/host/src/transport/scheduler.rs:161`（`host.install_assembly(bindings, resources)?`）。
- 历史账本run关联：`iface.run.linux-0cc25c1b-source-deps-core-registry`, `iface.run.linux-107bb7d9-default-core-registry`, `iface.run.linux-e9f8ab84-remaining-host-install`。
  - `iface.run.linux-0cc25c1b-source-deps-core-registry` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-107bb7d9-default-core-registry` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-e9f8ab84-remaining-host-install` = PASS；仅其actualVersions/command/evidence范围。
- 限制：本项是Scheduler装配快照的事务；不推定跨所有ExternalBackends能力拥有统一跨组件事务。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.scheduler-model-resources — SDK受管D96资源与有界事件分页

Native组件缺既有model.session/input/tool_result生命周期及模型事件回读的正式Resources路径。 新增scheduler.model-session-resources.v1、EventsReadRequest/EventsPage与model.events_read有限拉页；沿既有dispatcher，非新增L-C方法。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel模型会话注册表、事件环与Scheduler Resources。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.native-loop`, `iface.contract.state-ownership`, `iface.contract.lifecycle`；任务：`iface.P2.model-context`, `iface.P4.loop`, `iface.P6.contracts`。
- 冻结源码：`kernel/scheduler-protocol/src/model_session_resources.rs:12`（`pub const FEATURE`）；`kernel/host/src/transport/scheduler/resources.rs:552`（`fn events_page(`）。
- 历史账本run关联：`iface.run.linux-09328735-resources-host-resource`。
  - `iface.run.linux-09328735-resources-host-resource` = PASS；仅其actualVersions/command/evidence范围。
- 限制：页limit为1..64；max_bytes默认64KiB、范围1..1MiB；超页拒绝而非无界克隆。空页非完成，subscription_closed非会话关闭/远端停止。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.resource-source-fence — 按输入冻结资源来源身份与租约epoch

旧principal键可能被同主体后续连接替换，旧调用借新连接/新租约权限；资源目标和会话代次需再次围栏。 EntryIdentity以(session,input_ref)只插入一次，捕获ConnState/epoch；实际调用校验会话/代次/租约并绑定SchedulerSource。

- 首次修复：`107bb7d96880c37f9deef10becbb614d5ec86114`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel认证连接、租约及已准入调用。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.auth`, `iface.contract.state-ownership`, `iface.contract.lifecycle`, `iface.contract.model`；任务：`iface.P2.embedding`, `iface.P2.tools-auth`, `iface.P2.history-lifecycle`, `iface.P4.state`。
- 冻结源码：`kernel/host/src/transport/scheduler/resources.rs:9`（`pub(super) struct EntryIdentity`）；`kernel/host/src/transport/model_session/async_host.rs:20`（`pub(crate) struct SchedulerSource`）。
- 历史账本run关联：`iface.run.linux-09328735-resources-host-resource`。
  - `iface.run.linux-09328735-resources-host-resource` = PASS；仅其actualVersions/command/evidence范围。
- 限制：来源保留于受信宿主对象；请求payload不能覆盖身份。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.continuation-absolute-deadline — 人类续答及存储等待保留原绝对期限

NeedsHuman结束初次timeout后，continue可能获得新时长或无限I/O；准备阶段存储等待也可能越过deadline。 答复准入前检查原deadline；续答drive_loop按剩余绝对时间执行；已运行后超时保守OUTCOME_UNKNOWN。

- 首次修复：`107bb7d96880c37f9deef10becbb614d5ec86114`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel Scheduler执行生命周期。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.native-loop`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.history-lifecycle`, `iface.P4.loop`, `iface.P4.state`。
- 冻结源码：`kernel/core/src/scheduler/control.rs:210`（`let deadline = c.invocation.limits.deadline_ms;`）；`kernel/core/src/scheduler/execution.rs:230`（`Preparing the step may await storage.`）。
- 历史账本run关联：`iface.run.linux-107bb7d9-default-core-identity`, `iface.run.linux-e9f8ab84-default-core-identity`。
  - `iface.run.linux-107bb7d9-default-core-identity` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-e9f8ab84-default-core-identity` = PASS；仅其actualVersions/command/evidence范围。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.cold-load-settlement-fence — 冷加载和取消收尾保留来源围栏

InputCold异步重开路径丢SchedulerSource；取消时仅去掉来源会让清理调用获得新执行权限。 Source穿过cold reopen；settling只允许get/close/abort/interrupt在cancel_requested后收尾，仍检查session/call/turn/generation。

- 首次修复：`e9f8ab846f3b5670c3b36d43add758151f7dba62`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel D96会话与调度执行归属。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.auth`；任务：`iface.P2.model-context`, `iface.P2.history-lifecycle`, `iface.P4.state`。
- 冻结源码：`kernel/host/src/transport/model_session/async_ops/mod.rs:193`（`Plan::InputCold(plan)`）；`kernel/host/src/transport/model_session/async_host.rs:30`（`fn authorized_in(`）；`kernel/host/src/transport/model_session/async_ops/input/history.rs:72`（`source`）。
- 历史账本run关联：`iface.run.linux-09328735-resources-host-resource`。
  - `iface.run.linux-09328735-resources-host-resource` = PASS；仅其actualVersions/command/evidence范围。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.default-host-execute-build — 默认特性保留普通L-C execute入口

async_ops::execute误受cfg(test)限制，普通生产引用在默认特性构建cannot find function execute。 普通L-C调用保留execute，Scheduler仍用execute_bound；取消编译门误标。

- 首次修复：`e9f8ab846f3b5670c3b36d43add758151f7dba62`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel host构建及异步dispatcher。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.assembly`, `iface.contract.packaging`；任务：`iface.P2.embedding`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/model_session/async_ops/mod.rs:174`（`pub(super) fn execute(`）。
- 历史账本run关联：`iface.run.linux-e9f8ab84-rest-host-old-external`, `iface.run.linux-e9f8ab84-rest-host-old-duplex`。
  - `iface.run.linux-e9f8ab84-rest-host-old-external` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-e9f8ab84-rest-host-old-duplex` = PASS；仅其actualVersions/command/evidence范围。
- 限制：0cc与107 host-external构建失败留在原run中；不将编译失败记为测试执行。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.d96-generation-collision — D96生成设置与会话代次字段分离

资源适配器把Agent session generation整数写到D96 generation对象，破坏GenerationSettings schema。 D96 payload保留模型生成设置，受信代次只走SchedulerSource；非D96方法仍按原字段写代次。

- 首次修复：`9ab6bdffa4aaea947703338dabd7a582a0e6ee98`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel来源代次；模型配置归D96会话。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.state-ownership`；任务：`iface.P2.model-context`, `iface.P4.loop`。
- 冻结源码：`kernel/host/src/transport/scheduler/resources.rs:378`（`D96 `generation` is the model's GenerationSettings object`）。
- 历史账本run关联：`iface.run.linux-09328735-resources-host-resource`。
  - `iface.run.linux-09328735-resources-host-resource` = PASS；仅其actualVersions/command/evidence范围。
- 限制：e9资源测试和9ab资源测试曾失败，093局部资源测试PASS不能消除旧失败事实。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.resource-semantic-reply — 资源返回只封装一次并保留ABI语义

dispatcher返回内部semantic body，被误当完整JSON-RPC frame导致结果空值/封套丢失。 用原连接协商ABI经protocol::envelope封装一次；D96裸payload、ABI2 data/协商ABI3 payload均按原合同。

- 首次修复：`093287355acf12b24bbaea3a04adc7d002da4e32`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel dispatcher与wire封套。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.tool`, `iface.contract.product-compat`；任务：`iface.P2.embedding`, `iface.P2.model-context`, `iface.P2.tools-auth`, `iface.P4.loop`。
- 冻结源码：`kernel/host/src/transport/scheduler/resources.rs:467`（`let reply = protocol::envelope(`）。
- 历史账本run关联：`iface.run.linux-09328735-resources-host-resource`。
  - `iface.run.linux-09328735-resources-host-resource` = PASS；仅其actualVersions/command/evidence范围。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.public-model-json-profile — 公开富模型JSON profile和结构化事件合同

原D96正文/旧事件不足以保真完整请求、思考、工具start/name、finish metadata、usage缺失和供应商开放错误。 新增profile常量/协商函数、Factory.supported_profiles默认空、StructuredModelEvent/ModelEventType、semantic_kind、ToolCallStarted/ResponseMetadata/ResponseFailed。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel持有会话/预算/工具登记；适配器负责同源JSON与投影。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.context`, `iface.contract.tool`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P4.loop`, `iface.P6.contracts`。
- 冻结源码：`kernel/iface/src/model_session/profile.rs:88`（`pub struct StructuredModelEvent`）；`kernel/iface/src/model_session/operations.rs:149`（`fn supported_profiles`）；`kernel/iface/src/model_session/event.rs:140`（`pub fn semantic_kind`）；`kernel/docs/notes/model-json-profiles-v1.md:28`（`## 数据范围与保真`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：构造器只保证非空JSON和投影类别，不解析验证完整schema/逐字段同源；该验证归声明profile的适配器。
- 限制：event_json文本可保留\udxxx；普通Rust String标识符不能无损持有孤立UTF-16代理项。
- 限制：旧builtin OpenAI未声明rich profile，保留历史start/name arguments_delta与缺finish metadata限制。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.profile-host-admission — profile生产协商、输入能力门和wire透传

新增公开profile必须在分配/后端调用前协商，且不得被配置更新或改MIME绕过；原文不能被Host JSON模型重序列化。 open/get回报协商profile；会话内固定；未协商Structured拒绝；富RequestSnapshot保留内层文本并要求准确MIME、已协商及media_input=true。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel模型会话准入、registry与provider wire。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.auth`, `iface.contract.context`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P4.loop`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/model_session/async_ops/open.rs:67`（`negotiate_model_profiles`）；`kernel/host/src/transport/model_session/handlers_input/append.rs:243`（`rich model request profile requires a media-capable profile adapter`）；`kernel/host/src/transport/model_session/registry.rs:290`（`structured model event profile was not negotiated`）；`kernel/host/src/model_provider/protocol.rs:560`（`WireEventKind::Structured`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：media_input=false即使纯文本富请求也Unsupported；本轮没有通用富请求降级器。
- 限制：provider初始化支持列表及重启一致性是协议门；不能把旧工厂默认为完整profile支持。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.sticky-response-failure — 响应失败粘滞及结束分类不混同

源error或finish_reason=error之后的completed可能把失败重标成功；消息结束可能早于usage/真实流结束；资源释放必须有证。 记录provider_error并在发布completed前normalize成ResponseFailed；ResponseMetadata非终态；resource_released=false保留Unknown/占用，true才释放生成。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel单次生成任务、预算和资源占用。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.model-context`, `iface.P4.loop`, `iface.P4.state`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/src/transport/model_session/execution.rs:48`（`provider_error:`）；`kernel/host/src/transport/model_session/execution.rs:568`（`pub(crate) fn normalize_completion_event`）；`kernel/runtime/src/model/session_support/event_queue.rs:21`（`pub fn is_post_terminal_fact`）；`kernel/model-session/src/input_ledger/commit.rs:94`（`match event.kind.semantic_kind()`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：ResponseFailed非SessionFailed；HTTP读流取消非远端计算停止；订阅EOF非生成成功。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.trusted-readonly-tool-effect — 可信工具效果分类与只读资源调用

read_only原先一律拒tool，缺可信精确实现分类；模型目录/参数不能自行授予只读执行权。 新增ToolEffect及ToolContract.effect默认Unknown，受信安装捕获到不可变Binding；tool.list/authorize/exec按allowed_tools和ReadOnly过滤，排队后重查。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel工具绑定、授权和执行；可信工具实现声明效果。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.tool`, `iface.contract.auth`, `iface.contract.native-loop`；任务：`iface.P2.tools-auth`, `iface.P3.tools`, `iface.P4.loop`, `iface.P6.contracts`。
- 冻结源码：`kernel/iface/src/tool.rs:141`（`pub enum ToolEffect`）；`kernel/host/src/startup/tool_factory.rs:192`（`let effect = item.contract.effect`）；`kernel/host/src/transport/scheduler/resources.rs:502`（`fn tool_allowed(`）；`kernel/host/src/transport/service/binding.rs:275`（`pub(crate) effect:`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：Unknown/SideEffect在只读调用拒绝；目录注册和请求声明不构成权威；审批不可替换已冻结实现。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.public-managed-http-port — 公开受管HTTP字节流端口及本地取消令牌

模型包需要真正流式、可取消、宿主预算与凭据隔离的公开传输端口。 新增HttpStreamRequest/Head/Sink/Control/Cancel/Transport；响应头与body顺序交付，Ok只表示正常完整EOF，Control由可信宿主构造。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel可信传输控制；模型包负责供应商协议解析。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.auth`, `iface.contract.lifecycle`；任务：`iface.P2.model-context`, `iface.P3.model`, `iface.P6.contracts`。
- 冻结源码：`kernel/iface/src/env/http_stream.rs:71`（`pub trait HttpStreamTransport`）；`kernel/iface/src/env/http_stream.rs:63`（`pub struct HttpStreamControl`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：该trait本身不提供生产出站实现、不证明Control已从当前Scheduler执行deadline导出；不能把非受信值直接当作执行上下文。
- 限制：正在进行的ModelExecutionContext不在41e2冻结对象中，不记为已实现。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.managed-http-runtime — 受管HTTP生产实现与origin凭据绑定

普通模型HTTP需要统一出站准入、绝对期限、取消唤醒及响应体有界读取。 ManagedHttpTransport绑定origin与可信凭据；保留重复响应头字节；DNS/connect/TLS/read受期限约束；cancel shutdown本地socket；接Egress gate和StreamBody。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel出站授权、凭据及本地socket生命周期。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.auth`, `iface.contract.model`, `iface.contract.lifecycle`；任务：`iface.P2.tools-auth`, `iface.P3.model`, `iface.P4.composition`, `iface.P6.contracts`。
- 冻结源码：`kernel/runtime/src/model/managed_http.rs:26`（`pub struct ManagedHttpTransport`）；`kernel/runtime/src/model/managed_http.rs:228`（`fn execute_inner(`）；`kernel/runtime/src/model/transport/http.rs:213`（`pub fn resolve_with_cancel`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：HTTP/1.1直连、内置WebPKI根、identity编码；不跟随重定向；没有代理/自定义CA/压缩支持。
- 限制：max_body_bytes为可信构造参数且必须>0；不是公开全局固定1MiB。复用响应头上限64KiB；8为DNS并发上限。
- 限制：没有从此实现推出41e2所有生产模型调用均已接线；实际使用须由组装注入。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.stream-secret-redaction — 跨chunk及JSON转义形态密钥脱敏

逐chunk脱敏会泄漏分块的密钥；JSON混合大小写转义/反斜杠歧义也可能漏检。 SecretRegistry共享线性转义形态匹配；RegistryStreamRedactor保留可续成密钥的有界后缀，完整匹配输出***，正常EOF前缀掩码，错误取消丢尾；ManagedHTTP响应头/body/error用同一注册表。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel可信凭据注册与出站响应脱敏。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.auth`, `iface.contract.model`；任务：`iface.P2.tools-auth`, `iface.P3.model`, `iface.P6.contracts`。
- 冻结源码：`kernel/core/src/guard/secret_stream.rs:10`（`pub struct RegistryStreamRedactor`）；`kernel/core/src/guard/policy_escape.rs:62`（`pub(super) fn match_escaped_prefix`）；`kernel/runtime/src/model/managed_http.rs:181`（`struct RedactingSink`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：只对显式注册的密钥值及其JSON转义形态脱敏，不声称高熵秘密自动识别；跨chunk缓冲可能延迟部分字节。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.builtin-usage-optionality — 内置OpenAI usage保留缺失字段

usage_json原先把缺失token值补0，改变原语义。 只输出Some的input/output/total token字段，保留显式零与缺失区别。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：模型端点适配器提供usage；Kernel计量真实可表示投影。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.model`, `iface.contract.state-ownership`；任务：`iface.P3.model`, `iface.P6.contracts`。
- 冻结源码：`kernel/packages/model-endpoint-openai/src/session/inner.rs:474`（`pub(crate) fn usage_json`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：此修正不升级builtin OpenAI到富事件profile。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.iface-version-compatibility-registration — ifACE0.1.8及独立身份包兼容登记

公开API加法必须同步crate、api-baseline及独立iface身份包版本/消费者约束。 agentcpm-iface0.1.7→0.1.8；model0.1.4→0.1.5、env0.1.7→0.1.8、tool0.1.3→0.1.4；schemas/各mint依赖/API baseline同步。

- 首次修复：`41e2f4d16fe369c00d81960ebc0e5f269b263477`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel接口及分发版本登记。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.packaging`, `iface.contract.assembly`, `iface.contract.model`, `iface.contract.tool`, `iface.contract.auth`；任务：`iface.P0.sources`, `iface.P0.catalog`, `iface.P2.embedding`, `iface.P6.release`。
- 冻结源码：`kernel/iface/Cargo.toml:3`（`version = "0.1.8"`）；`kernel/iface/api-baseline.txt:2047`（`HttpStream`）；`products/iface-identity/model/schema.json:6`（`"version": "0.1.5"`）；`kernel/host/mint.toml:88`（`"iface/model" = "=0.1.5"`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：版本/schema登记不证明发布、push、源码安装或双平台发行验收完成。
- 限制：0cc..093的新host能力是基线之后0.1.7版本号下的补丁，不属于ca0基线已有能力。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.abi2-consumer-test-prerequisite — ABI2流消费测试前提与受管fixture修正

external_backends测试使用握手最低ABI常量请求model.stream（要求ABI2），且冲突配置/线程退出对照不够严格。 Client明确协商ABI2并断言；加可正常启动配置的对照、准确冲突诊断与owned Resident关闭，避免错因/悬挂host被当成产品失败。

- 首次修复：`9ab6bdffa4aaea947703338dabd7a582a0e6ee98`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：测试fixture持有其启动宿主线程和临时数据。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.assembly`, `iface.contract.model`, `iface.contract.packaging`；任务：`iface.P2.embedding`, `iface.P6.contracts`。
- 冻结源码：`kernel/host/tests/external_backends.rs:149`（`model.stream is an ABI 2 method`）；`kernel/host/tests/external_backends.rs:190`（`struct Resident`）。
- 历史账本run关联：`iface.run.linux-9ab6bdff-recheck-host-external`。
  - `iface.run.linux-9ab6bdff-recheck-host-external` = PASS；仅其actualVersions/command/evidence范围。
- 限制：e9 host-external的ABI不匹配失败保留原run，不能把此测试修正计作产品model.stream修复。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.atomic-tool-install — 外部工具安装失败回滚绑定与目录

工具绑定prepare后store创建/目录注册/激活记录失败可能残留prepared或半注册目录。 store创建失败abort；目录/activation record失败删除已注册项并abort；仅记录成功后激活冻结绑定。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel工具目录与绑定快照事务。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.atomic-install`, `iface.contract.tool`, `iface.contract.assembly`；任务：`iface.P2.atomic-install`, `iface.P2.tools-auth`, `iface.P2.embedding`。
- 冻结源码：`kernel/host/src/startup/tool_factory.rs:218`（`let mut registered: Vec<String>`）；`kernel/host/src/startup/tool_factory.rs:246`（`external tool activation record failed`）。
- 历史账本run关联：无；当前候选实际收据见末节及JSON candidateObservations。
- 限制：回滚目录/绑定范围明确；不将记录追加后的后续失败宣传为跨store统一事务。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## iface.change.committed-loop-execution-identity — Loop推进使用已提交轮次与绝对期限

Loop.step原先硬编码turn=1，并把绝对deadline改为相对剩余量；创建时ctx不能代表续答后已提交interaction。 ExecutionIdentity.deadline_ms保持原绝对时间戳，step使用current.interaction.turn。

- 首次修复：`0cc25c1bbd740c60993cb0e29cabe3ec96a38d93`；候选：`41e2f4d16fe369c00d81960ebc0e5f269b263477`。
- 状态归属：Kernel调度已准入输入与执行身份。Kernel责任如上；PilotDeck提供独立实现/兼容策略，组装层提供可信配置与实现。
- 合同：`iface.contract.native-loop`, `iface.contract.context`, `iface.contract.lifecycle`, `iface.contract.state-ownership`；任务：`iface.P2.history-lifecycle`, `iface.P4.loop`, `iface.P4.state`。
- 冻结源码：`kernel/core/src/scheduler/execution.rs:229`（`input.execution.deadline_ms = current.invocation.limits.deadline_ms;`）；`kernel/core/src/scheduler/execution.rs:246`（`.step(&ctx.invocation.context_ref, current.interaction.turn, input)`）。
- 历史账本run关联：`iface.run.linux-0cc25c1b-source-deps-core-identity`, `iface.run.linux-107bb7d9-default-core-identity`。
  - `iface.run.linux-0cc25c1b-source-deps-core-identity` = PASS；仅其actualVersions/command/evidence范围。
  - `iface.run.linux-107bb7d9-default-core-identity` = PASS；仅其actualVersions/command/evidence范围。
- 限制：源码存在不等于真实宿主验收；历史收据不外推至41e2完整候选或PilotDeck产品联合验收。

## 合同五层审计建议

保留13条合同原ca0规范来源；新增candidateSourceRefs分别指向41e2冻结代码。以下为建议，主ledger需独立应用。`PARTIAL`表示有局部源码证据，不能据此关闭整层；`UNKNOWN`表示本轮不足判断。

| 合同 | interfaceDefined | implemented | productionWired | externallyInjectable | jointlyVerified |
| --- | --- | --- | --- | --- | --- |
| iface.contract.assembly | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.native-loop | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.model | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.context | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.tool | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.history | UNKNOWN | UNKNOWN | UNKNOWN | UNKNOWN | UNKNOWN |
| iface.contract.auth | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.lifecycle | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.state-ownership | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.atomic-install | PARTIAL | PARTIAL | UNKNOWN | PARTIAL | UNKNOWN |
| iface.contract.product-compat | PARTIAL | PARTIAL | UNKNOWN | UNKNOWN | UNKNOWN |
| iface.contract.packaging | PARTIAL | PARTIAL | UNKNOWN | UNKNOWN | UNKNOWN |
| iface.contract.migration | UNKNOWN | UNKNOWN | UNKNOWN | UNKNOWN | UNKNOWN |

模型/工具/组装的接口、实现及可注入层有局部source evidence，保持PARTIAL；productionWired指PilotDeck正式入口，保持UNKNOWN。history/migration本轮未提供充足新增代码证据，保持UNKNOWN；product-compat仅封套局部、packaging仅版本登记，生产接线与可注入仍UNKNOWN。productionWired统一保留UNKNOWN（指PilotDeck正式入口接线，Kernel路径不能提升该层）；全部jointlyVerified保持UNKNOWN，不以Kernel局部测试、纯包测试或管理表同步替代产品组合验收。

## 明确限制与收据口径

- profile必须显式协商；`media_input=false`即使纯文字富请求也Unsupported。StructuredModelEvent构造器只验证投影类别、非嵌套/生命周期伪造及非空文本，同源逐字段校验归适配器。
- 请求/事件JSON文本可保留`\udxxx`；Rust UTF-8标识符不能持有孤立代理项。读取时PD ModelPull用pilotdeck_protocol::stringify_json，route provider/model用to_utf8并明确拒绝不可表示标识符；PD工作区并非本审计冻结对象，不列为Kernel fix。
- HTTP当前仅HTTP/1.1直连、内置WebPKI根、identity、不跟随redirect；响应体上限来自max_body_bytes构造参数（>0），64KiB为响应头上限，1MiB为事件分页上限，不能混称统一HTTP限制。
- HttpStreamControl字段公开并不证明其值来自当前调度执行；41e2不含正在设计的ModelExecutionContext。实际组装仍需验证可信deadline/session/correlation来源。
- PD mac-model-packages-v3收据为3+11+38+22=74 passed（另1 ignored），不是74项Kernel宿主联合测试；当前ledger尚无该批run，故未关联。Mac Kernel iface216原件已核，216PASS仅WIP记录源文件与冻结候选匹配范围，不伪造二进制或完整commit执行身份。
- ABI2 fixture修正与owned host线程清理单列为测试前提；它不构成产品model.stream修复。所有产物详情、hash、source evidence、历史run命令保存在JSON。

## 41e2最后一批实际收据（保留首败）

| 实际选择 | 结果 | 精确范围及缺口 |
| --- | --- | --- |
| `core-secret-stream` | PASS / executed | 5/5 PASS；仅core增量secret_stream，不证明受管HTTP总体通过。 |
| `runtime-managed-http` | FAIL / executed | 9 PASS/1 FAIL；delivers_first_chunk_before_eof_and_cancel_wakes_read在managed_http.rs:493断言head包含429/one/two失败。 |
| `host-readonly-tools` | FAIL / executed | 真实resident+Native场景FAIL：execution.open返回MethodNotFound(-32601)，未到只读tool合同验证；生产route缺口须修复后重跑。 |
| `host-resources` | FAIL / build | host lib构建E0603：model_provider/tests/structured.rs:30引用private transport::model_session，19目标未执行。 |
| `host-binding` | NOT_RUN / not_run | 共享host lib E0603编译阻塞，1项未执行。 |
| `host-approval` | NOT_RUN / not_run | 共享host lib E0603编译阻塞，1项未执行。 |
| `host-provider-structured` | NOT_RUN / not_run | 共享host lib E0603编译阻塞，4项未执行。 |
| `host-profile` | NOT_RUN / not_run | 共享host lib E0603编译阻塞，3项未执行。 |
| `host-execution-structured` | NOT_RUN / not_run | 共享host lib E0603编译阻塞，6项未执行。 |
| `openai-legacy-profile` | PASS / executed | 1/1 PASS；明确legacy builtin不发未协商rich profile discriminator。 |
| `openai-missing-usage` | PASS / executed | 1/1 PASS；缺失usage不补零。 |
| `fmt` | FAIL / build | FAIL：env/mod.rs导出换行和runtime/model/mod.rs模块排序两处。 |
| `clippy` | NOT_RUN / not_run | 首批因host lib共享编译失败未执行；另见独立core/runtime/iface clippy实际FAIL。 |
| `independent-clippy` | FAIL / build | 独立core/runtime/iface clippy -D warnings FAIL：managed_http.rs:146 AtomicUsize.fetch_update已deprecated。 |
| `mac-iface-216` | PASS / executed | agentcpm-iface 216 passed；执行时为WIP source，receipt逐文件哈希与41e2冻结对象匹配范围如下，不伪造完整冻结commit/二进制身份。 |

只读真实场景的失败发生在正式production route `execution.open`，返回MethodNotFound；未到工具分类执行断言。合同实现保持PARTIAL并明确入口缺口；PilotDeck正式入口productionWired保持UNKNOWN。后续作者修复未在本轮冻结候选重跑，不得覆盖首败。

Mac216逐文件哈希核对：114相同，0不同；执行时WIP源稳定，未保存binary hash。原件：`/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-iface-kernel-tests/public-profile-iface-first-receipt.json`；日志：`/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-iface-kernel-tests/public-profile-iface-first.log`。该记录只覆盖iface包测试，不覆盖真实宿主。

- `iface.change.external-scheduler-assembly`：EXECUTED_FAIL；`iface.run.linux-41e2f4d1-first-host-readonly-tools`。
- `iface.change.atomic-scheduler-install`：EXECUTED_FAIL；`iface.run.linux-41e2f4d1-first-host-readonly-tools`。
- `iface.change.scheduler-model-resources`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-resources`。
- `iface.change.resource-source-fence`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-resources`。
- `iface.change.cold-load-settlement-fence`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-resources`。
- `iface.change.default-host-execute-build`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-resources`。
- `iface.change.d96-generation-collision`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-resources`。
- `iface.change.resource-semantic-reply`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-resources`。
- `iface.change.public-model-json-profile`：EXECUTED_SCOPED_PASS_WITH_BLOCKED_HOST；`iface.run.mac-public-profile-iface-first-216`, `iface.run.linux-41e2f4d1-first-host-provider-structured`, `iface.run.linux-41e2f4d1-first-host-profile`, `iface.run.linux-41e2f4d1-first-openai-legacy-profile`。
- `iface.change.profile-host-admission`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-provider-structured`, `iface.run.linux-41e2f4d1-first-host-profile`。
- `iface.change.sticky-response-failure`：BUILD_BLOCKED_NOT_EXECUTED；`iface.run.linux-41e2f4d1-first-host-execution-structured`。
- `iface.change.trusted-readonly-tool-effect`：EXECUTED_FAIL；`iface.run.mac-public-profile-iface-first-216`, `iface.run.linux-41e2f4d1-first-host-readonly-tools`, `iface.run.linux-41e2f4d1-first-host-binding`, `iface.run.linux-41e2f4d1-first-host-approval`。
- `iface.change.public-managed-http-port`：EXECUTED_FAIL；`iface.run.mac-public-profile-iface-first-216`, `iface.run.linux-41e2f4d1-first-runtime-managed-http`, `iface.run.linux-41e2f4d1-first-fmt`。
- `iface.change.managed-http-runtime`：EXECUTED_FAIL；`iface.run.linux-41e2f4d1-first-runtime-managed-http`, `iface.run.linux-41e2f4d1-first-fmt`, `iface.run.linux-41e2f4d1-independent-clippy`。
- `iface.change.stream-secret-redaction`：EXECUTED_FAIL；`iface.run.linux-41e2f4d1-first-core-secret-stream`, `iface.run.linux-41e2f4d1-first-runtime-managed-http`。
- `iface.change.builtin-usage-optionality`：EXECUTED_SCOPED_PASS；`iface.run.linux-41e2f4d1-first-openai-missing-usage`。
- `iface.change.iface-version-compatibility-registration`：EXECUTED_SCOPED_PASS；`iface.run.mac-public-profile-iface-first-216`, `iface.run.linux-41e2f4d1-first-fmt`, `iface.run.linux-41e2f4d1-independent-clippy`。
