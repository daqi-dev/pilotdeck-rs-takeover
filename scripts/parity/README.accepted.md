# Original53 本批 accepted 并列汇总

本入口只登记主协调于用户项目决策授权内决定的精确 deadline 兼容处理，授权见 `state/decisions-20261114.md:13`。不冒称用户逐条批准，不把历史 #6 当作已有比较豁免。原 `score.py`、`g03_compare.py`、53分母、5轮、TS参考池和 strict成绩完全不改。

## 固定批次与运行

```sh
python3 -I scripts/parity/accepted_summary.py \
  --evidence-root /path/to/original/evidence-repository \
  --output /path/to/new-accepted-summary.json
```

`--evidence-root` 仅把不可变 registration 已列相对文件定位到指定证据根；不会重写原ledger/score内的绝对路径身份值。不存在的/非清单路径、`..`、替代registration或额外manifest参数均不获准。新输出必须不存在，使用原G03 exclusive发布。

唯一登记为 `registrations/original53-deadline-20261114.json`，其 SHA256 `bfba3dac717f009fc94ac0726e59bc74c30a11481991f2f40d20a7914a34b074` 固定在 `deadline_accepted.py`，不是动态信任任意manifest。登记绑定20份原raw、原ledger、cc7TS-host、source/bin/driver/catalog/adapter及原strictscore（SHA `598f21097518a76ecc2f5d90e38dc0081959aec89fb5def77f93ae4bf18a368b`）。完整strict输入673文件及代码4文件也逐字重核；原source路径和值保留，不把未来新引擎或新采集混入。

`accepted_summary.py` 先保留原完整 strict `50/53 FAIL`，只对两个deadline场景各5个原paired组合重新验证并生成单独 accepted视图；其它51项逐项原值相同。当前 accepted 是 **52/53 FAIL，sidecar_budget_limit仍失败**。因此命令正常生成此份汇总后退出1；它不是53/53 PASS，也不改写原score。输出始终并列 strict_original 与 accepted、原strict错误、candidate原index/sequence/全文/原行SHA及剩余比较结果。

## 精确规则

- 仅本20固定raw、`deadline`和`deadline_during_tool`。每行四个invocation封套必须齐全，原run归属/sequence/ledger/身份包装真实一致；identity/health/oracle/所有单侧E与partial-order始终来自未删行原raw。
- 每侧恰一条原timeout、sidecar_final failclosed、完整unknown/DEADLINE_EXCEEDED operation终态、failed/unknown/空output公开终态及完整durable.state；工具call/start、模型request/response及全部usage完整原值与类型严格。副作用为整数0、counts为空；不从durableStatusCount=0推断没有extra。
- 只当strict失败恰为E10/E11/G03且唯一差流parent/B、双方extra计数0/1或1/0时，允许一条字段全集/精确文案均相同的 parent durable.status(gateway_submit_failed)。它必须在相邻尾链timeout→failclosed之后，side_effect.state之前。任何重复/错归属/额外字段/错位置或C通道新增不适用。
- 独立视图只省略该精确记录，不改raw，不重编号，不拿视图health替换原health。原G03重算所有actor/channel残差及pair E10/E11；其它差异仍FAIL。0/0和1/1不触发适配，须原strict PASS。

现原件未记录完整transport wire、fence时间戳、完整operation磁盘历史或最终mock采样后的无限期副作用观察；本规则不补证这些未知，也不豁免对应产品验收。

## 控制与保留

```sh
python3 -I scripts/parity/controls/deadline_accepted.py \
  /path/to/original/evidence-repository /path/to/new-controls-directory
```

仅离线读取固定原件；120个正比较=2scene×5TS×5Rust×2方向加20原件self，不是新采集。原30个cross strict FAIL并列保留。内容负控使用显式私有方法对内存副本做变异，不能通过正式文件入口或CLI授予新原件资格；覆盖重复/位置/归属/unknown/source、sideeffect、工具完成、新req/usage、C通道、身份/单侧health，以及删invocation、已存在output、路径/registration/strict闭包替换。全部产物exclusive，控制目录必须全新。

原严格首FAIL/50/53、私有首审漏项及修前源码均保留。正式公共接线需独立审查、冻结和固定源码执行；开发控制通过不是自动正式评分。未来批次必须由根核真实证据后显式追加登记并重新独审/冻结，本入口没有选择外部manifest或任意未来批次的参数。冻结aggregate/FROZEN由指定登记者处理，不由本实现改动。
