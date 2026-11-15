# PilotDeck-RS / ifACE 接入实施

本目录对应2026-11-15批准的P0–P7接入计划。当前是**本地实施候选，待代码审阅与实际验证**。产品尚未完成正式Kernel组装，默认运行后端尚未切换，旧执行运行时尚未退役。

实施事实账本为`state/iface-integration.json`，来源目录为`state/iface-source-catalog.json`，上游差额为`state/iface-upstream-review.json`。统一交接入口仍是原工作区的`state/backend-handoff.md`。本目录解释代码与证据边界，不用文档声明替代验收。

## 先理解四种职责

| 部分 | 在接入中负责什么 |
|---|---|
| ifACE能力接口与规则 | 约定模型、工具、上下文等能力的输入输出、错误、权限、状态和替换语义；接口本身不执行模型请求 |
| Kernel宿主（host） | 管理能力绑定、身份、授权、预算、生命周期和权威记录，并调用实际实现 |
| Scheduler SDK | 向Loop组件提供公开执行与资源客户端；负责按协议组织调用，不代替模型/工具实现或宿主检查 |
| PilotDeck产品组装根 | 构造产品能力包，通过公开入口连接宿主与Loop，并把CLI、Gateway和渠道接到这套执行链 |

具体能力实现与管理检查共同组成能运行的Kernel。受信同进程L-A可以省通信，但不能省授权和状态检查。组件能实现公开trait、测试能调用它，与用户的正式入口真正走到它，是不同事实。

## 当前包边界与接线

以下描述产品本地候选`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`的源码范围，不能直接读成已验收：

- `pilotdeck-context-core`提供Snip、Micro、工具配对清理、预算快照与压缩决策，Gateway复用同一算法；tokenizer和投影从调用侧注入。
- `pilotdeck-model-core`提供原模型请求与事件的协议转换、参数修复、UTF-16及usage/结束原因等纯处理。`pilotdeck-model-iface`另提供公开模型会话适配，接收可信组装方提供的配置和`HttpStreamTransport`。这些包的存在不表示所有供应商协议、鉴权、流式和正式产品入口均已通过。
- `pilotdeck-turn-preparation`共享原factory完整载荷解码、原准入和完整回合准备。`pilotdeck-iface`已接`OriginalPayloadAdmission`、`OriginalTurnPreparation`和`ResourceCodec`边界，并调用原`run_admitted_turn`；宿主效果经当前调用绑定的Kernel资源客户端。此前“生产admission与resource codec尚未接通”的描述已过期。
- `ModelSessionCodec`已有公开profile与分页事件的候选实现，但工具、Context、通知/事件投递及各资源的实际产品转换仍须逐项闭合。**已接入原准入、完整准备和codec边界，不等于正式产品组装已经完成。** factory相关11项局部测试尚不能提供完整driver的实际运行证据。

## 当前候选与历史身份

| 身份 | 含义 |
|---|---|
| backend62 `33b23a773b437aaea33696292810cf67aa54fdef` | 产品历史起点 |
| 上游`ca7c7cc33db3c42cc81d78239657f3dfbdc422ca` | 本轮上游差额审阅身份；接收决定、代码实施、本地吸收、测试及合main分别记录 |
| Kernel `ca0e465be3b2788cbfaa078abd0dfc621de69e5a` / ifACE `0.1.7` | 合同历史起点；保留原静态审计与行号 |
| Kernel `41e2f4d16fe369c00d81960ebc0e5f269b263477` / ifACE `0.1.8` | 新公开model profile、失败事件、工具effect及HTTP流等候选API；已有局部实际FAIL和构建阻塞 |
| Kernel `481723` | 后续已包含route修复的本地候选；尚未验证，不能覆盖`41e2`原FAIL |

TS产品行为基线仍为`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。当前源码、本地候选、实际受测版本及历史继承证据分别保存。ifACE包版本、Scheduler SDK版本、Scheduler协议和L-C协商整数ABI也是独立版本轴。

Kernel通过单一独立源码检出和相对Cargo路径绑定，不复制私有实现。绑定指定完整提交后执行所需检查：

```sh
python3 scripts/iface-integration/bind_kernel.py --kernel-root /path/to/agent-kernel --expected-commit <完整提交>
cargo test --manifest-path src/Cargo.toml -p pilotdeck-context-core -p pilotdeck-model-core -p pilotdeck-iface
```

上述命令是操作示例，不表示当前候选已执行通过。绑定脚本检查提交、源码洁净度及已有绑定；`deps/agent-kernel`为不入库软链接。发行时仍需提供真实源码和组合产物身份。

## 如何阅读进度

每项合同分开记录**接口定义、能力实现、正式入口接线、公开注入、当前组合验收**五层。已有代码不自动意味着正式接线；静态检查和局部测试不自动意味着组合通过。UNKNOWN表示未核实，不等于未实现；单条PASS不等于任务已交付。

本轮远端已完成`PASS_READBACK`：13项合同、26项交付任务、448项上游处置、23项ifACE变更、64条验证记录。上游处置与ifACE变更放在任务物理表中，但各有独立记录类型与分母，不加入原26项交付任务。旧后端127项及原六表保持独立，原六表记录与哈希未变。管理回读成功只证明投影与关系一致。

[术语审查](terminology.md)覆盖较早的13合同、26任务、55记录，共94个稳定记录ID。它为当前显示提供词义规则，但不能据此声称后来新增的记录或448/23扩展行均已逐行完成语义审查。

- [ifACE代码解读](iface-guide.md)：接口、宿主、SDK、产品组装与版本轴。
- [合同与能力对应](contracts.md)：保留`ca0/0.1.7`历史起点，说明五层审计和当前候选边界。
- [上游增量处置](upstream-review.md)：逐项接收、实施及尚未闭合的行为差额。
- [Native Loop边界](native-loop.md)：已接原准入/完整准备/codec，以及正式产品组装的剩余工作。
- [Kernel变更追踪](kernel-changes.md)：逐项变更与实际证据，不把局部结果合成全量通过。
- [账本与飞书维护](../../scripts/iface-integration-status/README.md)：独立分母、真实关联、旧六表保护与证据要求。

P2生命周期、P3能力包、P4正式组装、P5入口兼容、P6组合及双平台发行、P7迁移与退役仍按阶段出口分别审阅和验收。
