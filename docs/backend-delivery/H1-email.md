# H1-email

- 任务：H1 email 原生渠道。
- 起点：`e8618392e7e4eb24c227eedd28e14c84237ca8a4`。
- 作者：当前 H1-email 子智能体；交付提交由主协调者集中创建，尚无交付 SHA。
- 固定基线：`OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读路径 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，本轮已核实 HEAD。

## 正式入口与行为

配置 `channels.email.enabled` 与 `channels.email.extra`。生产 loader 的 `register` / `register_with_network` 工厂为每个实例创建独立 IM 交互状态，共享宿主 `ResponderRef` 与 `TurnGateway`。接线由主协调者写公共模块和装配，当前作者未修改公共文件。

原生 IMAP 完成 TLS、可用时 STARTTLS、LOGIN、SELECT INBOX、UIDNEXT 近 300 条启动预热、UNSEEN 轮询和 UID 去重；原生 SMTP 完成 EHLO、STARTTLS/465 TLS、AUTH PLAIN/LOGIN、verify、固定主题纯文本发送。邮箱发件人映射为 chat id，`/new` 创建并保留新会话；并发活动 chat 跳过。入站优先处理 elicitation、permission，普通消息进入共享 turn；流式文本归并后邮件发送，失败工具和错误保留对应提示。

停止入口同步停止接收和外发准入，独立 owner 等待 poll、handler、outbound 清理；取消仅针对该 stream 的 `cancel_and_drain`，失败 stream 保留到后续 stop 重试，不用 session key 取消。复用的是本轮 C1 Signal 已有 Rust 生命周期设计，没有复制 TS 参考实现。

## 基线依据

固定 TS 源码路径均相对基线根目录：

- `src/adapters/channel/loadEnabledChannels.ts:86-88`：email 工厂只转交 cfg.extra。
- `src/adapters/channel/email/EmailChannel.ts:66-119`：配置/env、TLS与端口、UID priming、SMTP验证及首次 poll 时序。
- 同文件 `:155-202`：UNSEEN、seenUID、首个 envelope.from.address、空文本与解码行为。
- 同文件 `:204-223`：MIME heuristic；保留大小写敏感的初始 Content-Type 检测、大小写敏感十六进制 boundary、quoted-printable 按单字节字符解释等既有行为。
- 同文件 `:225-311`：交互优先、活动 chat、会话创建、turn 事件处理与最终文本。
- 同文件 `:313-325`：固定 from/to/subject/text 与发送结果。
- `EmailSessionMapper.ts:14-30`、`email-render.ts:3-40`：会话键和渲染。

真实 TS 取证使用外部 mock 替换 imapflow/nodemailer，通过原版 EmailChannel.start→submitTurn→sendMail→stop；不访问账号、真实邮件服务或凭据。新增 Rust 实现不依赖 Node。原生传输采用已有 tokio/rustls，不新增 Cargo 依赖。IMAP/SMTP 为直连 TCP；基线的 HTTP dispatcher 同样不作用于这两个库，`register_with_network` 接口接收但不使用 HTTP network 参数。

## 修改范围与状态

独占范围：`src/pilotdeck-gateway/src/channel/email/`、`src/pilotdeck-gateway/tests/fixtures/h1-email/`、本文件。未改 Cargo、lock、公共 loader/mod/assembly。

| 层次 | 状态 | 证据或剩余工作 |
|---|---|---|
| 接口 | 待验证 | extra/env及register接口已实现，依赖C1共享JsInteractionPort与owned stream接口集成 |
| 实现 | 待验证 | 原生产品路径与测试已写，未运行Cargo，不能称编译通过 |
| 接线 | 待集成 | 根协调者添加公共email模块和生产注册、取消UNMIGRATED拒绝 |
| 验证 | 进行中 | TS双自比、比较器负控及独立rustfmt通过；Rust测试和生产loader入口未运行 |
| 交付 | 待验证 | 未合入固定集成SHA，未发行验证，无真实账号验收 |

## 已运行的验证

平台 macOS aarch64。TS oracle 在隔离 `HOME/PILOT_HOME=_tmp/20261113-h1-email-oracle/home` 运行；无邮件或外部模型网络请求。

```sh
node src/pilotdeck-gateway/tests/fixtures/h1-email/oracle.mjs /Users/da/ws/pilotdeck-rs/resources/PilotDeck _tmp/20261113-h1-email-oracle/first.json
node src/pilotdeck-gateway/tests/fixtures/h1-email/oracle.mjs /Users/da/ws/pilotdeck-rs/resources/PilotDeck _tmp/20261113-h1-email-oracle/second.json
cmp _tmp/20261113-h1-email-oracle/first.json _tmp/20261113-h1-email-oracle/second.json
node src/pilotdeck-gateway/tests/fixtures/h1-email/verify.mjs src/pilotdeck-gateway/tests/fixtures/h1-email/expected.json _tmp/20261113-h1-email-oracle/first.json
```

两次结果相同，比较器正向 exit0。将首个sessionKey故意改错，比较器 exit1；失败记录 `_tmp/20261113-h1-email-oracle/negative.log`。原先驱动位于根 tests/fixtures，已按协调者要求移到 crate fixture 目录；命令列出当前重放路径。

独立 `rustfmt --edition 2024` 对五个 Rust 文件通过。首次执行因尚未建立 tests.rs 报模块不存在，建立后重跑通过；无产品测试失败被覆盖。本作者没有启动 Cargo，遵守本机唯一重型槽授权。

## 待执行验证与限制

由统一构建槽执行 `cargo test -p pilotdeck-gateway channel::email --lib` 与受影响 clippy；共 9 个测试覆盖 TS会话/body/render固定输出、UTF16状态、正式start/send/stop、本地IMAP/SMTP协议、正文伪UID及断帧负控、stop同步拒绝晚到外发且只cancel/drain已接受turn。生产配置loader路径还需根集成后验证；SMTP/IMAP TLS及AUTH LOGIN分支已实现但目前mock测试未覆盖，TLS额外CA沿用已有webpki_client_config。

独审发现并已修复但尚未运行Rust验证的项目：FETCH属性必须结构化解析以免正文伪UID污染去重；失去literal边界或中断命令后不能重用连接；start须等待首次poll。连接失帧立即关闭，后续需配置代际重启；没有擅加自动重连（固定基线没有配置ImapFlow自动重连）。网络超时与32MiB响应上限是原生资源边界，仍需统一审查其对大邮件场景的差异处理；当前不以该边界声称完整邮件兼容。

范围内 `unimplemented!`、`todo!`、`NOT_IMPLEMENTED`、TS转发兜底搜索未发现。未做真实账号操作、未提交、未推送或部署。
