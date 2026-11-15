# PilotDeck Native Loop 接入边界

`pilotdeck-iface` 把现有 `pilotdeck_agent::turn::run_admitted_turn` 暴露为公开 Scheduler SDK 的 `LoopFactory` / ifACE `LoopContract`。它是接入组件，复用原Loop算法并把受管资源调用交给Kernel。

本文描述产品本地候选`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`。该候选已接原admission、完整preparation与resource codec边界，旧文档“生产admission/codec尚未接通”的判断已过期。正式产品组装尚未完成，仍需代码审阅和实际验证；factory相关11项局部测试没有完整driver的实际运行证明。

历史`ca0/0.1.7`用于解释起点缺口；模型profile等新增合同属于后续`0.1.8`本地候选。Kernel `41e2`已有实际FAIL，后续`481723`包含route修复但尚未验证。

## 候选源码已具备的执行边界

- 组装方使用 `OriginalPayloadAdmission`，从真实 `CallContext` 和完整产品载荷调用共享的 `OriginalFactoryPayloadDecoder` 与 `OriginalTurnPreparation`。ifACE 的最小文本消息不能替代原有富内容边界。`CallContext.invocation.payload` 必须装原 execute 边界的 JSON 字符串，避免 SDK 的 `serde_json::Value` 丢失孤立 UTF-16 码元。
- `AdmittedTurn` 刻意不接受 `HostPort`。组件内部创建 `KernelHostPort`，所有模型、工具、上下文等宿主操作只能经当前调用绑定的 `SchedulerClient.resource` 发出。编解码器不能通过旧 `LocalGateway` 兜底。
- 准备阶段只接收组件内部创建的 port。通知和事件观察器通过 `OriginalBindings` 显式接收同一个 invocation-bound SDK `Client` 与 `CallControl`；它们必须实现真实投递、取消和未知效果结算，没有默认空观察器。
- 真正的 `run_admitted_turn` 在阻塞工作线程执行，控制面保持可响应。一次 step 的 future 被丢弃，不会导致第二次执行；相同输入重取保留的结果，变更输入或执行身份被拒绝。
- 组件依据可信 Scheduler 调用绑定检查 context、run、turn。截止时刻使用调用的绝对时间，转换一次为本地单调截止；不重设一段新的超时。
- 中断只声明 Cancel/Expire。收到中断不等于副作用撤销；未确认效果、丢失回复或工作线程崩溃保留 Unknown。没有自动重放。
- 原 PilotDeck 结果通过其现有 `turn_wire::result` 投影，并保留完整终态载荷 `result/messages/seedState/modelState`。Scheduler 业务结果为该 JSON 的字符串，保留被转义的孤立 UTF-16 码元；组装兼容层应按 PilotDeck 协议解析，不能直接用 `serde_json::Value` 重新规范化原消息。MaxTurns 维持旧入口的 Failed 分类。

## 共享准备包

`pilotdeck-turn-preparation` 仅依赖 `pilotdeck-agent` 与 `pilotdeck-protocol`，不依赖 transport、Gateway、Kernel 私有实现或宿主 IO。旧 transport 和新 Native 入口直接调用同一份代码，避免两份独立实现逐渐分歧：

- 完整 factory payload 解码、富内容和媒体规范化、权限配置、工具描述、消息/模型事件/请求/工具结果准入。
- 请求组装、token cap 与 calibration、工具并发安全判定、工具结果观察、原有三条压缩准备路径。
- 初始化严格保留 PlanTodo 读取 → 模型 metadata 读取 → file/model 状态恢复 → 通知观察器创建的顺序；状态解析失败时不会创建观察器。
- `PreparationEffects` 把原 `InstructionBridge` 的通知 IO 留在组装边界。旧 transport 使用原桥的 trait adapter；Native 要求提供正式 SDK 资源实现。文件状态只是终态 round trip 投影，不据此授予文件访问权限。

原 transport 的运行时、活动记录、事件发送、未知效果检查和关闭机制仍留在 transport。提取准备包不表示这些 IO 合同已经迁移。

## 资源映射与未完成项

`ResourceCodec` 是纯边界翻译接口，`ResourceStep` 表示一次既有 Kernel 请求、有取消边界的有界等待或已核实的 typed 回复。它没有默认“空成功”实现。原产品admission、完整preparation与codec调用边界已共享接入；模型会话codec也已有候选实现。这里的“已接”表示源码调用链存在，不能泛化为所有HostCall转换已完整、正式产品组装已完成或候选已通过。工具、Context、SDK通知/事件观察器的具体产品实现和真实宿主验证仍需逐项闭合。

| PilotDeck HostCall | Kernel 正式资源与接入要求 | 状态 |
|---|---|---|
| ModelPrepare / Materialize / Pull / Close | 模型会话 open/input_append/input_commit；正式分页事件；close/abort 后核实真正关闭。完整四协议、usage、工具调用和错误语义必须保留 | `ModelSessionCodec` 候选实现 prepare/pull/close；materialize 和 metadata 不虚假声明，真实宿主验收待完成 |
| ListTools / DecidePermission / Execute | tool.list/describe/authorize/exec、operation.get；授权和执行由 Kernel 负责，ACK 不等于完成 | 尚需产品 descriptor、permission、result 的完整转换 |
| PrepareContext | context.assemble；对照当前快照格式保留富内容 | 尚需产品 Context 实现与结果转换 |
| CaptureTurn / PersistCompaction | context.history.commit/read；明确输入消费、CAS 和恢复原子性 | 原整快照 append 不足以关闭产品持久化合同 |
| Budget / Steer / PlanTodo / Lifecycle | 逐项确定当前公开接口及唯一状态所有者 | 不存在对应资源时不声明支持，也不转发旧宿主 |

模型和工具编解码器须按已冻结的 ifACE 版本及新增正式资源合同验收，不重写一套简化的对话循环。本轮候选SDK新增的`model.events_read` 提供有界分页会话事件；消费时必须区分空页、订阅关闭和确认终态，并保留原模型请求、usage、工具调用及错误数据。不能用 session_close 的应答代替关闭事件，也不能把 Unknown 改成 Closed。

### 模型会话 codec 的具体边界

`ModelSessionCodec` 使用 Kernel 新增公开模型 JSON profile，必须在 open 的 `extensions` 声明两个 required profile，并核对 `negotiated_profiles` 回显。旧宿主或旧后端未确认协议时停止，不能把旧文本事件当成富模型事件继续执行。当前富请求合同还要求后端声明`media_input=true`，即使业务内容只有文字。profile协商、具体后端实现和受管HTTP route均须在同一候选上验证。

- prepare 打开并等待受管 request-mode 会话；模型请求使用原 canonical JSON profile，核对 provider/model 与已绑定 route 一致。
- pull 按宿主回显的实际输入限额分段 append，保留原请求字节与输入身份；commit 后才读取有界事件页。空页使用受原取消与 deadline 控制的短等待，不返回 done。
- 逐页校验 Agent session、model session、input、response、连续序号和 profile。原事件 JSON 由 PilotDeck parser 读取；工具 ID、名称、参数以及用量和结束原因必须与 Kernel 的语义投影一致，不能用 raw 事件改写已授权对象。
- `message_end` 只是原模型事件；后续 usage 保持原顺序。真正的 ResponseCompleted/ResponseFailed 到达后请求正常关闭，确认 SessionClosed 后才结束 pull。关闭 ACK、订阅结束与 Unknown 都不能代替关闭事实。
- 所有 Native 出口显式调用 codec 的资源收尾清单，包括 prepare 后的错误路径。收尾有界，未知结果优先于普通错误；只收到明确的准入拒绝时才撤销尚未打开的预留。
- 该 profile 保留的是既有 JSON 传输语义。对象中的 undefined 省略、数组 undefined 与非有限数变为 null，不能据此声称任意 JsValue 逐值可逆。孤立 UTF-16 通过原 JSON 转义保留；无法表示为 Unicode scalar 的文本/参数片段不伪造 Kernel 文本投影；工具身份字段不可无损表示时明确拒绝。

公共 profile 规范由 Kernel 的 `kernel/docs/notes/model-json-profiles-v1.md` 定义。这些是本轮新增候选接口，不是初始 `ca0e465` 已经具备的能力。当前 codec 不声明媒体通道、duplex、跨请求会话恢复或工具结果续传，不能据此关闭整个 Model 接入任务。

## 验证口径

已有分批局部回归涉及执行身份、重复step、取消前准入、future丢弃、明确拒绝和未知效果区分；共享准入回归覆盖富消息、孤立 UTF-16、初始状态、初始化顺序和取消前零调用。旧 transport 的 factory payload 目标和实际 budget loop 回归检验共享提取后的既有路径。这些测试只证明适配器边界与提取行为，不代表真实宿主、模型协议、工具权限、持久化或完整产品入口已经验收。构建和运行结果由本轮验收账本按原身份另行登记，不能合并历史通过数称当前组合通过。factory相关11项局部测试尚无完整driver；需要正式宿主装配、实际安装能力、真实产品入口以及失败/取消/关闭链的独立证据。`41e2`失败与`481723`修复待验状态分别保留。

能力位暂仅声明 `interruptible`，不声明 streaming、resume、steer、handoff、并行工具或人机阻塞。只有完整实现和实际验证后才逐项开启。
