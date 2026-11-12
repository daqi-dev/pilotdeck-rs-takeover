# H1 Matrix 与 Discord 原生渠道

- 任务：H1；负责人：根派工子智能体 `h1_matrix_discord`。
- 起点：`e8618392e7e4eb24c227eedd28e14c84237ca8a4`；交付提交：尚未提交。
- 固定 TS：`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。
- 状态：进行中。未编译、未正式入口对拍，不能视作已交付。

## 实现边界与基线依据

Matrix：`src/adapters/channel/matrix/MatrixChannel.ts:17-49` 定义选项/环境及 storage；`:85-103` 订阅 invite/message 并启动；`:135-215` 过滤、交互优先、忙碌门与 `/new`；`:218-266` 事件投影、错误与交互清理；`:269-305` 出站内容及 UTF-16 4000 分块。`MatrixSessionMapper.ts:15-33` 定义会话 key 与新会话粘滞。对应 Rust `channel/matrix/` 实现原生 Matrix Client-Server HTTP：whoami、sync、join、m.room.message send；不运行内部 TS 或 Node SDK。

Discord：`src/adapters/channel/discord/DiscordChannel.ts:63-88` 定义 Gateway intents、partial 与回调；`:121-198` 过滤/交互/忙碌/会话；`:200-249` typing 与回合事件；`:251-310` channel fetch、出站、typing 和 UTF-16 2000 分块。`DiscordSessionMapper.ts:15-33` 定义会话规则。对应 Rust `channel/discord/` 实现 REST 与 Gateway WS，包含 HELLO/IDENTIFY、heartbeat/ACK、READY、MESSAGE_CREATE、reconnect/RESUME、fatal close 与停止。

外部协议参考：[Matrix Client-Server v1.12](https://spec.matrix.org/v1.12/client-server-api/)、[Discord Gateway](https://github.com/discord/discord-api-docs/blob/main/developers/events/gateway.mdx)、[Gateway close codes](https://github.com/discord/discord-api-docs/blob/main/developers/topics/opcodes-and-status-codes.mdx)。这些仅补足外部服务协议，不作为固定 TS 产品行为的新标准。实际第三方 SDK 未安装，当前 SDK mock 证据证明适配器行为，不能冒充真实 SDK 网络请求对拍。

Rust 复用已有 Signal 的 Rust owned lifecycle 模式和 JS-string helper 接口；mapper 独立维护状态；共用 SMS 事件文本投影（非交互事件与此两渠道相同）。全部 HTTP 使用宿主 ChannelNetwork；Discord WS 使用同一网络快照的 `connect_ws`，无需新增 Cargo 依赖。stop 同步停止接收新工作，独立清理 worker 等待接收、入站、出站、typing 和精确 owned turn `cancel_and_drain`；失败保留 stream 供重试。

## 修改范围与公共接线

独占 `src/pilotdeck-gateway/src/channel/{matrix,discord}/`、crate 下 `tests/fixtures/{h1-matrix,h1-discord}/` 和本文件。未修改公共 loader/mod/assembly/Cargo。

根需声明模块并调用各自 `register_with_network(mounts, responder, Arc<ChannelNetwork>)`，签名与 Signal 一致；公开 `register` 允许独立消费者默认网络。依赖待合入的 C1 `JsInteractionPort`、`TurnEventStream::cancel_and_drain`、`network`、`ws_transport`。

## 五层状态及证据

| 层 | Matrix | Discord |
|---|---|---|
| 接口 | 进行中：选项/env/loader 构造写入 | 进行中：token/env/loader 构造写入 |
| 实现 | 待验证：原生 HTTP、sync 持久游标、mapper/交互/owned stop 写入 | 待验证：原生 REST/WS、mapper/交互/owned stop 写入 |
| 接线 | 待集成：根持有公共装配 | 待集成：根持有公共装配 |
| 验证 | 进行中：TS SDK 边界 mock 两次自比通过、负控退出1；Rust 测试未编译 | 进行中：TS SDK 边界 mock 两次自比通过、负控退出1；Rust 测试未编译 |
| 交付 | 待实现：无发行布局验收 | 待实现：无发行布局验收 |

轻量 Matrix oracle：本机 macOS，隔离 HOME/PILOT_HOME，`H1_BASELINE=... node src/pilotdeck-gateway/tests/fixtures/h1-matrix/oracle.mjs`。2026-11-13 两次结果一致（cmp exit 0），产物 `_tmp/20261113-h1-matrix-oracle/{first,second}.json`。覆盖 self/notice/edit 过滤、最终 trim、general→/new→sticky key、4000 UTF16 空格分块。没有访问真实账号。

Rust 已写 Matrix 过滤/mapper/chunk 单测与 transport 测试（whoami/auth、sync invite→join、/new 发送、since 持久化、stop 后拒绝出站）；Discord 已写过滤/mapper/chunk 单测及原生本地 REST/WS 测试（IDENTIFY/intents、READY 启动 receipt、op7→RESUME/session/sequence、/new 发送、fatal4004 失败清理）。Matrix syncToken 采用临时文件 write/sync_all/rename，保留原存储额外字段和权限；坏 JSON/读错误阻止启动，避免静默覆盖。Matrix transaction 使用 UUID 避免重启复用。Discord fatal auth 无 READY 不返回运行 handle，30 秒无 READY 结束启动并清理；所有 WS 写受 stop 与 30 秒超时约束。仅 rustfmt 解析通过；未运行 Cargo，验证由根与 E0 排队。

## 尚未闭合

1. C1 共享接口合入后的 gateway 编译、clippy 与定向测试。
2. 原生 REST/WS 本地协议 mock 测试执行；Matrix/Discord 正式配置入口 mock。
3. 交互确认失败重试、停止取消失败重试、重连/热更新正式入口验证。
4. Rust 比较以及固定集成提交完整门禁；现有 TS adapter SDK 边界双自比/负控不替代正式入口测试。
5. Matrix SDK 实际持久存储格式/初始同步行为与 Discord 第三方 SDK 的缓存/速率限制细节尚未实证；明确作为待验，不宣称完全等价。
6. 无真实账号验收、无推送/部署/发布。

占位扫描：本目录未出现 `unimplemented!`、`todo!`、`NOT_IMPLEMENTED` 或转发 TS 业务的回退；协议未支持分支仍须结合正式入口测试审查，空忽略分支只对应无关 WS frame/event。

## 2026-11-13 冻结首编补充

根协调者要求冻结本批 Rust 后进入首次编译队列。未自行运行重型构建、未提交。轻量 oracle 槽已归还。

Discord `oracle.mjs` 双次输出 cmp exit0，产物 `_tmp/20261113-h1-discord-oracle/{first,second}.json`；覆盖 author.bot/self/system 过滤、intents、最终 trim、general→/new→sticky key、2000 UTF16 空格分块及 stop。两渠道 `--negative` 在观测的分块结果中故意插入 `corrupted`，断言均 exit1；`negative.{stdout,stderr,exit}` 保留。

最终脚本 SHA-256：Matrix `5a1fe5c0383a929884e8fb5d8c98b96b2ad076ba5ac35a89a59674532230b062`；Discord `7747d1d642f73f3691047a0501caa089743edc2ecadb11daf457ab081e00c949`。固定参考树 package/lock/scripts/tests 搜索未找到两个 SDK 的版本约束，只找到适配器 optional require 和安装提示；不存在可声称固定的 SDK 流量或存储实测。
