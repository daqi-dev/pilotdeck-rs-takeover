# M1 Google 非流式 complete

固定 TS 为 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。生成器使用现有
`bindBaseline` 核验整棵只读源码，并记录实际入口相对 import 闭包和 Google SDK
SHA。不替换 `complete` 的结果、不注入 Google client、不使用外部服务。

`generate.mts` 的 35 格共 37 次 complete 调用、39 次真实回环 HTTP 请求。
入口包括 `createModelRuntime`、正式 title/suggestion 生成器及 token-saver judge。
锁定完整 path、headers、body、canonical response 或 thrown error，以及重试次数。
覆盖文本、中文、thinking/signature、并行 tool call/id 去重、SDK 原始响应投影、usage、
finishReason、空/损坏/原始类型 JSON、204、400、429/503 重试、断体重试、发送前取消、
读体取消、provider 整请求超时、超时 header 向上取整/与自定义同名 header 追加、
流式 idle budget 不作用于 complete。两个挂起响应必须实际观察客户端关闭连接。
仅去除 SDK 的传输元数据 `sdkHttpResponse` 及列出的 fetch 自动头，符合既有 Google
流式金样边界；其它响应字段、顺序和请求头均保留。

`formal.mjs` 另从固定源码启动真实 CLI `server`，鉴权 WS 后提交带
`sdkSessionConfig.promptSuggestions` 的会话。配置 Google agent、Google judge，
实际完成标题、建议和模型选择，检查持久标题、建议事件、历史消息及 selected model
的真实 HTTP URL。它支持 `M1_BINARY=/absolute/path/to/pilotdeck` 重放 Rust；不以
函数探针代替正式入口。`read_session_messages.stream.events` 是近期事件快照；
判断建议未进入对话历史时只读取 `messages` 字段。

运行时必须为每轮新建隔离 HOME/PILOT_HOME；以下命令从 C1 checkout 根执行，
`RUN` 指向新的 `_tmp/20261113-m1-google-complete/<run>`：

```sh
mkdir -p "$RUN/home" "$RUN/pilot"
HOME="$RUN/home" PILOT_HOME="$RUN/pilot" \
PDRS_BASELINE=/Users/da/ws/pilotdeck-rs/resources/PilotDeck LANG=en_US.UTF-8 \
node --import /Users/da/ws/pilotdeck-rs/resources/PilotDeck/node_modules/tsx/dist/loader.mjs \
  src/pilotdeck-gateway/tests/fixtures/model-complete-google/generate.mts "$RUN"

HOME="$RUN/home" PILOT_HOME="$RUN/pilot" \
PDRS_BASELINE=/Users/da/ws/pilotdeck-rs/resources/PilotDeck \
node src/pilotdeck-gateway/tests/fixtures/model-complete-google/formal.mjs "$RUN"
```

两轮 oracle 用 `check.mjs <expected> <actual>` 全量比较。`node --test check.test.mjs`
的 5 个变异是校验器负控；设置 `M1_NEGATIVE_RESPONSE=1` 则由真实 HTTP peer 返回
错误标题，正式 consumer/CLI 的正向标题断言必须失败。这些不是 Rust 产品变异负控。
原始失败与修正后的记录均保留在工作区 `_tmp/20261113-m1-google-complete/`。

Rust 专属测试为 `tests/model_complete_google.rs`：第一项通过
`HyperModelTransport` 回放全部真实 HTTP 观察；第二项通过正式
`NativeModelInvocationProvider -> RouterRuntime::decide` 验证 judge 选择和失败事件。
现有 `model_complete` 三协议与 `model_stream_google`、`http::google_tests` 应一起回归。
作者不运行 cargo；编译、定向测试、Rust 正式入口及产品变异负控由集中验证执行。

本补丁不接入 invocation audit、不实现 durable artifact hydration；两项已作为
独立 M1 缺口提案上报，不能因 Google complete 完成而标成已覆盖。
