# H1 首批渠道恢复窄批

状态：待验证。2026-11-13恢复，StaffDeck延期。起点C1 `bcad7cb1435103e31c9ec71877c7064f3cffc619` 加既有dirty；不将HEAD作为完整受测源码身份。不提交、不修改公共装配。本页仅记录本次窄修，整体入口仍为state/backend-handoff.md。

## 首败与归因

c1_validation独占构建后，Signal17/17、Mattermost13/13通过。HA取消精确项首败见`_build/20261113-051458-resume-ha-cancel/output.log`：peer为`Io(ConnectionReset, code54)`，同时closed=true/socket_present=false/network_present=false。server accept_async完成不保证client connect future已完成，取消可以发生在CONNECTING；Rust socket::run的stop优先分支会丢弃握手future。此前Close|None断言缺少已建立连接的因果前置。保留原断言，先发送auth_required并实际收到auth请求后才abort，明确覆盖已建立连接。另新增pending HTTP101的CONNECTING取消用例，精确要求EOF或ConnectionReset并等待真实stop_and_drain，检查socket/network/tasks归零。固定TS源码HomeAssistantChannel.ts:85-108 auth等待、:113-122 stop、:163-169 cleanup仅close；TS无start future abort入口，不把这段源码当同阶段wire实测。

WS8首跑7/1，deflate6为5/1，见`_build/20261113-051530-resume-ws-corrected/output.log`。fragment测试将transport解压明文与adapter归一后的expectedMessage比较，左值2544字符多一个合法尾空格。原捕获capture.mjs:10输入为`compressed-wire-message `加120次`compressible-content `，:38模型断言INPUT.trim()；固定TS MattermostChannel.ts:158实际trim。离线持续zlib重解码原fixture帧确认wire精确等于输入；新增expectedWireMessage保留2544字符，原expectedMessage不动。transport断言改为精确wire文本，没有trim解码结果、修改帧或产品。机器复核`_tmp/20261113-h1-resume/wire-space-proof.json`。

## 本次修改范围

- `channel/homeassistant/tests.rs`：上述取消阶段分层；第24压缩测试保留Pong，但Pong后以bounded yield等待真实Events析构计数2和active释放，再精确核对两个请求；held仅等待busy/admitted/active，不等流EOF。
- `channel/ws_transport/deflate/tests.rs`：window0/16明确要求握手成功，首次压缩帧返回Io(InvalidData, Invalid server_max_window_bits)，消息仍精确空；fragment测试比较expectedWireMessage。
- `tests/fixtures/h1-ws-wire/mattermost-deflate.json`：新增原wire文本字段；未改原帧与adapter期望。
- 本页。

没有修改产品实现、Signal/MM实现、公共channel/loader/native_process/assembly、Cargo或冻结判据。文件级rustfmt已执行；没有自行运行Cargo。下一步主协调统一冻结源码后编译并运行HA25项、WS8项及必要Signal17/MM13回归。正式Gateway继续存活/资源收口、真实互动/cron/reload与全量门禁仍待验，局部测试不代表H1交付。
