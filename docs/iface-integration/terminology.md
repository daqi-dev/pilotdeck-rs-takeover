# ifACE三表概念与用语一致性审查

本次只读审查覆盖账本13项合同、26项交付任务和55条历史验证记录，共94个稳定记录ID。结论是：五层事实和历史身份的分离已有基础，但仍需补上Model能力划分、受信L-A边界、上游接收/实施/发布阶段、统计分母及PASS显示范围。此审查完成不表示ifACE完整实装或产品验收完成。

审查输入账本SHA256：`4219a41f437c824855ac64bdd2defd0f9af71658c86243b0827491766a5543f0`。本地55条历史记录，管理同步收据仅证明当时13/26/36快照。原远端前置快照仅含合同/任务的真实recordID，未保留运行记录稳定ID到真实recordID的完整映射；不得按顺序猜配。

## 固定身份与职责边界

历史合同审计起点为Kernel `ca0e465be3b2788cbfaa078abd0dfc621de69e5a`、ifACE `0.1.7`、Scheduler SDK `0.3.0`。本轮另读当前Kernel `41e2f4d16fe369c00d81960ebc0e5f269b263477`及其ifACE `0.1.8`。当前规范§0仍明确正文为目标规范，不是代码完成报告；不得把旧测试换成新提交身份。当前§1.6、§6.9定义共同规则、管理检查与具体能力实现的职责；§6.1.3区分Agent会话、模型会话与单次生成；§6.9.3要求受信同进程调用仍受共同检查。

五层分别为接口定义、能力已有实现、正式入口接线、公开注入、当前组合已验收。`implemented`显示成“Kernel实现”会让人以为具体能力都须写在Kernel包内；保留远端字段兼容时，应注释为“能力已有实现，按责任栏区分Kernel管理检查及PilotDeck具体实现”。每层都要有独立证据，UNKNOWN不是未实现。

## 稳定术语表

| 用语/标识 | 稳定含义 | 使用约束 |
|---|---|---|
| ifACE / iface | 共同能力接口与行为规则 | 包括操作、数据、权限、状态、错误和替换规则；不是仅Rust trait，也不亲自执行能力。保留项目拼写及包名。 |
| 完整Kernel | 管理检查部分与已装配核心实现组成的运行整体 | 具体能力包可由PilotDeck提供；不能把Kernel只说成消息转发器，也不能说Kernel已实现全部产品算法。 |
| Kernel管理与检查部分 | 绑定、准入、授权、预算、生命周期及权威记录 | 这是职责定义，具体接线与本次验证另行举证。 |
| 接口定义 | 公开合同已声明 | 不推出有实现、已接线、可注入或组合验收通过。 |
| 能力实现 | 已存在的具体实现代码 | 明确实现归属及范围；implemented不应一概翻成Kernel包内实现。 |
| 正式入口接线 | 实际启动与执行入口消费实现 | 静态函数、trait实现或局部单测不能单独证明。 |
| 公开注入 | 独立使用方通过公开入口绑定外部实现 | 不依赖私有类型；还需证明默认初始化不覆盖绑定；与正式接线分别审查。 |
| 当前组合已验收 | 固定产品、Kernel、组装产物、配置与实际命令的组合满足断言 | 必须带能力范围；局部PASS不能泛化成产品完成。 |
| Model / 模型能力 | 单次模型请求、带元数据完成、流式或持续模型会话中的指定能力 | 分别写ModelContract::call、call_completion、ModelStreamContract、ModelSessionFactory；一种通过不证明另三种通过。 |
| 模型服务 | 实际提供推理能力的服务或供应商端点 | 协议转换包、模型配置、模型会话、Agent会话不是模型服务的同义词。 |
| 模型会话 | 持续输入输出且有独立关闭/控制生命周期的model_session_id对象 | 与Agent session_id、单次生成response_id分别记录，不合并生命周期。 |
| 生成设置 | GenerationSettings / ModelSessionConfig.generation | temperature、max_tokens等生成参数；此处generation不译成Agent代次。 |
| Agent会话代次 | M36 generation | 拒绝旧会话代次请求；与模型生成设置、connection_generation及推进权epoch分开。 |
| 连接代次 / 推进权代次 | connection_generation / epoch | 连接替换与推进权围栏；保留原字段名，不能互换generation。 |
| SDK | 客户端库或特定协议的开发工具包 | 必须指明哪一种SDK及版本；Scheduler SDK不是完整ifACE模型/工具/上下文实现。 |
| Native Loop | 具体Loop实现及受管调度接入 | native不承诺无IPC；Loop trait、Scheduler provider/NDJSON适配器、正式安装消费分别验证。 |
| 产品组装根 | 受信的产品启动与能力绑定代码 | 可依赖公开host入口及具体实现包；普通能力包仅依赖所需公开合同；组装根本身不是绕过Kernel检查的授权。 |
| 受信L-A | 宿主授权范围内的同进程Rust接入 | 可直接调用共同服务；涉及权限与共享状态仍走共同检查；受信不是组件自称可信。 |
| 旧执行运行时 | 旧LocalGateway等负责实际推进/权限/预算/生命周期的执行链 | 保留的CLI/Gateway协议适配或旧数据格式不是同一概念；避免把所有runtime模块称为应删除对象。 |
| 上游接收结论 | 审阅后接受一项行为需求或差额 | 接收≠代码已实施；需继续区分代码实施、本地候选吸收、实际验证、推送、合main。 |
| 本地候选吸收 | 修改进入明确本地候选 | 不等于推送或合main，也不等于产品验收通过；记录候选commit/tree身份。 |
| 运行记录PASS | 该条记录指定验证层、选择与身份范围内通过 | build、management、evidence_review和executed分开统计；ignored/filtered不计通过；不能自动关闭任务。 |
| 历史继承 | 审阅并沿用原执行的有限证据 | evidence_review不是本线程重跑；适用性审阅不是新测试；保持原提交/产物身份。 |
| 阶段 / 交付任务 / 处置项 / 运行记录 | 8阶段 / 26任务 / 上游逐项处置 / 55历史记录 | 分母不同；13合同五层审计另列；旧后端127分母独立，不能相加作完成率。 |
| UNKNOWN / needs_verification / delivered | 未核实 / 待验证 / 已交付 | 未知不是未实现；待验证不等于实现完成；delivered需同组合完整验收与依赖关闭。 |

## 保留原API标识

API方法、trait/类型、字段、crate、状态枚举、稳定记录ID、Git SHA、版本和原始命令保持原样；中文说明注明语义与版本，禁止全局替换generation/model/session/native/runtime/SDK或把示意名称当作已发布API。

例如 `ModelSessionConfig.generation: GenerationSettings` 是生成设置，而M36 `generation`是Agent会话代次。`connection_generation`与`epoch`分别为连接代次与推进权围栏。中文说明可以统一，原字段和类型不能改名。`ModelContract::call`、`call_completion`、`ModelStreamContract`与`ModelSessionFactory`分别保留，不能统一替换成“模型会话API”。

## 统计与历史记录规则

55条记录的executionKind分布为：{'build': 9, 'executed': 38, 'management': 6, 'evidence_review': 2}；结果分布为：{'FAIL': 13, 'PASS': 42}。这些是验证记录数量，不是交付完成率。八阶段、26交付任务、13合同五层审计、上游448路径处置与旧后端127任务各有独立分母。

所有55条历史run正文不可修改。本文件与JSON中建议仅应用于显示标题、验证层和范围注释；保持原事实JSON、result、原始时间/身份/命令、首次失败及历史FAIL。继承记录保持evidence_review，不变成executed；源码相同/包含关系不称新候选重跑。管理与构建记录也不能在汇总时混作产品实际验收。

接收上游增量应依次表述为“审阅接收结论”“代码已实施/待实施”“本地候选已吸收及身份”“指定验证通过/未通过”“已推送及分支”“已合main及提交”。不能用一个“已吸收”覆盖整条链。

## 文档与投影显示问题

- `docs/iface-integration/contracts.md:3`：历史ca0/0.1.7身份明确，但“当前规则入口”和起点代码描述容易被当最新完成报告。 建议：标题/导语注明历史起点审计；另挂当前41e2/0.1.8规范差异和候选验证映射，不覆写历史行号。
- `docs/iface-integration/iface-guide.md:61`：“Model是一次推理或模型会话的实现”易合并互不替代的四种能力。 建议：改为Model能力可包括单次请求、元数据完成、流式和持续模型会话；分别标类型、可选能力与证明范围。
- `docs/iface-integration/iface-guide.md:65`：“Kernel当前资源桥缺口”仅对ca0历史起点成立；后续已有定点修复与运行记录。 建议：写“ca0/0.1.7起点资源桥存在…”，本轮修复/候选/运行证据另查账本，不断言当前41e2仍同样缺口。
- `docs/iface-integration/iface-guide.md:71`：“本次起点”正确但同段代码实现、注入与接线容易被泛化为最新源码状态。 建议：每个起点限制保留ca0身份，增加五层记录链接，不以局部修复宣告全面闭合。
- `docs/iface-integration/iface-guide.md:83`：“当前carrier/SDK”缺时间限定且SDK名称易混用。 建议：以ca0审计时点说明carrier；当前41e2另审实际分发身份；区分Scheduler SDK与ifACE分发载体。

投影应保留原枚举，同时显示executed=实际执行、build=构建/静态门、management=管理验证、evidence_review=历史证据审阅。`验收执行`关系列实际可关联上述全部记录，表说明应写明“关联验证记录”。三表“状态”分别表达合同五层、任务交付状态、单条验证结果，不能共用完成率口径。

## 逐记录审查与显示建议

下表中recordID指稳定账本ID；括号内为原件可证明的远端recordID。运行记录远端ID未完整封存时明确不猜配。全量机器可读建议见`state/iface-terminology-review.json`，包含每条源记录SHA256与显示建议。

### 合同13项

| 稳定recordID / 已证远端ID | 建议显示标题 | 具体问题与范围 |
|---|---|---|
| `iface.contract.assembly` (`reczz28ZngJu1JNX`) | 公开组装与能力替换边界 | 明确普通能力包、产品组装根与Kernel管理检查职责；公开注入不等于正式产品入口已接通。 |
| `iface.contract.native-loop` (`reczz28ZngJu3XDr`) | 受管Native Loop与正式调度接线 | Native Loop、LoopContract、Scheduler provider与NDJSON适配器分开命名；不得将本地等同于无通信。 |
| `iface.contract.model` (`reczz28ZngJu4OQd`) | 模型能力：单次完成、流式与模型会话；四类供应商协议 | 原“模型会话及四类模型协议”把持续模型会话当作全部Model能力；要求四条能力路径分别说明，ModelSessionFactory注入不替代普通完成/流。 |
| `iface.contract.context` (`reczz28ZngJu2DSh`) | 上下文快照、内容编码与真实轮次 | 结构化内容是content_type与opaque bytes约定，不表示Kernel强制解码为UTF-8；turn不等于Agent generation。 |
| `iface.contract.tool` (`reczz28ZngJu0tbl`) | 工具公开绑定、受管执行与资源回收 | 工具执行者不授予权限；取消请求不证明副作用未发生，Unknown保留为结果不确定。 |
| `iface.contract.history` (`reczz28ZngJu42CN`) | 历史权威提交、持久化与恢复 | 历史视图/快照不等于权威消息提交；不得将纯Context对拍扩大为持久化与重启恢复通过。 |
| `iface.contract.auth` (`reczz28ZngJu4ckR`) | 共同授权裁决、凭据与可信出站 | Kernel授权决定与供应商鉴权配置区分；SecretBroker声明不证明出站凭据链已接通。 |
| `iface.contract.lifecycle` (`reczz28ZngJu3cFw`) | Agent会话取消、委派与父子会话生命周期 | 模型会话关闭、Agent会话结束、工具取消与子会话退出分开；不凭父子关系推导默认取消策略。 |
| `iface.contract.state-ownership` (`reczz28ZngJu2AbC`) | 历史、权限、预算与执行状态的唯一权威 | 唯一权威允许缓存/副本；不等于全部存入一个进程/类；具体机制逐项确认。 |
| `iface.contract.atomic-install` (`reczz28ZngJu3OD5`) | 能力安装失败回滚、卸载与注册一致性 | 局部注册表测试不替代正式宿主安装失败路径与资源释放验证。 |
| `iface.contract.product-compat` (`reczz28ZngJu3pHE`) | 非TUI CLI、Gateway与渠道行为兼容 | 原Rust入口回归不等于Kernel组合验收；协议兼容层与旧执行运行时区分。 |
| `iface.contract.packaging` (`reczz28ZngJu1cJa`) | 源码安装、独立消费与组合产物身份 | 源码包含、path依赖、staging、发行安装及双平台实际运行分别记录；SDK/carrier身份不可泛称完整发行通过。 |
| `iface.contract.migration` (`reczz28ZngJu31Ty`) | 新会话默认切换、旧会话归属、静止导入与旧执行链退出 | 回合边界替换不是任意状态热迁移；新会话默认、旧会话归属、导入、回退与退役分别闭合。 |

### 任务26项

| 稳定recordID / 已证远端ID | 建议显示标题 | 具体问题与范围 |
|---|---|---|
| `iface.P0.sources` (`reczz28ZngLZIFxQ`) | 固定本次候选与历史发布基线的独立身份 | 侧线程发布与隔离构建是历史证据继承；本次只读身份核查不称重新发布或重新构建。 |
| `iface.P0.upstream` (`reczz28ZngLZLEZL`) | 审阅上游增量并逐项实施已接收项 | 接收结论、代码实施、本地候选吸收、验证、推送及合main分别记；448处置路径、59语义深审路径、36待闭合项不是26交付任务分母。 |
| `iface.P0.catalog` (`reczz28ZngLZIJ3E`) | 刷新四张来源表并判定条款适用范围 | 来源读取/筛选完成不等于合同闭合；字典、背景、产品需求参考与规范候选区分。 |
| `iface.P1.contracts` (`reczz28ZngLZJuqj`) | 审计接入合同、职责与五层证据 | 五层独立判断；ca0/0.1.7是历史起点，当前41e2/0.1.8不是历史验证的新身份。 |
| `iface.P1.tables` (`reczz28ZngLZK2hR`) | 维护独立三表、投影与完整回读 | 本地55历史记录与已发布36记录是不同快照；首轮13/26/0、后续13/26/36都需附时点；管理PASS不代表产品交付。 |
| `iface.P2.embedding` (`reczz28ZngLZKJw9`) | 公开宿主组装与Native Loop正式接线 | external_backends测试证明指定consumer范围；Kernel局部成功不等于PilotDeck真实组装通过。 |
| `iface.P2.model-context` (`reczz28ZngLZHdwr`) | 分别补齐模型能力与上下文公开注入 | 模型完成/流/模型会话分别验证；Context轮次与编码单列；不同候选的测试不能拼成同组合验收。 |
| `iface.P2.tools-auth` (`reczz28ZngLZHcCu`) | 工具公开绑定、共同授权与可信出站接线 | 现有共享P2测试关联不是所有工具/授权合同覆盖；实际读写、拒绝、取消及供应商鉴权分别追踪。 |
| `iface.P2.history-lifecycle` (`reczz28ZngLZJt2b`) | 历史权威提交与Agent生命周期前置 | core-identity局部测试不能泛化历史持久化、父子/委派及重启行为。 |
| `iface.P2.atomic-install` (`reczz28ZngLZLyme`) | 能力注册失败回滚与资源回收 | 注册表行为测试不等于实际宿主失败中途无泄漏；未验证不等于未实现。 |
| `iface.P3.pure` (`reczz28ZngLZPJEx`) | 提取独立算法与策略库并验证原行为 | 纯库/TS对拍是算法范围证据；源码包含于b351不等于其完整候选重跑。 |
| `iface.P3.model` (`reczz28ZngLZMuHd`) | 提取四类供应商模型实现并适配公开能力 | 模型协议、服务端点、模型会话是不同概念；codec首轮编译失败与normalization旧PASS分别保留。 |
| `iface.P3.tools` (`reczz28ZngLZLpzf`) | 提取工具实现并适配公开工具合同 | 工具包提供执行能力；授权、预算与生命周期共同检查不在工具包中另建权威。 |
| `iface.P3.context` (`reczz28ZngLZMIqK`) | 提取上下文算法与历史视图实现 | 上下文视图/裁剪/压缩/缓存不自动证明权威历史提交及恢复；纯库对拍与产品路径区分。 |
| `iface.P4.composition` (`reczz28ZngLZOj3u`) | 实现受信产品组装根与公开宿主启动 | serve_resident是具体公开入口标识；产品组装根可连接具体实现，普通能力包不依赖私有HostState。 |
| `iface.P4.loop` (`reczz28ZngLZNKsB`) | 由Kernel正式调度PilotDeck Native Loop | preparation/适配器拆分单测不是生产ResourceCodec、native可执行体、真实宿主或模型→工具→模型闭环。 |
| `iface.P4.state` (`reczz28ZngLZOFQZ`) | 统一历史、权限、预算与生命周期权威 | AO/automation旧路径定点PASS与Cron FAIL分别保留；单点排空不代表Kernel组合状态权威闭合。 |
| `iface.P5.cli` (`reczz28ZngLZPEyL`) | 将非TUI CLI兼容入口连接至Kernel | factory payload拆分测试不等于真实CLI run/resume/退出码验收。 |
| `iface.P5.gateway` (`reczz28ZngLZNgkg`) | 将Gateway兼容入口连接至Kernel | upload/services原入口或捕获产物回归仅证明指定子集，不能称Kernel Gateway全链已接通。 |
| `iface.P5.channels` (`reczz28ZngLZOzzD`) | 将既有渠道提交与会话管理接入Kernel | 计划范围不是实现完成；渠道特有的代次、队列、凭据、退出等缺口分别验证。 |
| `iface.P6.contracts` (`reczz28ZngLZOwN8`) | 验收同一候选ifACE合同与真实宿主组合 | fmt/clippy是静态门，不能作为运行联合验收替代；不同候选历史PASS不自动闭合本任务。 |
| `iface.P6.product` (`reczz28ZngLZN8ww`) | 验收同一Kernel组合的产品行为兼容 | Context/providers/jsonrepair/token局部回归不是CLI/Gateway/渠道正式入口整体验收。 |
| `iface.P6.release` (`reczz28ZngLZMVng`) | 验收双平台源码安装与实际发行产物 | 构建通过、安装通过、实际运行通过分开；本机依赖副本的隔离构建不叫新机安装。 |
| `iface.P7.migration` (`reczz28ZngLZNTJm`) | 验证旧会话后端归属、静止导入与回退 | 静止导入不是活动状态热迁移；Unknown、审批中和在途调用不得笼统称可恢复。 |
| `iface.P7.default` (`reczz28ZngLZMAex`) | 将新建Agent会话默认绑定到Kernel | 新会话默认切换不更改旧会话归属；失败不得静默跨后端重放。 |
| `iface.P7.retirement` (`reczz28ZngLZOM1j`) | 退役旧执行链及临时执行回退 | 旧执行运行时与保留协议适配/旧格式转换分开；不承诺删除所有runtime命名模块。 |

### 历史验证55条（仅显示纠正）

| 稳定recordID / 已证远端ID | 建议显示标题 | 具体问题与范围 |
|---|---|---|
| `iface.run.mac-pure-build-first`（远端ID映射未封存） | 【构建尝试】纯算法首轮编译失败 | 本条为构建尝试，不表示产品运行断言已执行；FAIL保留原失败性质。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-pure-tests`（远端ID映射未封存） | 【局部执行记录】纯Context 6项与Model 4项测试通过 | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；算法/拆包或原Rust路径回归，不证明新Kernel组合完成；ignored和filtered不计通过。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-pure-clippy`（远端ID映射未封存） | 【静态检查】纯算法包Clippy通过 | 本条为静态检查，不表示产品运行断言已执行；FAIL保留原失败性质。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-context-build-first`（远端ID映射未封存） | 【构建尝试】Gateway Context首轮编译失败 | 本条为构建尝试，不表示产品运行断言已执行；FAIL保留原失败性质。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-context-oracle`（远端ID映射未封存） | 【局部执行记录】Gateway Context oracle 3项通过（64个固定TS场景） | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；算法/拆包或原Rust路径回归，不证明新Kernel组合完成；ignored和filtered不计通过。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-context-integration`（远端ID映射未封存） | 【局部执行记录】Gateway Context集成13项通过、3项ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；算法/拆包或原Rust路径回归，不证明新Kernel组合完成；ignored和filtered不计通过。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-upload`（远端ID映射未封存） | 【局部执行记录】Gateway upload 86项通过、3项ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；原Rust Gateway局部回归；未确认源码身份不可被后续提交反填。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-services`（远端ID映射未封存） | 【局部执行记录】Gateway services 54项通过、2项ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；原Rust Gateway局部回归；未确认源码身份不可被后续提交反填。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-providers`（远端ID映射未封存） | 【局部执行记录】Gateway providers 27项通过、1项ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；算法/拆包或原Rust路径回归，不证明新Kernel组合完成；ignored和filtered不计通过。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-jsonrepair`（远端ID映射未封存） | 【局部执行记录】Gateway jsonrepair 1项通过、1项ignored（86个固定TS场景） | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；算法/拆包或原Rust路径回归，不证明新Kernel组合完成；ignored和filtered不计通过。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-token`（远端ID映射未封存） | 【局部执行记录】Gateway token accounting 4项通过 | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；算法/拆包或原Rust路径回归，不证明新Kernel组合完成；ignored和filtered不计通过。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-native`（远端ID映射未封存） | 【局部执行记录】Native Loop适配器8项局部测试通过 | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；KernelResourceInvoker替身适配器单测，不是serve_resident真实宿主联合验收。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.linux-0cc25c1b-first-core-registry`（远端ID映射未封存） | 【构建尝试】Linux 0cc25c1b core-registry FAIL | 本条为构建尝试，不表示产品运行断言已执行；FAIL保留原失败性质。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-0cc25c1b-source-deps-core-registry`（远端ID映射未封存） | 【局部执行记录】Linux 0cc25c1b core-registry PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-0cc25c1b-source-deps-core-identity`（远端ID映射未封存） | 【局部执行记录】Linux 0cc25c1b core-identity PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-0cc25c1b-source-deps-host-external`（远端ID映射未封存） | 【构建尝试】Linux 0cc25c1b host-external FAIL | 本条为构建尝试，不表示产品运行断言已执行；FAIL保留原失败性质。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-107bb7d9-default-core-registry`（远端ID映射未封存） | 【局部执行记录】Linux 107bb7d9 core-registry PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-107bb7d9-default-core-identity`（远端ID映射未封存） | 【局部执行记录】Linux 107bb7d9 core-identity PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-107bb7d9-default-host-external`（远端ID映射未封存） | 【构建尝试】Linux 107bb7d9 host-external FAIL | 本条为构建尝试，不表示产品运行断言已执行；FAIL保留原失败性质。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-default-core-registry`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 core-registry PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-default-core-identity`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 core-identity PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-default-host-external`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-external FAIL | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-remaining-host-install`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-install PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-remaining-host-resource`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-resource FAIL | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.management-first-failure`（远端ID映射未封存） | 【管理验证】账本首轮25控制中的1项异常错误 | 只证明账本/同步/视图或控制范围；不证明Kernel接线或产品行为。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.management-controls`（远端ID映射未封存） | 【管理验证】管理同步26项离线控制通过 | 只证明账本/同步/视图或控制范围；不证明Kernel接线或产品行为。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.management-sync-first`（远端ID映射未封存） | 【管理验证】三表首轮真实写入及完整回读通过 | 只证明账本/同步/视图或控制范围；不证明Kernel接线或产品行为。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.management-views`（远端ID映射未封存） | 【管理验证】四个接入视图filter/group回读 | 只证明账本/同步/视图或控制范围；不证明Kernel接线或产品行为。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.management-identity-controls`（远端ID映射未封存） | 【管理验证】管理身份边界28项离线控制通过 | 只证明账本/同步/视图或控制范围；不证明Kernel接线或产品行为。 |
| `iface.run.mac-captured-gateway-upload`（远端ID映射未封存） | 【局部执行记录】捕获Gateway产物复验 upload：86 PASS / 3 ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；本次运行捕获二进制，明确区别原未知身份执行；不代表后续preparation/always-on修改通过。 |
| `iface.run.mac-captured-gateway-services`（远端ID映射未封存） | 【局部执行记录】捕获Gateway产物复验 services：54 PASS / 2 ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；本次运行捕获二进制，明确区别原未知身份执行；不代表后续preparation/always-on修改通过。 |
| `iface.run.mac-captured-gateway-providers`（远端ID映射未封存） | 【局部执行记录】捕获Gateway产物复验 providers：27 PASS / 1 ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；本次运行捕获二进制，明确区别原未知身份执行；不代表后续preparation/always-on修改通过。 |
| `iface.run.mac-captured-gateway-jsonrepair`（远端ID映射未封存） | 【局部执行记录】捕获Gateway产物复验 jsonrepair：1 PASS / 1 ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；本次运行捕获二进制，明确区别原未知身份执行；不代表后续preparation/always-on修改通过。 |
| `iface.run.mac-captured-gateway-token`（远端ID映射未封存） | 【局部执行记录】捕获Gateway产物复验 token：4 PASS / 0 ignored | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；本次运行捕获二进制，明确区别原未知身份执行；不代表后续preparation/always-on修改通过。 |
| `iface.run.inherited-published-baselines-readback-20261115`（远端ID映射未封存） | 【历史证据继承审阅】侧线程backend62/Web16双仓发布继承：本线程只读回查PASS | 保留原侧线程执行身份；本线程只读核查，不是重跑、重发或本次ifACE联合验收。 |
| `iface.run.inherited-web16-isolated-build-20261115`（远端ID映射未封存） | 【历史证据继承审阅】侧线程Web16隔离构建继承PASS及篡改拒绝；index哈希独立核对 | 保留原侧线程执行身份；本线程只读核查，不是重跑、重发或本次ifACE联合验收。 |
| `iface.run.mac-native-preparation-first`（远端ID映射未封存） | 【局部执行记录】Native/preparation首条命令10PASS/1FAIL | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；拆分/准备适配器的局部测试，不等于生产ResourceCodec、native可执行体或真实宿主已消费。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-native-preparation-rerun`（远端ID映射未封存） | 【局部执行记录】Native/preparation修正夹具后13PASS（11+2） | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；拆分/准备适配器的局部测试，不等于生产ResourceCodec、native可执行体或真实宿主已消费。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-transport-factory-extraction`（远端ID映射未封存） | 【局部执行记录】Transport factory payload拆分21PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；拆分/准备适配器的局部测试，不等于生产ResourceCodec、native可执行体或真实宿主已消费。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-transport-preparation-lib`（远端ID映射未封存） | 【局部执行记录】Transport默认factory拆分11PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；拆分/准备适配器的局部测试，不等于生产ResourceCodec、native可执行体或真实宿主已消费。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-always-on-drain`（远端ID映射未封存） | 【局部执行记录】AO drain 7PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-automation-drain`（远端ID映射未封存） | 【局部执行记录】Automation drain 14PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.mac-cron-lifecycle-first`（远端ID映射未封存） | 【局部执行记录】Cron lifecycle 12PASS/1FAIL；四切断点扩展待复验 | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；本条结果为12PASS/1FAIL；“四切断点扩展待复验”是后续动作，不计本轮通过。 |
| `iface.run.linux-e9f8ab84-rest-host-context`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-context PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-rest-host-model-runtime`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-model-runtime PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-rest-host-health`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-health PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-rest-host-old-external`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-old-external PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-e9f8ab84-rest-host-old-duplex`（远端ID映射未封存） | 【局部执行记录】Linux e9f8ab84 host-old-duplex PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-9ab6bdff-recheck-host-external`（远端ID映射未封存） | 【局部执行记录】Linux 9ab6bdff host-external PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-9ab6bdff-recheck-host-resource`（远端ID映射未封存） | 【局部执行记录】Linux 9ab6bdff host-resource：资源合同断言FAIL | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。；原标题FAIL_OR_INVALID容易歧义：显示为资源合同断言FAIL；保留原result=FAIL及Null解包原因，不改写历史标题正文。 |
| `iface.run.linux-09328735-resources-host-resource`（远端ID映射未封存） | 【局部执行记录】Linux 09328735 host-resource PASS | 仅本条选择、平台、实际版本/产物及验证层范围；PASS不自动关闭任务、合同或阶段。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-09328735-checks-fmt`（远端ID映射未封存） | 【静态检查】Linux 09328735 fmt PASS | 本条为静态检查，不表示产品运行断言已执行；FAIL保留原失败性质。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.linux-09328735-checks-clippy`（远端ID映射未封存） | 【静态检查】Linux 09328735 clippy FAIL | 本条为静态检查，不表示产品运行断言已执行；FAIL保留原失败性质。；Kernel指定候选的局部目标，不能拼接不同候选结果冒充41e2/0.1.8或完整PilotDeck组装重跑。 |
| `iface.run.mac-model-codec-first-compile`（远端ID映射未封存） | 【构建尝试】Model codec首轮编译FAIL：缺serde_json dev依赖 | 本条为构建尝试，不表示产品运行断言已执行；FAIL保留原失败性质。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |
| `iface.run.management-validation-publication-sync`（远端ID映射未封存） | 【管理验证】36条验收记录三表同步与保护旧六表回读PASS | 只证明账本/同步/视图或控制范围；不证明Kernel接线或产品行为。；13合同/26任务/36运行记录为当时已发布快照；本地当前55记录不是此条已发布范围。；原命令、配置或完整源码身份未确认按原件保留，不以当前源码/证据文件SHA反填。 |

## 当前0.1.8新增API的能力边界

`0cc`至`093`的宿主/SDK修订仍属ifACE `0.1.7`。`41e2`首次公开ifACE `0.1.8`的新model profile、`ResponseFailed`、`ToolEffect`及`HttpStreamTransport`等API。新增公开名称、已有某实现、实际接线及指定组合验证仍需分层表述，不能把旧builtin模型路径或PilotDeck适配一概称为已支持。

`MODEL_EVENT_PROFILE_V1`与`MODEL_REQUEST_PROFILE_V1`要求明确协商。当前富请求路径还检查`media_input=true`，即使业务内容只有文字；“支持纯文字请求”不能省略profile能力前提。`StructuredModelEvent::new`检查投影类别与JSON文本非空，完整字段和语义投影的同源验证由profile适配器负责，不能称宿主构造器已作完整逐字段验证。依据为`kernel/iface/src/model_session/profile.rs:12,103`与`kernel/host/src/transport/model_session/handlers_input/append.rs:209`。

`9ab`中的external_backends测试将initialize改为ABI2，是调用方测试前提修正；不能将其称为产品实现修复，也不改写`e9`在ABI1上的原FAIL。旧API兼容不等于自动获得新profile能力，旧测试继承不等于新API已运行通过。

## 应用与验证边界

本次仅新增本术语审查文档与独立审查JSON；未改接入账本、55条历史run、同步器、产品实现、远端表或旧六表。后续由负责投影的写入方显式应用显示建议并回读，不把这份审查记作cargo通过或联合验收。合同/任务如后续修订正文，仍须保留原始审计身份、重新校验关联与投影哈希。
