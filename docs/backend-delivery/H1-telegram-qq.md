# H1 Telegram / QQ 原生适配器

- 负责人：H1 Telegram/QQ 子任务作者；公共接线及构建由根协调者负责。
- 本轮起点：`e8618392e7e4eb24c227eedd28e14c84237ca8a4`；交付提交尚未产生，子任务不自行提交。
- 固定基线：`OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读参考 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。
- 状态：进行中。**未编译、未完成正式入口验证，不能称已交付。**

## 正式入口与接口

Telegram 配置触发 `telegram::register_with_network(mounts, responder, network)`，注册键 `telegram`。接收配置 `token`、`webhookUrl`；令牌回退 `TELEGRAM_BOT_TOKEN`。Webhook 模式只调用 Telegram setWebhook，不新增接收 HTTP 路由，与基线适配器的能力范围一致。

QQ 配置触发 `qq::register_with_network(mounts, responder, network)`，注册键 `qq`。支持 `appId`、`clientSecret`、`allowGroups`、`triggerPrefixes`、`maxMessageLength`；凭据与允许群回退 `QQ_BOT_APPID`、`QQ_BOT_SECRET`、`QQ_ALLOW_GROUPS`。公开 `QQSessionMapper` / `QQSessionMapperState` 与 `on_state_change` 回调供 CLI bundle 保存 `activeByChatKey`。状态提供 `from_js(&JsValue)` / `to_js()`，内部 `JsObject` 保留 UTF-16 key、JS 属性顺序及更新时的原有 key 位置。公共 bundle 接线仍由根协调者闭合。

两者均使用共享 `ChannelNetwork` 的 HTTP dispatcher；QQ WS 也通过同一 dispatcher 快照创建。原生业务不调用 Node/TS。每个构造器创建独立 JsInteractionPort helper，TurnGateway / responder 由宿主注入。

## 基线证据

源码证据：

- `src/adapters/channel/telegram/TelegramChannel.ts:51-103`：SDK、缺少凭据、set/deleteWebhook、getMe、stop；`:109-193`：互动优先级、busy、/new、turn、typing；`:196-233`：send 失败与 UTF-16 长度拆分。
- `src/adapters/channel/telegram/TelegramSessionMapper.ts:16-44`：/new sticky session 与默认 general；`telegram-render.ts:3-44`：事件片段。
- `src/adapters/channel/qq/QQChannel.ts:53-142`：配置与启动；`:145-328`：群触发与 C2C，尤其 C2C `/new` 原文作为普通输入；`:304-406`：msg_id/msg_seq、chunk、500ms 延迟、失败即终止。
- `src/adapters/channel/qq/QQSessionMapper.ts:16-54`：group/user session 映射。
- `src/adapters/channel/qq/qqbot-gateway.ts:61-108`：token / discovery / socket；`:110-154`：HTTP body；`:156-266`：identify、resume、heartbeat、op7、op9、5s重连；`:268-315`：token cache 与发现。
- `src/cli/ChannelAdapterBundle.ts:154-167`：QQ mapper 读取与状态回调保存。

真实 TS 特征运行使用仓库 TypeScript 编译器在内存转换固定源码，mock grammy 与 QQ fetch/ws；未修改/复制基线源码，未访问真实账号、服务或 home。驱动位于 `src/pilotdeck-gateway/tests/fixtures/h1-{telegram,qq}/oracle.cjs`。

2026-11-13 macOS 各执行两次并 `cmp` 一致：

- `_tmp/20261113-h1-telegram/oracle-{first,second}.json`：start/deleteWebhook/getMe、typing、/new sticky session、4096 拆分、send 失败、stop。
- `_tmp/20261113-h1-qq/oracle-{first,second}.json`：token / gateway HTTP、identify/resume、群 /ask /new、群 /chat、C2C /new、消息 msg_id/msg_seq、拆分、stop。

运行命令（产品相关 Node 驱动始终隔离 HOME/PILOT_HOME）：

```sh
HOME="$PWD/_tmp/20261113-h1-telegram/home" PILOT_HOME="$PWD/_tmp/20261113-h1-telegram/home/pilot" node src/pilotdeck-gateway/tests/fixtures/h1-telegram/oracle.cjs
HOME="$PWD/_tmp/20261113-h1-qq/home" PILOT_HOME="$PWD/_tmp/20261113-h1-qq/home/pilot" node src/pilotdeck-gateway/tests/fixtures/h1-qq/oracle.cjs
```

这两项自比只证明特征驱动稳定，不代表 Rust 对拍已通过。mock SDK/API 所覆盖的调用与真实 wire 认证成功仍需正式入口本地 mock 验证。

## 实现与验证边界

独占文件为 `src/pilotdeck-gateway/src/channel/{telegram,qq}/`、对应 crate fixtures、本交付记录。未修改 Cargo、lock、公共 module/loader/assembly。

Telegram 实现原生 Bot API set/deleteWebhook、getMe、getUpdates offset、sendMessage、sendChatAction；4096 UTF-16 单元、LF/空格切分。QQ 实现 token/cache、gateway discovery、WS identify/resume、heartbeat、invalid-session 2s 重认、断线 5s 重连、群/C2C 消息、协议 body、分块与延迟。

生命周期复用 C1 原生 Signal 的资源所有权设计：同步关闭 admission，接收/handler/outbound 有注册表，stop 等待 owned turn cancel_and_drain；失败保留 stream，后续 stop 重试，丢弃 stop 的 waiter 不丢失清理 owner。该清理比基线 TS 的 best-effort stop 更强，用于本轮已经要求的取消与释放；尚未追加接受差异，不将其算作验收通过。

拟运行受影响测试（由根/E0分配 Cargo 槽）：

```sh
cd src
cargo test -p pilotdeck-gateway --lib channel::telegram::tests
cargo test -p pilotdeck-gateway --lib channel::qq::tests
```

Telegram 四项组件测试包含 sticky/new/final reply、send失败、UTF-16拆分、取消失败重试。QQ 八项组件测试包含群/C2C区别、identify/chunk、禁用凭据、发送失败/token缓存、持久状态UTF-16/顺序、GET请求头与send成功不读body、真实 localhost WS identify→heartbeat→reconnect→resume→invalid-session→stop。最后一项预计约7秒，不使用真实外部服务。

`rustfmt --edition 2024` 已对两个 mod.rs 及其子模块执行成功。第一轮root `cargo check` exit101，精确错误记录 `_tmp/20261113-c1-resume-validation/root-first-errors.txt`；QQ/Telegram自身类型、所有权与boxed歧义已修复但尚未复验。正式CLI配置链、持久状态、热更新及发行组合尚未验证。

## 五层状态与下一步

| 层次 | 状态 | 未闭合项 |
|---|---|---|
| 接口 | 待验证 | 根接入共享network/helper和QQ bundle持久状态 |
| 实现 | 待验证 | 首次编译及两组组件测试 |
| 接线 | 待集成 | 公共mod、loader、native assembly、QQ bundle由根负责 |
| 验证 | 进行中 | TS特征自比完成；Rust、正式入口、故障负控、独审未完成 |
| 交付 | 待实现 | 固定集成提交与发行布局验证 |

已扫描 `unimplemented!`、`todo!`、`NOT_IMPLEMENTED`：两个适配器目录没有这些占位或转发TS的兜底。空分支包括忽略未知事件/ACK/非消息、无互动答复、空输入、已完成JoinHandle、typing best-effort、定时器结束，均是明示协议控制流；不是未实现入口。

当前已知需要后续核实：Telegram grammy polling 的 wire级失败/退避细节；恶意/非正常配置的JS类型容错；scope内正式入口对拍与热更新。未闭合项不能因上述组件测试通过而自动关闭。

第二候选静态修正：QQ GET /gateway 不发送 Content-Type；消息发送成功不读取响应体；失败按 getAppAccessToken/getGateway/sendGroupMessage/sendC2CMessage 区分；恢复 dispatch/入站日志与 C2C 错误前缀；初次 token/discovery 在 start() 返回前完成，连接提示先于 WS open，保留失败后的可关闭 handle。第一候选源码 hash 存 `_tmp/20261113-h1-qq/first-candidate.sha256`，不将未编译候选当成功。

2026-11-13 root首次检查失败修正：保留root的TurnRequest默认字段；QQ改正String/JsValue构造、UTF-16/UTF-8转换与BodyExt boxed歧义；Telegram以借用数组并逐项clone保留JsValue Drop所有权。非正QQ分块上限真实TS正控2返回，0/-1各100ms未返回后由父终止自建worker；证据及窄挂起修复登记 `specs/gateway-parity/diffs/H1-QQ-CHUNK-LIMIT-001.md`。当前检查状态仍是待复验。

81bc7e5d5定向首轮：Telegram3通过/1失败、QQ7通过/1失败。两个失败都是双LF恰落max-1/max的测试期望错误：真实固定TS再次验证首块保留第一LF、第二块为rest；产品chunk未改，仅修正断言。原始首次失败保留 `_build/20261113-060046-root-81bc-h1-corrected/output.log`，新证据 `_tmp/20261113-h1-qq/double-lf-oracle.json`，驱动 `tests/fixtures/h1-telegram/chunk-oracle.cjs`。本轮QQ builder `from_config(&JsObject)` / `with_network(self,Arc<ChannelNetwork>)` 已供公共bundle复用字段解析及HTTP/WS双接线。修正后的测试尚未复验。
