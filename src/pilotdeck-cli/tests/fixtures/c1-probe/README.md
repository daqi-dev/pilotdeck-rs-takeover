# C1 Gateway 健康探测特征

固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，Node `22.22.0`。

`generate.mjs` 调用固定基线的 `dist/src/gateway/client/probeServer.js` 正式探测函数，对应源码 `src/gateway/client/probeServer.ts:10-31`。所有服务使用本进程创建的随机回环端口，并显式传入测试 token；不会探测固定 18789 或读取实际 token 文件。

```sh
env HOME=/tmp/c1-probe-isolated-home PILOT_HOME=/tmp/c1-probe-isolated-pilot \
  node src/pilotdeck-cli/tests/fixtures/c1-probe/generate.mjs \
  /Users/da/ws/pilotdeck-rs/resources/PilotDeck
```

2026-11-13 连续两次运行的十三项语义结果一致，登记为 `oracle.json`。覆盖完整响应头、挂起响应头、完整头但未完成的 body、非法 header、正常重定向、21 请求的跳数上限、带凭据的跳转、Fetch 禁用端口、非成功状态、100/102/103 后续成功响应，以及只发送 100 而不发送最终响应。`probe.rs` 的真实 TCP 测试读取该 oracle，并比较在线判定、请求数和确实写出的响应数。

这组函数边界特征不能代替正式 `pilotdeck` 入口验证。固定 18789 的 cron/默认 CLI 场景仍由 E0 在 Docker 私有网络执行。Rust 测试及故障注入是否实际通过，以该轮构建证据为准。

Node 收到未请求的 `100 Continue` 后返回 200，会将探测视为失败；同包和分包复核均如此。102/103 后收到 200 则成功。现已将三种状态纳入 oracle；共享 HTTP 传输是否通过以当前 Rust 运行记录为准。

Rust 首次新增 100 格的实际失败保留在 `_build/20261113-030422-probe-12/output.log`：`continue-then-ok` 返回 true，固定 TS 返回 false。新公开 `http/fetch.rs` 已注册每跳 1xx 回调，只在 100 时立即拒绝；原生 Node 22 另行实测错误为 `TypeError: fetch failed`，cause code 为 `UND_ERR_SOCKET`。新增 Gateway 测试 `unsolicited_continue_rejects_without_a_final_response_or_probe_timer` 直接调用 fetch，没有使用 CLI 的 200 ms 定时器，mock 永不发送最终响应；它同时检查返回错误和连接释放。修复是否通过，以集中运行记录为准。

首次 Rust 集中测试中，主 bin 的九项测试通过，嵌入 `sdk_signal_exit` 的同一测试在 mock 读取请求时报 `WouldBlock`。原日志保留在 `_build/20261113-025233-cli-tests/output.log`。独立 macOS 随机端口探针证实 `accept` 继承监听 socket 的 `O_NONBLOCK`；`set_read_timeout` 不清除此标记。测试服务器已显式将已接受 socket 改为 blocking，并按短周期处理取消/EOF；没有延长产品 200 ms 预算，未收到请求或未写出响应仍由精确计数判失败。
