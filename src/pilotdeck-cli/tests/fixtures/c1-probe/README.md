# C1 Gateway 健康探测特征

固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，Node `22.22.0`。

`generate.mjs` 调用固定基线的 `dist/src/gateway/client/probeServer.js` 正式探测函数，对应源码 `src/gateway/client/probeServer.ts:10-31`。所有服务使用本进程创建的随机回环端口，并显式传入测试 token；不会探测固定 18789 或读取实际 token 文件。

```sh
env HOME=/tmp/c1-probe-isolated-home PILOT_HOME=/tmp/c1-probe-isolated-pilot \
  node src/pilotdeck-cli/tests/fixtures/c1-probe/generate.mjs \
  /Users/da/ws/pilotdeck-rs/resources/PilotDeck
```

2026-11-13 连续两次运行的九项语义结果一致，登记为 `oracle.json`。覆盖完整响应头、挂起响应头、完整头但未完成的 body、非法 header、正常重定向、21 请求的跳数上限、带凭据的跳转、Fetch 禁用端口、非成功状态。`probe.rs` 的真实 TCP 测试读取该 oracle，并比较在线判定和请求数。

这组函数边界特征不能代替正式 `pilotdeck` 入口验证。固定 18789 的 cron/默认 CLI 场景仍由 E0 在 Docker 私有网络执行。Rust 测试及故障注入是否实际通过，以该轮构建证据为准。

另有未闭合线索：Node 收到未请求的 `100 Continue` 后返回 200，会将探测视为失败；共享 HTTP 传输是否相同尚待 Rust 实测。该线索未计入这九项已登记场景，不能据九项通过宣称完整 HTTP 解析行为均已对齐。
