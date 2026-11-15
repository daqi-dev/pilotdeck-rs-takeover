# ifACE 代码与 PilotDeck 接入解读

本文解释ifACE如何把能力接口、Kernel宿主、SDK与PilotDeck产品组装连起来。历史源码定位固定于Kernel `ca0e465be3b2788cbfaa078abd0dfc621de69e5a` / ifACE `0.1.7`；当前本地候选已推进到ifACE `0.1.8`，不能把历史限制或历史PASS直接移到新候选。版本和合同见[合同审计](contracts.md)，实际任务与结果见`state/iface-integration.json`。当前仍在实施、审阅和验证阶段。

产品`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`已接原准入、完整准备和resource codec边界；正式产品组装及完整driver尚未完成。Kernel `41e2`已有实际FAIL与构建阻塞，后续`481723`已含route修复但尚未验证。以下先解释应怎样运行，再说明哪些代码和验证已经具备。

## ifACE 解决什么问题

ifACE 规定不同组件共同遵守的能力接口和运行规则，包括输入输出、错误、权限前提、状态和更换实现时的行为。它既向使用者说明如何请求模型、工具和会话资源，也向实现者说明应怎样提供这些能力。

接口本身不执行模型请求、文件写入或循环。提供能力的组件与 Kernel 的管理检查代码装在一起，才形成可运行的 Kernel。例如 PilotDeck 可以提供上下文裁剪算法和模型协议实现；Kernel 仍负责按共同规则检查当前调用是否允许、属于哪个会话、是否已经取消，以及如何记录执行事实。这个分工来自规范 §1.6，而不是从 Rust 文件夹名称推断。

目标中的完整产品调用可以按下面的顺序理解；这张图说明职责，不是当前所有箭头均已完成的报告：

```text
PilotDeck CLI / Gateway / 渠道
        ↓ 产品输入与输出适配
Kernel 接纳会话／执行，选择已注册的 PilotDeck Loop
        ↓ Loop 通过公开 SDK 请求资源
Kernel 检查身份、代次、权限、预算及调用状态
        ↓ 调用已绑定的能力实现
PilotDeck Model / Tool / Context 包
        ↓ 结果、错误或 Unknown
Kernel 记录事实并返回，产品层按原协议展示
```

纯算法调用不必穿过跨进程通信；有权限或执行状态要求的资源调用仍需经过相应检查。本地同进程运行不代表可以绕过授权或自行改写权威历史。

## 几组容易混淆的名字

| 名称 | 含义 | 本次用法 |
|---|---|---|
| L-A | 受信同进程Rust契约，例如`Arc<dyn ModelContract>` | 宿主允许的信任范围内使用；受管能力仍须经过共同检查 |
| L-C | 进程之间的协议，含版本、身份和方法语义 | 产品控制与独立执行者通过受管协议访问 Kernel |
| Native provider | 非 Wasm 的组件执行接入；当前 SDK 提供双向 NDJSON provider 适配 | PilotDeck Loop 受 Scheduler 管理，资源请求返回共同资源路径 |
| Scheduler SDK | 公共执行、状态、输出及资源调用类型 | 使用`LoopFactory`、`Client::resource`等公开面；不是模型/工具的实际实现，也不是宿主本身 |
| 产品组装根 | 选择实现、构造实例、连接能力的可信启动代码 | 可依赖公开宿主入口`serve_resident`与具体实现包；能力包不依赖私有HostState；完整产品组装仍待完成 |

Native 不能简单等同于“全部在同一个进程”。`kernel/scheduler-protocol/src/provider.rs:1` 明确把该模块定义为双向 NDJSON codec／adapter；真正生命周期与权限仍由宿主管理。外部 provider、L-C 方法和 L-A trait 是不同接缝，能力覆盖需要分别验证。

## 从哪些代码读起

| 入口 | 读完应理解什么 |
|---|---|
| `kernel/docs/KERNEL-MECHANISMS-AND-ABI.md` §0、§1.6、§6.9 | 哪些是共同规则、谁提供能力、谁做检查，以及可替换要怎样验收 |
| `kernel/iface/src/lib.rs:25` | 历史起点的公开模块、异步返回类型和版本常量 |
| `kernel/iface/src/loop_.rs:23` | Loop 的描述、推进、打断、状态；等待不是完成 |
| `kernel/iface/src/model.rs:73` 与 `model_stream.rs:43` | 单次调用、显式结果和真实增量流的区别 |
| `kernel/iface/src/tool.rs:86` | 工具调用身份、环境归属、参数、结果和 Unknown |
| `kernel/iface/src/context/contract.rs:12` | 上下文快照与可替换的组装能力 |
| `kernel/scheduler-protocol/src/contract.rs:248` | 组件用什么公共客户端请求资源，以及 Loop 工厂如何接入 |
| `kernel/host/src/startup/embed.rs:187` | 历史公开宿主入口；后续注入与接线修订须按各自候选核查 |
| `kernel/host/src/transport/scheduler/resources.rs:71` | 资源请求如何重新检查会话、代次、委派权限并进入具体能力路径 |

这些是冻结提交中的定位；开发后的文件行号可能变化。需要重现时用完整 commit 执行 `git show <commit>:<path>`。

`BoxFuture` 是统一异步返回形态，不等于“所有方法只能立刻返回”。`loop_.rs:7` 明确允许一般实现等待 I/O，要求等待时不能占住其他会话共用的锁；旧同步适配器和个别探测入口仍存在立即 Ready 约束，必须按实际调用点检查，不能把一次 poll 的历史限制推广给全部组件。

## 四类能力分别承担什么

**Loop 是算法与推进策略。** `LoopContract` 定义 `step`、打断和查状态的共同操作；PilotDeck 的原有循环策略可以作为实现保留。Kernel 管理执行身份与生命周期，Loop 不再自行拥有第二份权限、预算或会话权威。把整个旧 LocalGateway 装进 Loop 虽然可能能跑，却不能证明资源访问已经受 Kernel 共同规则约束。

**Model包含几种不能互相替代的能力。** `ModelContract::call` 的响应 bytes 表示已抽取内容；不能偷偷塞入供应商整个响应信封。需要 usage、finish_reason、结构化 tool_calls 时应保留 `ModelCompletion`。流式能力用独立 `ModelStreamContract` 表达，供应商增量由实现包映射；把最终字符串切成多段不符合真实流式语义。`ModelSessionFactory` 又是另一组持续会话能力，注入它不能自动补齐普通完成与流式路径。

**Tool 是实际能力执行。** `ToolSpec` 说明目录与参数，`ToolContract` 提供执行行为，`ToolCall` 带调用身份和环境，`ToolResult` 能表达结果未知。工具执行者负责完成已获准的操作和释放本次资源；审批决定不能由模型参数或工具自行伪造。取消请求已经发出，不等于副作用确定没发生；Unknown 必须留给查询和恢复逻辑处理。

**Context 是发给模型之前的上下文组装。** `ContextSnapshot` 同时保存 session、turn、不透明 bytes、内容类型及能力版本。裁剪、工具结果配对、压缩策略可以由 PilotDeck 提供；快照是一种视图，不自动替代原始历史。`ca0/0.1.7`起点资源桥对turn和编码存在接入缺口。后续修订与局部测试按各自提交记录，不能继续把起点缺口说成所有当前源码的现状，也不能由局部修复宣告产品上下文完整通过。

## 源码为什么看起来“有接口却接不上”

Rust 中 `pub` 只说明相对于所在模块的可见性。一个结构体即便写着 `pub`，如果它的上层模块没有公开，也没有从 crate 根再导出，外部产品仍无法按该名字构造它。

在历史起点`ca0/0.1.7`里，`serve_resident` 已从 host crate 根导出；`ExternalBackends` 有外部模型会话工厂和工具列表，但工具绑定类型的公共可命名性不足。Scheduler 的 Native 安装代码已经存在，其模块注释却明确记录正式装配接线断开。Context 合同存在，实际资源桥又把轮次固定为 1。这些分别是“接口声明”“代码实现”“公开注入”“正式接线”和“语义完整性”的问题。

这些起点限制是本轮改动的原因；当前是否已补足，要看[Kernel变更追踪](kernel-changes.md)、候选源码和实际运行证据。产品目前已接原准入、完整准备和codec边界，但仍缺正式产品组装，不能把“已有适配器”称为“用户入口已切换”。

表格为每项能力单列五层结果：接口已定义、能力已有实现、正式入口接线、公开注入、当前组合已验收。“能力实现”可包括Kernel管理检查代码和PilotDeck提供的具体实现，责任归属另列。对前几层的静态证明不填成最后一层的PASS。

## 依赖、版本与安装

历史起点`ca0`的`agentcpm-iface`、`ABI_VERSION`和`API_VERSION`均为`0.1.7`；Scheduler SDK为`0.3.0`，其协议字符串为`agentcpm-scheduler/1`。`HANDSHAKE_ABI_VERSION = 1`是握手整数的起点，不代表每个L-C方法都接受ABI1。

`41e2`首次公开ifACE `0.1.8`的新模型JSON profile、`ResponseFailed`、`ToolEffect`与`HttpStreamTransport`等API。`0cc`至`093`的宿主/SDK增量仍属`0.1.7`。当前`481723`候选包含后续route修复，但缺本候选实际验证；`41e2`的实际失败仍保留。Rust包版本、SDK版本、Scheduler协议、L-C协商整数ABI、实际候选提交是独立身份，不互相替代。

公开 L-C 连接还要通过 `initialize` 协商整数 `abi_version`，每个方法按自己的最低版本检查。例如本次起始提交的 `host/src/transport/method_table.rs:354` 将 `model.stream` 声明为 v2b-2 方法，即要求协商 ABI 至少为 `2`；`host/src/transport/dispatch.rs:223` 会实际拒绝协商 ABI 1 的调用。Rust trait 可编译、ifACE crate 为 `0.1.7` 或 Scheduler 使用 `/1`，均不能跳过这一方法级门。接入用例须分别记录 Rust 包版本、Scheduler 协议、L-C 实际协商版本及必要 features，保留低版本拒绝的负向用例。

源码有少量历史说明仍写“冻结版 v0.1.0”，carrier 示例说明也记录了它编写时的 `0.1.6`。当前版本值应从相应提交的清单和常量读取，不能照抄旧说明文字。

历史起点的外部Rust契约分发使用 `agentcpm/iface` carrier，包含同一份 `agentcpm-iface` 源码。`packages/iface-*` 的旧生成 SDK 已冻结，不应该手改成一个未实际分发的新 SDK。源码联调可以固定到同一 Kernel 提交的 path 依赖；发行验收必须证明实际安装包、Cargo 类型来源和组装产物身份一致。

`examples/iface-consumer/contracts` 与 `carrier` 展示了独立实现、相同 trait 类型和真实 Kernel 消费点的验证方法，也明确写出了证明范围：离线 staging 不是线上安装，测试用 echo model 不是生产模型，FsBackend／EmbeddingContract 的深层消费点仍有公共注入限制。不能将这些 fixture 的通过扩展成所有 PilotDeck 能力已经可接入。

## 当前模型会话适配怎样工作

PilotDeck的`ModelSessionCodec`把原模型请求和事件转换为ifACE `0.1.8`的公开profile，`pilotdeck-model-iface::PilotDeckModelFactory`提供模型会话实现，并接收可信宿主提供的配置与HTTP流传输。它们分别是使用侧codec和能力实现侧adapter，SDK则传送资源请求，host负责共同检查；三者不应统称“模型服务”。

profile需要在open时明确要求并核对协商回显。当前富请求路径还要求实现声明`media_input=true`，即使业务内容只有文字。`StructuredModelEvent::new`检查事件与投影类别及JSON文本非空，完整字段与同源投影验证由profile适配器负责；不能说构造器替所有后端做了完整语义验证。旧builtin实现未声明新profile时明确拒绝，不能由旧文本兼容推导富profile已支持。

Agent的`session_id`、持续模型会话的`model_session_id`与单次生成的`response_id`分别管理。`GenerationSettings`与`ModelSessionConfig.generation`是生成参数；M36的`generation`是Agent会话代次，`connection_generation`与`epoch`又分别表示连接代次与推进权围栏。中文说明注明含义，原API字段和类型保持原样。

## 接入后如何判断成功

先在同一调用点换两个实现，保留调用方和业务断言，再验证拒绝、慢调用、取消、绑定切换、丢失回包和不兼容情况。最后从真实 PilotDeck CLI、Gateway 与渠道入口检查产品行为。两类验证都需要：前者证明接口确实可用，后者证明用户的产品路径真正接通。

本次不采用任意运行中状态热迁移。新会话切换 Kernel、旧会话保留归属、静止时导入并保留原件，是已选定的产品迁移方式；默认切换与旧运行时退出仍以相应实际验收为条件。
