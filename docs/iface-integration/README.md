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

## 历史候选包边界与接线

以下描述产品本地候选`bd87f2e4b29b7de53d907f50d5ab90d755d236a6`的源码范围，不能直接读成已验收：

- `pilotdeck-context-core`提供Snip、Micro、工具配对清理、预算快照与压缩决策，Gateway复用同一算法；tokenizer和投影从调用侧注入。
- `pilotdeck-model-core`提供原模型请求与事件的协议转换、参数修复、UTF-16及usage/结束原因等纯处理。`pilotdeck-model-iface`另提供公开模型会话适配，接收可信组装方提供的配置和`HttpStreamTransport`。这些包的存在不表示所有供应商协议、鉴权、流式和正式产品入口均已通过。
- `pilotdeck-turn-preparation`共享原factory完整载荷解码、原准入和完整回合准备。`pilotdeck-iface`已接`OriginalPayloadAdmission`、`OriginalTurnPreparation`和`ResourceCodec`边界，并调用原`run_admitted_turn`；宿主效果经当前调用绑定的Kernel资源客户端。此前“生产admission与resource codec尚未接通”的描述已过期。
- `ModelSessionCodec`已有公开profile与分页事件的候选实现，但工具、Context、通知/事件投递及各资源的实际产品转换仍须逐项闭合。**已接入原准入、完整准备和codec边界，不等于正式产品组装已经完成。** factory相关11项局部测试尚不能提供完整driver的实际运行证据。

## 历史候选与身份基点

| 身份 | 含义 |
|---|---|
| backend62 `33b23a773b437aaea33696292810cf67aa54fdef` | 产品历史起点 |
| 上游`ca7c7cc33db3c42cc81d78239657f3dfbdc422ca` | 本轮上游差额审阅身份；接收决定、代码实施、本地吸收、测试及合main分别记录 |
| Kernel `ca0e465be3b2788cbfaa078abd0dfc621de69e5a` / ifACE `0.1.7` | 合同历史起点；保留原静态审计与行号 |
| Kernel `41e2f4d16fe369c00d81960ebc0e5f269b263477` / ifACE `0.1.8` | 新公开model profile、失败事件、工具effect及HTTP流等候选API；已有局部实际FAIL和构建阻塞 |
| Kernel `481723` | 该条为早期局部源码审阅时的481723候选；不能覆盖`41e2`原FAIL，后续候选和验证见账本 |

TS产品行为基线仍为`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。当前源码、本地候选、实际受测版本及历史继承证据分别保存。ifACE包版本、Scheduler SDK版本、Scheduler协议和L-C协商整数ABI也是独立版本轴。

Kernel通过单一独立源码检出和相对Cargo路径绑定，不复制私有实现。绑定指定完整提交后执行所需检查：

```sh
python3 scripts/iface-integration/bind_kernel.py --kernel-root /path/to/agent-kernel --expected-commit <完整提交>
cargo test --manifest-path src/Cargo.toml -p pilotdeck-context-core -p pilotdeck-model-core -p pilotdeck-iface
```

上述命令是操作示例，不表示当前候选已执行通过。绑定脚本检查提交、源码洁净度及已有绑定；`deps/agent-kernel`为不入库软链接。发行时仍需提供真实源码和组合产物身份。

## 如何阅读进度

最新[第五批最终回读收据](../../_tmp/20261115-iface-fifth-preparation/sync-final-links.json)为 `PASS_READBACK`：**13条合同、576条任务及变更记录、443条验收记录，共1032条**。576条物理任务记录分为26条交付任务、463条上游处置、74条Kernel变更、13条产品变更；本批新增5条Kernel变更和90条验收记录，没有关闭交付任务。完整账本SHA256为 `7b9009046fba96a5f5f7480b611a9ee1dd85cf5977fe21fc53fd0cbe67ce6c08`。原353条验收记录、六张旧表及既有首次失败保持原状；26交付分母和旧后端127分母各自独立。

第五批[独立显示语义复审](../../_tmp/20261115-iface-fifth-terminology-independent/display-semantic-review-final-v4.json)已关闭5类问题、7条相关记录，明确API消费者检查、零用例文档命令、受影响范围与完整矩阵、Mac实际dirty输入及配置/产物身份的区别。显示审查、源码审查、构建成功、实际运行和管理同步分别记载。表内证据截止到 **Kernel `5d6d4569`及当时Mac共享driver/factory的108项dirty输入验证**；后续`856`至`ee4c`和当前产品候选的结果另批追加，不拼成同一组合通过。第五批代码解读见[HTTP准备来源与实际清理](../../docs/iface-integration/kernel-preparation-cleanup-5d6.md)。

本批同步保留了两次首败：第一次请求超过已观测的5MiB请求限制；按字节分批后，27个新run的大事实字段在CLI直接回读中为空。具体丢失位置未确定，不能据此宣称官方单格阈值。经[独立审查](../../_tmp/20261115-iface-projection-review/final-frozen-review-v3.json)，只更新这27条的“事实JSON”及随之重算的“行SHA256”；事实列引用完整Git事实的稳定ID、canonical SHA256和字节数，其他文本列保持原值。完整事实未裁剪，旧353条投影未改。定向修复及最终关联写入分别全量回读；原失败收据、journal备份和root逐项处置依据保留，未重建已存在记录。表中任何PASS仍只适用于对应运行范围。


每项合同分开记录**接口定义、能力实现、正式入口接线、公开注入、当前组合验收**五层。已有代码不自动意味着正式接线；静态检查和局部测试不自动意味着组合通过。UNKNOWN表示未核实，不等于未实现；单条PASS不等于任务已交付。

此前 `sync-v2` 已发布13项合同、26项交付任务、463项上游处置、38项ifACE变更和145条验证记录。[第三批历史回读收据](../../_tmp/20261115-iface-latest-preparation/sync-v2.json)为 `PASS_READBACK`：当时三张物理表分别有 **13、544、238 条记录，共795条**。任务表的544条包括26项交付任务、463项上游处置、50项Kernel变更和5项产品变更，保留为该批历史身份。

[第四批历史回读收据](../../_tmp/20261115-iface-fourth-preparation/sync-final.json)为 `PASS_READBACK`：三张物理表分别有 **13、571、353 条记录，共937条**。任务表的571条包括26项交付任务、463项上游处置、69项Kernel变更和13项产品变更；比第三批新增27项变更追踪和115条验证记录。第四批当时的账本SHA256为 `ed1d2648d053dc20d6014a9ad283a4d00b6cab4dc1ca05ba8e47bfd4ed1d81b6`；[正式投影](../../_tmp/20261115-iface-fourth-preparation/projection-final.json)与已审v4的表内容和事实相同。原六表的记录数及内容哈希在本次同步前后完全相同。交付分母仍为26，旧后端127项仍独立统计；管理回读只证明投影与关系一致，不表示产品通过。

[术语审查](terminology.md)覆盖较早的94个稳定记录ID；此前 `sync-v2` 的685条物理记录也完成显示用语复核。第三批对795条物理记录完成显示语义审阅及两项修订复核，238条验证事实JSON与全部795项首次失败在该次修订前后未变。第四批[独立显示语义审查](../../_tmp/20261115-iface-fourth-terminology-independent/display-semantic-review-final-v4.json)覆盖142个新增行及29个既有变更行，原766个逐字未变行继承第三批审查；共关闭44行的59个显示单元修订，明确命令来源、历史版本锚、resident结果及管理自检边界。第四批v2到最终v4的937条事实JSON和首次失败均逐字不变，第三批原238条运行整行（含事实JSON及首次失败）也保持不变。这些是显示与证据身份核验，不表示全部937条重新进行源码实现审查或联合验收。

第四批表的证据截止为 **PilotDeck `e1b759b0`／Kernel `f7931007`**。真实resident v6仍是 **e1b＋Kernel `0291dbe0`，1次自定义harness／3项检查PASS**，没有组合成e1b＋f793的产品联合验收。后续Kernel `190ad`大矩阵及新driver开发中代码不在本批，不能用本批回读推定其已同步或通过。

- [ifACE代码解读](iface-guide.md)：接口、宿主、SDK、产品组装与版本轴。
- [合同与能力对应](contracts.md)：保留`ca0/0.1.7`历史起点，说明五层审计和当前候选边界。
- [上游增量处置](upstream-review.md)：逐项接收、实施及尚未闭合的行为差额。
- [Native Loop边界](native-loop.md)：已接原准入/完整准备/codec，以及正式产品组装的剩余工作。
- [Kernel变更追踪](kernel-changes.md)：逐项变更与实际证据，不把局部结果合成全量通过。
- 第四批Kernel解读：[8a2e公开pull HTTP与冻结首败](kernel-http-pull-8a2e.md)、[276／aba／52后续修复](kernel-http-followup-52a7.md)、[f793显式fixture与实际验证边界](kernel-explicit-fixtures-f793.md)；产品生产能力、公开适配、试件与验收装置另见[产品变更来源](../../state/iface-product-followup-e1b.json)。
- [账本与飞书维护](../../scripts/iface-integration-status/README.md)：独立分母、真实关联、旧六表保护与证据要求。

P2生命周期、P3能力包、P4正式组装、P5入口兼容、P6组合及双平台发行、P7迁移与退役仍按阶段出口分别审阅和验收。
