# H1 BlueBubbles 生产集成候选

日期2026-11-13，负责人h1_inventory；源码起点为根cf143ff8加并行WIP（主已封存81bc测试身份，本包不覆盖旧结果）。独占私有bluebubbles目录及h1-bluebubbles-native夹具、本页；公共channels装配归h1_protocol。未自行Cargo、未提交。

## 行为与改动

`register_with_network(mounts,responder,network)`新增真实每实例HelperInteraction、host ChannelNetwork HTTP transport和owned生命周期。旧register/new保留旧组件测试注入缝；产品正式mount已由公共作者切换新接口，仍待集中验证。BlueBubbles本身无需Node；Node仅测试固定TS oracle使用。

新增私有http.rs以已有ModelHttpTransport/fetch消费host dispatcher；URL、query/password、Bearer header、正文键序和总发送预算保留，旧独立HttpClient保留给legacy特征回放。新实例共享host网络快照，不为每次请求重建调度器；代理热更新由既有ChannelNetwork所有权承担。

新增lifecycle.rs集中持timer/polls/消息/outbound任务；停止同步关闭admission和poll，独立cleanup worker不随stop等待者drop消失。精确stream cancel_and_drain失败保留重试；任务panic为不可自动清除的真实失败。出站调用者丢弃也仍有任务owner，stop取消HTTP后等待。完成task及时收割避免长期轮询堆积JoinHandle。普通start/stop参数和输出保持，停止资源排空为窄bug差异，不扩展其它协议。

## 证据与状态

固定TS源码cc7c4e6、Node22.22.0实际adapter.start/poll/stop及loopback HTTP，held Gateway generator两轮a/b逐字节相同SHA519dbd7006e9f926bbf24bdb3e40140d8557b60749654b219d7472ab8f014bfd。原始 `_tmp/20261113-bluebubbles-stop-{a,b}`；驱动和oracle/metadata位于tests/fixtures/h1-bluebubbles-native。它证明stop未收口已接纳generator，不声称正式native/provider SSE已通过。新窄差异A8.3-CH-BLUEBUBBLES-003-R1明确此边界和正式待验。

新增6项owned组件测试：真实helper permission/elicitation通过注入HTTP交回responder；stop等待者drop仍持精确stream；失败取消保留重试；丢弃出站caller后stop仍关闭HTTP；worker异常不能伪装重试成功；实际pending submission future被停止丢弃并释放owner。均未集中编译/运行，旧金样回放也须作为受影响回归。直接rustfmt通过。

接口已实现、实现/接线待验证、验证待验证、发行交付待验证。公共作者需在确认stop合同后加BlueBubbles OwnedChannel能力；不能把普通TurnGateway提前启动当成真实取消能力。下一步集中all-targets check、新6项+旧BlueBubbles/helper回归、真实生产mount/server文字与互动、代理/CA/ConfigStore热更新、held-turn disable/re-enable和错误负控。未覆盖范围如实保留，不以模块测试替代正式入口。


公共作者静读补充：提交前的pending submit也必须受stop控制。已核 `channel/turn_submitter.rs:77-83` owned creation/transfer在同一poll完成，故新owned select丢弃pending future不会隐藏已接纳run。新增确定性pending-submit测试先确认future已poll且尚无stream，要求stop一秒内完成、owner Drop恰一次；旧legacy await语义保留。该修正尚未编译/执行。stop对每次调用继续输出原日志；fatal worker原始错误保留供后续stop返回，避免空错误或伪成功。
