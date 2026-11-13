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
