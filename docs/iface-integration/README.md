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

当前正式三表为**第十七批：13条合同、790条任务及追踪、1644条验收记录，共2447行**。790=26交付任务（其中 `iface.P6.release` 与 `iface.P7.migration` 由 `待实现` 推进至 `进行中`，当前任务状态：`待实现` 2、`进行中` 18、`待验证` 3、`已交付` 3）+463上游处置+194 Kernel接入变更+107产品接入变更。本批新增8条产品源码责任记录（Wave 3：`b347d242f` 新增 `pilotdeck-session-migration@0.1.0`、`011bf802e` MintHub 隔离令牌/幂等/`proj-online` 线上流水线、`f6c0124de` `pilotdeck-tool-{core,iface}@0.1.2`、`352404df9` CLI/Gateway opt-in `ScopedResident` 接入与 E2E、`55d2dd946` `pilotdeck-iface@0.1.1` 生产二进制 `pilotdeck-native-loop` 与 `atomic_install` 4/4、`2dfde96bf` Wave 3 五分支合入与 20 包 `CANONICAL_ORDER_20`、`8f53eb4a3` macOS/BSD `O_NONBLOCK` `accept()` 移植修复、`9c085535b` 20 包批量安装求解修复与线上闭环）与19条真实记录（`cb8`/`macOS` 双平台、线上 MintHub Wave 1–3 真推与 `proj-online` 60 包消费安装闭环、3 份 `check_frozen`/零占位符/`gitleaks` 门禁、87 项账本单测及如实保留的首败与先祖失败）。安装底座提交为 `ad8eb5cf4`（代码基线 `9c085535b`）。

[第十七批实际回读](../../_tmp/20261116-seventeenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-seventeenth-root/independent-publication-review-final.json)均通过：5项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；81,220个文本/事实单元和8,908个LINK单元（3,917非空）逐值相符，2,420个旧 `recordID` 保持不变、新增27个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-seventeenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-seventeenth-root/zero-preview.json)确认0创建、0更新、0操作；1,625条历史运行、463项上游处置、23条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。线上 MintHub 已完成 Wave 1（18 包）/Wave 2（19 包）/Wave 3（20 包：新增 `pilotdeck-session-migration@0.1.0`，升级 `pilotdeck-tool-{core,iface}@0.1.2` 与 `pilotdeck-iface@0.1.1`）真实发布及 `proj-online` 8 根无 `path` 纯远端消费者 60 包（20 `pilotdeck-rs/*` + 40 external）安装闭环，且 `~/.mint/credentials.toml` 未落盘（`9c085535b` 前顺序 20 次安装命中 10 万状态上限首败留存）。双平台实测：远程 `cb8`（`Linux aarch64`）在 `2dfde96bf` 上 `fmt`/3 项 `clippy`/`doctest`、`pilotdeck-iface` `113/113`（`resident_model` 3 + `managed_model` 4）、split-crates `105/105`、`gateway kernel-resident` `31/31`、CLI 直连 `pilotdeck-native-loop` E2E `3/3` 通过，全工作区 `9,048/9,049` 通过（仅保留 `unsafe_boundary::compiler_enforces_gateway_boundary` 1 项已知先祖失败），`8f53eb4a3` 后置复验 `pilotdeck-iface` `113/113` 全通过；本机 `macOS aarch64` 在 `8f53eb4a3` 上 split-crates `105/105`、`atomic_install` `4/4`、CLI native-loop E2E `3/3` 通过，`gateway kernel-resident` `-j 6` 命中 1 项 `MODEL_SESSION_BAD_STATE` 竞态失败（留存）后于 `-j 1` 串行 `31/31` 通过（7,917 skipped）。`FullLate`（`LOOP_SETTLEMENT_FEATURE`）、CLI/Gateway 默认切换、非 Recovery Context writer 与旧 runtime 退役仍未关闭。

第十七批账本SHA256为 `2724b094f731973e0d64d3d1e8740146226c02024bd8d99005440c19c17f2bc8`，功能元数据SHA256为 `46bfb562eb1cffff01510827dd9aace7caf3f9f9dd3e0b6fdf41e42d3932c08b`，显示覆盖SHA256为 `fce931ffbbebf154ee419de8a4315972f8dacb249c8977839257742907194a99`，回读收据SHA256为 `8497ea8c2649ab54a3af87a034bfcd4e24025f8df11f9977b91e74ffb1d5c523`，远端完整快照 canonical SHA256 为 `b9fed09aae63909ef19074cf1b61080f1284bbaa1b6a09d5328ade5cefbde62c`。管理回读通过不代表产品全量完成；以下第十六批及更早段落均为历史快照。

第十六批历史三表为**第十六批：13条合同、782条任务及追踪、1625条验收记录，共2420行**。782=26交付任务（其中 `iface.P5.channels` 由 `待实现` 推进至 `进行中` 并绑定通道 E2E 记录）+463上游处置+194 Kernel接入变更+99产品接入变更。本批新增6条产品源码责任记录（Wave 1–2：`79e7fab61` 基线回归恢复、`a09a6b340` 18 包 MintHub `0.1.0` 清单与流水线、`a4eee92b3` `pilotdeck-tool-{core,iface}@0.1.1` 深化、`fe147d956` 新增 `pilotdeck-context-iface@0.1.0`、`c2fe6ed29` `ScopedResidentSessionBinder` 与三入口 E2E、`dd0158900` `Cargo.lock` 同步）与23条真实记录（macOS `aarch64` 与 `cb8` `Linux aarch64` 双平台全套编译/测试/MintHub `--verify-install`/门禁/87 项单测及如实保留的首败日志）。安装底座提交为 `7a8d99cde`（代码基线 `dd0158900`）。

[第十六批实际回读](../../_tmp/20261116-sixteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-sixteenth-root/independent-publication-review-final.json)均通过：5项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；80,333个文本/事实单元和8,811个LINK单元（3,854非空）逐值相符，2,391个旧 `recordID` 保持不变、新增29个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-sixteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-sixteenth-root/zero-preview.json)确认0创建、0更新、0操作；1,602条历史运行、463项上游处置、22条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。双平台复验：本机 `macOS aarch64` 上 `tool-core`/`tool-iface`/`context-iface` `41/41` 通过、`pilotdeck-gateway --features kernel-resident` + `legacy_proxy` + 三入口（`LocalGateway`/WS SDK-CLI/Channel `TurnSubmitter`）E2E `7/7` 通过；远程 `cb8`（`Linux aarch64`）`fmt`、3 项 `clippy`、`doctest`（20 crate `0` 失败/`1` ignored）、`pilotdeck-iface` `107/107`（含 `resident_model`/`managed_model`）、`pilotdeck-gateway --features kernel-resident` `12/12` 及 MintHub `0.1.0`（18+40=58）与 Wave-2（19+40=59）`--verify-install` 均通过；`LANG=en_US.UTF-8` 下全工作区 `8,978` 项通过 `8,977` 项（仅保留 `unsafe_boundary::compiler_enforces_gateway_boundary` 对 `pilotdeck-context-iface -> agentcpm-iface` 本地路径边界 1 项失败）。`FullLate`（`LOOP_SETTLEMENT_FEATURE`）、CLI/Gateway 默认切换、非 Recovery Context writer 与 `MINT_TOKEN` 线上发布仍未关闭。

第十六批账本SHA256为 `37f7f02c7c07799fd625c9a3d81d83fbaf9db0e9ba8ddbad4a50524422a7dd6d`，功能元数据SHA256为 `e1e0566b098662797ef79037f3a733e777509ddd6816d8f7be00c77dcd212c77`，显示覆盖SHA256为 `5aa3f82aa07c6d7769c9be61fbb732803bdca4a81a2985b45c7cb752f1b2dbdb`，回读收据SHA256为 `d1970c0ef4b49fef16964b069ee4276ac11f7fc1794b0ddadd8c28777663046f`，远端完整快照 canonical SHA256 为 `c09b198ed7cd09f983145d3872bdd7f51f6d8f215c9413ff94cdb49d2d9270b2`。管理回读通过不代表产品全量完成；以下第十五批及更早段落均为历史快照。

第十五批历史三表为**第十五批：13条合同、776条任务及追踪、1602条验收记录，共2391行**。776=26交付任务+463上游处置+194 Kernel接入变更+93产品接入变更。新增15条源码责任记录（8 Kernel至 `fb54c74a8`／7 PilotDeck含 `a5934da26`、`329af8631`、`f5c68835e`、`0f48b8742`）与27条真实命令记录（涵盖 Step 4 P2 夹具首败保留与修复通过、Step 5 Native Loop 组装 `154+105` 及独立闭环、Step 6 `NativeProductReads` 与真实宿主 `Model->read_file->Model` 联合 E2E 等）。安装底座提交为 `e454e4e28`。

[第十五批实际回读](../../_tmp/20261116-fifteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-fifteenth-root/independent-publication-review-final.json)均通过：4项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；79,414个文本/事实单元和8,712个LINK单元（3,789非空）逐值相符，2,349个旧 `recordID` 保持不变、新增42个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-fifteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-fifteenth-root/zero-preview.json)确认0创建、0更新、0操作；1,575条历史运行、463项上游处置、22条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。最终目标复验 `nextest -p pilotdeck-iface` `106/106`（含 `resident_model`/`managed_model`）与 `nextest -p pilotdeck-gateway --features kernel-resident` `5/5`（含默认 `TMPDIR` 下 `p2_*` 与 `real_host_kernel_session_runs_model_read_file_model_e2e`）均通过；全工作区 `nextest` 首跑如实保留 `8852 PASS / 10 FAIL / 746 SKIP`（含 2 项既有先祖回归 `9f65dd316`/`9cda9db84` 与 6 项 macOS 外部 PDF/ripgrep/Linux `/proc` 环境项）。生产 `KernelSessionBinder` 对 Kernel `fb54c74a8` 因缺 `LOOP_SETTLEMENT_FEATURE` 仍按设计 fail-closed；非 Recovery Context writer 与 CLI/Gateway 默认生产切换尚未关闭。

第十五批账本SHA256为 `08318f3eede0983198e3382a52d784d6279455aa353e025b428acbf8fb40c0ea`，功能元数据SHA256为 `d3dc913f28311d43d54c34c1e0336bc9972dbde7069e9cb3147e5af9da9c2b71`，远端完整快照 canonical SHA256 为 `37bed433e528841f9afea5fa285948b23e19f2a446445d30b04c87a16d51f21d`。管理回读通过不代表产品全量完成；以下第十四批及更早段落均为历史快照。

第十四批历史三表为**第十四批：13条合同、761条任务及追踪、1575条验收记录，共2349行**。761=26交付任务+463上游处置+186 Kernel接入变更+86产品接入变更。新增23条责任记录（17 Kernel／6产品）、127条原命令记录（113 PASS／11 FAIL／3 INVALID；继续区分命令记录数与测试条数、首次失败与后继修复）。安装底座提交为 `22b631acd`。

[第十四批实际回读](../../_tmp/20261116-fourteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-fourteenth-root/independent-publication-review-final.json)均通过：5项实际操作（3批记录、2批LINK）全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；78,001个文本/事实单元和8,556个LINK单元（3,699非空）逐值相符，2,199个旧 `recordID` 保持不变、新增150个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-fourteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-fourteenth-root/zero-preview.json)确认0创建、0更新、0操作；1,448条历史运行、463项上游处置、22条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。

第十四批账本SHA256为 `1ba08810321ccd331f36989a8cc4ae84a00a82ab3dc5ad4f965c6438849cd863`，功能元数据SHA256为 `ff550720b742fa8a4333269417391230c9661a04245495d33e6e58b1d923e76c`，远端完整快照 canonical SHA256 为 `4dceab8272a302ed6eeb0e3e34defcbbfb646ce9bcc1e79fb7c457919486af44`。管理回读通过不代表产品完成；以下第十三批及更早段落均为历史快照。

第十三批历史三表为**第十三批：13条合同、738条任务及追踪、1448条验收记录，共2199行**。738=26交付任务+463上游处置+169 Kernel接入变更+80产品接入变更。新增24条责任记录（11 Kernel／13产品）、116条原命令（97 PASS／17 FAIL／2 INVALID；51次执行、40次构建、25次管理检查）。命令记录数与测试数分开；术语审查区分接收决定、代码实施、实际运行、远端可见和主线合入，也区分ifACE公共接口、Kernel实现、产品适配、入口接线与测试维护。

[第十三批实际回读](../../_tmp/20261116-thirteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-thirteenth-root/independent-publication-review-final.json)均通过：4项操作（2记录、2LINK）逐项回读，73,352个文本/事实单元和8,060个LINK单元逐值相符，2059个旧ID保留、新增140个，六张旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-thirteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-thirteenth-root/zero-preview.json)确认0创建、0更新、0操作。旧1332条运行、463项上游处置、34个旧非空首败、28条原功能关联、27条完整大事实引用及353条保护事实保持，26／127分母独立。本批新增4条明确功能关联只覆盖3条独立PR原运行中的2／4／6／10个命名子组，不扩大旧17条功能运行的适用范围。

第十三账本SHA256为 `a754ec125ec0b50a7ff02b0469ea20674418bd75acfba5c5e372f54ae1ab0bbb`，完整功能元数据为 `2524bc372e3073b72bb1fa9e5c6da04dde0e45dd8649f073e1acd92126ecbcf0`，远端完整快照canonical SHA256为 `0e453898ce863ae82821bd2b833e13336c61197bf85fe3a8cd08e21170516c69`。表内主接入截止 **Kernel70 `a759543e`／PD `26bbfc`**，独立PR按其各次真实版本另记。68首编及lint失败、69实际55 PASS、70仅构建/静态通过分列；26bb实际71 PASS／1 FAIL／1 ignore和原Cron失败保留。自定义宿主探针原collector INVALID与同PID补充材料并存，`cleanup_observed=false`不改；旧文件名库存查询INVALID也不当执行。独立PR原24条命令及各首次失败保留，独立候选不改变主接入pin。其后新代码、实测和远端推送另批登记，历史观察时点的状态不覆盖当前事实。管理回读不代表产品完成；以下第十二批及更早段落均为历史快照。

第十二批历史三表为**第十二批：13条合同、714条任务及追踪、1332条验收记录，共2059行**。714=26交付任务+463上游处置+158 Kernel接入变更+67产品接入变更。新增21条责任记录、126条原命令（117 PASS／9 FAIL；73次执行、25次构建、28次管理检查），更新8项上游的当前实施观察。术语审查继续区分接收决定、代码实施、实际运行、远端可见和目标主线合入；接口变化、能力实现、产品接线和测试维护分别分类。

[第十二批实际回读](../../_tmp/20261116-twelfth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-twelfth-root/independent-publication-review-final.json)均通过：5项操作逐项回读，68,980个文本/事实单元和7,592个LINK单元逐值相符，1912个旧ID保留、新增147个，六张旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-twelfth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-twelfth-root/zero-preview.json)确认0创建、0更新、0操作。旧1206条运行、34个旧非空首败、28条功能关联、27条完整大事实引用及353条保护事实保持，26／127分母独立。

第十二账本SHA256为 `32f3175de94b77bebd0c0cf7264a68bcb2a02a6d172853f7c4ec2d25f91fca8c`，完整功能元数据为 `bd9de99c7ee0425627fd40dc2b30bec950600a9634a014c47a15958326daaa3c`，远端完整快照canonical SHA256为 `2d04e7c066d53805bfcf950d546e4463d20a76c4bbb607c2a787ac03a867f323`。表内截止 **Kernel66 `4f08f4b0`／PD `b2b4fb11`**，后继代码与运行另批登记。Kernel62/64生产首编失败、63/65/66的实际范围及65权限夹具首败分别保留；PD恢复公开WatchStop旧签名与后继Feishu断言修复独立记录，b2新组合131 PASS／1 FAIL／1 ignore和旧e594精确Cron二进制的失败不合并。历史未执行prep缺失源码引用的缺口继续明记。管理回读不代表产品完成；以下第十一批及更早段落均为历史快照。

第十一批历史三表为**第十一批：13条合同、693条任务及追踪、1206条验收记录，共1912行**。693=26交付任务+463上游处置+140 Kernel接入变更+64产品接入变更。新增20条责任记录、75条原命令（66 PASS／7 FAIL／2 INVALID；41次执行、22次构建、12次管理检查），更新8项上游的当前实施观察。接收决定、代码实施、实际运行、远端可见与目标分支合入分别记录，命令数不等于测试数。

[第十一批实际回读](../../_tmp/20261116-eleventh-root/receipt.json)与[独立全量核验](../../_tmp/20261116-eleventh-root/independent-publication-review-final.json)均通过：5项操作逐项回读，64,444个文本/事实单元和7,109个LINK单元逐值相符，1817个旧ID保留、新增95个，六张旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-eleventh-root/zero-plan.json)与[零写预览](../../_tmp/20261116-eleventh-root/zero-preview.json)确认0创建、0更新、0操作。旧1131条运行、34个旧非空首败、28条功能关联及27条完整大事实引用保持，26／127分母独立。

第十一账本SHA256为 `81ab67ce5490ca0530f31bf15f1dc6421dac2f4bb8fa73d5ba368f62cffa7905`，完整功能元数据为 `00b5cf2ad9b1749fabc0ab274618cf6dbe0c9f5f8ea84dee7f6cde1738031ee0`，远端完整快照canonical SHA256为 `8587cbc444402b03b18b7d102dac0bb3ffa35b7db17520792d5bb2b26d82267a`。表内截止 **Kernel61 `bcaa43eb`／PD `39089ace`**，其后新候选另批记录。39089生产构建通过，但有6处测试目标编译错误，其中包含WatchStop公开返回类型的源码兼容破坏，不能统称无公共接口变化的夹具问题；后继恢复旧签名另列。两个standalone探针只到setup失败，均为INVALID、fullLate NOT_REACHED。一个未执行旧prep的源码引用被覆盖且无原稿，该缺口明确保留，未伪造证据。管理核验不代表产品验收。以下第十批及更早段落均为历史快照。

第十批历史三表为**第十批：13条合同、673条任务及追踪、1131条验收记录，共1817行**。673=26交付任务+463上游处置+133 Kernel接入变更+51产品接入变更。新增19条接入变更、84条原命令（74 PASS／10 FAIL；44次执行、27次构建、13次管理检查），并更新BlueBubbles与API Server两项上游的当前实施观察。命令数不等于测试数；原决定、理由、实际身份和首败分别保留。

[第十批实际回读](../../_tmp/20261116-tenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-tenth-root/independent-publication-review-final.json)均通过：3个记录批次和2个LINK批次共5项实际操作已回读，61,429个文本/事实单元和6,784个LINK单元逐值相符，1714个旧ID保留、新增103个，六张旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-tenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-tenth-root/zero-preview.json)确认0创建、0更新、0操作。旧1047条运行、34个非空首败、28条功能关联及27条完整大事实引用保持，26／127分母独立。

第十批账本SHA256为 `0e190a96e77eac6afb686d5d3f475f9bac47d36b4cc615dbd352781d8cba40bc`，完整功能元数据为 `fbdc4a1445e9ff219428c4ee02b4bfa9653651f292df1370d460c120ea52f1ff`，远端完整快照canonical SHA256为 `ef25d91194c13941f3f51b8f20f4dfab62dab7103d318e05d40fb212fc8f9bfe`。表内截止 **Kernel58 `4841eca3`／PD `f5691665`**：f569首次生产编译仍FAIL、0测试；后继修复与后继运行另批登记。Blue实际14 PASS／1旧ignore、API 67 PASS分别保留inflight所有权及异常路径余差，不能整体称上游等价。公开Rust构造类型变化、SDK/Host改动、产品接线与测试维护分别分类；管理回读不代表产品验收。以下第九批及更早段落均为历史快照。

第九批历史三表为**第九批：13条合同、654条任务及追踪、1047条验收记录，共1714行**。654=26交付任务+463上游处置+124 Kernel接入变更+41产品接入变更。新增22条责任记录和96条原命令记录（84 PASS、10 FAIL、2 INVALID；43次执行、30次构建、23次管理检查）。命令记录数与独立测试数分开统计；26项接入交付与旧后端127项分母不变。

[第九批实际回读](../../_tmp/20261115-ninth-root/receipt.json)为 `PASS_READBACK`，5项操作均实际回读；[独立全量核验](../../_tmp/20261115-ninth-root/independent-publication-review-final.json)确认58,195个文本/事实单元和6,437个LINK单元相符，1596个旧记录ID保留、新增118，六张旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261115-ninth-root/zero-plan.json)与[零写预览](../../_tmp/20261115-ninth-root/zero-preview.json)确认0创建、0更新、0操作。旧951条运行、33个非空首次失败、28条显式功能关联及27条大事实引用完整保留。

第九批账本SHA256为 `b81ef588665bd93409e2e8edc53143e9753649cd3f22f1d4f6984cab6bb63387`，完整功能元数据为 `265f51b816967b8a0b33d7a65a15168b5ab1959de7c8af430c274aa0f7f5a7bc`，完整远端快照canonical SHA256为 `fed68f4c1c4b05fb97ba4e3e970d445fdc1afc939f71c08f2dc1921bd85d3fac`。代码证据截止 **Kernel55 `50b96187`／PD `b4c340e8`**。16条原上游记录已补充当前实施观察（15条日志、1条runtime status），保留原接收决定、理由和来源审阅；日志实际只验证31处中的1条业务分支，不能整体关账。术语审查区分接收决定、代码实施、本地候选吸收、实测、远端可见与目标主线合入，也区分接口变化、能力实现、产品接线和测试维护。

第九批保留Context首次19 PASS／1 FAIL及尚未完成的41例oracle；原mutation装置的首个无新产物INVALID、后继缺两个spec JSON文件的构建FAIL分别保留。第十批及更晚代码结果正在独立审阅，未写入本批。以下第八批及更早段落均为历史快照，不能覆盖本段当前数量、截止身份或验收范围。

第八批历史三表为**第八批：13条合同、632条任务及追踪、951条验收记录，共1596行**。632=26交付任务+463上游处置+110 Kernel接入变更+33产品接入变更；143条接入变更不等于143项公开接口变化。新增34条责任记录与239条原命令记录，239=210 PASS+28 FAIL+1 INVALID，按类型为133次执行、69次构建、37次管理检查；这些是命令记录数，不能当作通过测试数。原712条运行事实、598条分类和28条功能关联保持，未按共享合同扩展功能证据。

[第八批实际回读](../../_tmp/20261115-eighth-root/receipt.json)为 `PASS_READBACK`，7项操作全部实际回读；[独立全量核验](../../_tmp/20261115-eighth-root/independent-publication-review-final.json)确认54,487个文本/事实单元、6,039个LINK单元相符，1323个旧记录ID保留、新增273，六张旧表计数和内容SHA不变。[零差异计划](../../_tmp/20261115-eighth-root/zero-plan.json)及[零写预览](../../_tmp/20261115-eighth-root/zero-preview.json)为0创建/0更新/0操作。旧非空首次失败不变；两个原空首次失败随明确关联的新FAIL补齐。管理一致性通过不改变产品交付状态或26／127两个独立分母。

第八批账本SHA256为 `3aba249d7ce2bda6d1ecfca1af231d6b822655fbc342f8a6ade0b5848db9b1c3`，完整功能元数据SHA256为 `86565b188a6b4ba6d1977fe71abdd111ffbff8948b3555793db4c7b8f423514e`，完整远端快照canonical SHA256为 `7d178dcce9ca016c31fa7e046c099e3bae9a0b5925fb370067486edf32588a99`。表内新增代码证据截止 **Kernel50 `a0a0f368`／PD `983e6d46`**，保留Linux43/47挂起、49/50编译首败和Mac105e整批FAIL、606整批FAIL（on前置失败、Host0，比较无效）、983仅fixture自检等实际范围。Kernel51以后与PD日志、runtime status、Context的新结果另批登记；既有463条上游处置的旧实施状态不能自动当成这些后继已经回填。

以下保留第七批功能链结构发布的历史身份：**13条合同、598条任务及追踪、712条原运行，共1323行**。598=26交付任务+463上游处置+86 Kernel接入变更+23产品接入变更。任务表实体类型统一为“交付任务／上游增量处置／接入变更”，变更性质独立多选；同一合同下的运行不自动继承功能关联。完整例子见[完整模型输入检查](model-input-inspection-example.md)，功能稳定ID为 `iface.feature.model-input-inspection`，精确28条显式关联，其中17条原运行。

[字段迁移收据](../../_tmp/20261115-feature-schema-root/schema-receipt.json)为 `PASS_READBACK`，20项操作已核；[记录发布收据](../../_tmp/20261115-feature-schema-root/records-receipt.json)为 `PASS_READBACK`，13项操作均实际回读，1238个旧记录ID保留，85个新增，六张旧表前后计数/SHA相等。该历史批账本SHA256为 `d91ecbe015355ad2bb12896df4c51b10cf01fdbbb20b1550f3a18c39f2da270b`。这是管理一致性核验；P2至P7产品实施与联合验收仍按各自阶段出口审阅，26交付分母与旧后端127分母独立。[零差异计划](../../_tmp/20261115-feature-schema-root/zero-plan.json)与[零写dry-run](../../_tmp/20261115-feature-schema-root/zero-preview.json)均确认0创建、0更新、0操作。

Kernel本轮精确观察目标为2026-11-15T12:47:40Z的master `fd074191f4e134c79e836d764ec2f2ed4f196bf3`。88条有可读本地实施提交的Kernel记录共33个不同候选，逐一证实非该master祖先，仅据此记NOT_MERGED；不据此断言远端没有等效功能或其他ref不可见。远端原已有payload/messages/tools校验、ledger/admission及worker/claim双授权；本地sealed profile/final inspector/media门差额另有明确源码审阅。PD目标origin/main本轮未取得精确head及祖先证明，继续UNKNOWN；远端可见与目标主线合入分列。后续Kernel48 Linux及49首编不在本批712条，不能回填旧事实。

以下第六批及更早批次保留其历史身份与当时收据，不代表当前总数。

第六批历史**代码证据批**：[实际回读收据](../../_tmp/20261115-iface-sixth-preparation/sync.json)为 `PASS_READBACK`，三表分别 **13、588、631 行，共1232行**。588条任务表物理记录包括26条交付任务、463条上游处置、82条Kernel变更和17条产品变更；比第五批新增8条Kernel变化、4条产品责任边界变化及188条compact记录。该代码批的完整账本SHA256为 `7ccb4e75caa54970231e7bed28f1ee3b6744b483d5b612efd7c8d0d95c99f32b`。原443条运行事实及投影、旧550条追踪投影、合同审计、首次失败和旧六表保持原状；该批没有宣告产品交付，26／127分母仍独立。

第六批源码截止为 **Kernel `ee4c6471`（第28轮）／PD `d9aac452`**。Mac实际执行保持 **dirty2ac输入快照，依赖Kernel的HTTP closure实际为856**；最终25份普通Rust测试收据为558PASS／55ignore，事后4846项Git输入对应不冒充clean d9aac重跑。1595的Native1792是完整libtest通过，不能由此宣称真实Mint链执行；1104默认1838含22内部SKIP，只完成1816项行为；ee4c正式Mint artifact-chain为普通Rust integration测试1项FAIL，源码构建成功不等于整链验收通过。第29轮以后及Mac c182／f616等后续证据另批记录。

[第六批独立语义复审](../../_tmp/20261115-iface-projection-review/sixth-semantic-review-v1.json)确认候选事实与证据身份；说明中的e7 list预期数已按原receipt更正为3／实际列出9，原运行事实一直保留正确3／9。见[更正后的冻结说明](../../_tmp/20261115-iface-sixth-preparation/SIXTH-REVIEW-NOTES-v3.md)。原27条大事实引用policy保持不变，新记录只引用完整原receipt的path与SHA，不在每行复制manifest，也不裁剪完整事实。正式入口接线、公开注入和联合实测继续分开记录。代码解读见[原owner释放、消费信用与SDK观察](kernel-release-demand-sdk-ee4c.md)。

P0来源固定、目录刷新、P1三表维护的管理接受由root另批审阅；其记录数和接受状态以独立收据为准，不混入上述代码批的产品运行通过数。管理同步回读只确认数据与关系一致。

第五批[历史最终回读收据](../../_tmp/20261115-iface-fifth-preparation/sync-final-links.json)为 `PASS_READBACK`：**13条合同、576条任务及变更记录、443条验收记录，共1032条**。576条物理任务记录分为26条交付任务、463条上游处置、74条Kernel变更、13条产品变更；该批新增5条Kernel变更和90条验收记录，没有关闭交付任务。完整账本SHA256为 `7b9009046fba96a5f5f7480b611a9ee1dd85cf5977fe21fc53fd0cbe67ce6c08`。原353条验收记录、六张旧表及既有首次失败保持原状；26交付分母和旧后端127分母各自独立。

第五批[独立显示语义复审](../../_tmp/20261115-iface-fifth-terminology-independent/display-semantic-review-final-v4.json)已关闭5类问题、7条相关记录，明确API消费者检查、零用例文档命令、受影响范围与完整矩阵、Mac实际dirty输入及配置/产物身份的区别。显示审查、源码审查、构建成功、实际运行和管理同步分别记载。第五批表内证据截止到 **Kernel `5d6d4569`及当时Mac共享driver/factory的108项dirty输入验证**；`856`至`ee4c`与dirty2ac共享包结果已在第六批单列，不与第五批拼成同一组合通过。第五批代码解读见[HTTP准备来源与实际清理](../../docs/iface-integration/kernel-preparation-cleanup-5d6.md)。

第五批同步保留了两次首败：第一次请求超过已观测的5MiB请求限制；按字节分批后，27个新run的大事实字段在CLI直接回读中为空。具体丢失位置未确定，不能据此宣称官方单格阈值。经[独立审查](../../_tmp/20261115-iface-projection-review/final-frozen-review-v3.json)，只更新这27条的“事实JSON”及随之重算的“行SHA256”；事实列引用完整Git事实的稳定ID、canonical SHA256和字节数，其他文本列保持原值。完整事实未裁剪，旧353条投影未改。定向修复及最终关联写入分别全量回读；原失败收据、journal备份和root逐项处置依据保留，未重建已存在记录。表中任何PASS仍只适用于对应运行范围。


每项合同分开记录**接口定义、能力实现、正式入口接线、公开注入、当前组合验收**五层。已有代码不自动意味着正式接线；静态检查和局部测试不自动意味着组合通过。UNKNOWN表示未核实，不等于未实现；单条PASS不等于任务已交付。

此前 `sync-v2` 已发布13项合同、26项交付任务、463项上游处置、38项ifACE变更和145条验证记录。[第三批历史回读收据](../../_tmp/20261115-iface-latest-preparation/sync-v2.json)为 `PASS_READBACK`：当时三张物理表分别有 **13、544、238 条记录，共795条**。任务表的544条包括26项交付任务、463项上游处置、50项Kernel变更和5项产品变更，保留为该批历史身份。

[第四批历史回读收据](../../_tmp/20261115-iface-fourth-preparation/sync-final.json)为 `PASS_READBACK`：三张物理表分别有 **13、571、353 条记录，共937条**。任务表的571条包括26项交付任务、463项上游处置、69项Kernel变更和13项产品变更；比第三批新增27项变更追踪和115条验证记录。第四批当时的账本SHA256为 `ed1d2648d053dc20d6014a9ad283a4d00b6cab4dc1ca05ba8e47bfd4ed1d81b6`；[正式投影](../../_tmp/20261115-iface-fourth-preparation/projection-final.json)与已审v4的表内容和事实相同。原六表的记录数及内容哈希在本次同步前后完全相同。交付分母仍为26，旧后端127项仍独立统计；管理回读只证明投影与关系一致，不表示产品通过。

[术语审查](terminology.md)覆盖较早的94个稳定记录ID；此前 `sync-v2` 的685条物理记录也完成显示用语复核。第三批对795条物理记录完成显示语义审阅及两项修订复核，238条验证事实JSON与全部795项首次失败在该次修订前后未变。第四批[独立显示语义审查](../../_tmp/20261115-iface-fourth-terminology-independent/display-semantic-review-final-v4.json)覆盖142个新增行及29个既有变更行，原766个逐字未变行继承第三批审查；共关闭44行的59个显示单元修订，明确命令来源、历史版本锚、resident结果及管理自检边界。第四批v2到最终v4的937条事实JSON和首次失败均逐字不变，第三批原238条运行整行（含事实JSON及首次失败）也保持不变。这些是显示与证据身份核验，不表示全部937条重新进行源码实现审查或联合验收。

第四批表的证据截止为 **PilotDeck `e1b759b0`／Kernel `f7931007`**。真实resident v6仍是 **e1b＋Kernel `0291dbe0`，1次自定义harness／3项检查PASS**，没有组合成e1b＋f793的产品联合验收。后续Kernel `190ad`大矩阵及新driver开发中代码不在本批，不能用本批回读推定其已同步或通过。

- [ifACE代码解读](iface-guide.md)：接口、宿主、SDK、产品组装与版本轴。
- [从一次请求理解当前 Step、权限与清理](kernel-scoped-79.md)：冻结 Kernel 79 的实际责任边界、源码锚和分版本实测；旧指南继续保留其历史身份。
- [合同与能力对应](contracts.md)：保留`ca0/0.1.7`历史起点，说明五层审计和当前候选边界。
- [上游增量处置](upstream-review.md)：逐项接收、实施及尚未闭合的行为差额。
- [Native Loop边界](native-loop.md)：已接原准入/完整准备/codec，以及正式产品组装的剩余工作。
- [Kernel变更追踪](kernel-changes.md)：逐项变更与实际证据，不把局部结果合成全量通过。
- 第四批Kernel解读：[8a2e公开pull HTTP与冻结首败](kernel-http-pull-8a2e.md)、[276／aba／52后续修复](kernel-http-followup-52a7.md)、[f793显式fixture与实际验证边界](kernel-explicit-fixtures-f793.md)；产品生产能力、公开适配、试件与验收装置另见[产品变更来源](../../state/iface-product-followup-e1b.json)。
- [账本与飞书维护](../../scripts/iface-integration-status/README.md)：独立分母、真实关联、旧六表保护与证据要求。

P2生命周期、P3能力包、P4正式组装、P5入口兼容、P6组合及双平台发行、P7迁移与退役仍按阶段出口分别审阅和验收。

第七批发布后远端完整export的canonical SHA256为 `e9ddb7116e471d03f97c4c0ebbd98e8b9a35b938371a71c9c1c00ef78660be61`，与zero-plan及dry-run所校验的snapshot一致。dry-run不执行LINK写入；LINK零差异由[独立完整回读报告](../../_tmp/20261115-feature-schema-root/independent-publication-review-final.json)逐项解析远端record ID并匹配稳定ID证明。schema、records、snapshot及零差异原件的raw SHA256逐份保存在同一报告的 `publicationArtifactRawSha256`。
