# H1 旧渠道生产集成审查

日期：2026-11-13；负责人 h1_inventory。本轮全Rust冻结期间只读审查，未改Rust、未Cargo、未产品或TS执行。固定参考 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。源码结论来自本轮实际读取，不沿用旧覆盖状态。最新WhatsApp 24/24是另包集中结果，不构成本报告三渠道的完成证据。

## 必须关闭的生产缺口

`src/pilotdeck-gateway/src/native_process/channels.rs:117-142` 的 production_loader_mounts_with 虽接收host responder/network，却仍调用SMS、Slack、BlueBubbles旧register。正式create_configured_with `:565-579`选择该mount；默认ChannelStartDeps装普通TurnGateway `:405-417`，仅名单内新渠道由OwnedChannel覆盖为owned `:585-612`。SMS/Slack没有读取start deps中的turn_gateway，因此不能只看deps里存在它就判已接通。

| 渠道 | 已确认缺口 | 可观察后果 | 必修性质 |
|---|---|---|---|
| SMS | `channel/sms/mod.rs:1923-1941`register同时gateway None、twilio_factory None；constructor`:410-459`原样保存；start`:491-519`缺factory即noop | 任何合法SMS配置均先输出twilio not installed，根本未监听webhook；native model与互动不可达 | 成功路径未实现与接线缺失，P0；依赖缺失日志不能冒充完成 |
| Slack | `channel/slack/mod.rs:1148-1168`gateway None；start`:241-253`只装logger；process_message`:802-813`gatewayNone直接Ok | REST/WS可能显示connected，但普通message不提交native turn，用户收不到模型回复；permission/elicitation也不可达 | 正式turn与互动接线缺失，P0 |
| BlueBubbles | `channel/bluebubbles/mod.rs:876-900`明确no_interaction；start`:241-245`会读取deps.turn_gateway | 普通文本turn可进入native，不是整体不可用；permission/elicitation事件被NoInteraction吞掉，pending=false，提示空值，后续答案无法交native coordinator | 真实互动接线缺失，P0 |

共同数据结构问题：SMS/Slack用旧 `channel/sms/gateway.rs:130-156` 的SmsGateway（submit_turn/respond_elicitation/permission_decide），没有取消drain能力；公共TurnGateway和ResponderRef已经存在，需正确桥接而非把某个gateway None改为假成功对象。BlueBubbles已有 `helper_interaction(responder)`兼容接口，注册时改传真实helper即可触达现有状态机；每构造独立helper，不能共享chat状态。

`native_process/assembly.rs:1786`已调用set_interaction_responder，DeferredInteractionResponder `channels.rs:250-270`在尚未就绪时返回明确Gateway not ready；不是静态总 unavailable。生产旧register未消费这个真实responder，是当前缺口。`im_interaction.rs:158-194` NoInteraction则确实固定false/None/空提示；它不是已接好的错误传播方案。

## 网络与资源所有权

三者旧网络路径：Slack `mod.rs:100,227,293-343`独立HttpClient::new和connect_async；BlueBubbles `mod.rs:228,796`独立HttpClient；SMS sender.rs仅SmsSender/TwilioFactory trait，无真实产品REST sender。HttpClient `channel/http_client.rs:41-57`固定webpki connector，没有host ChannelNetwork注入或热配置快照。

BlueBubbles固定TS `BlueBubblesChannel.ts:99,249`直接fetch，代理调度所有权由 `src/cli/proxy.ts:31-80`和network/fetch.ts:199-202说明（全局dispatcher/热更新）。因此BlueBubbles必须接host network才能保持实际fetch代理、NO_PROXY、TLS/CA与reload能力。Slack/Twilio是SDK路径，基线未锁/未装可选SDK，不能据global fetch规则直接推定每个SDK的代理行为；先抓所选固定SDK实际请求/代理能力，再实现本期约定network接线，不宣称当前直连与基线一致。生产统一register_with_network由h1_protocol装配。

stop所有权独立于普通接线：Slack `mod.rs:130-157`只取sink发Close，没有持reader/消息handler JoinHandle；`:390-424`两个层次tokio::spawn无owner。BlueBubbles `mod.rs:283-312`初poll/timer/每poll均spawn无JoinHandle；stop`:837-861`置running false和abort poll即返回，不等待accepted turn。SMS已有HTTP connection drain（`:347-381,645-797`）但消息处理`:1247`另spawn；旧SmsGateway stream没有cancel_and_drain。

这些停止行为与TS stop只关渠道的部分行为相近，不能直接当普通parity回归修掉；本期已授权挂起/资源泄漏修复，须先用正式禁用held-turn mock取得精确基线遗留证据，再以窄差异条目与正向断言闭合。不得简单把三键加入OwnedChannel名单：只有adapter自身真实持有并取消排空stream后才可领取owned能力，否则native执行可在未poll时先启动，造成更难清理的悬挂。

## 固定TS依据及夹具可信性

固定TS `src/adapters/channel/loadEnabledChannels.ts:23-29,61-67,90-93`负责Slack/BlueBubbles/SMS构造；三个TS start都装入真实deps.gateway：SmsChannel.ts:57-58、SlackChannel.ts:45-46、BlueBubblesChannel.ts:58-59。真实TS普通turn分别SmsChannel.ts:265-288、SlackChannel.ts:187-211、BlueBubblesChannel.ts:202-225；互动优先于busy路径分别`:207-264`、`:123-186`、`:144-201`。SmsChannel.ts:82调用真实twilioFactory，`:158-186`签名验证，出站sendReply使用client.messages.create；不能用Rust可注入的假Sender证明发行sender存在。

可选SDK区分：TS会require twilio/Bolt，失败才进入缺包noop。当前固定package.json/pnpm-lock没有锁定这两SDK；`tests/fixtures/channel-slack/bolt-stub.mjs:1-12`更明确承认基线没装真实Bolt，夹具定义它自己的七成员wire contract。Rust Slack网络恰复刻这个contract：apps.connections.open JSON token、WS type=message、auth.test和postMessage。该夹具可以证明TS adapter的高层调用顺序与参数，并能做已有渲染/互动特征测试，但不能证明真实SocketMode envelope/ack、心跳、SDK重连或HTTP鉴权兼容。Rust reader`:390-421`只接受type=message。是否还缺真实events_api/envelope_id ack需实际固定SDK oracle确认；本报告没有联网或运行SDK，不能凭回忆宣布精确协议。

因此static审查确认SMS是伪unavailable：Rust根本没有SDK加载行为，却永久注入None重现TS“缺依赖”分支；这不是完整native交付允许保留的能力缺失。Slack不是静态unavailable，而是已连接后gateway静态缺失造成静默丢工作。BlueBubbles不是整体unavailable，而是静态NoInteraction造成互动不可用。错误路径/旧夹具通过不能覆盖三者的正式成功路径。

## 最小分工与文件范围

- SMS（h1_telegram_qq）：`channel/sms/{mod.rs,gateway.rs,sender.rs,tests.rs}`及必要新增同目录桥接/owned文件；新 `tests/fixtures/h1-sms-native/**`与交付文档。保留已有HTTP/签名/超时特征行为，不整块重写服务；真实sender、native turn+responder桥接、network注入和精确stop所有权分别验证。
- Slack（h1_matrix_discord）：`channel/slack/{mod.rs,tests.rs}`及新增同目录内部网络/生命周期文件；新 `tests/fixtures/h1-slack-native/**`。可复用SMS gateway桥接，接口由两作者先对齐；不得各建不兼容公共协议。真实SDK wire oracle不足独立记录，不把旧bolt-stub装成正式SDK网络标准。
- BlueBubbles（h1_inventory）：`channel/bluebubbles/{mod.rs,interaction.rs,tests.rs}`及必要新增私有文件；新 `tests/fixtures/h1-bluebubbles-native/**`与本包文档。先register_with_network(responder,network)接真实helper和HTTP，后基于held正式证据补owned停止；不改普通文本/轮询/mapper行为。
- 公共装配（h1_protocol独占）：`native_process/channels.rs`、公共mod/loader及必要assembly；作者提供register_with_network接口，不同时编辑mount。HttpClient公共文件不得多作者并行修改；推荐每私有模块使用已有ChannelNetwork/ModelHttpTransport seam或由唯一负责人新增窄公共桥接。

正式验收各包至少：启用→合法配置真实连接/监听→入站普通文本→native模型实际请求→出站正文；permission/elicitation完整提示、答案、连续问题及发送失败重试；同chat busy与独立chat并发；HTTP失败/认证错误；proxy/noProxy/CA及真实ConfigStore更新；禁用held-turn的准确请求关闭、无迟到发送、重新启用可新回合；cron（基线提供时）；退出后无owned worker/连接残留。新装置先TS自比与真实错误负控；未装真实SDK的格明确分开，不能用mock注入层测试冒充完整生产链。

当前均待实现/待验证，没有因为本报告关闭任何模块。解冻后按上述独占范围实施；本轮未停用旧装置，旧fixture继续保留其有限用途。
