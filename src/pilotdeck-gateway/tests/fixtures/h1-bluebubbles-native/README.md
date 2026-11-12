# BlueBubbles生产集成特征证据

固定参考cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，Node22.22.0。`stop-oracle.mjs`核源码/dist转译身份、清空环境并隔离HOME/PILOT_HOME，仅随机loopback HTTP；实际TS start/poll/stop及脚本held Gateway generator。它是组件证据，不是正式server/provider SSE验收。

用已安装固定Node在根工作树新目录运行：

```sh
/opt/homebrew/bin/node src/pilotdeck-gateway/tests/fixtures/h1-bluebubbles-native/stop-oracle.mjs _tmp/bluebubbles-stop-new
```

2026-11-13 a/b两轮oracle字节相同SHA519dbd7006e9f926bbf24bdb3e40140d8557b60749654b219d7472ab8f014bfd，原始目录 `_tmp/20261113-bluebubbles-stop-a`与`-b`。afterStop exited=false，释放held门后才exited=true，stop后无POST。新Rust期望准确owned取消排空，不能直接自比TS旧停止状态。

新6项Rust owned_tests尚未集中编译/执行；旧BlueBubbles gold replay继续验证legacy组件行为。正式集成仍需真实native mount→模型/工具→permission/elicitation答案、ConfigStore代理热更新、held-turn disable/re-enable、资源释放以及错误正文/漏取消负控。不能把此模块oracle或mock注入transport当成正式入口通过。
