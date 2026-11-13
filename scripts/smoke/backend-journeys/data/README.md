# 可复用后端用户旅程数据

本目录提供自行编造的中英文项目文件、CSV业务数据、用户任务、真值与专用oracle，供正式后端端到端runner使用。它不是新的runner，不调用模型API，不包含账号、密钥、个人或客户数据，也不携带运行日志、provider对话和生成的测试结果。

当前实际执行状态以[统一后端交接记录](../../../../state/backend-handoff.md)及其指向的固定身份证据为准。纳入交付时，`sales-margin`与`renewal-after-restart`已有真实执行记录，但首轮失败不计PASS；重试、修正或在途执行也必须经过独立复核后才能更新结论。其余三条任务当时尚未实际执行。本目录的存在、装置自测通过或模型宣称完成都不代表用户旅程通过。

## 五项任务

| ID | 难度 | 用户目标 | 确定真值与关键约束 |
|---|---|---|---|
| sales-margin | 基础 | 读取销售CSV与中英文口径，核算退货后的指标，写JSON及中文分析 | 净量48；收入531600分；成本297300分；毛利234300分；毛利率4407基点；地区细项、币种和实际来源必须正确 |
| procurement-deadline | 组合 | 同时满足预算、整包、优先级与自然日交期，制定采购单 | CABLE 30件、9000分；DOCK 15件、27000分；总计36000分，余额4000分；ADAPTER延期；不能选便宜但逾期的Remote报价 |
| renewal-after-restart | 恢复 | 首轮确认谈判条件，仅保留会话历史；同session重启后写续费账单 | 240000−18000＋12000−5000＝229000分；折扣7.5%与信用额50元只在第一轮用户消息，第二轮实际模型请求必须包含恢复历史 |
| denied-budget-change | 组合 | 实际请求修改受保护预算，权限拒绝后停止 | 必须实际走permission_request→deny→失败工具结果；原文件逐字节不变，不产生替代业务文件、不换工具绕过 |
| cancel-committed-receipt | 恢复 | 凭据提交后取消等待，再重启核查已有账目 | OP-CAN-021只有一条凭据、7500分；实际脚本恰好调用一次；durable信号后取消并核实owned脚本退出；续轮不得再次入账 |

`manifest.json`是runner的任务输入。`scenarios/<id>/input/`为可人工查看的同内容原始文件；runner应只复制选定scenario的`files`到全新隔离项目。不要把manifest、scenario规格、真值、generator或oracle复制到模型可见项目，避免泄漏验收答案。

`expected-calculations.json`供独立复算与人工review。`generate.py`使用整数分和明确舍入规则重新生成合成输入及manifest，也会产生各场景的便于检查的scenario.json；修改数据后需重新固定manifest身份、重新校准，不得对在途运行改写合同。

## 结果判据与可接受变化

`verify.py`消费正式runner生成的`journey.json`及其隔离项目文件；它不启动server、不生成模型答复、不伪造真实工具事件。

- 金额分、数量、基点要求整数，不能用布尔值或数字字符串代替。JSON对象键顺序、采购orders顺序、额外说明字段和自然语言措辞可变化；规定数值与业务约束不得变化。
- 输入逐文件SHA必须不变。要求的输出必须实际落盘且可解析；Markdown必须有实质内容与真实来源路径，JSON来源不得虚构。业务文件保护不能由模型一句“未修改”代替。
- 必须有实际工具started/finished身份与读取证据。兼容OpenAI的role=tool和Anthropic的user.content.tool_result；完整文件参数可从assistant.tool_use.input补齐。读取证据不能只来自用户prompt中的文件名。
- 必须核对runnerPassed、固定manifestSha256、每轮真实终态及原始WS帧。恢复必须同session、有实际server退出/重启记录，且第二轮provider请求含第一轮谈判事实；不能把旧提示重新塞进第二轮user消息冒充恢复。
- read_file/write_file/edit_file的完整结构化路径必须落在隔离项目。bash或缺完整参数的访问列为manual/unknown，返回`passed_with_manual_review`与`fullyVerified:false`；**不得自动宣布未访问项目外文件**。此oracle不是OS沙箱。
- 字符数和引用检查不能保证报告自然语言与JSON完全一致。数值、来源引用、权限决定、输入保护以及报告语义都需要独立审查；真实模型不可确定的措辞和排版不作为固定字符串判分。

当前优先使用sales与renewal进行正式运行后的独立复核。权限拒绝与取消需要runner真实接线及原始证据；取消的实际终态尚未固定时，oracle明确拒绝通过。不要把超时、模型口头拒绝或仅看到一行账目当作生命周期通过。取消样本的脚本会在真实append与fsync之后等待，只能在受控取消流程中执行。

```sh
python3 -B scripts/smoke/backend-journeys/data/verify.py \
  --manifest scripts/smoke/backend-journeys/data/manifest.json \
  --scenario sales-margin --run /ABSOLUTE/ISOLATED/JOURNEY/OUTPUT
```

将scenario换为`renewal-after-restart`可复核恢复任务。固定manifest SHA-256为：

```text
46c4b80260eebe7616aaad4d6f4649f8febf409b3fd6bd7bc54fa7e44a142800
```

`self_test.py`仅用合成artifact检验oracle的正负控，不是产品或真实LLM证据。需要复算装置时，在临时副本中运行；生成的oracle-self-test.json等结果不提交到本目录。正式运行失败应由统一runner保留原始首次失败，不用装置自测或后续重试覆盖。
