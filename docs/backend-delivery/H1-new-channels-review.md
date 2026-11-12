# H1 新渠道独立静态审查

审查者：native_regression；2026-11-13。范围：email、matrix、discord 候选与 `tests/fixtures/h1-*`。本批未改作者代码、未运行 Cargo、未运行 TS/产品取证，未占用构建或 bbx 槽。候选仍在编辑，本记录针对首次所读实现，修复后必须复核；临时文件哈希位于 `_tmp/20261113-h1-new-review/source-sha256.txt`。不阻止作者将固定候选交 E0 早编。

## 已确认问题

### P1：email FETCH 的正文能够伪造 UID 和 envelope

`channel/email/wire.rs:225` 调用 `uid(line)`；`:258-262` 在包含 BODY literal 的完整 FETCH 文本上用正则查首个 UID。`:226-244` 同样以全文 `find` 定位 ENVELOPE/BODY。服务端的 FETCH 属性顺序并无当前解析器要求的 UID→ENVELOPE→BODY 顺序：如果 BODY 在先，正文包含 `UID 999`，而真正属性为后面的 `UID 4`，当前代码将消息登记为 999，进一步影响 `email/mod.rs` 的 seen UID 去重，产生丢信或重复。正文中伪造 ENVELOPE 还会影响回复目标解析。

修复建议：在 FETCH 顶层属性序列中按语法解析属性，literal 仅作为值，不再参与 UID/envelope 搜索；本地 mock 返回 BODY-first，正文含 UID/ENVELOPE 诱饵，证明真正 UID、发件人与正文均正确。固定 TS `EmailChannel.ts:165-178` 读取 ImapFlow 提供的 `msg.uid`、结构化 envelope、source，三者分离。

### P1：Discord 在认证或 READY 前报告真实运行句柄

`channel/discord/mod.rs:981-987` 设置 running/accepting，spawn receive 后立即返回真实 Handle。receive 在 Gateway fatal 4004/4014 等仅写日志并退出，不清 running。故 Gateway 不可达、无 HELLO 或认证失败时，start 仍已报告成功且 cron 发送路径处于接收状态。

固定 TS `DiscordChannel.ts:85` 在 `await this.client.login(this.token)` 后才返回真实句柄，异常由 `:86-89` 的 start failed/noop 处理。需要启动 receipt：等待 READY 或明确启动失败，保留后续断线恢复；补无 HELLO、4004、READY 后重连的本地 WS 测试。当前 Discord tests 仅过滤、mapper、chunk，尚无 Gateway 协议测试。已将问题发作者。

## 高风险待核项目

- **email 失帧连接复用。** `wire::command` 超限或 literal 读取超时可在响应中途退出；`receive` 对 poll 错误只记录日志后继续同一个 Imap。后续 SEARCH 可能消费前一 FETCH 的剩余行/标签，造成持续错误。需要明确不可恢复读错后的连接作废/受控重连策略，本地 mock 证明错误不会污染下一事务。基线 ImapFlow 的此处实际错误/重连策略尚未取证，不能把自动重连当成已核基线事实。
- **email 首次就绪时序。** 固定 TS `EmailChannel.ts:116` 首次 `await pollOnce()` 后才返回 start；Rust spawn receive 后立即返回。需覆盖启动后立即 stop、已有未读邮件的首次分派时序。
- **email SMTP 2xx 与认证协商。** 手写 RCPT 只接受 250；251/252 等服务器响应是否被固定 nodemailer 接受，需 package 边界或本地 baseline mock 实测。当前成功脚本只覆盖 AUTH PLAIN、250 接受、点透明处理，不能外推完整 SMTP 兼容。
- **Matrix sync 落盘。** `matrix/mod.rs:384-397` 直接 write 截断存储文件；进程在中间失败可丢 syncToken，重启从初始同步重新接收。只写 syncToken 是否兼容固定 `SimpleFsStorageProvider` 的真实存储格式尚无证据。`h1-matrix/oracle.mjs` 将该类 mock 为空，故不能证明存储迁移或真实 since 语义。应补真实 SDK 格式证据与损坏/重启测试，必要时原子写。
- **Matrix 邀入阻塞。** receive 逐个 await joinRoom 后才分派同一 sync 的已加入房间消息；单个 invite join 慢响应阻塞普通消息。TS `MatrixChannel.ts:86-96` 注册独立 invite/message 事件，固定 SDK 的 emit 是否串行等待还未取证，需证明后再判断差异。当前 Rust mock只证明成功 join。
- **Discord SDK 替代范围。** Gateway resume、ACK 丢失、Invalid Session、fatal close、REST 429/backoff 未有本批实跑证据；h1-discord TS oracle当前未见。手写 op code 存在不能等同于具备真实 SDK 全部行为。优先通过本地 Gateway 及 REST mock覆盖正式接线。

## 资源所有权检查

三者都采用独立 cleanup worker、共享 watch receipt、outbound admission 同步关闭、per-turn `cancel_and_drain` 和失败 stream 保留机制；静态结构符合 owned drain 方向，没有看到直接按 session key 误取消新轮的路径。未执行掉 waiter、断线、取消失败后重试、任务 panic 的组合，不能据此声称 stop 已验证。Matrix 的同步文件写、email logout以及各发送请求仍需用阻塞 mock 确认 stop 不会在真正释放前回执成功。

## 证据边界与下一步

email oracle 真实导入固定 dist adapter，但 mock 了 ImapFlow/nodemailer；可证明 mapper、正文提取、渲染和 SDK 调用意图，不能证明原生 IMAP/SMTP 字节协议。matrix oracle 真实导入 adapter，但 mock 了整个 SDK client/storage；可证明过滤、映射、分块，不能证明 sync、认证、初始历史、存储或重连。发现与待核项已向对应作者、根协调者发送；作者修复后以新候选固定身份审阅，再由 E0 统一执行。
