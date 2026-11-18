# ifACE 接入合同与历史起点审计

审计对象是 Agent Kernel `ca0e465be3b2788cbfaa078abd0dfc621de69e5a`，ifACE `0.1.7`、Scheduler SDK `0.3.0`。本文件的源码行号全部针对这个 Git 对象；同名工作区中的后续修复不自动成为已验证事实。产品起点为 PilotDeck-RS backend62 `33b23a773b437aaea33696292810cf67aa54fdef`。实际推进状态及测试结果只在 `state/iface-integration.json` 维护。

## 当前候选与历史审计的关系

下文保留`ca0/0.1.7`起点的静态审计和原源码行号。产品本地候选`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`已接原admission、完整turn preparation与resource codec边界；它仍未完成正式产品组装，factory相关11项局部测试也没有完整driver的实际运行证明。因此既不沿用“原准入/codec尚未接”的旧表述，也不把候选代码写成产品已交付。

Kernel `41e2f4d16fe369c00d81960ebc0e5f269b263477`首次公开ifACE `0.1.8`新增profile、失败事件、工具effect及HTTP流等API。本轮有局部PASS，也有实际FAIL和构建阻塞。后续本地候选`481723`已含route修复，尚未验证；不能用最新源码覆盖`41e2`原结果。逐项源码和证据见[Kernel变更追踪](kernel-changes.md)，代码用法见[接入解读](iface-guide.md)。

## 事实与规范的使用顺序

- Kernel 接入规则以 `kernel/docs/KERNEL-MECHANISMS-AND-ABI.md` §0 为规则入口；本文件引用的历史对象中93行明确说明正文是目标规范，不是代码完成报告。接口声明、实现、接线、注入与本轮实测分别判断。
- PilotDeck 对外行为仍按固定 TS 基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的实测、测试、源码及已接受差异判断。来源表不能自行改变产品范围和行为。
- 冲突或能力缺口登记为具体合同问题；不能以“表中已完成”“编译通过”或单个测试替身通过，关闭完整产品任务。
- 用 `git -C <kernel-checkout> show ca0e465be3b2788cbfaa078abd0dfc621de69e5a:<path>` 重现本文的源码引用。公开声明的存在只作为源码证据，不是本轮运行证据。

## 四张来源表的历史读取快照

根协调在 2026-11-15 完整读取了四张表。读取原件保存在 `_tmp/20261115-iface-sources/`，各文件的 SHA256 与读取时间见 [来源目录](../../state/iface-source-catalog.json)。11 月 13 日旧快照未改写。

| 来源 | 本次全量记录数 | 本次保存用途 |
|---|---:|---|
| 机制定义 | 42 | 全部保留为机制词典，不生成 42 项交付任务 |
| 机制包总表 | 61 | 全部保留为包职责词典，不直接决定 Rust crate 边界 |
| 功能分层分包总表 | 1,076 | PilotDeck 95 条为产品要求线索，rust-core 38 条为资产候选，Kernel 101 条为合同候选，文档需求 52 条为规范候选，13 条作为范围外技术背景；其余 777 条逐记录登记排除 |
| 开发交付表 | 124 | Pilotdeck-ts 15 条、Kernel 依赖候选 28 条、媒体专项背景 4 条；其余 77 条登记排除或标题缺失未决 |

开发表已由旧快照的 121 条变为 124 条。旧筛选只包含 15 条 Pilotdeck-ts 任务并明确排除了 Kernel，不能直接沿用为本次完整依赖范围。本次 Kernel 候选覆盖接口文档、最小机制、调度、路由、恢复、后台任务、外部 Kernel 接线、工具执行、原 rust-core 接入和源码交付等主题；是否成为必做任务仍需当前代码和精确合同确认。

目录保留 `baseToken + tableId + recordId`、读取时间、选取字段哈希及完整记录 ID 清单。仅保留技术编号、名称、来源和机制，不复制人员、原表状态、备注、测试正文或业务数据。目录中 449 条被保留记录含词典和背景，**不是接入任务分母**；所有记录的 `productStatusInherited` 均为 `false`。

## 合同逐项落点

下表的“起点核查”描述冻结版本的静态事实；没有任何一项由此被标记为联合验收通过。

| 账本合同 ID | 规范／类型锚点 | 起点核查与实现责任 | 必需验收 |
|---|---|---|---|
| `iface.contract.assembly` | 规范 §1.6（204 行）、§6.9.3–6.9.4（1316、1351 行）；`host/src/startup/embed.rs:187` | `serve_resident` 已公开，但 `ExternalBackends` 仅会话工厂及工具集合；外部 Tool 类型没有从 crate 根再导出。完整 Model／Context／Scheduler 注入需补足并沿正式启动路径验证。产品组装负责构造，普通组件不依赖私有 HostState。 | 独立 consumer 构造与真实启动；配置冲突明确拒绝；注入不被默认初始化覆盖 |
| `iface.contract.native-loop` | 规范 §1.4.1、§6.8；`iface/src/loop_.rs:61`；`scheduler-protocol/src/contract.rs:287` | `LoopContract` 与 `LoopFactory` 已定义；`host/src/transport/scheduler.rs:70` 的安装逻辑存在，模块注释明确记录装配接线断开。能实现 trait 不等于默认入口使用它。 | 正式宿主安装 Native provider 后实际执行 PilotDeck 循环；取消与关闭走同一受管路径 |
| `iface.contract.model` | 规范 §6.1.1–6.1.3；`iface/src/model.rs:73`、`model_stream.rs:43,91`、`model_session.rs` | 普通完成、显式完成、流式和模型会话是不同能力。`call_completion` 默认不提供 usage／tool_calls；完整实现必须覆盖而非丢弃已有元数据。PilotDeck 包负责四种供应商协议；Kernel 负责准入和调用生命周期。 | 四类请求／内容行为；真增量、结构化工具调用、未知 usage、错误和取消；未提供能力明确不可用 |
| `iface.contract.context` | 规范 §6.1.4、§6.9；`iface/src/context/contract.rs:12,46` | ContextSnapshot 为带 content_type 的不透明 bytes；起点 Scheduler 的 `resources.rs:253` 将 turn 固定为 1，随后 UTF-8 解码，不能声称完整承载结构化快照及真实轮次。 | 连续多轮身份正确；内容编码不被静默改义；初始化之后保留注入；原上下文策略行为保持 |
| `iface.contract.tool` | 规范 §6.1.1、§6.9.3–6.9.5；`iface/src/tool.rs:86,127,136` | 注册目录、执行绑定和授权是不同事实。ToolResult 的 unknown 必须保留。起点 Scheduler `resources.rs:161` 对 read_only 直接拒绝 tool.authorize／tool.exec，需与工具 effect 合同联合核实。 | 未绑定、拒绝、取消、Unknown、回收；只读能力与只读约束；拒绝时后端执行次数为零 |
| `iface.contract.history` | 规范 K1/K2、§6.8；`iface/src/store.rs:119`；`host/src/transport/scheduler/resources.rs:177` | `ca0`起点资源桥保存`agent.history.v1` 整份 payload，读取最后一份；该路径没有表达输入消费与历史提交的原子事务或预期 revision。不能据此认定原 PilotDeck 提交语义已具备。 | 并发、重复提交、崩溃恢复、输入消费与消息顺序；未知结果不重放 |
| `iface.contract.auth` | 规范 §2.4、§6.9.4；`iface/src/tool.rs:52`；`host/src/transport/scheduler/resources.rs:71` | 权限与审批由 Kernel 共同检查路径决定；工具／模型实现提供能力，不能自行授予权限。凭据需留在可信执行边界，类型声明不证明凭据管理已接线。 | allow／deny／审批中取消；拒绝或必要审计失败时无副作用；日志与事件不泄漏凭据 |
| `iface.contract.lifecycle` | 规范 §1.4.1、§3.1、§6.8；`iface/src/agent/delegate_backend.rs`、`scheduler-protocol/src/contract.rs:225` | 同会话控制、跨会话委派、子执行退出策略分别审计，不能用一个 cancel 测试替代所有关系。具体行为以正式生产调用链确认。 | 父子退出、撤权、在途取消、超时、后台完成及 Unknown 状态保持 |
| `iface.contract.state-ownership` | 规范 §1.6、§6.1.4、§6.9.4 | Kernel 管理执行事实与共同约束；PilotDeck 保留产品策略和格式转换。缓存、投影和压缩候选视图不得成为第二份权威历史。 | 各权威写入点唯一；取消后无重复执行／消费；同会话不同入口看到相同受权威约束的状态 |
| `iface.contract.atomic-install` | 规范 §6.5、§6.9.6；`iface/src/tool.rs:143`；`host/src/transport/scheduler.rs:116` | 起点安装循环先写 loops 再逐条 register，随后安装资源、构造 runtime，最后 set 单次组件槽；中间失败须证明不会留下半激活状态。 | 重复 ID、资源冲突、安装中途失败、原有注册保留、成功安装与退出回收 |
| `iface.contract.product-compat` | PilotDeck 固定 TS 基线；规范 §6.1 的应用专有适配边界 | CLI、Gateway、渠道参数、错误、鉴权、事件与落盘仍由产品兼容层承接。Kernel 没有义务直接实现全部 PilotDeck 私有消息格式。 | 真实产品入口对拍；未接通入口不得用库测试代替验收 |
| `iface.contract.packaging` | 规范 §6.3、§6.9.8；`examples/iface-consumer/contracts/README.md:22`、`carrier/README.md:12` | 历史起点的正式契约载体是agentcpm/iface；旧 packages/iface-* 是冻结的历史 SDK。carrier 离线 fixture 只证明布局与 Cargo 消费，不等于真实仓库安装或真实产品运行。 | 单一类型来源、独立 consumer、源码安装、实际产物身份、macOS／Linux 对应范围 |
| `iface.contract.migration` | 规范 §5.4、§6.8、§6.9.6；用户本次迁移约定 | 回合边界替换不等于任意私有状态热迁移。新会话默认切换、旧会话归属与静止导入均须产品实现和实测。 | 原件保留、身份映射、拒绝活动／审批中／Unknown 导入、不自动跨后端重放、回退演练 |

上表代码路径以 Kernel 仓库的 `kernel/` 为根；`examples/` 引用以仓库根为根。

## 必须分开的五层结论

1. **接口已定义**：规范和公开类型表达了需要的操作与语义。
2. **能力已有实现**：有具体实现，且能力范围经过代码核查；分别注明Kernel管理检查与PilotDeck具体能力包的责任。
3. **生产已接线**：实际产品启动和执行入口会调用该实现。
4. **外部可注入**：独立产品能通过公开入口完成绑定，不访问私有类型。
5. **组合已验收**：固定 PilotDeck、Kernel、组装程序、配置与产物身份的真实运行满足断言。

第一层不能推出第二层，第二层不能推出第三、四层。编译和源码审阅不计第五层。Kernel 规范 §6.9.8（1470 行）要求同一个使用方、同一套业务断言更换实现，并覆盖拒绝无执行、慢实现不堵塞独立会话、在途调用保留原绑定、丢包不盲重做、不兼容拒绝以及共同权威状态。

本轮尚未通过的接线、状态事务、供应商适配和产品入口验收继续单列跟踪；来源审计或远端回读完成不能解除这些依赖。

当前远端快照为13合同、26交付任务、448上游处置、23项ifACE变更和64条验证记录，已完成管理`PASS_READBACK`，原六表保持不变。26项任务是交付分母，448和23是同一任务物理表内的独立追踪记录，64是混合执行/构建/管理/证据审阅记录，不能相加算完成率。旧后端127项另账。早期[术语审查](terminology.md)逐行覆盖13/26/55共94记录，不表示本次所有新增记录均已逐行语义审过。
