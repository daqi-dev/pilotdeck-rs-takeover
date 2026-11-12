# ui-smoke：原版 ui/server 接 Rust gateway

把基线的 Web 界面服务端（`resources/PilotDeck/ui/server`，基线 cc7c4e6，不改它的源码）分别接到基线 gateway 和 Rust gateway 上，用脚本扮演浏览器跑一遍典型前端流程，比较两组的全部输出。它证明的是：**真实上游 ui/server 实际发出的 gateway 请求，Rust 的应答与基线一致到 ui/server 返回给前端的每个字节（归一化后）**。它不是验收：流程只覆盖下面列的操作，模型是假的。

## 怎么跑

在仓库根目录，先构建 Rust（`cargo build -p pilotdeck-cli -p pilotdeck-gateway -p pilotdeck-transport`），然后：

```sh
export UI_SMOKE_ROOT=/private/tmp/pdrs-ui-smoke/root   # 隔离根目录（默认就是它），两组先后用同一个
node scripts/ui-smoke/run.mjs ts   _build/ui-smoke         # A 组：基线 gateway + 原版 ui/server
node scripts/ui-smoke/run.mjs ts   _build/ui-smoke ts2     # 基线再跑一次（自比，证明哪些值本来就不稳）
node scripts/ui-smoke/run.mjs rust _build/ui-smoke         # B 组：Rust gateway + 原版 ui/server
node scripts/ui-smoke/compare.mjs _build/ui-smoke ts ts2   # 应为 3 处（见「已知不稳」）
node scripts/ui-smoke/compare.mjs _build/ui-smoke ts rust
```

每组约 1.5 分钟（基线 gateway 停机要多等 15 秒，见下）。只依赖 Node 22 自带模块。

端口（`UI_SMOKE_PORT_BASE`，默认 39100）：39100 假 provider，39101 gateway，39102 转发器，39103 ui/server，全部只监听 127.0.0.1；18789 有监听者时拒跑。

## 组成

| 文件 | 作用 |
|---|---|
| `run.mjs` | 建隔离目录（`HOME`、`PILOT_HOME`、`TMPDIR` 都在根目录下，预写 `pilotdeck.yaml` 与固定的 `server-token`），按序起 provider → gateway → 转发器 → ui/server，跑 `drive.mjs`，最后按进程号逐个停（停机结果写 `<侧>.stop.json`） |
| `provider.mjs` | 假模型（OpenAI chat completions），按最后一条消息里的标记回文本 / read_file / write_file / ask_user_question / 挂住 |
| `proxy.mjs` | ui/server 与 gateway 之间的透明转发器，WS 帧与 HTTP 请求逐条记到 `<侧>.gateway.jsonl`（去掉 `Sec-WebSocket-Extensions` 以免压缩） |
| `drive.mjs` | 扮演前端：HTTP API + `/ws` 聊天 WebSocket 的典型流程，再按 `routes.mjs` 把其余路由各打一次 |
| `routes.mjs` | ui/server 的完整路由清单（带源码位置），不打的写明原因 |
| `compare.mjs` | 归一化两侧产物并用 `scripts/bbx/diff.mjs` 比较 ui 输出、gateway 流量、provider 请求；生成 method 清单 `<侧>.inventory.json` |
| `halfopen-shutdown.mjs` | 复现：WS 客户端不发 close 帧就断开后 gateway 能否响应 SIGTERM 退出 |

ui/server 的起法与生产一致（`ui/server/webRuntimeSupervisor.js:27-38`）：`cwd=resources/PilotDeck/ui`，`node --import tsx server/index.js`，它从源码 import 基线的 gateway 客户端（`ui/server/pilotdeck-bridge.js:60`）。用到的环境变量：`SERVER_PORT`、`HOST=127.0.0.1`（缺省 `0.0.0.0`，`index.js:3584`）、`PILOTDECK_GATEWAY_URL`（缺省 `ws://127.0.0.1:18789/ws`，`pilotdeck-bridge.js:77`）、`PILOTDECK_SKIP_BROWSER_OPEN=1`（否则会打开浏览器，`index.js:3695`）、`PILOT_HOME`（token 文件 `$PILOT_HOME/server-token`，`pilotdeck-bridge.js:79`；数据库 `$PILOT_HOME/auth.db`，`load-env.js:50`）。本机登录默认关闭（`constants/config.js:16`），不需要取 token。

gateway 与 ui/server 都在 `sandbox-exec` 里跑：只许连 localhost、只许写根目录与 `/dev`、读真实 `~/.pilotdeck` 被拒。跑完各进程日志里没有一条被拒的操作，说明这套流程不联网、不写根目录之外。

## 比较规则（compare.mjs 开头有完整说明）

- ui 侧：去掉 ui/server 异步广播帧（`loading_progress` / `projects_updated` / `session-activity`）和 ui/server 自己生成的随机值（memory dream id、API key、VAPID 公钥、导出文件名时间、下载 zip）。
- gateway 侧：去掉请求 id；按 method 分组、组内按发出顺序比；项目快照轮询（`list_projects` / `list_sessions` / `describe_project`）只比去重后的 (method, params) 集合。
- provider 侧：按命中规则分组、组内按到达顺序比请求的方法、路径、头、体。
- 之后交给 `scripts/bbx/diff.mjs --self`（UUID、时间戳、毫秒数、进程号归一化）。

## 已知不稳（基线自比也有）

带附件那一轮（`turn-with-attachment`）的 `context_budget` 事件里，消息部分的本地 token 估算随上传目录里的随机 UUID 变 1–2 个（`submit_turn[10].events[3]` 的 used / ratio / breakdown 等 6 处，ui 侧对应 3 处）。

## 不覆盖

- `routes.mjs` 里标了 skip 的路由（联网、起 claude / task-master CLI、弹 GUI、让 ui/server 退出）；`/shell` 与 `/plugin-ws/*` 两个 WS 路径。
- StaffDeck / 知识库 / SOP 绑定未配置，`/api/module-host/*`（ui/server 走 HTTP 调 gateway 的那一路）这次没有被触发。
- 重连、gateway 重启、并发多标签页、多个浏览器连接。
