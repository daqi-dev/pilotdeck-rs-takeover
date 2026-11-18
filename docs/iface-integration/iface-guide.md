# ifACE代码与PilotDeck接入解读

ifACE规定组件之间怎样调用、谁有权执行、结果怎样表达；Kernel宿主落实这些规则，SDK帮助组件发送和接收调用。PilotDeck保留自己的循环算法和模型协议转换，再由产品启动代码把它们接到Kernel。

本页当前源码说明固定于Kernel **`0291dbe02651e51e40d8f1689fe6f2e0c2930693`**（下文简称`0291`），产品代码固定于 **`b871eb765966681d73c2ee70db4bafa7215098bd`**（简称`b871`）。它们是本地实施候选；ifACE `0.1.8`尚未发布，局部源码或测试通过不能说明完整产品已经接通。本文未执行Cargo；验证描述来自已保存收据。

历史起点仍为Kernel `ca0e465be3b2788cbfaa078abd0dfc621de69e5a` / ifACE `0.1.7`。此前以产品`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`解释原准入、完整准备和codec接入的内容，是早期审阅基点。起点缺口和后续修订不相互覆盖；[合同审计](contracts.md)保留历史源码定位，[Kernel变更追踪](kernel-changes.md)保留逐候选证据。

## 合同、SDK和宿主各做什么

| 部分 | 普通使用者可以怎样理解 | 代码落点 |
|---|---|---|
| ifACE合同 | 共同的“怎么请求、怎么返回、什么能做”的约定；包括权限、错误、状态和替换规则 | Kernel `kernel/iface/src/`的公开trait、类型及规范§1.6、§6.9 |
| Scheduler SDK | Loop使用的公共客户端和执行协议；把资源请求送回宿主，接收结果、输出和取消 | Kernel `kernel/scheduler-protocol/src/contract.rs:248`的`Client`、`:287`的`LoopFactory` |
| Kernel宿主（host） | 接受连接、核对身份，安装实现并管理调度、授权、预算、记录和收尾 | Kernel `kernel/host/src/startup/embed.rs:252`的`serve_resident` |

合同本身不发HTTP请求，SDK也不会自动补上缺失的模型或工具实现。实际能力可以由PilotDeck提供；这些实现与Kernel的管理检查部分共同组成能运行的Kernel。产品组装代码负责构造和连接它们，可以使用公开宿主入口及具体实现包。普通能力包不需要读取私有`HostState`。

“受信同进程L-A”表示宿主允许该代码在指定信任范围运行，可以直接调用共同服务。它仍须遵守授权、预算和状态规则；少一次通信不等于少一次检查。Native Loop也不等于整个产品在同一进程：当前provider使用双向NDJSON，实际启动的是经校验的本机可执行组件。

## 一次真实调用怎样走

下面的链路已有对应源码。图中的产品正式入口及所有能力组合，仍需要完整产品组装和独立运行证据：

```text
产品组装根
  └─ serve_resident(options, ExternalBackends)
       ├─ 安装模型、工具、Context和冻结的SchedulerAssembly
       └─ 正式连接收到execution请求
            ↓
          Scheduler：核对绑定、会话、调用权限和执行限制
            ↓
          Native provider → PilotDeck NativeFactory / Loop
            ↓ 复用原准入、完整准备和run_admitted_turn
          KernelHostPort → SDK Client::resource
            ↓
          Kernel资源桥重新检查身份、授权、代次、期限与当前调用
            ├─ Model：受管模型会话 → PilotDeck模型适配 → 受管HTTP
            ├─ Tool：已绑定工具 → 共同授权与执行路径
            └─ Context：已注入的上下文实现
            ↓
          结果 / 明确失败 / Unknown → 权威记录和产品输出
```

启动时，`ExternalBackends`显式接收普通模型端点、独立模型会话工厂、工具、Context和`SchedulerAssembly`。普通模型、流式和模型会话是不同能力，不能因为注入一种就推定另外几种可用。外部Context在默认store/trace初始化后安装，避免随后被默认实现覆盖；Scheduler安装发生在就绪之前。见Kernel `kernel/host/src/startup/embed.rs:187`、`:217`与`kernel/host/src/startup/resident/external_backends.rs:41`。

正式连接先识别Scheduler帧，再选择调度入口；等待组件时不持有整个`HostState`锁。Scheduler装配会校验Native程序及其SHA256，并把绑定和资源快照一起安装。见Kernel `kernel/host/src/transport/server/conn_loop/frames.rs:67`与`kernel/host/src/transport/scheduler.rs:63`。这说明路由和装配代码存在，不意味着任意产品入口都已使用这条链。

PilotDeck端的`NativeFactory`实现公开Loop工厂。`AdmittedTurn`不接受另一个宿主对象；适配器创建调用绑定的`KernelHostPort`，由它把模型、工具和上下文请求送给`Client::resource`。实际循环仍是原`run_admitted_turn`，不必重写成简化对话循环。见产品`src/pilotdeck-iface/src/lib.rs:71`、`:394`、`:410`及`src/pilotdeck-iface/src/resources.rs:140`、`:206`。

**Kernel掌管执行权，PilotDeck掌管具体策略与转换。** Loop决定下一步怎样推进，模型包处理供应商协议，Context包处理裁剪和压缩策略；它们不能自行授予权限、重置预算或建立另一份权威历史。资源桥会重新检查请求，而不是把先前一次`tool.authorize`当作永久通行证。见Kernel `kernel/host/src/transport/scheduler/resources.rs:151`、`:352`。

## 当前模型路径：先检查内容，再执行已准入任务

`ModelContract::call`、带元数据的`call_completion`、`ModelStreamContract`和`ModelSessionFactory`分别表达普通调用、完成结果、真实增量流和持续模型会话。一条路径通过不能代替其他路径。Agent会话`session_id`、模型会话`model_session_id`和单次生成`response_id`也有不同生命周期。

当前PilotDeck路径使用`ModelSessionCodec`调用模型会话资源，`PilotDeckModelFactory`提供会话实现；后者接收可信组装方提供的配置与`HttpStreamTransport`。codec负责调用侧转换，factory负责实际能力，SDK传请求，host做共同检查。它们合在一起才构成指定模型路径。

公开request/event profile必须在打开模型会话时协商，缺少所需profile就明确拒绝。`0291`的富请求还要求request模式：先按序接收并封住完整输入组，不能在半份请求时就启动推理。这里的“封住”表示检查和执行必须使用同一份输入与配置版本。见Kernel `kernel/host/src/transport/model_session/handlers_input/append.rs:209`。

`inspect_input(input_id, expected_revision)`让可信模型适配器检查完整请求，返回输入身份、配置版本和“实际内容是否需要媒体能力”。它是只读检查：不得发网络请求、消费输入、启动推理或生成provider事件。默认实现返回`Unsupported`，不伪造“没有媒体”。宿主核对返回身份与版本，并在内容确实需要媒体而后端未声明`media_input`时拒绝。**纯文字请求现在不再一律要求`media_input=true`**；未知或无法判断的内容不能假称纯文字。见Kernel `kernel/iface/src/model_session/operations.rs:166`、`kernel/iface/src/model_session/input.rs:125`、`kernel/host/src/transport/model_session/execution/input_profile.rs:13`、`:57`。

PilotDeck用原JSON解析器检查嵌套内容，封住检查结果；当前适配只接受已检查为允许的纯文字请求。它不会借`inspect_input`声明完整媒体通道已实现。见产品`src/pilotdeck-model-iface/src/session.rs:281`、`:330`及`src/pilotdeck-model-core/src/request/media.rs:304`。

检查完成后，`scoped`模型调用携带宿主已准入的执行上下文`ModelExecutionContext`：真实Agent会话、模型会话、job、主体、绝对期限和本次本地取消令牌。它不能从模型正文反序列化，也不能代替授权Permit。期限取父执行与有效模型期限中较早者，续答或网络重连不会凭空获得新时长；本地取消也不证明远端已经停止。见Kernel `kernel/iface/src/model_session/execution_context.rs:1`、`kernel/host/src/transport/model_session/execution.rs:248`、`kernel/host/src/transport/model_session/execution/scope.rs:89`。

`commit_input_scoped`把这些限制传到实际模型执行。需要注意兼容边界：公开trait的默认实现仍委托旧`commit_input`，单有方法名不证明旧后端会消费上下文。PilotDeck当前实现明确要求scoped调用，普通`commit_input`返回拒绝。工具结果的`commit_tool_result_scoped`则默认拒绝；传入原上下文只允许符合原在途生成约束的处理，不能在响应结束后用旧凭证重新启动推理。见Kernel `kernel/iface/src/model_session/operations.rs:184`、`:207`和产品`src/pilotdeck-model-iface/src/session.rs:323`。

这里的模型“生成任务”与`GenerationSettings`生成参数、M36 Agent会话`generation`代次是不同含义；`connection_generation`和推进权`epoch`也不能互换。代码字段与API名称保留原样，中文说明按调用场景解释。

## 只读、失败和Unknown怎样判断

`read_only`是本次执行的限制，不是工具名称的描述。宿主只允许可信安装时绑定为`ToolEffect::ReadOnly`的具体实现进入只读执行；默认`Unknown`和`SideEffect`不能放行。模型参数或工具目录里自称“只读”不能授予权限。允许该effect也仍需通过共同授权及资源范围检查。见Kernel `kernel/iface/src/tool.rs:136`、`kernel/host/src/startup/tool_factory.rs:192`、`kernel/host/src/transport/scheduler/resources.rs:502`。

明确拒绝、明确失败和结果未知要分开。执行前校验拒绝可以确认后端尚未派发；模型已经报错后，后来的完成事件不能把它重标为成功。`ResponseFailed.resource_released`还区分“失败且资源已释放”和“失败但释放未确认”。取消ACK、close ACK、订阅结束或回包丢失，都不能单独证明副作用未发生或资源已回收。Unknown要保留给查询和收尾，不能自动重放有副作用的操作。见Kernel `kernel/host/src/transport/model_session/execution.rs:422`、`:436`、`:468`及`kernel/host/src/transport/model_session/registry.rs:243`。PilotDeck资源桥对已发出但无法确认的调用保留`AcceptedUnknown`，见产品`src/pilotdeck-iface/src/resources.rs:104`。

Context的快照是带内容类型的不透明bytes和真实轮次，不自动等于原始历史或原子提交。工具的可信执行上下文、参数改写后的再授权、产品Context操作以及历史revision/输入消费原子性，仍须按具体缺口和正式路径验证；不能用模型会话局部通过代替。详细边界见[工具与Context审计](tool-context-audit.md)。

## 包版本、协议和no-default不能混为一谈

以下值都来自Kernel `0291`，各有独立含义：

| 身份轴 | 当前本地候选值 | 它不能证明什么 |
|---|---|---|
| Rust crate / carrier | `agentcpm-iface 0.1.8` / `agentcpm/iface 0.1.8` | 本地版本登记不表示已经向外发布或安装 |
| 独立身份包 | `iface/model 0.1.7`、`iface/types 0.1.10`、`iface/env 0.1.8` | 不要求所有包与carrier同号；不沿用早期model `0.1.6`描述当前检查接口 |
| Scheduler SDK | `agentcpm-scheduler-protocol 0.3.0` | 不表示所有模型/工具能力已经实现 |
| Scheduler wire协议 | `agentcpm-scheduler/1` | 不等于L-C的整数ABI |
| L-C协商 | `initialize`协商的整数`abi_version`，具体方法另有最低要求 | 握手起点为1，不代表`model.stream`等方法可用ABI1 |

清单锚点为`kernel/iface/Cargo.toml:3`、`kernel/iface/mint.toml:9`、`products/iface-identity/model/mint.toml:7`、`products/iface-identity/types/mint.toml:7`、`products/iface-identity/env/mint.toml:7`、`kernel/scheduler-protocol/Cargo.toml:3`及`kernel/scheduler-protocol/src/lib.rs:18`。源码联调应固定完整Kernel提交和单一类型来源，不能拿carrier离线fixture当作线上发行验收；历史`packages/iface-*`生成SDK也不能凭手改声明冒充新发行物。

Cargo的`--no-default-features`表示不启用包的默认编译feature。当前host默认包含`wasm-host`，该选项可构建不带Wasm宿主的Native路径；它不表示关闭授权、预算或审批，也不表示自动切换产品默认后端。未编译能力必须明确拒绝，不能安装半份组件。见Kernel `kernel/host/Cargo.toml:120`、`:153`。另一种“没有默认回退”则是运行语义：缺stream、session factory或所需profile时保持不可用，不偷偷选旧模型或旧运行时；见`kernel/host/src/startup/embed.rs:215`。

## 哪些已经测过，哪些还不能说完成

五层事实依次是：**接口定义、能力实现、正式入口接线、公开注入、当前组合验收**。源码证明前几层的局部边界；真实运行只证明所选用例和版本范围，不能自动关闭整项产品任务。

`0291`的Linux收据证明指定受影响范围：默认与`--no-default-features`生产host构建、各自选定的装配/审批测试及静态门通过，另有一个Native只读工具真实resident自定义场景通过。74是跨配置的标准测试执行次数，不是74个独立用例，更不是完整PilotDeck产品矩阵。范围与实际产物身份见`_tmp/20261115-iface-linux/kernel-0291dbe0-summary.json`。

产品已有源码构建的模型resident fixture，目标是`serve_resident → Native PilotDeck Loop → D96 → PilotDeck Model → 受管HTTP`，刻意不含工具、Context或可选hostModules。代码见产品`src/pilotdeck-iface/tests/resident_model.rs:1`与`src/pilotdeck-iface/tests/resident_model/host.rs:1`。截至本文读取的`b871 + 0291`第二次收据，该fixture **exit101、valid_pass=false、3项检查通过数为0**；预期success marker文本不是实际通过证据。该路径仍待排查和有效复验，不能把resident就绪日志或已生成测试二进制写成模型闭环PASS。收据为`_tmp/20261115-iface-tests/pd-b871-kernel0291-resident-v2/resident-receipt.json`。

完整产品还要把同一组能力和状态权威接到真实非TUI CLI、Gateway及渠道入口，验证模型→工具→模型、取消/关闭、权限与恢复，并完成安装发行和迁移条件。当前不据局部结果切换默认后端，也不退役旧执行链。

## 历史基点怎样复现

`ca0/0.1.7`起点中，`serve_resident`虽已公开，外部工具类型、完整模型/Context注入、Native正式接线、真实轮次与编码仍有不同缺口。后续修订逐项推进，不把起点静态审计改成后续实现报告，也不把后续PASS反填到旧受测身份。Kernel规范§0明确正文是目标规范，不是代码完成报告。

本页所有标为Kernel的当前行号均对应`0291dbe02651e51e40d8f1689fe6f2e0c2930693`；标为产品的行号对应`b871eb765966681d73c2ee70db4bafa7215098bd`。查看其他工作区或后续修改时，以冻结对象重现，不直接套用当前文件行号：

```sh
git -C <Kernel源码检出> show 0291dbe02651e51e40d8f1689fe6f2e0c2930693:kernel/host/src/startup/embed.rs
```
