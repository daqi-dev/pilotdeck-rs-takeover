# ifACE 接入三表维护

当前正式三表为**第十八批：13条合同、791条任务及追踪、1649条验收记录，共2453行**。791=26交付任务+463上游处置+194 Kernel接入变更+108产品接入变更。新增1条产品源码责任记录（`22fe88fec`：MintHub 20 包拉取后 4 阶段拼装脚本、安装账本脚手架、跨包 E2E crate `pilotdeck-minthub-stitched-e2e` 与[双平台实测报告](../../docs/iface-integration/minthub-stitched-verification.md)）与5条真实记录（`macOS`/`cb8` 首败留存、`macOS` 与 `cb8` 同版 4 阶段拼装通过及门禁）。安装底座提交为 `6211ae03e`（产品基线 `22fe88fec`）。

[第十八批实际回读](../../_tmp/20261116-eighteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-eighteenth-root/independent-publication-review-final.json)均通过：6项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；81,407个文本/事实单元和8,928个LINK单元（3,930非空）逐值相符，2,447个旧 `recordID` 保持不变、新增6个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-eighteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-eighteenth-root/zero-preview.json)确认0创建、0更新、0操作；1,644条历史运行、463项上游处置、24条既有非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。双平台拼装实测：`macOS aarch64` 与 `cb8`（`Linux aarch64`）均锁定 60 个远端 MintHub 包（`install.json` SHA `ef920219...`, declared=50/excluded=10, 0644）、14 纯包脱离 Kernel 过 `1,044/1,044`、`@0.1.6` 探针稳定报 `107` 错、全 20 下载包过 `1,309/1,309` + harnesses 与 `pilotdeck-minthub-stitched-e2e` `3/3`。

第十八批账本SHA256为 `f25319d165f48a531a79fc6c70ca4e9b62bb4995ed918043ebc2ddc94db8434f`，功能元数据SHA256为 `a6a5aaafd9f5a77aa5884458866eb6f40e3c87129cbfdab80cccd244bfd0d4eb`，显示覆盖SHA256为 `0480c8f69d55dfee139cebe251da251233476d3acecda5184f4e0919b35f1680`，回读收据SHA256为 `58333c4c3b01147d62ac3e2af8b940a5b05e256ca2d0afa2a2dbcbfaf111f815`，远端完整快照 canonical SHA256 为 `815e7225d5f6cd5f429295779a3c04d2f614e2c4ddc44bd61ecabe06e3bc035a`。管理回读通过不代表产品全量完成；以下第十七批及更早段落均为历史快照。

第十七批历史三表为**第十七批：13条合同、790条任务及追踪、1644条验收记录，共2447行**。790=26交付任务（其中 `iface.P6.release` 与 `iface.P7.migration` 推进至 `进行中`）+463上游处置+194 Kernel接入变更+107产品接入变更。新增8条产品源码责任记录（Wave 3：`b347d242f` `pilotdeck-session-migration@0.1.0`、`011bf802e` MintHub 隔离令牌/`proj-online`、`f6c0124de` `pilotdeck-tool-{core,iface}@0.1.2`、`352404df9` opt-in CLI/Gateway `ScopedResident`、`55d2dd946` `pilotdeck-iface@0.1.1` + `pilotdeck-native-loop`、`2dfde96bf` 20 包工作区与 `CANONICAL_ORDER_20`、`8f53eb4a3` macOS `O_NONBLOCK` 修复、`9c085535b` MintHub 批量 `proj-closure` 修复）与19条真实记录（MintHub Wave 1–3 真推 + `proj-online` 60 包、双平台实测与首败/先祖失败留存、87 项单测与门禁）。安装底座提交为 `ad8eb5cf4`（代码 `9c085535b`）。

[第十七批实际回读](../../_tmp/20261116-seventeenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-seventeenth-root/independent-publication-review-final.json)均通过：5项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；81,220个文本/事实单元和8,908个LINK单元（3,917非空）逐值相符，2,420个旧 `recordID` 保持不变、新增27个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-seventeenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-seventeenth-root/zero-preview.json)确认0创建、0更新、0操作；1,625条历史运行、463项上游处置、23条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。线上 MintHub Wave 1–3 20 包真实发布与 `proj-online` 60 包纯远端消费安装闭环通过（`~/.mint/credentials.toml` 未落盘）；双平台实测：`cb8` `2dfde96bf` 下全工作区 `9,048/9,049`（1 已知先祖失败）、`pilotdeck-iface` `113/113`（含 `8f53eb4a3` 后置复验）、split-crates `105/105`、`gateway` `31/31`、CLI native-loop E2E `3/3`；`macOS` `8f53eb4a3` 下 `105/105`、`atomic_install` `4/4`、CLI native-loop E2E `3/3`、`gateway` `-j 6` 1 竞态失败留存后 `-j 1` `31/31`。`FullLate`、默认切换、非 Recovery writer 与旧 runtime 退役待继续完成。

第十七批账本SHA256为 `2724b094f731973e0d64d3d1e8740146226c02024bd8d99005440c19c17f2bc8`，功能元数据SHA256为 `46bfb562eb1cffff01510827dd9aace7caf3f9f9dd3e0b6fdf41e42d3932c08b`，显示覆盖SHA256为 `fce931ffbbebf154ee419de8a4315972f8dacb249c8977839257742907194a99`，回读收据SHA256为 `8497ea8c2649ab54a3af87a034bfcd4e24025f8df11f9977b91e74ffb1d5c523`，远端完整快照 canonical SHA256 为 `b9fed09aae63909ef19074cf1b61080f1284bbaa1b6a09d5328ade5cefbde62c`。管理回读通过不代表产品全量完成；以下第十六批及更早段落均为历史快照。

第十六批历史三表为**第十六批：13条合同、782条任务及追踪、1625条验收记录，共2420行**。782=26交付任务（其中 `iface.P5.channels` 由 `待实现` 推进至 `进行中`）+463上游处置+194 Kernel接入变更+99产品接入变更。新增6条产品源码责任记录（Wave 1–2：`79e7fab61` 基线回归恢复、`a09a6b340` MintHub `0.1.0` 18 包清单与流水线、`a4eee92b3` 工具深化 `0.1.1`、`fe147d956` 新增 `pilotdeck-context-iface@0.1.0`、`c2fe6ed29` `ScopedResidentSessionBinder` 与三入口 E2E、`dd0158900` `Cargo.lock` 同步）与23条真实记录（双平台编译/测试/MintHub `--verify-install`/门禁/87 项单测与首次失败保留）。安装底座提交为 `7a8d99cde`（代码 `dd0158900`）。

[第十六批实际回读](../../_tmp/20261116-sixteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-sixteenth-root/independent-publication-review-final.json)均通过：5项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；80,333个文本/事实单元和8,811个LINK单元（3,854非空）逐值相符，2,391个旧 `recordID` 保持不变、新增29个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-sixteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-sixteenth-root/zero-preview.json)确认0创建、0更新、0操作；1,602条历史运行、463项上游处置、22条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。双平台复验：本机 `macOS aarch64` `tool-core`/`tool-iface`/`context-iface` `41/41` 与 `kernel-resident` + `legacy_proxy` + 三入口 E2E `7/7` 通过；远程 `cb8` `Linux aarch64` `fmt`/3项`clippy`/`doctest`、`pilotdeck-iface` `107/107`、`kernel-resident` `12/12` 及 MintHub `0.1.0`（58闭环）/Wave-2（59闭环）`--verify-install` 均通过；`LANG=en_US.UTF-8` 下全工作区 `8,977/8,978` 通过（仅剩 `unsafe_boundary::compiler_enforces_gateway_boundary` 对 `pilotdeck-context-iface -> agentcpm-iface`）。`FullLate`、默认入口切换、非 Recovery writer 与 `MINT_TOKEN` 线上发布待继续完成。

第十六批账本SHA256为 `37f7f02c7c07799fd625c9a3d81d83fbaf9db0e9ba8ddbad4a50524422a7dd6d`，功能元数据SHA256为 `e1e0566b098662797ef79037f3a733e777509ddd6816d8f7be00c77dcd212c77`，显示覆盖SHA256为 `5aa3f82aa07c6d7769c9be61fbb732803bdca4a81a2985b45c7cb752f1b2dbdb`，回读收据SHA256为 `d1970c0ef4b49fef16964b069ee4276ac11f7fc1794b0ddadd8c28777663046f`，远端完整快照 canonical SHA256 为 `c09b198ed7cd09f983145d3872bdd7f51f6d8f215c9413ff94cdb49d2d9270b2`。管理回读通过不代表产品全量完成；以下第十五批及更早段落均为历史快照。

第十五批历史三表为**第十五批：13条合同、776条任务及追踪、1602条验收记录，共2391行**。776=26交付任务+463上游处置+194 Kernel接入变更+93产品接入变更。新增15条源码责任记录（8 Kernel至 `fb54c74a8`／7 PilotDeck含 `a5934da26`、`329af8631`、`f5c68835e`、`0f48b8742`）与27条真实命令记录（含 Step 4–6 首次失败保留、Native Loop 组装、真实宿主 `Model->read_file->Model` 联合 E2E 及账本工具 87 单测）。安装底座提交为 `e454e4e28`。

[第十五批实际回读](../../_tmp/20261116-fifteenth-root/receipt.json)与[独立全量核验](../../_tmp/20261116-fifteenth-root/independent-publication-review-final.json)均通过：4项实际操作全部 `VERIFIED_READBACK`，`.write-journal.json` `pendingOperations=[]`；79,414个文本/事实单元和8,712个LINK单元（3,789非空）逐值相符，2,349个旧 `recordID` 保持不变、新增42个（0删除），六张受保护旧表计数及内容SHA不变。[零差异计划](../../_tmp/20261116-fifteenth-root/zero-plan.json)与[零写预览](../../_tmp/20261116-fifteenth-root/zero-preview.json)确认0创建、0更新、0操作；1,575条历史运行、463项上游处置、22条旧非空首败、27条完整大事实引用及353条受保护历史事实全部保持，26／127分母独立不混淆。最终目标复验 `nextest -p pilotdeck-iface` `106/106` 与 `nextest -p pilotdeck-gateway --features kernel-resident` `5/5`（默认 `TMPDIR`）均通过；全工作区 `nextest` 首跑如实保留 `8852 PASS / 10 FAIL / 746 SKIP`；生产 `KernelSessionBinder` 对 Kernel `fb54c74a8` 仍缺 `LOOP_SETTLEMENT_FEATURE` 而 fail-closed，非 Recovery Context writer 与 CLI/Gateway 默认切换待继续完成。

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

`state/iface-integration.json` 是本阶段事实账本。它与原后端 127 项叶子任务和网页联调账本分别维护；飞书只是投影。唯一执行交接仍为 `state/backend-handoff.md`。本工具不读取凭据，不管理旧六表，不运行产品、不调用模型、不自动建表或删除记录。root 是唯一外部写入人。

三表名称固定为 **ifACE 接入合同与能力映射、ifACE 接入交付任务、ifACE 接入验收记录**。合同分别记录接口定义、能力实现、正式入口接线、公开注入和当前组合验收五层事实。“能力实现”包括按责任分工提供的Kernel管理检查与具体能力包，不能理解为全部算法都写在Kernel内部。任务使用新 `iface.*` ID；关联 `backend.*` 只表示来源资产，不继承旧完成状态。验收表一条记录对应一个用例在一个明确版本组合下的一次执行或证据审阅，批次 ID 只作关联，不能当成额外一次运行。

历史初始账本为13合同、26交付任务、0验证记录。此前 `sync-v2` 快照为13合同、26交付、463上游处置、38项ifACE变更、145条验证记录。[第三批历史回读收据](../../_tmp/20261115-iface-latest-preparation/sync-v2.json)为 `PASS_READBACK`：当时三张物理表分别有 **13、544、238 条记录，共795条**；任务表的544条包括26交付任务、463上游处置、50项Kernel变更和5项产品变更，该批身份保留。

[第四批历史回读收据](../../_tmp/20261115-iface-fourth-preparation/sync-final.json)为 `PASS_READBACK`：三张物理表分别有 **13、571、353 条记录，共937条**。任务表包括26交付任务、463上游处置、69项Kernel变更和13项产品变更；比第三批新增27项变更追踪、115条验证记录。第四批当时的账本SHA256为 `ed1d2648d053dc20d6014a9ad283a4d00b6cab4dc1ca05ba8e47bfd4ed1d81b6`；[正式投影](../../_tmp/20261115-iface-fourth-preparation/projection-final.json)仅来源路径与已审v4不同，表内容和事实完全相同。原六表的记录数及内容哈希在此次同步前后完全相同，交付分母仍为26，旧后端127项独立统计。管理同步回读只证明数据和关系匹配，不表示产品通过。

条款引用与任务来自批准计划，来源读取或记录增加不自动改变合同审计层或交付状态。早期术语审查覆盖94个稳定ID，此前 `sync-v2` 对685条物理记录做了显示用语复核。第三批对795条物理记录完成显示语义审阅，并对两项修订独立复核；238条验证事实JSON和全部795项首次失败在该次修订前后保持不变。第四批[独立显示语义审查](../../_tmp/20261115-iface-fourth-terminology-independent/display-semantic-review-final-v4.json)审查142个新增行及29个既有变更行，766个逐字未变行继承第三批审查，关闭44行共59个显示单元修订。第四批v2到最终v4的937条事实JSON及首次失败逐字不变；原第三批238条运行整行（含事实JSON及首次失败）保持不变。显示审阅及证据身份核验不表示全部937条都重新做过源码实现审查或联合验收。

第四批表中证据截止为 **PilotDeck `e1b759b0`／Kernel `f7931007`**；真实resident v6实际组合仍为 **e1b＋Kernel `0291dbe0`，1次自定义harness／3项检查PASS**，没有e1b＋f793组合PASS。后续Kernel `190ad`大矩阵及新driver开发中代码均不在本批。新增Kernel解读分别见[8a2e公开pull与冻结首败](../../docs/iface-integration/kernel-http-pull-8a2e.md)、[276／aba／52后续修复](../../docs/iface-integration/kernel-http-followup-52a7.md)、[f793显式fixture与实际范围](../../docs/iface-integration/kernel-explicit-fixtures-f793.md)，产品变更见[独立来源记录](../../state/iface-product-followup-e1b.json)；历史0291 guide保留原身份。

第六批历史**代码证据批**：[实际回读收据](../../_tmp/20261115-iface-sixth-preparation/sync.json)为 `PASS_READBACK`，三表分别 **13、588、631 行，共1232行**。588条任务表物理记录包括26条交付任务、463条上游处置、82条Kernel变更和17条产品变更；比第五批新增8条Kernel变化、4条产品责任边界变化及188条compact记录。该代码批的完整账本SHA256为 `7ccb4e75caa54970231e7bed28f1ee3b6744b483d5b612efd7c8d0d95c99f32b`。原443条运行事实及投影、旧550条追踪投影、合同审计、首次失败和旧六表保持原状；该批没有宣告产品交付，26／127分母仍独立。

第六批源码截止为 **Kernel `ee4c6471`（第28轮）／PD `d9aac452`**。Mac实际执行保持 **dirty2ac输入快照，依赖Kernel的HTTP closure实际为856**；最终25份普通Rust测试收据为558PASS／55ignore，事后4846项Git输入对应不冒充clean d9aac重跑。1595的Native1792是完整libtest通过，不能由此宣称真实Mint链执行；1104默认1838含22内部SKIP，只完成1816项行为；ee4c正式Mint artifact-chain为普通Rust integration测试1项FAIL，源码构建成功不等于整链验收通过。第29轮以后及Mac c182／f616等后续证据另批记录。

[第六批独立语义复审](../../_tmp/20261115-iface-projection-review/sixth-semantic-review-v1.json)确认候选事实与证据身份；说明中的e7 list预期数已按原receipt更正为3／实际列出9，原运行事实一直保留正确3／9。见[更正后的冻结说明](../../_tmp/20261115-iface-sixth-preparation/SIXTH-REVIEW-NOTES-v3.md)。原27条大事实引用policy保持不变，新记录只引用完整原receipt的path与SHA，不在每行复制manifest，也不裁剪完整事实。正式入口接线、公开注入和联合实测继续分开记录。代码解读见[原owner释放、消费信用与SDK观察](../../docs/iface-integration/kernel-release-demand-sdk-ee4c.md)。

P0来源固定、目录刷新、P1三表维护的管理接受由root另批审阅；其记录数和接受状态以独立收据为准，不混入上述代码批的产品运行通过数。管理同步回读只确认数据与关系一致。

第五批[历史最终回读收据](../../_tmp/20261115-iface-fifth-preparation/sync-final-links.json)为 `PASS_READBACK`：**13条合同、576条任务及变更记录、443条验收记录，共1032条**。576条物理任务记录分为26条交付任务、463条上游处置、74条Kernel变更、13条产品变更；该批新增5条Kernel变更和90条验收记录，没有关闭交付任务。完整账本SHA256为 `7b9009046fba96a5f5f7480b611a9ee1dd85cf5977fe21fc53fd0cbe67ce6c08`。原353条验收记录、六张旧表及既有首次失败保持原状；26交付分母和旧后端127分母各自独立。

第五批[独立显示语义复审](../../_tmp/20261115-iface-fifth-terminology-independent/display-semantic-review-final-v4.json)已关闭5类问题、7条相关记录，明确API消费者检查、零用例文档命令、受影响范围与完整矩阵、Mac实际dirty输入及配置/产物身份的区别。显示审查、源码审查、构建成功、实际运行和管理同步分别记载。第五批表内证据截止到 **Kernel `5d6d4569`及当时Mac共享driver/factory的108项dirty输入验证**；`856`至`ee4c`与dirty2ac共享包结果已在第六批单列，不与第五批拼成同一组合通过。第五批代码解读见[HTTP准备来源与实际清理](../../docs/iface-integration/kernel-preparation-cleanup-5d6.md)。

第五批同步保留了两次首败：第一次请求超过已观测的5MiB请求限制；按字节分批后，27个新run的大事实字段在CLI直接回读中为空。具体丢失位置未确定，不能据此宣称官方单格阈值。经[独立审查](../../_tmp/20261115-iface-projection-review/final-frozen-review-v3.json)，只更新这27条的“事实JSON”及随之重算的“行SHA256”；事实列引用完整Git事实的稳定ID、canonical SHA256和字节数，其他文本列保持原值。完整事实未裁剪，旧353条投影未改。定向修复及最终关联写入分别全量回读；原失败收据、journal备份和root逐项处置依据保留，未重建已存在记录。表中任何PASS仍只适用于对应运行范围。

## 离线检查和投影

从仓库根执行，批次产物放在新 `_tmp/YYYYMMDD-.../` 目录：

```sh
python3 scripts/iface-integration-status/extension.py check --ledger state/iface-integration.json --feature-metadata <功能元数据原件>
python3 scripts/iface-integration-status/extension.py schema --out <批次目录>/schema.json
python3 scripts/iface-integration-status/extension.py project --ledger state/iface-integration.json --fact-references state/iface-projection-fact-references.json --feature-metadata <功能元数据原件> --out <批次目录>/projection.json
python3 scripts/iface-integration-status/extension.py plan --projection <批次目录>/projection.json --remote <批次目录>/remote.json --out <批次目录>/plan.json
python3 scripts/iface-integration-status/test_status.py
python3 scripts/iface-integration-status/test_payload.py
python3 scripts/iface-integration-status/test_fact_cells.py
```

当前投影必须显式携带 `--fact-references state/iface-projection-fact-references.json`。该policy固定27条引用的完整事实哈希，并保护旧353条事实；每次投影重读完整账本核验，不删除或缩短原事实。未来追加运行不会改写已有引用。单格50,000 UTF8字节、每请求4MiB及200行是本地写入预算；单项超预算停止，不截断内容。旧已验证大格只读保留，不能以预算修复为理由重写。

`repair_fact_cells.py`是本批缺格事件的受限修复工具，不是普通同步的自动恢复开关。它绑定原投影、未变账本、完整before快照、27个recordId及每行哈希，只更新事实引用和行哈希，并持有同一写锁与独立repair journal。其成功不会自动清除原write journal；root必须在确证原创建身份及修复后内容后另行追加明确resolution。原未知写入守卫、失败收据及操作备份始终保留。

编辑前保存旧账本，随后使用 `check --previous <旧账本>` 核查记录没有删除、验收原件没有改写。`--verify-evidence-root <仓库根>` 会实际读取并核对全部证据文件 SHA256；普通检查只核字段与引用，不声称已读取证据。验收原件可以位于隔离批次目录；不得把凭据或用户正文入账。

任务状态 `null` 表示未核实，其余状态为 `planned / in_progress / needs_verification / needs_integration / delivered / blocked`。已交付需要明确 reviewer、reviewedAt、scope、appliesToVersions 和 accepted run IDs；每个验收用例都有 PASS，版本适用范围一致，前置交付关闭。一个局部 PASS 不关闭整项任务。`verificationKind=runtime` 的任务必须有实际执行记录；离线管理测试或证据审阅不能替代它。

运行记录中的 `actualVersions` 永远保存原受测身份；跨版本继承使用 `executionKind=evidence_review` 和单独的 `inheritance.appliesToVersions / scope / reason / sourceEvidence`。不得把继承说成重跑。`NOT_RUN` 不登记实际受测版本、完成时间或通过。原失败记录不可变，后续修复另加记录，首次失败由原记录生成。

构建记录使用 `executionKind=build`，管理工具与飞书回读使用 `management`，均不能关闭 `runtime` 任务。历史源码或二进制身份缺失时，以 `identityStatus=UNCONFIRMED` 和明确原因保留；禁止拿日志哈希冒受测产物。当前文件已被后续构建覆盖时，其事后 SHA 仅放 `observedArtifacts`，不回填为历史实际版本。未保存的开始/结束时间保持空，`recordedAt` 仅表示补录时间；未封存的 argv/config 也显式标未确认。包含未知身份的证据不能关闭交付。

## 独立绑定与真实关联

历史初次建表时，由root核实CLI建表API，创建三张表及TEXT字段，取得实际table IDs后创建原六个单向LINK字段。当前沿用已核实绑定并包含批准的扩展字段与“关联交付任务”关系；不要重新建表或用初始字段集合覆盖当前绑定。`schema` 输出逻辑字段及目标表类别；不猜数值类型。关系如下：

| 来源表 | LINK 列 | 目标表／账本字段 |
|---|---|---|
| 合同 | 接入任务 | 任务／taskIds |
| 任务 | 接入合同 | 合同／contractIds |
| 任务 | 前置接入任务 | 任务／dependsOn |
| 任务 | 验收执行 | 验收／runIds |
| 任务中的变更／上游记录 | 关联交付任务 | 原26条交付任务／taskIds |
| 验收 | 接入合同 | 合同／contractIds |
| 验收 | 接入任务 | 任务／taskIds |

辅助文本列中的稳定 ID 不冒充飞书关系。同步先创建文本记录，完整回读取得真实 record IDs，再计算并写 LINK，最后按 ID 集合回读。不会按名称或最新版本猜关系。

绑定放在 Git 元数据目录下，例如 `git rev-parse --git-path iface-integration/binding.json` 所指位置，不入库。绑定必须显式给出：

```json
{
  "schema": "iface-table-binding/v1",
  "projectId": "pilotdeck-iface",
  "baseToken": "实际主Base token",
  "schemaConfirmedByRoot": true,
  "protectedTableIds": ["旧表1", "旧表2", "旧表3", "旧表4", "旧表5", "旧表6"],
  "tables": {
    "contracts": {
      "name": "ifACE 接入合同与能力映射",
      "tableId": "新合同表ID",
      "fields": ["schema中该表全部TEXT与LINK列名"],
      "linkFields": {
        "接入任务": {
          "link_table": "新任务表ID",
          "bidirectional": false,
          "targetKind": "tasks",
          "sourceField": "taskIds"
        }
      }
    },
    "tasks": {"说明": "填写对应实际表身份、完整fields和当前四个linkFields"},
    "runs": {"说明": "填写对应实际表身份、完整fields和两个linkFields"}
  }
}
```

示例不是可执行绑定。示例仅展示历史初始字段，当前绑定必须使用扩展后实际schema。所有六个旧表ID必须不同且与三个受管表分离；LINK目标也只能是本组三个受管表。首次绑定前要核实真实表名、字段类型、LINK 目标和单向属性。运行时重新检查实际字段集合和 TEXT/LINK 类型。

## root 同步流程

`root_apply.py` 通过 `--engine` 显式复用已有 `scripts/project-status/project_status.py` 的 `Lark` 类。当前产品冻结分支未携带旧管理脚本，因此第一次可指向 `/Users/da/ws/pilotdeck-rs-cc2c/scripts/project-status/project_status.py`；其他机器须提供这份已核实管理引擎。引擎封装 `lark-cli base` 的完整分页、错误响应、被忽略字段和大请求处理，不通过复制旧账本带入历史状态。Python 仅用标准库。

```sh
python3 scripts/iface-integration-status/root_apply.py export --engine <已核实project_status.py> --base-token <主Base> --tables <独立binding.json> --out <批次目录>/remote.json
python3 scripts/iface-integration-status/extension.py plan --projection <批次目录>/projection.json --remote <批次目录>/remote.json --out <批次目录>/plan.json
python3 scripts/iface-integration-status/root_apply.py apply --engine <已核实project_status.py> --base-token <主Base> --tables <独立binding.json> --projection <批次目录>/projection.json --plan <批次目录>/plan.json --out <批次目录>/preview.json
python3 scripts/iface-integration-status/root_apply.py apply --engine <已核实project_status.py> --base-token <主Base> --tables <独立binding.json> --projection <批次目录>/projection.json --plan <批次目录>/plan.json --out <批次目录>/sync.json --execute
```

默认 apply 只是预览，写入数为零；只有 root 加 `--execute` 才写。执行前重建投影、重读远端并核对计划，执行期间每次写前检查来源漂移。未知行、重复 ID、缺身份、未知字段、非完整快照、验收原件改写都会停止。写操作仅允许受管三表的批量创建和更新，没有删除、自动 schema 修改或旧表写入路径。

绑定旁的 `*.write-journal.json` 持久记录每次不确定写入；换一个收据文件不能绕过它。请求发出前先落 journal；超时或未知回执后停止，必须先回读。只有全部对应记录及值已实际可见，才允许新计划继续；仍不可见的创建不能盲重试，需 root 结合服务端历史明确处置。不要删除 journal 来消除错误。执行期间使用独占文件锁，防止两次同绑定同步并发。

每次快照都保存旧六表记录数与内容哈希，写后必须保持一致。外部并发修改也会导致计划漂移，需要重新审阅，不自动覆盖。最终文本和 LINK 均无差额才记 `PASS_READBACK`；无变化记录 `NO_CHANGE`，不冒充新同步或新产品验收。首次失败收据保留。

历史首轮已有四个filter/group视图的回读记录；它们只证明当时配置。当前更新是否包括某个视图或页面布局须按对应回读收据说明，不能由数据同步推导未核实的仪表盘或布局也已完成。

### 上游处置与 ifACE 变更追踪

任务表的「记录类型」现分为「交付任务」「上游增量处置」「接入变更」。后两类来自独立数组，使用稳定 ID，在同一物理表呈现；它们不是交付任务，不能作为前置任务，也不加入原 26 项交付分母。汇总分别计数。「关联交付任务」是单向关系，仅可指向原交付任务；前置关系仍仅接受原交付任务。记录保留上游精确版本和路径、审阅层级、处置理由、实现与实测状态、合入状态、可核实的提交和实际验收记录。路径清单、路径及依赖清单与生产语义审阅是不同深度，不能把清单处置理解为语义审计通过。上游接收决定、代码实施、本地候选吸收、实际测试、远端推送与main合入分别记录；空提交或未核实不能理解为已交付。

投影中附加来源文件也按 SHA256 校验，每次写入前重新生成投影。历史 `root_apply.migrate_journal` 仅处理早期「Kernel实现」更名为「能力实现」及既定新增字段，不能用于本轮功能元数据结构迁移。本轮使用下述独立 schema 流程，要求旧绑定与 journal 精确匹配、待核实操作为空、表身份和既有关联不变。保留原 journal 副本与全部 attempts，并记录哈希迁移；禁止通过新绑定绕过不确定写入。


## 功能身份、分类和独立适用性

当前维护链在原三表、原表ID及原稳定ID上增加字段，不增加交付任务或虚构运行。任务表的“记录类型”固定由原集合派生：`tasks`为交付任务，`upstreamDecisions`为上游增量处置，`kernelChanges/productChanges`为接入变更。“变更性质”是独立必填多选分类：ifACE公共接口、Kernel实现、PilotDeck能力适配、产品组装与入口兼容、测试与维护工具。作者必须逐条给出分类及依据，投影不按仓库、失败结果或合同猜标签。

功能身份保存在独立 `iface-feature-metadata/v1` 原件，含 `features / records / associations` 三数组。账本可通过 `featureMetadata={path,sha256}` 引用，或在命令中显式提供 `--feature-metadata`；两处均提供时必须指向同一SHA绑定原件。旧账本仍可执行历史校验；当前新schema投影必须带完整元数据。`records`精确覆盖原26交付任务和全部上游/Kernel/产品变更，`changeNatures`与`classificationBasis`必填，完整解释保存在“分类依据JSON”。每条分类及功能关联须以`recordSha256`绑定本次ledger中原记录的canonical全行；每个显式runId另以`runFactSha256`绑定完整原run，保留版本不变但更换命令/结果/证据也会拒绝旧关联。`sourceLedger`是独立历史来源原件，不用其rawSHA制造ledger描述符循环；增加无关运行不会自动扩大已有适用性。元数据自身的证据可用`check --verify-evidence-root`逐文件校验。

每条`associations`明确指定功能ID、collection/recordId、具体`relatedChangeIds`、proofKind、proofScope、appliesToVersions、原件证据及reviewer/reviewedAt。三表均投影可筛选“功能ID”、“功能关联JSON”、“关联变更ID”，并用“关联接入变更”LINK指向原任务表的具体Kernel或产品变更。共享合同不会自动给其他记录添加功能关系，也不会扩大证明范围、改变合同审计或关闭交付。关系不是一次新运行。

`executed/build/management/evidence_review`关联须明确原runIds且类型一致，版本对象须逐项精确来自原actualVersions；验收表记录只能关联自身运行。历史UNCONFIRMED身份可以如实关联，但须`observedOriginalIdentityOnly=true`，保留原reason/scope，不能回标当前commit。跨版本源码适用性使用独立`source_review`关联和明确依据，不能把旧运行改成新版本运行。原run事实JSON、实际身份与首次失败保持不变；新增独立投影列会使行SHA改变。

“本地提交”、“实测状态”、“远端可见”、“目标主线合入”分别表示不同事实。`branchIntegration.remote/targetRemote`必须为实际Git URL，`remoteName`另列；Kernel目标分支为master，PilotDeck为main。PilotDeck本地origin接管库与上游来源库须分别保留URL，不能只按origin名称当同一库。未知可见性/合入保留UNKNOWN及原因；远端VISIBLE须明确refs和观测证据，NOT_VISIBLE须完整远端refs检查，目标分支非祖先不能自动推出远端不可见。MERGED/NOT_MERGED须给出精确候选commit、观测目标commit、时间、证据和祖先关系。历史事实中的旧mergeStatus字面保留，当前“合入状态”依据此独立分支观测投影，不写死远端main已合入。

```sh
python3 scripts/iface-integration-status/extension.py check --ledger <冻结账本> --previous <旧账本> --feature-metadata <功能元数据原件> --verify-evidence-root <仓库根>
python3 scripts/iface-integration-status/extension.py project --ledger <冻结账本> --fact-references state/iface-projection-fact-references.json --feature-metadata <功能元数据原件> --out <批次>/projection.json
python3 -m unittest discover -s scripts/iface-integration-status -p 'test_*.py'
```

## 原三表的受控结构迁移

此流程只支持从0042维护脚本的已确认schema新增本轮20个字段：三表各三个功能TEXT及一个单向LINK，任务表另加八个分类/分支TEXT。不会删除、更名、改变旧字段类型或重建记录。目标绑定仍是原三个table IDs；旧六表ID和全部旧LINK目标保持。`schema-target`生成的目标绑定默认`schemaConfirmedByRoot=false`，需root对照批准schema复核后确认；它不是实际字段已存在的证明。

```sh
python3 scripts/iface-integration-status/root_apply.py schema-target --base-token <Base> --tables <binding.json> --out <批次>/binding-proposed.json
# root复核目标schema后将候选的schemaConfirmedByRoot确认为true；原binding此时仍不改。
python3 scripts/iface-integration-status/root_apply.py schema-export --engine <已核实引擎> --base-token <Base> --tables <binding.json> --new-tables <批次>/binding-proposed.json --out <批次>/schema-before.json
python3 scripts/iface-integration-status/root_apply.py schema-plan --base-token <Base> --tables <binding.json> --new-tables <批次>/binding-proposed.json --before <批次>/schema-before.json --out <批次>/schema-plan.json
python3 scripts/iface-integration-status/root_apply.py schema-apply --engine <已核实引擎> --base-token <Base> --tables <binding.json> --new-tables <批次>/binding-proposed.json --before <批次>/schema-before.json --plan <批次>/schema-plan.json --out <批次>/schema-preview.json
python3 scripts/iface-integration-status/root_apply.py schema-apply --engine <已核实引擎> --base-token <Base> --tables <binding.json> --new-tables <批次>/binding-proposed.json --before <批次>/schema-before.json --plan <批次>/schema-plan.json --out <批次>/schema-receipt.json --execute
```

迁移使用与普通同步相同的binding写锁，读取实际field-list的字段ID、类型、LINK目的地及单向属性，并封存全部原记录和六张保护表的内容/结构哈希。批准计划绑定完整before快照及新旧binding SHA。每次仅创建一个批准字段；派发前持久化同binding的`.schema-journal.json` pending，字段实际回读后保存fieldId及完整属性。每步均检查旧字段ID/属性、原record IDs/值、旧六表以及binding/原write journal没有并发变化。新增字段若已出现非空并发内容也会停止。

原write journal必须存在且pending为空；独立fact repair若未验证也阻断。任何字段创建结果未知或部分成功都保留schema pending与原失败收据，禁止换收据盲重试。即使服务实际已创建字段，本工具也不自动重建或猜成功；root须读取服务和原件后另行明确处置。普通记录同步也会检查未解决schema journal。成功schema journal保留三表全部`verifiedSchemaFields`，后续普通同步开始、每次实际派发前及最终再次读取实际field-list，精确核fieldId及属性；同名同型字段被替换也会阻断。全部结构与原值回读通过后，才原子写每个本地文件来更新binding和原journal的bindingMigrations；原attempts、resolutions及历史条目完整保留。每次本地原子文件保存都会fsync临时文件、replace后fsync父目录；任一步失败均停止。两个文件无法跨文件原子提交，若中断造成binding/journal不匹配，会保持阻断并要求root处置，不能据此绕过未知状态。

结构迁移不写任何记录值。迁移成功后，按正常export→plan→preview→apply流程写分类/功能投影并解析真实LINK record IDs，最后全量回读；它与schema迁移分别有独立收据。上述操作都只由root执行，脚本测试只使用离线FakeClient。


已完成旧结构迁移的接续：同路径旧journal若为`iface-schema-migration/v1 / PASS_READBACK`，仅在四项已知历史操作全部`VERIFIED_READBACK`、当前binding与历史newBinding及原write journal迁移链一致、原16个after字段对象与实际field-list逐ID/属性一致、六保护表before/after内容哈希相同且匹配当前快照时允许接续。旧格式没有pending字段；未知或部分完成不会因缺字段被当作成功。`schema-plan`将原journal字节SHA加入批准计划，因此安装此兼容修复后须重新生成schema-plan，再做preview；已封schema-before仍须与实际读回完全相同。

实际执行会将旧journal的完整原始UTF8、完整JSON对象及原始字节SHA嵌入新schema journal和新receipt的`predecessor`，随后按原fsync规则持久化；dry-run不改旧文件。此保留方式允许逐字恢复旧证据，不删除历史。新格式STARTED/UNKNOWN/部分完成仍阻断，不会回退引用其成功predecessor来绕过本次未知写入。旧保护表当时未保存schema SHA的部分只证明当时内容；本次schema-before另行固定当前结构哈希，不把新hash声称为历史已保存。
