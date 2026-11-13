# 2026-11-14 项目决策

## 主协调自主决策与模型分工

用户明确授权原文：

> 有了表格之后，是不是可以更高效率的用 6.1 sol 和 6.1 terra 子智能体来并发了？ 当然关键难点，也可以用 Astra 来攻关。或者你有更好的安排？ 项目所有决策你都可以自行决策。

主协调据此自行决定项目内技术取舍、排程、实现与有证据支持的兼容性处理；不再重复请求例行项目决策审批。保留固定基线、StaffDeck 延期、当前后端及非 TUI 范围、真实验收和首败记录。接受差异仍按明确原因、精确范围、独立审查及追加冻结登记处理，不为变绿放宽标准。没有因此执行推送、正式发布、生产部署或删除外部资源。

Sol 承担边界清楚的实现、取证与验证整理；Terra 承担复杂接线和独审；Astra 定点攻克难归因的并发、取消、恢复问题。作者与独审分离，公共文件唯一 writer，双平台各一 heavy，避免并发构建挤压资源。首个新 Astra 子任务为 Original53 deadline 两场景终态根因，只读分析已有实际原件。

## Original53：本批 deadline 宿主竞态的精确 accepted 汇总

主协调于 2026-11-14 基于本会话用户“项目所有决策你都可以自行决策”的明确授权，决定正式登记以下有限比较适配。此处是授权范围内的主协调决定，不冒称用户逐字批准了这一具体规则。旧 #6 对基线缺陷的批准只作背景，不能代替本次机器规则的独立审查。

依据：Astra重新核固定TS源码与20份真实raw，均有相同 unknown/DEADLINE_EXCEEDED、fail-closed、公开failed终态、模型请求/响应/usage；只有3格在timeout尾部多一条TS宿主catch生成的gateway_submit_failed。原报告`_tmp/20261114-original53-deadline-analysis/review.json` SHA `ec3ec95ef341fea3272a4d3916072e81c67077cd4f05d5f8430ec2b90fcf6c8b`。Rust sidecar不改成cancelled，不加延时；Rust宿主既有terminal claim保护另做实际验收。

批准范围仅本批登记的20份原件、`deadline`与`deadline_during_tool`、固定cc7 TS-host拓扑。先在未改raw上执行原identity/health/oracle/单侧E/partial-order；仅当唯一差异流为parent/B、最多一个额外状态且完整文案/字段/身份/尾部位置精确成立时，在独立accepted视图将其视为可选。operation unknown、公开terminal、原值model request/response/全部usage、工具生命周期及副作用快照继续严格核验；其他所有通道和G03、pair E10/E11全部严格。原sequence不重编号，原trace不改写。

独立Astra按最终module `f03387d3e164dfaa8f3319fe89a849b6962f8a22df55710cdd843ca014740334`、ready `8d787685c71fa54e1baec3a378bcffd309cd34459a2a8c1145dcd058cceb56a8` 实际复验120正控/52负控；原30个strict FAIL仍并列，其他51项×5轮的255个compact逐项等于原score。负控区分完整入口身份层与真正内层内容变异；已修删invocationId漏拒和覆盖已有输出问题，首版本保留。独审最终收据SHA `286cb1c2ef5ac8a0cebb12b7c54b9a0b65bba9ea2ea07886958ddc3579ecaf1b`。

准许新增独立accepted汇总、精确差异条目和必要冻结登记；不改原score.py/G03算法、53分母、5轮要求、TS参考池或原strict `50/53 FAIL`。accepted汇总须与strict并列，预算失败仍保，不能因这两场景适配声称53/53。公共接线须再次独审并在固定源码上运行，私有通过不直接冒正式评分。

未来批次不自动获准。后续使用固定规则引擎、不可变批次manifest和根审核的显式登记；每批source/bin/TS5/Rust5/ledger/日期/driver/catalog/adapter/strictscore均独立绑定，新manifest由根核真实证据后追加登记，不能接受任意外传manifest自授权。现raw缺完整transport wire、fence时间戳和最终采样后的无限期副作用观察，不把这些未知说成已经通过，也不豁免对应产品验收。

## settings-update：固定三原件的配置监听及版本计数差异

主协调基于本会话用户授予的项目自主决策权，决定登记本批精确差异。此决定不借用旧 A6.3 或场景既有 regex 的批准状态，不冒称用户逐条批准。先保留原公共比较的 DIFF 2；绕过旧 accepted 过滤的原归一化比较实际为 DIFF 3，三处全部明确列在本决定中。

只适用产品 `d67705eece5a035b9f689809bf5b4088755ebcb8`、固定 TS `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`、采集装置 `644a8d80a45916bbe7a0b0ee93ff6e331a9dbf98` 的三份完整原件：TS1 SHA `470a8eda78ffb832e517152e340cc95cc735e3359c877107c7b3a0fa96daa228`、TS2 SHA `c855e606c8b817a008ea3dea8f82460cd5f63d0e87f5bd88c21f4742d245a0ff`、Rust SHA `6efc3c541a8cbd661b1244c73567f6cce24b7fcdb0a94b42e7053d602d734ba0`，路径见 `_tmp/20261114-watch-exact-policy/exact-diff-draft.md`。未来采集、其他候选或相似现象不自动适用。

精确允许：① `$.steps[25].frames[0].result.version` 为 number，TS 两侧值均 9、Rust 值 10；② `$.steps[27].externalConfigRewrite.observationExpired` 在 TS 两侧为 true、Rust 不存在；③同一对象 observed 在 TS 两侧为 false、Rust 为 true。任何其他值、类型、对象或步骤不放宽；原始值、完整有序内容及身份锁不因接受而移除。

监听差异依据：三侧均在原有文件同 inode 只做一次 4096→2048 原地写入；TS 两侧 30 秒窗口未见目标通知，Rust 约 309ms 收到完整目标 config_changed，并以关联快照证明 sourceHash 和版本推进；下一次唯一 watch-rewrite 模型请求均使用 max_tokens=2048。固定 TS 对文件本体一次 fs.watch，而 update_settings 原子替换文件后没有重挂，结合其自身 watcher 测试的通知承诺，构成基线缺陷依据。旧 inode 导致失效是有源码支持的推断，原件未捕获注册 inode/内核事件，不伪称逐事件因果证明。三侧顶层 notifications 总数均 4；不声称 TS 永远不通知或内部快照一直未变化。

版本计数另行依据：step25 的 9/9/10 发生在 step26/27 外部改写前；三侧 snapshot 除版本和 loadedAt 外一致，完整配置、contentHash 与 env/default 来源一致，下一次写 4096 后捕获的版本各恰加 1。缺少 step9→25 每次重载的原因轨迹，不能指认某次 watch 回调；不以随后外部改写解释既有计数差，也不以主观因果推断扩大接受范围。重建 step25 source 字节与当时来源 SHA 相同的证据明确是重建，不冒充直接采集。

私有验证及非作者 Terra 独审：原件 self SAME、三侧 validateArtifact 通过、Rust 实际正控通过、两个 accepted 正控和 77 个负控全部通过；原模块 SHA `e11547d9f2e5f96f7043b14bc7a83a6df2dc9b0e40729d58e45b2094016978cd`。额外 version/type/缺失/其他步骤/source/config 20 个直接语义负控全部拒绝，1 个正控通过；独审补证 `_tmp/20261114-watch-independent-9wyVLQ/step25-run-wBUUXx/evidence.json` SHA `a9ec7c15cffc4489ed679bce0fd67f95d2a9cbbc74d17e8b7e04c385925a2515`，作者补证脚本 SHA `674ffd27dde9e1ee5734270fe8d2956a2cbf18cdd226d7ae427f9f5731c62061`。

准许最小公共迁移、新增精确差异条目与必要冻结登记；公共接线须重新独审，并在固定源码上重判原件。全部真实写入字节、sourceHash、快照、完整通知与归属、请求/响应/usage/落盘链及公共 id 仍严格，不删除或放宽旧场景规则，不修改 Rust 行为或原件。私有通过不代替公共固定源码结果，也不把此固定旧候选证据转记为新候选或整模块已交付。

## 性能目标与收口优先级

主协调重新核本轮正本 `docs/plan/backend-completion.md:240,248,273` 及非作者范围审计 `_tmp/20261114-package-remaining-audit/scope-note.md`（SHA `deb3f949d381b2f804ccd1448f88f73797883bf0b23b563d4031b0e06c664999`）。硬要求是同扩展集合、真实安装体积/启动分布/全进程树内存/排除网络后的开销比较与完整性能报告；缩小部署、降低内存、避免执行退化是目标，未达指标必须报告。未见本会话明确把 full 小于 TS 另定为硬阈值，不能自行把改进目标升级成无限阻断交付的门。

因此暂停进一步 source archive 压缩探索，优先产品真实失败、完整必需能力与最终组合验收。已完成 rg/librsvg 无损转换保留并按其真实新prefix身份参与后续包装；未重新离线构建的限制继续保留。原 full 1,168,781,633B、两项净减54,459,368B与332,579,305B、算术余量781,742,960B及TS295,973,079B不变；算术估计不是新包实测。最终包仍按原完整分母计量和如实报告，不能删源码/许可或把估计写成达标。剩余体积优化属于明确未达目标及后续工作，不因本决定删除或改绿。

## 外部 HTTP 模型：修复宿主物化时的基线崩溃

主协调按用户自主决策授权及意向书“崩溃、与类型矛盾的基线 bug 默认修”作出精确实现决定。固定 cc7 基线的真实回环 HTTP prepare、原 createAgentTurnCapabilities、原 createRouterRuntime 和默认 RouterRequestMaterializer 已实际串联：外部 prepared 无 opaque，原桥实际传入 undefined，读取 requestPatch 抛出 TypeError。证据 `_tmp/20261114-external-materializer-baseline/esm-second/result.json`、原件身份及回收收据保留；POST 一次、native dispatch 零。此证据仅覆盖真实生产 factory 的最窄桥调用，未冒称完整 AgentLoop 预算 turn。首个 .ts 探针的 CJS 装置失败也保留。

仅当生产选择的是外部 HTTP modelProvider 时，装配独立 prepared-execution materializer，采用固定基线 `src/agent/loop/AgentLoop.ts:2508–2530` 已有 fallback 语义：完整保留 prepared.request 的 provider 专有控制、prompt、tools、输出上限等字段，只以候选 messages、cacheBreakpoints、cachePlan 更新窗口与缓存，并以 prepared.provider/model 保持路由。候选缺少缓存字段时不残留旧缓存。外部 opaque 只供外部执行身份使用，不交给 native RouterDecision；此修复不新增远端 materialize POST，不自动暴露生产 wrapper 原本丢弃的可选远端 materializer。

宿主原配置 metadata、sticky 行为、durable 记录、权限、native router 物化、stdio/TCP 协议字段及错误投影均不随本决定放宽。正常 prepared 身份仍贯穿 prepare、预算和 stream；所有非此崩溃的差异继续核验。实现由 model 作者完成，Astra 非作者复核，集中候选须实际验证保持字段、缓存替换/缺失、身份及无额外调用，另核 context 投影在 native/stdio/TCP 的边界。当前是允许实现并验收的决定，尚非测试通过或交付声明；公共差异条目及适用冻结登记须随实际验证证据补齐。

## LSP：绑定来源读取与 prepared cwd 两项精确基线修复

主协调根据用户项目自主决策授权及既有基线 bug 默认修规则，正式接受两项分别登记的修复：`T1-LSP-BOUND-SOURCE-READ-001` 与 `T1-LSP-PREPARED-CWD-001`。前者限自有 Unix native LSP source，使用原 workspace 目录描述符逐段 NOFOLLOW、有界读取并保持合法根内链接；后者限准备器返回不同 cwd 时执行完整 prepared command，同时保留原 LSP workspace URI/name。不得借此扩大工具权限、改变普通 LSP 结果或放宽完整场景评分。

固定 TS 的实际 owned canary 竞态原件、prepared cwd 首次 FAIL wire、原类型合同和源码均已经作者与非作者复核，原首败及取证汇总 turn 被工具终止的历史保留，不重采安全 probe。非作者最终复核 `_tmp/20261114-lsp-diff-independent/review.md` SHA `f1d7bf5a9992c0a3e7dc803a4945f2dbb3e5068d23cb165461b3b12c94556453`，36份原件及36/37/38相关源码对象SHA均核同。36双平台七项安全读取与一项真实prepared child控制实际PASS，整批Mac557/562、Linux558/563的五个生命周期首败不抹除；37双平台7/8与38双平台仅更正一项1/1另列，不合并伪称重跑全绿。

准许只追加这两条公共差异记录，原冻结判据与历史差异文件不改，不产生整场景忽略规则。非Unix、自定义/guest端口、所有中间目录竞态排列、并发增长屏障、hardlink/mount隔离及最终发行组合的未验范围继续明确保留。该决定接受有证据的窄修复，不能将其解释为LSP整模块或本期交付完成。

## 外部模型物化修复的42实际验证与登记

42 `4d42d66005a71e3bd46a233c6dc19adadc282887` 已在macOS/Linux实际完成model14/14及真实Handler context投影控制PASS，S1独立逐名核原件；Mac最终收据SHA `de579e76556de9e827f826a9586d26a0c678cb507d32d709ddaaeac93a689a02`。根据此前精确决定，准许正式追加 `A7-EXTERNAL-MODEL-MATERIALIZER-001`。原工厂桥崩溃事实、prepared字段保留/缓存替换/无native opaque解引用及零远端materialize请求边界不扩大。

42整个受影响批次双平台仍117/129 FAIL；发现的外部model临时next future被durable Pending轮询丢弃属于独立P1，不被接受差异豁免。43仅该stream所有权及必要fixture/oracle修复，仍须同源实际验证。本次登记不把两materializer函数控制当成完整Gateway预算turn或三拓扑验证，不改原判分实现或FROZEN，不重新请求已经得到的项目决定授权。

## 产品44与正式判据82a的统一候选登记

主协调按用户本会话全部项目决定授权，将产品44 `1ab5904c78c95553822ffa846fc7feb7ee7b0d57` 与已经正式登记的判据82a `82a1010b09fcd88ebf5ee69a3b7f15ce0817e3b9` 合为同一个后续验证候选。使用临时index逐文件导入，不用harness整树覆盖产品；产品44独有差异条目、源码、发行内容与历史首败保留。44两平台受影响203/203实际通过，不代替统一候选的最终全门、发行和真实拓扑。

`specs/parity/FROZEN.md` 和 `multimodal.md` 逐字采用82a已登记对象，对应用户已明确批准的Original53单类/父关闭及必要重新冻结、此前root精确deadline决定与冻结事件；不重新解释或扩大其适用范围。SDK `D1-SDK-SIGNAL-ABORT-ORDER-001.md` 与对应交付文档也逐字采用82a正式记录，保留44旧产品snapshot供追溯。这是把本会话已接通并验过控制的正式harness记录纳入统一候选，不是重新修改该正式记录或新增可接受差异。其artifact与同binary独立8-wire两层证明均必需，既有一处文案纠正不把没有RPC原帧的artifact冒称直接观察RPC；同run终态、恢复与全部协议检查均保持。用户的SDK精确补充授权和root既有接线决定不再重复请求。统一候选应对canonical82a及所需冻结登记基准分别核验，不能因两个临时分支的旧快照不同而把已批准记录当新许可。

源码闭包审查发现5个模型正式夹具未在产品44树内，全部已经在82a有固定Git对象且与工作区逐字一致，随本次一并纳入；它们是验收装置，不能据纳入推定通过。两个决定文件按root最新原文保存，管理三表与实时交接独立维护，不从旧管理树覆盖当前状态。

## Original53：45批唯一 deadline 的新原件精确登记

主协调依据用户本会话“项目所有决策你都可以自行决策”的明确授权，核对本批实际原件、独立源码及语义审查后，批准下述新的有限接受范围。这是用户委托范围内的主协调决定，不冒称用户逐字批准了此具体规则，也不把27批旧许可自动沿用到新原件。

只适用45产品 `e99ee172e25c29c34ce299f03808773f16f2ec59`、tree `abbe2740ec0e78a6c929d19ec8276ea744308e4d`、Mac debug sidecar SHA `566b54f3d75054b94d66bf48a8bb40fbe5873e0d631c6e79590124f07fe50341`、固定cc7宿主的本次已完成5TS＋5Rust。原正式score SHA `4e47f9cbcc9d961a1a32a134b4c6578463c32c8e40425d1eaf912b65ff759798` 保持 valid_score=true、52/53 FAIL、refused空；预算场景与deadline_during_tool均严格通过，不获得新豁免。唯一许可为登记10份deadline raw中的Rust r2/r5原index/sequence 24的两条完整 `gateway_submit_failed` parent/B记录；原行不含换行SHA分别 `c2aecace83735066844c4e9773df571bbced9c6eae48dfacc21bbfd0c5e0f436`、`0b43bafdeef8ffc39018cf30096fb19244e1202408477a9d06c4d0c25144ff35`。必须逐份绑定raw、ledger、日期、source/bin、driver、输入闭包和完整invocation envelope，其他八份deadline不得借用extra，未来raw、release binary和其他场景不自动适用。

Astra独立核20原件及全部有序比较：deadline100格为68 strict PASS、32仅parent/B/E10/E11/G03差异；仅省略两条已固定原行的内存残余比较100/100成立，原sequence不重排，不对投影视图冒作原health。during-tool100格原strict全部通过，仅作控制。每份deadline均有相同一个未归一化model request、无model response、无tool记录；usage字段真实缺失且严格相同，不解释为实测零token。timeout index22、sidecar_final unknown证明23、后续0已观察副作用、operation `result_unknown/DEADLINE_EXCEEDED/lastAppliedSequence=3` 与terminal `failed/unknown/空output` 全部严格。固定TS abort/持久化/queue关闭与catch当前fence的路径支持同类收尾竞态解释；新raw缺完整内部时序和全部wire，不声称直接测出了竞态时刻。独审原件报告SHA `6adad3bff78a878c3cd8cacd0796a32e859db7231177414a9575c511313c370c`，独立10语义负控全部拒绝。

具体私有候选engine `e93b822b3b19107942a2521beba16f3fcff064f7871539d9ae2c1b6e49bde050`、summary `c0a1854c04501df1b55581883a4656b0e266f45bdd4cdc0e4605721693dbc97b`、registration `7b6b50fcac5f16738572b52871c98f90e1099b801e47c1b22613058bb8a6f2ad` 已经作者控制及非作者实际复验。独立120比较仅20方向格适配，5额外直接语义负控拒绝，既有输出不覆盖；私有summary的其他52场景对象逐值等原score。完整来源677项逐一核SHA：672 capture-root input、唯一managed-root accepted-diffs原ledger input、4 managed-root code。原score绝对键及原值不改，只允许预登记相对文件的同SHA读取映射，不接受外传manifest或caller Map自授权。代码独审SHA `3ab9161c4a26c8282fa619f49c53ecf454bea0189136add41f777d2eb1040ae7`、实际结果SHA `1e0b158b131fb0ebf5edf70dac7030be521b46a4c34a07ac1afd3448f15d4b8b`；旧27公共CLI仍strict50/53、accepted52/53预算FAIL。

准许只新增公共helper、不可变registration、controls、差异条目和独立 `scripts/parity/accepted_summary_mac45.py` 入口；旧accepted_summary.py、旧helper、旧registration及原严格判分文件保持字节不变，不为新批修改FROZEN/原accepted-diffs账本。ROOT/HERE定位、注册状态及固定SHA等机械投影必须列出，并在公共投影上再次独审、运行控制和冻结检查。当前私有accepted53/53不代替正式登记后的运行结论；正式accepted汇总必须与原strict52/53 FAIL并列，不修改原score、原trace、53分母或5轮要求。

## Cron run-now：既有单时钟修复的正式登记

主协调依据用户本会话项目自决授权，正式接受并登记 `O1-CRON-RUNNOW-CLOCK-001` 的既有修复，不改变产品或增加比较器。固定TS的 runTaskNow 先取时刻构造 once.runAt，createTask 又取时刻并拒绝已在过去的schedule；跨毫秒自拒是实际TS1失败和源码共同支持的原因推断，原件未直接记录两次时钟。Rust同次run-now共享一次时刻，普通过去schedule的拒绝保持。

固定三raw SHA分别为 `3c058184dbf71815e0e587aa9668f2c9dcf4a6d164ca67f4d0cef4e4302b5bd4`、`456828ba648355dd1d23d78c96ec76558e557d72374d6bb166a64e8de55cf164`、`99c11e278b14b2b35c15bce39ab08701d690a23f22e6b40a8fe49dad415ecba4`。保留原self/pair DIFF27；仅原TS2成功分支与Rust在现公共比较规则下SAME，不声明原始字节相等或原profile变绿。三份原件经现公共validateArtifact离线验证errors=[]，收据SHA `021cf5edee2fb32d50ddd9a6b9741ecc4c7656a1375455c403ec20b2046107ab`；期望identity来自raw自身，这项收据不能单独证明外部manifest来源。Linux45和Mac46的单时钟确定性测试实际PASS，保留各自source身份。

公共条目SHA `a805e90459dbe3f6f17814a3f5eff895de4dbf992d411286601795cab846578c` 已由非作者final_gate_audit完成语义及最终机械审查。仅追加该条目及本决定，不扩大到其他时钟、未来原件、取消或恢复，不免掉27处派生差异。

## Original53 45批公共入口的机械冻结补充

前述45批决定中的“不为新批修改FROZEN”指不改历史判据、历史说明及已有对象。新独立Python入口与固定registration必须进入现有check_frozen.py生成清单，否则新增装置没有完整冻结身份。主协调现明确授权唯一必要的机械登记：先固定公共五文件，再由原writer重新生成FROZEN连续哈希表，仅新增四个scripts/parity文件SHA行、旧行与表外字节保持；新增精确冻结事件，原accepted-diffs、旧入口/helper/registration/README、score和所有历史事件均不动。新diff条目按既有append-only校验，不纳入scripts清单Change边。

最终公开投影已由Astra实跑120比较（20方向格适配）、47语义负控、11身份负控、2辅助正控全过，新CLI空PATH无Gitexit0、旧CLI整体JSON逐字不变exit1。最终结果SHA `f40371940ed34fa481d3556139506f3277006329c32b1c5a2aa1c87bd35cebff`；新summary SHA `fc1fcf864e5ee5777477b21b04cfa677d873f748020ec00e45aa5ed31fcc9ea3`。第二非作者Terra审查及干净checkout冻结检查须完成后才宣称正式登记结束。此项补充只完善本批许可的可审计身份，不再改变任何接受语义，不把旧原strict52/53 FAIL改写为PASS。

## stdio 原入口：固定三原件的身份表示和标题邻槽次序

主协调依据本会话用户项目自主决策授权，核对原件、固定TS异步标题路径、私有候选及Terra非作者独审，正式接受本批精确表示差异。只适用capture `5f9f752b-e5a8-48ef-b1c3-5b5d74c5e8de` 的原场景 `agent-loop-sidecar-stdio`，三raw SHA分别 `a35485cb027083ad22843a511655f8b46daa733f6572d07606d37c95c721ef90`、`abcaf5c56fb8a8c260568faa7cccb0f859f35efc2ba6dad355975ca6d73dbaa9`、`4438cfe1ccca372721ac71d2b76b44d170c10476afd2e9cf0d437b95d5b8f191`。capture checkout46，实际二进制build45及其两SHA、runtime不变证明、原scene/harness/完整evidence inputs必须逐项固定；不把capture版本混作新binary。

许可仅三种各自类型不同的opaque身份在8个明确journal路径的严格等值别名，以及Rust同turn新provider标题与operation_started在原index3/4的唯一邻槽payload互换。所有原槽sequence/entryId/parentEntryId/createdAt保留；先核完整23行parent/sequence、唯一三journal全tuple、stream、terminal/result/messages/seed、title三副本与projectionanchor，projection原值完全不改且不得引用被交换entry。变换须可逆恢复整个原JSON，之后运行固定原完整比较器，其他业务/provider/内容不豁免。固定TS标题promise未await后继续loop的源码支持此有限异步完成次序，不允许跨accepted/terminal或一般化FLOAT_OVER扩张。

候选SHA `079cd9068ff19df4d5ee9813dfd9ceadd62c11edf0426458ce7d1357d74ea753`、manifest `45ad740416fb2333545d734aeb950322c5f8e791b2727a56d0db3c2617805039`，完整结果 `53b07fabac8e264328a17cdb81e7b2f6177d3f7a944432c456b5fbb13c19a588`。原self DIFF8/pair DIFF69及旧self8/pair157均保；私有派生self/pair SAME。24身份和语义负控拒绝，provider内容变更DIFF1、标题三副本一致变更DIFF3；Terra独立构造视图逐对象核同、六独立关系负控及实际原比较器内容控制通过，报告 `_tmp/20261114-stdio-rebind-terra-review/review.md`。

准许新增独立公共固定manifest/入口及精确差异条目，输出继续并列strict与accepted。迁移只改ROOT/HERE、固定manifest引用/SHA、批准来源和全新输出目录的显式参数，不改原diff、场景、raw或产品；必须再核公共投影语义与实际运行，再纳入Git冻结候选及append-only检查。未捕获的本批sidecar子PID/lifetime和独立wire sequence仍是未验，不由旧拓扑PID或durable lastAppliedSequence10补造。未来原件/其他二进制不自动适用。

## Maintenance 固定首件负控的因果判定

主协调依据本会话项目自主决策授权接受这一精确证据判定：仅 `_tmp/20261114-maintenance-negative-first-review/bound-files.json`（SHA `ac65bbe1b912dfee85fe3ef94fb7e78e0cc4d55c044fa2a779bbd27aa5b32548`）绑定的24个原件，允许以同模型完成、唯一故障marker、同run非final completed后同requestId精确gateway_submit_failed/INJECTED_FORMAL_RESOLVE_FAILURE且无final的因果链，证明旧同步错误传播已被实际检出。原driver先等final、后断言completed，故原等待FAIL和wrapper INVALID完整保留；不是将超时判PASS或修改公共业务合同。

正诊断原driver全部通过，负诊断仅额外恢复旧同步传播，源、二进制与输入身份分开封存；不冒shipping。作者5项语义负控及非作者3项关联/次序负控均拒，root再次以普通隔离Python执行固定checker（SHA `5bb805504239cbaf0bba3fab95c78c3ca4335a5d230c791fb2bf2a057dff04ec`）exit0，收据 `_tmp/20261114-maintenance-root-acceptance/receipt.json`。不适用其他原件、任意timeout或未来运行，不重采、不改超时/产品。

## C1 HTTPS 固定三原件的出站名称与运行时警告

主协调依据本会话项目自主决策授权批准 D1-C1-HTTPS-PRESENTATION-001 的精确范围：仅当前三份capture/24case/268原始读项（非作者固定读集SHA `5cfba6f7b72c97dbcc4fd7d5f9ac10738661884d7637fcca8dba5b0b27ce0602`），八个准确request rawHeaders[4]的Content-Type/User-Agent/iLink-App-ClientVersion大小写，以及TS stderr offset0一次准确SQLite两行。其余键值、顺序、重复、正文、退出、stdout/业务stderr、凭据/配置落盘均严格；Rust不伪造Node提示。原严格self8SAME/pair8DIFF完整保留，非历史六HTTP对象豁免的泛化。Watch独审SHA `bfe16be1b03c26c27c3487174e27b57413d7c9af6fa5a4c38b94d966e53563df`，公共入口必须自锁读集/原judge闭包/buildreceipt，实际负控与独审后统一冻结，不适用未来原件或cron挂起。不是用户逐字审批。

## 内置工具剩余名称的明确责任

固定注册枚举中的agent/subagent/send_message/send_attachment/lsp/search_tools/knowledge_query完整工具合同由既有backend.T1.other-builtin-tools主责承接，依据原“等全部适用内置工具”范围；backend.T1.builtin-catalog负责逐名对账，R1代际、H1路由和X1 LSP服务仅为相关证据。不新增/删除任务或改变分母，不以映射存在代替输入输出、副作用、失败和适用取消义务通过。具体尚缺控制仍逐项保留。

## C1 Cron 固定原件的终端输出后连接未关闭

主协调依据本会话项目自主决策授权及基线挂起 bug 默认修规则，批准 `D1-C1-CRON-HANG-001` 的精确范围。只适用 `_tmp/20261113-e0-resume/c1-cron-collector49-formal-first/cron/` 的三份固定 capture、42 次 TS 与14次 Rust46 实际运行。TS已经输出完整准确业务或使用错误终态，此后实测1500.257–1501.970ms仍 child alive、owned WS open=1、无close，外层随后一次owned PGID SIGTERM清理；这是有界观察，不声称已证明无限不退出。固定TS `pilotdeck.ts:297,363–424,626` 的连接后各return/catch缺close与该观测一致。

Rust14次必须自然退出，六格exit0、八格exit1，实际WS关闭，无signal/timeout。完整stdout/stderr、hello、RPC ID/方法/参数/响应、token与98个实际home/project树继续严格；只接受该已证生命周期差与对应close/cleanup后缀次序差，不接受启动超时、缺终端输出、缩短观察、错PGID、提前清理、Rust缺RPC/response或过早退出。原strict self14SAME但productPassed=false、pair14DIFF及更早collector28INVALID保持原样。

私有ready SHA `91c06134933c7884fb99351f6bdb94d2028b171b4c374282239afef3232dcfae`，固定384原judge读键、63额外原件和4构建收据自锁。非作者S1实际重读全部pin、复跑50控制及3附加负控，结果与作者整文件相同；独审 `_tmp/20261114-cron-hang-independent/review.md` SHA `03aac51e5d66486bfc35bcc578bffc1b09619caa583818f3532bcfd8012c50f4`。允许最小公共迁移、追加差异及必要冻结登记；可信registration须不可变、禁止caller manifest自授权，公共投影另审并实际运行。此决定不是未来capture、其他CLI timeout或新二进制的通用豁免，不改产品行为或原判分器，不把私有候选当正式冻结完成。
