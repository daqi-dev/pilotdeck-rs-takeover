---
id: C-DEADLINE-MAC45-EXACT-001
---

## 类别

原版缺陷的固定新45批精确比较补充；不改产品、原G03、strict、53分母或五轮。旧27条目C-DEADLINE-HOST-OPTIONAL-001及其登记/入口保持原字节，不延展其许可。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。`resources/PilotDeck/src/gateway/client/InProcessGateway.ts:1387-1432` timeout先abort并await持久化后关闭queue；`:1727-1750` catch只检查fence未排除timedOut；`:1824-1848` timeout不等待pump而结束fence，支持同一种宿主optional状态竞态解释。新20raw及两scene各100有序原G03诊断见 `_tmp/20261114-original53-mac45-formal-first/deadline-triage.json` SHA3f26c0c3efd21739d410d4fbbcd402c956708a6656e7c934210591c0bf4d3e90；非作者原件审 `_tmp/20261114-original53-45-deadline-independent/review.json` SHA6adad3bff78a878c3cd8cacd0796a32e859db7231177414a9575c511313c370c。原raw无完整wire/fence实时轨迹，不能声称已直接证明竞态时刻或无限未来副作用。

## Rust 行为

本条没有修改Rust。固定TS宿主调用source e99ee172e25c29c34ce299f03808773f16f2ec59、tree abbe2740ec0e78a6c929d19ec8276ea744308e4d的sealed debug Rust sidecar SHA566b54f3d75054b94d66bf48a8bb40fbe5873e0d631c6e79590124f07fe50341；五遍完整53场均valid、身份和清理合格。原strict SHA4e47f9cbcc9d961a1a32a134b4c6578463c32c8e40425d1eaf912b65ff759798仍为52/53 FAIL，仅deadline paired2/5失败。独立新summary并列strict与accepted，原score不改。预算与deadline_during_tool的原五对均PASS。

## 范围

仅 `scripts/parity/registrations/original53-mac45-deadline-20261114.json` SHA21bdf4fa82738a8165a6a29c0593787d113facdf139b2b1ad53c420f49bf7517 中deadline固定TS5/Rust5共10raw及ledger；只允许Rust r2/r5原index/sequence24的两条完整parent/B durable.status(gateway_submit_failed)。各case extraCount/indices、原行/封套/文案、实际身份、source、timeout→sidecar_final failclosed→extra→side_effect→operation→durable→terminal→identity精确尾链全部固定；其它8份deadline仍strict。during_tool另10raw只作strict control、不获适配；其它52场逐对象保持原score。未来raw、别轮、别场景、同源码新binary或release文件均不自动准入。

## 机器判据

新独立入口 `PDRS_BASELINE=BASELINE python3 -I -B scripts/parity/accepted_summary_mac45.py --evidence-root ROOT --output NEW_JSON`。新helper内嵌registration SHA，重读固定原raw及ledger、原strict完整673 input/4 code闭包，逐值保留全部原absolute键：672 input为root、唯一historical ledger input及4 code为managed旧前缀；677明确IO绑定只读同SHA相对文件，无任意前缀替换、外manifest或caller Map授权，运行时不需要Git。原health/oracle/单侧E/partial-order先验、完整model req/resp/typed usage、工具、unknown终态和取样副作用必须严格；只在唯一parent/B与E10/E11/G03差异下检查精确原record移出后的残余channel/multiplicity，不重编号或用投影health掩盖原失败。

`python3 -I -B scripts/parity/controls/deadline_accepted_mac45.py ROOT NEW_DIRECTORY` 已执行公共投影：120原件比较（20方向格适配两条原record，during_tool60格零适配）、47底层语义负控、11身份负控、2辅助正控全过，输出分开计数；200原G03诊断另计。负控含原记录字段/位置/归属/重复/缺失、别round/scene、业务终态/副作用/model/usage/C事件/健康、固定SHA原件/ledger/677闭包/registration/外manifest/别名/既有输出。输出exclusive，old strict52及paired2/5完整保留，另52场对象不变。公共投影仍须非作者审和冻结，不能只据私有候选绿判正式完成。

## 批准记录

2026-11-14主协调依据用户已明确授予的项目内自主决策权，正式批准仅上述新45数据边界并授权独立公共入口；根决定已由root追加至 `state/decisions-20261114.md`，标题为“Original53：45批唯一 deadline 的新原件精确登记”。这不是用户逐字批准该规则，也不是旧27许可自动延展。私有实现非作者审 `_tmp/20261114-original53-45-deadline-independent/candidate-review.json` SHA3ab9161c4a26c8282fa619f49c53ecf454bea0189136add41f777d2eb1040ae7 已通过；公共投影的源码SHA/路径变化、actual controls和非作者复审由独立收据闭合，FROZEN由指定登记者处理，本作者不写。新独立入口保持旧accepted_summary.py/旧登记/helper全部原字节与默认输出。
