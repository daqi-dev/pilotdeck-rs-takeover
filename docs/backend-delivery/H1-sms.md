# H1 SMS 原生生产接线候选

任务起点为根封存快照 `81bc7e5d5`；负责人为 H1 Telegram/QQ 子任务作者，公共 NativeChannels/OwnedChannel 接线由 H1 protocol 作者负责。交付提交尚未产生。状态：进行中，**本候选尚未编译或验收**。

## 已核实缺口与基线

原生产 `sms::register` 传 `gateway: None` / `twilio_factory: None`，`start()` 忽略 `deps.turn_gateway` 并先走 `twilio not installed` noop。HTTP回包后另起的 `handleIncoming` 不在旧stop的HTTP连接集合里；原stop没有覆盖已接受turn与出站exchange。

固定基线为 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读目录 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。`src/adapters/channel/sms/SmsChannel.ts:12-18` 可选 require Twilio SDK；`:54-91` 读取gateway/config并构建SDK client。基线package.json、pnpm-lock.yaml没有Twilio依赖条目，本机node_modules也没有该SDK。

因此分别记录两种证据：实际缺失SDK时的noop；源码预期的 `messages.create({body,from,to})`。没有下载未锁定SDK当作固定基线。新增native sender的provider wire依据官方 [Messages resource](https://www.twilio.com/docs/messaging/api/message-resource) 与 [API Basics](https://www.twilio.com/en-us/blog/developers/best-practices/programmable-sms-api-basics-best-practices)：Messages路径、HTTP Basic、form Body/From/To。该引用属于外部provider契约，不声称是缺失SDK的wire实测结果。

现有 `tests/fixtures/channel-sms` TS特征与负控仅作为已有装置线索，尚未在本候选重新跑全矩阵；包含300秒超时的全量生成不能偷算作本轮已验证。

## 修改与公共接口

只改 `channel/sms/*`、新 `tests/fixtures/h1-sms/*` 与本记录；不改Cargo或公共装配。

- `sender.rs`：原生Twilio REST；生产 `native_factory(ChannelNetwork)`，测试 `native_factory_at(transport,local_base)`。Basic认证、POST `/2010-04-01/Accounts/{sid}/Messages.json`、UTF-8 form Body/From/To，错误读取provider message。
- `native.rs`：将共享 `TurnGateway` / `ResponderRef` 桥接现有 `SmsGateway`；历史HTTP、签名、mapper、permission和elicitation实现继续复用。
- `mod.rs`：新增 `sms::register_with_network(mounts,responder,network)`；该生产构造器装原生factory和native边界，start实际读取 `deps.turn_gateway`。旧factory和gateway测试注入接口保留；公共生产装配必须切到新入口。
- 通过ChannelAdapter的cron投递也进入相同native outbound owner，不绕过停止边界。

普通HTTP路径、签名、默认监听值、表单解析及历史fixture逻辑未整块重写。`SmsChannel::with_native(responder)` 只供内部生产构造与本地组件验证。

## 资源所有权

native入站handler、outbound exchange全部登记；停止函数同步关闭admission并通知HTTP监听及native watch。独立清理worker持有watch receipt，即使stop返回future或外部send waiter被丢弃，owner仍继续清理。

清理依次等待既有HTTP owner、join已登记handler/outbound，再对每个未正常完成的native stream调用 `cancel_and_drain`。handler的pending next/submit/respond均受stop watch唤醒；取消失败保留stream并返回Aggregate错误，后续stop再次尝试；panic/JoinError留下fatal错误，不以重试清空后假装成功。成功清理才释放sender并清空互动helper。旧HTTP服务本身保留已有60秒有界连接排空规则。

只有上述合同在实际候选通过后，公共作者才将SMS加入OwnedChannel capability名单；实现存在不等于名单已获验收。

## 新验证集合

`channel::sms::native_tests` 新增5项，均为待执行：

- `native_webhook_turn_and_provider_rest_are_connected`：真实localhost webhook→共享gateway请求→真实localhost provider HTTP；断言路径、认证、form正文。
- `native_permission_reply_uses_host_responder_while_turn_is_open`：真实webhook答案→共享host responder，不新建turn。
- `native_stop_owns_cancel_receipt_and_retries_failed_drain`：首取消失败、重试stop的waiter丢弃、后续观察成功、不发late结果。
- `dropped_outbound_waiter_does_not_orphan_exchange`：外部send waiter被取消，stop仍释放其持有的pending provider future。
- `native_sender_surfaces_provider_failure`：真实本地provider失败消息。

建议统一Cargo槽：

```sh
cd src
cargo test -p pilotdeck-gateway --lib channel::sms::native_tests
cargo test -p pilotdeck-gateway --lib channel::sms::tests
```

`rustfmt --edition 2024 src/pilotdeck-gateway/src/channel/sms/mod.rs` 已完成；Cargo由根/E0调度，作者未自行运行。正式CLI配置链、动态重载、发行布局、故障负控及独审均未闭合。外部真实账号没有使用或验证。

新短TS特征驱动 `tests/fixtures/h1-sms/oracle.cjs` 已准备：真实固定SmsChannel及Node HTTP listener，Twilio仅在require seam mock，记录SDK调用参数、失败与缺失SDK分支；不是SDK wire模拟通过。2026-11-13已由根分配轻量槽2执行两轮，cmp一致；原始证据 `_tmp/20261113-h1-sms/oracle-{first,second}.json`。记录真实webhook 200/TwiML、共享session提交、SDK body/from/to、send失败false及缺失SDK不监听。槽已归还，始终隔离 HOME/PILOT_HOME。

## 五层状态

| 层次 | 状态 | 下一步 |
|---|---|---|
| 接口 | 待验证 | H1 protocol核对新register及OwnedChannel资格 |
| 实现 | 待验证 | 首次编译与native/旧SMS组件回归 |
| 接线 | 待集成 | 公共装配调用新network入口 |
| 验证 | 进行中 | 新短TS特征、真实本地HTTP测试、正式入口及负控 |
| 交付 | 待实现 | 固定集成候选及发行组合 |
