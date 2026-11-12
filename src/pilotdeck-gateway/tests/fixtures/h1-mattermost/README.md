# H1 Mattermost 验证

固定源码 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，Node `22.22.0`。驱动只启动拥有的正式 `dist/src/cli/pilotdeck.js server` 子进程；HTTP/WS/provider 全部监听隔离随机 loopback 端口，HOME/PILOT_HOME/cwd 独立，无固定 18789，无产品模块替换。父协调者负责 Rust 正式装配、Cargo 与 E0 验证。

## 正式入口

```sh
node src/pilotdeck-gateway/tests/fixtures/h1-mattermost/server-probe.mjs \
  /Users/da/ws/pilotdeck-rs/resources/PilotDeck \
  "$PWD/_tmp/20261113-h1-mattermost/NEW_DIRECTORY" \
  root-url,api-prefix,encoded-token,rejected,missing,lifecycle,cron-unsupported,lifecycle-active
node src/pilotdeck-gateway/tests/fixtures/h1-mattermost/verify.mjs \
  LEFT_CAPTURE RIGHT_CAPTURE NEW_REPORT.json
```

每格保存命令、隔离环境、PID、源码/入口/驱动 hash、HTTP 请求原文本、WS upgrade URL/Origin、RPC 帧、provider 原始请求、stdout/stderr、退出码。随机端口归一化仅用于 Origin 比较；provider 自比只投影 model/stream/user 输入，未声称全 Gateway provider 请求对拍。原始字段均留存。

E0 验证原生正式入口时，在场景列表之后追加绝对 Rust 二进制路径（第四参数）；驱动直接启动该二进制的 `server` 命令并记录二进制 hash。Rust 模式的 `lifecycle-active` 期望 provider 已关闭且零迟到 POST；两项 null 场景期望 server 仍可响应 `/new`。这些是待父登记差异的验收期望，尚未运行 Rust、不可视为已经通过。`verify.mjs` 当前用途是八格 TS 自比，原始 TS/Rust 跨实现比较另由集成验收执行。

2026-11-13 TS `server-l` / `server-m` 首批八格两轮通过，自比一致、10 个负控被拒绝，产物在 `_tmp/20261113-h1-mattermost/self-compare-8.json`。追加六个 HTTP/options 场景在 `http-options-b/c` 两轮通过、自比一致，`http-options-self-compare.json` 记录 7 个负控被拒绝。另有两项 null 帧、BOM 与 me-null，共 18 个正式场景，均分别保留原始证据；这不是 Rust 的通过统计。

- root-url：REST `/users/me`，WS `/api/v4/websocket`，`/new` 回复与普通 turn 发帖。
- api-prefix：REST `/api/v4/users/me`，WS **`/api/v4/api/v4/websocket`**，保留基线路径拼接。
- encoded-token：REST 原 Bearer；query 首先 encodeURIComponent，随后 WHATWG URL 将单引号改为 `%27`。
- rejected / missing：正式启动记录错误且无 WS；不当成渠道已连接。
- lifecycle：断线约 4 秒重连；配置 disable 后 server 保活至少 4.5 秒、零新连接；重新 enable 新连接。
- cron-unsupported：先在隔离 projects 目录放合法 `.cwd` 注册，普通消息暖启动，再 `cron_create` / `cron_run_now`。确认模型收到 `fixture-cron`、无 error 事件，正式链打印 `result delivery was not handled`，无 cron 发帖。结论是**基线明确不支持**，Rust 维持 trait 默认 false。
- lifecycle-active：provider SSE 挂住时禁用渠道、server 保活 4.5 秒；TS provider 仍开，释放流后旧渠道还 POST 一条。Rust 的 owned cancel/drain 与无迟到发帖期望须由父登记窄差异并在正式入口验收；当前 TS 证据不能冒充 Rust 验收。

可单独传 `null-envelope,null-post` 核实无效上游帧导致正式 TS server 自主 exit 1（没有 stderr，驱动未先发终止信号）；`null-b/c` 两轮实测一致，记录 `null-self-compare.json`。Rust 安全忽略这两种 null 帧属于待登记的崩溃修复。

首次失败全部保留：server-a 对 ESM fallback 失败的装置假设错；server-b 忽略自动 title 请求；server-c 忽略 URL 对单引号二次编码；server-f 误写 RPC 帧类型；server-g 缺 projectKey；server-h/i/j/k 的 cron 未注册项目只能证明失败结果投递，不能证明成功 cron。server-l 起预置合法项目且校验 provider 请求及无错误事件。null-a 误期待 stderr 错误文本，实际只自主 exit 1。以上均不登记为产品差异。

## HTTP 与配置追加场景

- `redirect-get`：`/users/me` 302 → `/users/me-final`，同源保留 Bearer。
- `redirect-post`：`/posts` 307 → `/posts-final`，保留 POST、正文和 Bearer。
- `false-url` / `zero-url`：extra 允许 unknown，原值 false/0 走 required 日志，零网络；不能先 String() 转换。
- `nonascii-token`：中文 token 在 Headers ByteString 转换阶段拒绝，无 HTTP/WS，错误位置包含 Bearer 前缀的 7 个码元。
- `latin1-token`：é 在 REST Header 中按 Latin-1 字节，WS query 则按 UTF-8 encodeURIComponent。
- `bom-me`：GET UTF-8 BOM + JSON 正确识别 bot，bot 自己的 /new 被过滤，仅外部用户产生一条 POST。
- `me-null`：GET JSON null 导致 token-check TypeError，空 handle、零 WS；这与入站 null 帧杀死 server 的已取证缺陷不同，原生保留这个失败分支。

`verify-http.mjs LEFT RIGHT NEW_REPORT` 校验追加六格自比及 7 个负控。Rust HTTP 复用已有 `http::fetch::fetch` / `fetch_redirect`，不另造 redirect；全局 `ChannelNetwork` 由装配层共享，支持配置代理热更与额外 CA，WS 也使用同一对象。

## 模块专项证据

`value-oracle.mjs`：导入固定编译模块，真实 HTTP 调 `sendReply`，覆盖 6 个会话映射、11 个渲染事件、5 个 UTF-16 分块。`values-e/f` 两轮一致；生成的 `values.json` 供 Rust 单测读取。分块保留孤立代理项、空格、newline 边界。

`interaction-oracle.mjs`：真实 MattermostChannel 与真实共享 helper，Gateway 是明确的测试替身，HTTP 是 loopback。三条轨迹覆盖 elicitation、permission 队列、初始 prompt 500 后重试；`interactions-c/d` 两轮一致，产物 `interactions.json`。这是模块边界特征测试，**不是**正式 Gateway 权限/询问端到端证明。两脚本在导入产品模块前重置 HOME/PILOT_HOME/cwd 和继承环境；早期模块采样未显式重置继承环境，已用这些隔离运行替代，生成结果逐字节不变。

Rust `channel/mattermost/tests.rs` 的 12 项测试对以上值及消息文本作断言，并通过真实 HTTP/WS 验证 busy/new/thread、helper 交互和 owned stream 的取消；包括无等待 burst 顺序、取消失败保留精确 stream 后重试、取消 stop waiter 后下一 waiter 仍等待、HTTP redirect/BOM/me-null 与 false/0；尚待父统一构建执行。正式 Rust 注册、热更新、权限协调器、provider 连接取消、重复停止与无重连仍需集成验证，未标完成。
