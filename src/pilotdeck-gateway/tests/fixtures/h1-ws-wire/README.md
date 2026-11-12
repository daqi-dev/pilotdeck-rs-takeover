# H1 正式 WS wire 特征取证

本装置启动固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的真实 `dist/src/cli/pilotdeck.js server --host 127.0.0.1 --port 0`，分别启用 Mattermost 和 Home Assistant。Node 固定为 `v22.22.0`（内置 Undici 6.23.0）。没有注入产品模块、替换全局 WebSocket、调用私有 adapter 方法或借 mockGateway 冒充正式 server。

所有 HOME、PILOT_HOME、配置、项目与 cwd 都在每次指定的新输出目录下；HTTP、WS、模型 provider 和产品 server 只绑定随机 loopback 端口。Bearer/token 都是夹具字面量。捕获只操作自己创建的 detached 子进程组，最后 SIGTERM，3 秒仍未退出才 SIGKILL；每格保留 PID、信号、自然退出结果及原 stdout/stderr。没有固定 18789、真实 HOME、真实服务或凭据。

`capture.mjs` 使用基线已有 `ws` 包创建 HTTP/WS mock，配置 `perMessageDeflate: {threshold: 0}`。只观察自有 mock 的 TCP 读写，不改变产品 socket。完整请求 headers/rawHeaders、HTTP 101、两个方向的原始 TCP 字节、每帧 FIN/RSV/opcode/mask/payload、持续 inflate 后的明文以及实际 provider/回复请求都保留。`verifyWire` 从原始 TCP 重新分帧、持续解压，再与派生记录逐项核对。

## 复现

在工作树根目录执行，输出路径必须尚不存在：

```sh
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/capture.mjs /Users/da/ws/pilotdeck-rs/resources/PilotDeck "$PWD/_tmp/20261113-h1-ws-wire/ts-new-a"
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/capture.mjs /Users/da/ws/pilotdeck-rs/resources/PilotDeck "$PWD/_tmp/20261113-h1-ws-wire/ts-new-b"
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/compare.mjs _tmp/20261113-h1-ws-wire/ts-new-a _tmp/20261113-h1-ws-wire/ts-new-b _tmp/20261113-h1-ws-wire/self-compare-new.json
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/capture.mjs /Users/da/ws/pilotdeck-rs/resources/PilotDeck "$PWD/_tmp/20261113-h1-ws-wire/fragment-new-a" --fragmented
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/capture.mjs /Users/da/ws/pilotdeck-rs/resources/PilotDeck "$PWD/_tmp/20261113-h1-ws-wire/fragment-new-b" --fragmented
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/compare.mjs _tmp/20261113-h1-ws-wire/fragment-new-a _tmp/20261113-h1-ws-wire/fragment-new-b _tmp/20261113-h1-ws-wire/fragment-self-compare-new.json
node src/pilotdeck-gateway/tests/fixtures/h1-ws-wire/context.mjs _tmp/20261113-h1-ws-wire/ts-new-a _tmp/20261113-h1-ws-wire/context-new.json
```

`identity.json` 固定 SHA、Node、源码/入口/执行文件/驱动 hash。`command.json` 保存隔离环境、argv、cwd；`result.json`/`results.json` 保存所有网络和行为记录。跨轮比较只归一化随机 host/Origin/Sec-WebSocket-Key、HA notification_id 的毫秒；所有随机 mask/accept 与原始 TCP 仍保留并独立重验，帧形状及解压后消息参加比较。装置的 1 MiB 帧边界只限制这个固定小样本，不是产品上限。

## 已实际完成的基线证据

过程材料位于 `_tmp/20261113-h1-ws-wire/`，不入库。

| 集合 | 两轮目录 | 机器比较 | 结果 |
| --- | --- | --- | --- |
| 两渠道压缩 Binary → 压缩正常文本 | `ts-b`、`ts-c` | `self-compare.json` | 4 次正式入口捕获通过，自比相同，7 个变异负控全拒 |
| 两渠道压缩 Binary → 分片压缩文本，中间 Ping/Pong | `fragment-a`、`fragment-b` | `fragment-self-compare.json` | 4 次正式入口捕获通过，自比相同，同组 7 个变异负控全拒 |

负控针对扩展 offer、默认 header、压缩文本标记、错误处理 Binary、HA 出站压缩、回复内容及原始 TCP 损坏。它们是 7 种变异各对两组执行，不是 14 种独立协议边界。

`ts-a` 是保留的装置首败：MM 实际处理压缩消息成功，但初版断言没有遵守消息 `.trim()`；HA token 错放 `extra`，正式产品报 HASS_TOKEN 未设置且未连接 WS。随后按固定 loader 的正式 token 字段修正装置，再完成上述两轮，没有把首败登记成产品差异。

实测共同握手默认头为 `connection: upgrade`、`upgrade: websocket`、`sec-websocket-version: 13`、`sec-websocket-extensions: permessage-deflate; client_max_window_bits`、`accept: */*`、`accept-language: *`、`sec-fetch-mode: websocket`、`user-agent: node`、`pragma: no-cache`、`cache-control: no-cache`、`accept-encoding: gzip, deflate`，另有随机 Host/Sec-WebSocket-Key。MM 额外发送 Origin；HA 没有 Origin。服务端实际协商响应是 `Sec-WebSocket-Extensions: permessage-deflate`，没有 no_context_takeover/window 参数。

正常 `posted` / `state_changed` 的 TCP 帧确实带 RSV1，持续解压后为预定 JSON，正式产品随后把正确文本送到模型并产生正确回复。不是只发送压缩 offer 却实际使用未压缩帧。

压缩 Binary 中也是合法事件 JSON，内容为 `/new binary-must-ignore`。两渠道都忽略它，既没有模型请求，也没有 `/new` 回复。随后压缩文本仍成功处理。`context.mjs` 对完整原始流先重验，再只移除 Binary 帧后重新持续解压，仍保留此前 HA auth/result 等消息；两渠道都因缺失字典得到 `Z_DATA_ERROR: invalid distance too far back`，实测报告为 `context-dependency-verified.json`。这证明 transport 必须先 inflate Binary 维护上下文，然后由 adapter 忽略 Binary。`mattermost-deflate.json` 保存 MM 实测两个压缩帧的原始 payload，供真实 Rust TCP/WS 模块测试回放；同一脚本核对夹具与正式捕获逐字节一致。

字典审计新增时发现两处装置问题，均只修脚本并重新离线重验，没有改原始捕获：早期普通捕获没有后来增加的 `fragmented: false` 字段，因此改为直接核对 FIN/opcode；zlib 错误事件不会保证调用 flush callback，因此解压 Promise 同时监听 error 并 reject，避免负控以未完成的 top-level await 退出。过程记录在 `context-audit-repairs.md`。

分片轮中，第一片是 `FIN=0, RSV1=1, opcode=1`，最终 continuation 是 `FIN=1, RSV1=0, opcode=0`。中间插入 Ping，实际收到 Pong 后才发送最终片；最终片之前 provider 请求数是 0。由此覆盖分片组合、交错控制帧以及完成前不提交半条消息。

HA 的 auth、subscribe_events、约 4.7 KiB call_service 三条出站文本都 masked 且 RSV1=0，即未压缩。MM 只有 Pong/Close，没有 WS 应用文本；因此本证据不能声称实测了 MM 出站应用文本压缩选择。

这里没有测过 server_no_context_takeover、各 window 参数、非法 RSV/mask/扩展组合。共享 transport 作者单独核查 Node 内置 Undici 并补这些探针；不能用 `node_modules/undici` 替代实际 Node 内置版本，或只按 RFC 推测基线行为。

## Rust 源码证据和接入边界

只读本机 Cargo registry 中 `tungstenite-0.28.0`：

- `src/handshake/client.rs:263–268` 对未请求的 extension 响应校验只有 TODO，因此不是握手代码先拒绝压缩扩展。
- `src/protocol/mod.rs:636–645` 对任意 RSV1/2/3 返回 `ProtocolError::NonZeroReservedBits`。只增加 offer header 无法处理真实压缩帧。
- `src/protocol/frame/frame.rs` 的公开 `FrameHeader` 与 `Frame::from_payload` 可以保留原始帧信息；`FrameCodec` 是模块私有接口，不能直接从外部复用。
- 本地已缓存并锁定 `flate2 1.1.10`，`src/mem.rs:404` 的 `Decompress::new(false)` 支持 raw deflate；`new_with_window_bits` 受 `any_zlib` feature 限制且范围是 9–15，不能假设默认后端可直接调用。共享 transport 作者随后以 Node 内置实现的 20 格双跑（`_tmp/20261113-h1-ws-deflate/double-run-compare.json`）确认 window 8 也成功，因此完整兼容方案改为直接使用已有锁定的 `zlib-rs 0.6.8`，不能停在 flate2 的窗口包装上。依赖/feature 由共享作者与父协调统一，本文不修改 Cargo。

最小接入位置是共享代理/TLS socket 与 tungstenite 之间的 incoming frame 解码层，必须在 HTTP 握手开始前安装：101 与第一个压缩消息可能同一次 read 返回，握手完成后再包 socket 会漏掉 tungstenite 已缓存的尾帧。成功协商后解压所有压缩数据帧、清 RSV1、重写长度，保留 opcode/FIN 与控制帧处理；保持字典、支持 continuation 和控制帧及时通过。不得把非法 masked/RSV/control 帧悄悄修成合法帧。具体协商和错误行为以共享作者的 Node 实测为准；其独立探针已确认该 Node 版本协商 no_context_takeover 后仍保留字典，不能凭 RFC 自行加 reset。本目录正式渠道捕获只协商默认 context，独立原生协议探针不能冒充额外正式渠道运行。

Mattermost 产品仅窄改为忽略 `Message::Binary`，由共享层先解压。新增 Rust 模块测试重放正式捕获的 Binary→text，断言只有正常文本进入原会话、没有 `/new` 发帖；真实共享 decoder 未挂载时该测试应失败。13 项 Rust 测试尚未运行，未宣称 Rust 正式入口通过。父协调首次统一 all-targets check 退出 101（`_build/20261113-042603-h1-first-check/output.log`）；本模块的 String→JsValue 构造与弃用 Atomic 方法已局部修正，文件级 rustfmt 通过，等待统一复查。作者没有自行运行 Cargo。共享传输与注册装配仍由父协调统一验收。
