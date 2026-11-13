---
id: C-DEADLINE-HOST-OPTIONAL-001
---

## 类别

原版缺陷的本批有限比较适配；不改产品、原G03算法或strict成绩。历史#6及6-R1仅作宿主竞态背景，不被冒称已有比较豁免。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`resources/PilotDeck/src/gateway/client/InProcessGateway.ts:1387-1432` timeout先abort、await持久状态再关queue；`:1727-1750` catch只看fence未排除timedOut，`:1824-1848` timeout不等pump即结束fence。20份实际raw均unknown/DEADLINE_EXCEEDED且公开failed；只3格额外观察到parent持久gateway_submit_failed。报告 `_tmp/20261114-original53-deadline-analysis/review.json` SHA ec3ec95ef341fea3272a4d3916072e81c67077cd4f05d5f8430ec2b90fcf6c8b；原score valid_score=true、FAIL50/53完整保留。实际raw未保存完整wire/fence时序和无限未来副作用，不能升级为其已通过。

## Rust 行为

本次没有修改Rust。Original53仍采用固定TS宿主调用sealed Rust sidecar，未知终态不改成cancelled、不加延迟。独立accepted_summary仅在新结果中并列strict与accepted；原score.py、G03、原raw、53分母、5轮和TS池不动。预算失败保留，当前accepted是52/53 FAIL。

## 范围

仅 `scripts/parity/registrations/original53-deadline-20261114.json` SHA bfba3dac717f009fc94ac0726e59bc74c30a11481991f2f40d20a7914a34b074 中20固定原raw及ledger，两scene deadline/deadline_during_tool、cc7TS-host。最多一条精确parent/B durable.status(gateway_submit_failed)，字段全集、文案、封套归属和相邻timeout→failclosed→extra→state→operation→durable→terminal→identity尾链均严格。原health/oracle/单侧E/partial-order/身份、完整终态、model req/resp/usage、工具及副作用原值另外校验。只有strict的E10/E11/G03与唯一parent/B差流可适配，剩余所有流严格。pairwise0/0或1/1不适配。未来批次不自动准入，不接受任意manifest。

## 机器判据

`python3 -I scripts/parity/accepted_summary.py --evidence-root ROOT --output NEW_JSON`，内部代码固定registration SHA及原strictscore SHA，独立复核strict input673/code4完整闭包和本20原件/来源，重算两scene五paired。保留严格50/53与完整错误，accepted52/53且budget FAIL；exclusive输出，existing拒绝。`python3 -I scripts/parity/controls/deadline_accepted.py ROOT NEW_DIRECTORY` 实际120原件比较并保30strict FAIL；负控涵盖重复/字段/位置/child/缺unknown/错source/副作用/工具完成/新模型/usage/C通道/身份/health/删invocation/原件与registration及strict闭包替换。candidate原行index、sequence、全文和行SHA均输出；不重编号或改写输入，不用投影视图掩盖原始health。

## 批准记录

2026-11-14主协调基于用户“项目所有决策你都可以自行决策”的明确授权，正式决定本批精确accepted汇总，见 `state/decisions-20261114.md:13`。这是授权内根决策，不冒称用户逐字批准具体规则。私有最终module SHA f03387d3e164dfaa8f3319fe89a849b6962f8a22df55710cdd843ca014740334、ready8d787685c71fa54e1baec3a378bcffd309cd34459a2a8c1145dcd058cceb56a8，Astra独立120正/52负及其它51×5=255原paired相同；修前漏项及源码已归档。公共迁移仍须Astra非作者复审、指定登记者冻结及固定源码执行，不由私有通过冒正式评分。本条只新增，不修改accepted-diffs aggregate或FROZEN。
