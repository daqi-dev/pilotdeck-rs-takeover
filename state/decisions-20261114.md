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
