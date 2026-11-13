# 2026-11-13 本轮用户决定

本文件保存明确批准的决定；当前进度仍只维护在 `state/backend-handoff.md`。不以 agent 建议代替用户批准。

## D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001：保留 Rust 完整终态

批准来源：本轮用户对 `request_user_input_async` 的明确回答。

问题原文：

> 是否批准这项精确兼容性差异：在已验证的 CLI 信号取消场景中，允许固定 TS 基线漏掉唯一的 result 终态，但要求其他事件、同 run 的落盘终态、恢复及八项协议检查全部通过；Rust 仍必须完整输出终态？具体提案已准备在 _tmp/20261113-sdk-terminal-proposal/D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001.draft.md。

用户回答原文：

> 批准，保留 Rust 完整终态（推荐）

批准的提案 SHA256 为 `1b542e2bb7d2c3c43c2057cd4273ef38286d081313c861734dca5f5ac73de34a`。只覆盖具名 `c1-sdk-cancel-resume` 的实际信号场景：固定 TS 的六个事件完整且逐值对应，只缺唯一 CLI `result sequence=7`；两侧当前 run 的 durable terminal、取消触发、CLI canonical footer、持久恢复、同 binary 八项原始 wire 检查均须成立。Rust 必须保留准确终态。TS self 不享受此差异；任意其他截断、错身份、缺 durable terminal 或 Rust 漏终态仍须拒绝。

这是新增的精确兼容性取舍，不追认它属于旧 crash 修复许可，也不声称基线违反自己的测试或发生持久数据丢失。原始正式 INVALID 与所有原件保留；只有新增差异、公共严格接线和新的正式验证通过后，才能登记新一轮成绩。既有账本与冻结标准不回写。

## D1-BROWSER-ADDON-IDENTITY-001：浏览器 addon 使用独立插件 ID

批准来源：本会话用户对精确 v2 提案的明确回答：

> 批准 v2 ID 修正及配套修改（推荐）

仅将插件 ID browser-use 改为 pilotdeck-browser-runtime；安装目录、技能命名空间、MCP key 和工具名保持。禁用键为 extension.builtinPluginsEnabled.pilotdeck-browser-runtime，旧 browser-use:false 不禁用新 ID，无兼容 alias。打包校验只允许精确(browser-use,pilotdeck-browser-runtime)目录/ID例外，其他不匹配拒绝。已审私有候选 review SHA74cba978845d346ab88ed76ce1b6efcde3179471c3bffb62244c72933b8413de；正式应用/独审证据在 _tmp/20261113-browser-addon-id-application/application.json。冻结为75e1e82并继承于18 e287；本批准不授权推送或发布。

## 两个固定 observer 场景的三组精确验收补充

批准来源：本会话用户对 _tmp/20261113-observer-final-policy-proposal/approval-overview.md 的明确回答：

> 批准这三组精确补充，保留其余差异（推荐）

批准仅覆盖：

1. 两个具名固定场景，observer started在被观察任务tool_result之前或其tool_result/step_completed之后的已证连续窗口；同一reviewer完成的三事件在当前父轮或紧接下一父轮排空。完整payload、实体/run/seq与WS/registry/history/durable因果链必须通过，其他事件次序不改变。
2. 仅memory场景四个精确派生字节数，在实际UTF-8重算和唯一调用完整报告关联通过后，接受已归一durationMs造成的差额。
3. 仅memory-gate[0..2]/memory-selector[0..2]六个采集headers对象的键顺序；全部键值、类型、现有数组顺序、method/url/body和长度仍严格。无同精确TS self变序实证，不称volatile或原始header行等价。

批准的私有候选SHA：memory/header a2f8a53c7ea610e373486901a1f4a90ba33774249f87986b6a08d1e5ede5dee8；lifecycle 7a6d343e023e20785e23b96a9cc22a4b273066237f72e5dddabbf0fdf6c21342。此前14+21控制及独立静审通过，不算产品执行。

原memory DIFF583/fork DIFF524及原件保留；正式接入仍需负控与独审后重判，不直接标SAME，不接受其余91/scene别名残差或其他字段。不得外推其他场景/custom/external，不修改Rust输出/完整终态，不排序原数组、不删原事件。冻结aggregate/FROZEN保持；新的精确差异条目只追加对应diffs目录。

## Original53：精确单类参考池准入及冻结重登记

批准来源：本会话用户对精确提案的明确回答。

问题原文：

> 是否批准 Original 53 的这项精确准入修订及必要的冻结重新登记：仅 cancel_during_tool 当日至少 20 次独立有效 TS 参考全部两两一致时，允许单一类别；每次 Rust 仍须匹配该类全部成员，完整 5 次 TS＋5 次 Rust、其他 52 项、身份及负控要求保持不变？原 REFUSED 保留，私有候选和独审见 _tmp/20261113-original53-single-class-proposal/freeze-event-DRAFT.md；完整 53 项新增控制尚待正式数据验证。

用户回答原文：

> 批准精确单类准入及冻结登记（推荐）

只批准所述单类准入和必要的正式冻结重新登记；原20份TS参考仍按原规则REFUSED，原件保留，不追写通过。其他52项、k=5、同日/独立/身份、类别两两一致和Rust逐成员匹配、登记锁及负控不放宽。新完整53控制和正式5+5未执行；批准不等于验收通过。候选精确三文件hash及旧新登记链由新增specs/parity/frozen事件保存；受控writer在真实干净提交后生成FROZEN哈希区，历史文字/账本不改。无推送或发布授权。

## A 组：observer、memory 与精确派生场景补充

批准来源：2026-11-13 本会话用户对 request_user_input_async 的明确回复。问题完整范围见 `_tmp/20261113-final-compatibility-approval/README.md` A组和已审 `_tmp/20261113-observer-supplement-proposal/APPROVAL-DRAFT.md`（SHA256 `840cf64375241ffbd092029ba568bbb79a01507928ff55f2948389b79bb5cfcf`）。

用户回答原文：

> 批准 A 组精确范围（推荐）

仅批准两个固定observer场景相邻provider次序、逐侧重算的context/counters/history、固定memory标识/schema/行序及一个fileSize；另仅两个普通场景固定step的fileSize，以及always-on-fire-execfail截断UUID有限场景扩展。每项门槛以所引六项精确合同为准，完整业务、引用、原payload及字节重建仍必需。tail必须锁step3/step10、read_session_messages、s-gap及实际成功response。时间只复用原ISO；同ID关系实现修复不允许全局159 UUID重标。tree键序不在批准内，未证明差异继续保留。

候选 `tail-scope-fixed-ready.json` SHA256 `6989d8300623bf0fa3d24e1b4bb6c3c09144e3057760fc4ee6bc5f1ad5f59373`；Inventory独立44/44及5额外memory拒绝控制通过。独审发现的任意step漏洞已修，旧38及首证保留。批准不等于formal SAME；公共接线独审、冻结身份和实际重判另行完成，原raw/FAIL不覆盖。无watcher、Original53或任意fileSize/UUID/数组排序许可。

## B 组：Feishu stop 严格派生与固定 SDK 日志

批准来源：2026-11-13 本会话用户对 `_tmp/20261113-feishu-stop-final-triage/POLICY-CANDIDATE.md` 精确提案的明确回复。该说明SHA256 `a87386738b194632dc0de311e00868df804920d71faa26dabb07688a014a048b`，candidate ready SHA256 `c3a3d6c7f40cde23b639da16a58bc880708a145d57c3b8a40ce6b828ca38a80f`。

用户回答原文：

> 批准 B 组精确范围（推荐）

停止泄漏修复的62路径须真实原socket关闭因果、完整receipt复制、token/calibration及durable父链/UTF8 byte/SHA/stat逐侧重算后准入。另仅2路径处理固定SDK1.65.0的3组startup info和1组not.registered warn各一次及原阶段顺序，其他输出逐字相同。SDK源码/严格verifier/tokenizer身份继续绑定；不接受所有日志、usage或预算差异，不与Node SQLite既有warning规则混淆。

root独立执行原sealed正控及26重签内容负控，SDK窄静审通过；不把该私有验证冒产品新增执行。原DIFF64和基线泄漏观测保留，TS self不享受此差异，公共接线与新formal结果另核。

## C 组：三个固定文件的 native 诊断差异

批准来源：2026-11-13 本会话用户对 `_tmp/20261113-diagnostics-policy-proposal/README.md` 精确提案的明确回复。README SHA256 `caa47cf06150771e40913a0217a888f93ec27a23499ad9f8938bac400b993b0d`；manifest SHA256 `c58356b54a5860844e2bd45a0847dfae5b69e039b3509916c03724995e7cd1e4`；可信raw入口SHA256 `627e56b5196ca493a79800507122c59902df5a73154cd512b79942d1020ec7fe`。

用户回答原文：

> 批准 C 组精确范围（推荐）

仅固定tool-write-file-diagnostics输入：bad.html允许TS无诊断/native一条恢复诊断；bad.csv允许开引号L2与EOF L3定位不同；bad.ts允许TS三条恢复诊断与Oxc一条首错误。此为存在/定位/数量新增许可，不冒既有有限文案许可。十五实际写出文件内容及模式保持，provider、token校准、durable、fileSize和skeleton全部派生副本仍严格；416具体路径及双侧有限值固定，新增差异不能自动扩表。其他文件/输入/语言/场景/任意parser升级不获许可。

可信入口自行读同raw、核固定闭包并生成完整views/all；不信外部passed/Map。17实际内容负控、4准确入口拒绝控制与SDK静审已完成；入口guard首判码错误记录保留。批准后仍须公共接线独审和真实原件重判，原DIFF416不覆盖，不直接判绿。

## Original53：本批父关闭双类补充及冻结重登记

批准来源：2026-11-13 本会话用户对 `_tmp/20261113-parentclose-candidate/ROOT-APPROVAL.md` 的明确回答。该提案SHA256 `8cbaa614371bfef35e0b96ca450b1efef0c1aa12b1b4ac5e632bfce2878c784d`；六文件候选manifest SHA256 `f30fabbfcf7b15479641af7dbcee749aabd9a424ba7a6ed67617191799c19409`。

问题原文：

> 是否批准 Original53 父关闭场景精确准入及必要冻结登记：仅本批原 S52 且唯一排除 parent-close 时，使用预先锁定的 20 份同日 TS 两类参考（2 completed／18 cancelled），每次 Rust 必须匹配唯一类全部成员并通过完整业务链。原 S52 保留；原 cancel、53 项、完整 5TS＋5Rust 不变；配套只修原 run.json 输入闭包。两次独审及 16＋5 控制通过，未来 S53 不自动适用。

用户回答原文：

> 批准精确父关闭准入及冻结（推荐）

仅 `sidecar_parent_close_after_admission`，独立parent池/锁在读取Rust前固定，至少20份同日同固定TS来源的独立有效观测，completed/cancelled每类至少2成员，完整190对实际为154类内PASS/36跨类FAIL。每Rust先真实admission、child request、parent close、operation/terminal链与无副作用，再用原G03匹配唯一类的全部成员。新终态、部分成员匹配、错日期/身份、跨池复用或缺ledger拒绝。

原TS5十对S52不改；补充必须绑定产生原S52的同source/r1..r5及每轮全部53trace SHA。ruler自行读取原件重算；score第二次实际用于Rust的定标必须等首次已验证锁定结果，输入hash重新核验并平铺外层闭包。**ruler只接受原S52且唯一excluded parent-close，未来S53仍拒绝，不自动扩大准入。**

完整5TS＋5Rust、53分母、其他52项、原cancel已批策略及全部身份/负控不变。run_all仅补随原trace复制原run.json的输入闭包，原缺失/错误ledger负控继续拒。允许这份精确六文件及规则的必要冻结重新登记，不改G03，不覆盖原FAIL。独立16integration+5ruler破坏控制已复验、第二非作者静审通过，但真正5Rust评分和正式冻结未因本批准自动通过；无推送或发布授权。
