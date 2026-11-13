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
