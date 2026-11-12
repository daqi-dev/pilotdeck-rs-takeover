# H1 Home Assistant：正式入口与局部行为验证

固定基线是 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。驱动要求 Node 22.22.0，读取基线已有 `dist` 和 `ws` 依赖，不构建或修改参考库。所有产品进程使用独立 HOME、PILOT_HOME、项目、TMPDIR 和随机 loopback 端口；凭据仅为 `fixture-*`。不继承宿主代理或账户环境，不接触真实服务。

`capture.mjs` 运行正式 `pilotdeck server --host 127.0.0.1 --port 0`，没有产品模块替换、虚假 Gateway 或全局 WebSocket 注入。HA WebSocket peer 与 OpenAI SSE provider 都是真实本地监听器。它验证：

- `auth_required → auth`、`auth_ok → subscribe_events`；订阅返回 `success:false` 仍完成启动并收发。
- 官方 loader 只传 `extra.url/token`，配置中的 watchPrefixes/title 不会生效；URL `/prefix/` 变成 `/prefix/api/websocket`。
- state 前缀、事件类型、相同状态、空白过滤，Unicode 文本、`persistent_notification.create` 格式。
- `/new` 确实移除先前会话历史；忙时普通消息和 `/new` 均跳过；重连保留新会话历史。
- 真实五秒重连、连接间 ID 递增、SIGINT 自然退出。
- `--interactions`：真实 `ask_user_question` → 渠道数字选择 → Gateway 回答 → 模型继续；真实 bash 权限请求 → 渠道拒绝 → 确认及模型继续，并检查被拒绝命令未创建隔离项目文件。
- `--stop-probe`：模型 SSE body 挂起时写隔离配置禁用渠道；保持 Gateway 存活、检查 WS 关闭和 provider 连接，再释放旧模型，观察旧 adapter 的回复失败。
- `--cron`：正式 Gateway WS hello、`cron_create`、`cron_run_now`，真实模型完成后通过 Home Assistant 通知，最后用 `cron_list` 确认复制出来的新任务已完成。使用隔离 PILOT_HOME 对应的 General workspace；新进程 cwd 本身不是已注册 dialog project。
- `--reconnect-rejected`：成功连接关闭后五秒重连，但 HTTP 403 只发 ErrorEvent，不会合成 close；再等 5.5 秒没有第三次握手。停止时再发一次 ErrorEvent。

`startup.mjs` 独立验证正式入口的无 token、HTTP 403 握手失败、鉴权拒绝和真实二十秒鉴权超时。四条失败路径都返回 noop 渠道，服务器仍可自然停止。HTTP 403 的第一条 ErrorEvent 不会触发 close，所以仍等完整鉴权计时器；日志严格为 ErrorEvent、鉴权超时、清理时第二条 ErrorEvent，不能把连接失败直接当成 close。

每次运行要求新的输出目录，保存驱动快照、固定源码提交、入口/可执行文件/驱动 SHA256、命令与隔离环境、stdout/stderr、原始 WS/模型请求、归一化 contract、PID、信号、退出状态和首次失败。通知 `gw_<毫秒>` 仅在 contract 中归一化，原始 wire 保留；随机端口、HOME 和会话 UUID 不进入 contract。正常场景保存前三次对话的非 system 历史，交互场景另检查真实工具结果中的所选答案/拒绝，原始完整模型请求全部保留。启动与重连实耗单独记录，不把机器调度抖动放入等值比较。

HA 的 service result 与后续 state 使用真实 ping/pong 往返隔开。收到通知不表示 adapter 已消费回执；如果 result 和下一 state 被 TCP 合批，基线的 busy guard 会正确跳过后一条。fixture 不用固定 sleep 猜回执是否已处理。

## 执行

以下命令从 Rust 工作树根目录运行；输出路径须不存在：

```sh
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$OUT_A"
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$OUT_B" "$OUT_A"
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$INTERACTION_A" '' --interactions
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$INTERACTION_B" "$INTERACTION_A" --interactions
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/startup.mjs "$TS_BASELINE" "$STARTUP_A"
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/startup.mjs "$TS_BASELINE" "$STARTUP_B" "$STARTUP_A"
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$STOP_A" '' --stop-probe
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$STOP_B" "$STOP_A" --stop-probe
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$CRON_A" '' --cron
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$CRON_B" "$CRON_A" --cron
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$REJECT_A" '' --reconnect-rejected
node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$REJECT_B" "$REJECT_A" --reconnect-rejected
```

有 compare 目录时同时做五个 contract 检查器负控：错误 token、通知 service、忙时多提交、订阅类型和丢失新会话历史。真实入口负控另跑：

```sh
HA_FIXTURE_FAULT=auth-token node src/pilotdeck-gateway/tests/fixtures/h1-homeassistant/capture.mjs "$TS_BASELINE" "$NEGATIVE_OUT"
```

这会修改**隔离配置**中的假 token，经正式 loader 和 WS 真实发送 `wrong-fixture-token`，校验器应以 token 断言失败、退出 1。它不修改基线、并非直接篡改已采集 JSON。核验失败原因必须是该 token 断言，不能把任何非零退出都算负控通过。

候选 Rust CLI 可以使用 `compare.mjs BASELINE RUST_BINARY TS_OUTPUT NEW_OUTPUT [basic|interactions|cron|startup|stop|reconnect-rejected] [ACCEPTED_STOP_DIFF_PATH]`，或导出的 `capture({ baseline, out, command: ABS_BINARY, args: ['server','--host','127.0.0.1','--port','0'], interactions, cron })` 和 `captureStartup(...)`。候选的可执行文件哈希取 `command`，不会把 `server` 子命令当文件读取。stop 比较要求根协调登记的差异文件；仅修改明确的 provider 取消/旧回复两项期望，不广泛忽略差异。

停止缺陷需要不同的明确期望。固定 TS 在渠道禁用后仍保持旧 provider 请求；授权修复后的 Rust 应在停止完成前取消并排空自身 turn。运行候选时传 `stopProbe:true, stopExpectation:'native-cancels'`，要求健康服务器、关闭 HA WS、关闭旧 provider，并且不尝试旧回复。不能直接把 TS 的“保留连接”结果作为 Rust 正确行为，更不能只用进程退出后的全局清理替代该观察。

## 局部 Rust 测试与独立证据

根协调集中运行：

```sh
cargo test -p pilotdeck-gateway channel::homeassistant --lib
```

新模块当前 24 个测试覆盖 UTF-16 mapper/render、JS 对象身份状态比较、数值 ID 和严格 true、10s 回执、cron 同一回执路径、失败提示重试、UTF-16 elicitation、忙时入站顺序、真实 WS/20s/5s、启动取消、stop 同步关闭入口、pending admission drop、精确 stream 取消失败保留 owner 并重试、成功 drain 后清 helper。HTTP 403 的真实 loopback 拒绝测试检查首连仍保留 20s deadline 和三条错误日志顺序，重连被拒绝后观察超过五秒确认没有第三次握手。持久 shutdown worker/receipt 验证首 stop 等待被丢弃后，第二次等待仍 Pending，只有旧资源释放后才完成；worker 异常和收割已结束的 JoinError 都保留失败。出站 worker 验证 cron caller 暂停或丢弃 receipt 时仍由 adapter 完成真实 WS 回执清理和断线，停止会 join 出站操作，完全不 poll 的 receipt 也不丢失所有权。双 worker 真实 WS 测试把两条未压缩 frame 合为一次 TCP write，验证空 Ready stream 仍只接受第一条；完成后的分次写入则均接受；正式 Node Blob binary 消息被忽略。

第 24 项回放 `compressed-ready.json` 的两个压缩帧。固定 TS adapter + 原生 Node WebSocket + 空脚本 Gateway 的两轮实际探针，均观察到同一 TCP read 中未压缩双帧为一次提交/一次 busy，而压缩双帧为两次提交/零 busy；脚本 Gateway 实际挂住首 turn 的负控变为一次提交/一次 busy，并由断言拒绝。该证据属于模块与真实网络组合，不是正式 Gateway。原始记录在 `_tmp/20261113-h1-homeassistant-compression/{c,d,negative-hold}`。共享 decoder 的 Pending 只拆开读取批次；HA 在批次 guard 释放后，等待本批自有消息任务第一次续体 poll 返回，确保空流完成，又不等待挂起流终结。专项测试用双 worker 和真实 WS 检查这两种情况，并以 Ping/Pong 确认挂起 turn 没有堵住网络读取。

独立审查在 `_tmp/20261113-h1-homeassistant-review/` 保存了直接 unchanged adapter + 真实 WS 的回执、10s/5s、单 read 双 frame 与分 read 实测。该证据属于模块层，不能冒充正式 server。权限/询问和 stop 所需的公共 owned-turn / helper 实现由共享作者负责并另行审查。

## 本轮证据及尚未闭合项

宿主产物位于 `_tmp/20261113-h1-homeassistant/`，不入库。`ts-a` 首次失败由 mock 误把非流式标题请求当主对话挂起引起；修正后 `ts-b/c`、增强历史的 `ts-d/e` 自比通过。`interaction-b` 保留了 ACK 与下一 state 合批导致 `/new` 被跳过的驱动失败；修正为 ping/pong 屏障后 `interaction-c/d` 自比通过。`startup-a/b` 三格、后续 `startup-d/e` 含 HTTP 403 的四格自比通过。`stop-a/b` 和当前驱动的 `stop-c/d` 证明固定基线在正式 disable/reload 后保留模型连接的资源缺陷。`reconnect-rejected-a/b` 证明重连握手错误不触发第三次连接。`negative-auth` 是实际错 token 负控。`cron-a` 至 `cron-e` 保留了装置构建中的失败：hello 缺 clientVersion、未启用 cron、未传 projectKey、cwd 未注册、误将 runNow 原任务 ID 当复制任务 ID；逐项按固定协议/实际结果修正，`cron-f/g` 已完整通过并自比。

首次 Rust 集中运行的原 23 项为 **22 passed / 1 failed / 0 ignored**，日志 `_build/20261113-045223-h1-homeassistant-first/output.log`。唯一失败是取消启动时对 peer Close/EOF 的断言；当前仅增强实际返回值及状态诊断，没有放宽断言。第 24 项及批次续体修改尚未运行，不能改写首批分母或成绩。作者未运行 cargo；编译、单测、正式候选对拍及装配检查由根协调统一执行。共享 proxy/noProxy/额外 CA 的 `ChannelNetwork` 已接入，每次握手读取当前快照，其网络测试由指定作者提供。正式停止修复还需与公共 owned-turn 身份/取消审查一起闭合。
