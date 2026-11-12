# H1 Slack native 生产接线补齐

- 起点：本轮根工作区，先前候选 `81bc` 的源码及二进制已经根封存；本批尚未提交。
- 负责人：根派工 `h1_matrix_discord`；公共装配：`h1_protocol`。
- 状态：待验证。私有源码已冻结，未自行 Cargo，未称交付。
- 固定 TS：`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读参考树。

## 问题与结果

原 `slack::register` 固定 `gateway: None`，旧 `start` 不消费 `deps.turn_gateway`，`process_message` 遇空 gateway 返回成功。旧网络使用独立 HTTP/WS dispatcher，仅识别旧 stub 的 `type:message`，且默认 base 与请求 path 拼出 `/api/api/...`。原 reader/handler detached，stop 没有持有入站回合或出站任务的完整 ownership。

现在生产挂载使用 `slack::register_with_network(mounts, responder, network)` 创建私有 `NativeSlackChannel`。它从 start deps 取得真实 TurnGateway；共用 host ResponderRef，使用各 adapter 独立的 JS-string interaction helper；HTTP 与 WS 使用相同 ChannelNetwork。原生 Socket Mode 识别 `events_api` envelope，在派发前发送 `envelope_id` ACK，处理 hello、ping/pong、disconnect，连接结束后重新请求临时 WS URL。Web API 正确拼接 `/api/<method>`，app token 用于 connections.open、bot token 用于 auth.test/postMessage 的 Bearer header。

普通消息经过现有 Slack mapper，进入 native TurnGateway 并读取真实事件流；回复复用已有 Slack markdown/链接转换和 39000 UTF16 分块，保留线程发送上下文。交互优先于忙碌门，确认失败使用共享 helper 释放/重试逻辑。stop 同步停止准入，持久 cleanup worker 等待 receiver、handler、outbound 和精确 accepted stream `cancel_and_drain`；失败保留 stream/helper 供重试，丢弃 stop waiter 不丢清理所有权。StartupOwner 使等待 auth.test 时被取消的 start 释放已打开 receiver。

## 基线与协议依据

`src/adapters/channel/slack/SlackChannel.ts:44-98` 定义 start 取 gateway、Bolt 注册、start/auth.test 顺序及失败；`:102-180` 定义过滤/mention/thread/交互/忙碌/map；`:182-222` 定义提交和事件投影；`:224-285` 定义回复、markdown 和 chunk。`SlackSessionMapper.ts` 的会话规则继续使用既有 Rust mapper。

特意保留 TS 自身 root-thread 键差异：`handleSlackMessage` 在 thread_ts 等于 ts 时 admission/map 用裸 channel，但 `processMessage` 无相等判断，helper capture/clear 使用 channel:thread；新 native SendContext 按两处各自条件处理，没有静默修成统一 key。

基线未锁或安装 @slack/bolt，既有 `tests/fixtures/channel-slack/bolt-stub.mjs:1-12` 明确是七成员 stub contract。旧 fixture 的 WS `type:message` 及 JSON 请求顺序不能证明真实 SDK 网络流量。新原生协议补充依据仅来自官方 [Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/)、[apps.connections.open](https://docs.slack.dev/reference/methods/apps.connections.open/)、[chat.postMessage](https://docs.slack.dev/reference/methods/chat.postMessage/)；不把最新 SDK 或官方协议文档冒充固定 TS SDK 实测。

## 旧装置停用范围

旧 public `SlackChannel` 与 `slack::register` 暂时保留，只保护已建立的高层 adapter stub 合同、mapper/render/permission 历史回归；它们退出生产 NativeChannels 挂载。生产 success、网络协议、native gateway/responder 和 stop 所有权由新 `register_with_network` 及 native 测试替代。旧 stub 暂不删除，避免丢失既有合同回归；旧测试通过不能再作为生产 Slack 可用证据。

兼容旧 `type:message` frame 仅保留在新 reader 的输入识别，正式 Socket Mode `events_api` 有独立检查与测试；不把旧 frame 当真实服务协议。

## 五层状态

| 层 | 状态 |
|---|---|
| 接口 | 待验证：正式挂载签名同 Signal，私有测试 seam 不新增配置字段 |
| 实现 | 待验证：native Socket Mode/Web API/gateway/responder/owned lifecycle 已写 |
| 接线 | 待验证：公共装配由 h1_protocol 改接新 mount；须集成候选检查 |
| 验证 | 进行中：固定 adapter SDK mock 双自比/负控通过；8 项 native 测试未编译执行 |
| 交付 | 待实现：无发行布局/真实账号验收 |

## 实际验证与证据

仅执行文件级 rustfmt，通过。未自行运行 Cargo。

本机 macOS，隔离 HOME/PILOT_HOME：

`H1_BASELINE=<固定参考树> node src/pilotdeck-gateway/tests/fixtures/h1-slack-native/oracle.mjs`

两次结果 `cmp exit0`，产物 `_tmp/20261113-h1-slack-native-oracle/{first,second}.json`。覆盖 self/bot/edit 过滤、mention、普通/root thread session、`/new` 粘滞、markdown/链接和 thread_ts 出站。`--negative` 篡改观测 thread_ts，断言 `exit1`；首次 stderr/exit 保留。同一脚本不访问真实账号或网络，轻量 oracle 槽已归还。

已写但未编译的 8 项 native 测试：过滤/mention/root-thread键；真实本地 WS events_api→ACK→TurnGateway→thread reply 与真实 HTTP 请求路径/Bearer；disconnect 重新连接；权限 answer 到共享 responder；取消失败保留 stream/helper 并重试；stop waiter 丢弃仍等待 drain；等待 auth 期间丢 start 仍关闭 receiver；blocked HTTP caller receipt-drop 不撤销 owner，stop 后取消并 drain。

## 未闭合

- 根统一 check/clippy 和上述 native 测试实际执行；首次失败须保留。
- 正式配置入口、真实 host 提交桥、热更新的组合 mock 证据。
- 固定 SDK 版本/流量不存在，SDK retry/cache/rate-limit/timeout 等未观测语义不能声称等价；真实外部服务尚未授权验收。
- 同一集成提交的完整门禁、发行布局和资源释放组合验收。

占位扫描未见 `unimplemented!`、`todo!`、`NOT_IMPLEMENTED` 或内部 TS fallback。无关 WS frame/event 忽略分支按外部协议定义保留，不代表未实现的必需交互。
