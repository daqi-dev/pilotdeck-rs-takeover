# bbx：黑盒对拍

同样的输入分别喂给基线（`resources/PilotDeck` 的 `dist`，Node 运行）和 Rust 实现，比较对外可观察的输出。接手后（2026-11-10）以它为判定「Rust 与基线一致」的主要手段。

## 它为什么可信

- **被测对象是真进程**：基线是 `node dist/src/cli/pilotdeck.js server --port N`，Rust 是编译出来的 `pilotdeck server --port N`，参数和环境变量两侧相同。采集器不 import 任何一侧的代码。
- **输入完全相同**：两侧先后在同一个固定路径 `/private/tmp/pdrs-bbx/root` 的干净目录里跑，同样的配置文件、同样的初始文件、同样的环境变量、同样的端口、同一个假 provider、同样的步骤。路径和端口因此不需要归一化。
- **记录的是原样输出**：WS 帧、HTTP 响应（状态、头、体）、CLI 的 stdout / stderr / 退出码、假 provider 收到的请求（方法、路径、头、体）、结束后 home 和 project 下的全部文件、停机退出码。
- **归一化规则都写在 `diff.mjs` 开头**，每条都是「基线自己连跑两次也会变」的东西：UUID、时间戳、进程号、请求指纹、耗时、两处随机路径段、会话文件里标题记录的落点。场景还可以声明 `volatile`（基线自比已证明不稳的路径）和 `accepted`（基线自身的 bug，Rust 不照做），两者每次比较都会打印略过了几处。不比较的字段只有三个（采集侧标记、启动耗时、服务进程日志）。
- **自检**：`check.mjs --self` 让基线跑两次互相比。必须 SAME，否则该场景不稳定，结论不能用。
- **能抓错**：见下面「负控」。

总共三个文件、不到 500 行，没有依赖。

## 用法

在仓库根目录：

```sh
cargo build --manifest-path src/Cargo.toml -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli

node scripts/bbx/check.mjs --self            # 基线自比，应全部 SAME
node scripts/bbx/check.mjs                   # 基线 vs Rust，全部场景
node scripts/bbx/check.mjs turn-text         # 只跑一个场景
```

产物默认在 `_build/bbx/`：每个场景两侧各一份原始采集 `<场景>.<侧>.json` 和一份差异 `<场景>.diff.txt`。`--out DIR` 可改位置。退出码 0 表示全部 SAME。

单步使用：

```sh
node scripts/bbx/run.mjs ts   scripts/bbx/scenarios/turn-text.json /tmp/a.json
node scripts/bbx/run.mjs rust scripts/bbx/scenarios/turn-text.json /tmp/b.json
node scripts/bbx/diff.mjs /tmp/a.json /tmp/b.json
```

采集是独占的（固定路径和端口 38411 / 38412；入口场景另外会用到 38413 / 38414），同一时刻只能跑一个；不要和它并行跑另一个 `check.mjs`。

## 场景格式

`scenarios/*.json`：

| 字段 | 含义 |
|---|---|
| `name` / `about` | 名字和它想证明什么 |
| `config` | `pilotdeck.yaml` 的内容；省略用默认（指向假 provider）；`null` 表示不写配置文件 |
| `files` | 初始文件，键是相对根目录的路径（`home/.pilotdeck/...`、`proj/...`），值是字符串或 JSON |
| `env` | 额外环境变量 |
| `provider` | 假 provider 的应答规则，按顺序匹配请求体文本：`when` 全部包含、`unless` 全部不包含；`reply` 可以是 `{text}`、`{tool:{name,arguments}}`、`{status,body}`。规则里的字符串（含工具参数）可用下面的 `{ROOT}` 等占位符。请求按命中的规则名分组记录 |
| `server` | `false` 表示不起服务（纯 CLI 场景） |
| `steps` | 依次执行：`{ws, params, answer}`（`answer` 见下）、`{http: "GET /path", headers, body}`、`{cli: [参数...], stdin}`、`{serve: [参数...]}`（见下）、`{connect: true}`（重连；也可以给一个完整的 hello 帧）、`{disconnect: true}`（客户端主动断开）、`{sleep: 毫秒}` |
| `volatile` | 该场景里确认不稳定的差异路径（正则）。会被略过并打印。用之前先用 `--self` 证明它确实不稳定 |

`ws` 步骤可以带 `answer`，用来在这一步等待期间应答服务端发来的询问（权限询问、提问）：`"answer": [{"on": {"type": "permission_request"}, "ws": "permission_decide", "params": {"sessionKey": "s", "requestId": "{event.requestId}", "decision": "allow"}, "count": 1}]`。收集该步骤的帧时，每遇到一个属于本步骤、且 `on` 里各字段都与事件相等的事件帧，就用第一条还有余量的规则发一个请求；`params` 里的 `{event.字段}` 换成触发事件的字段值，`count` 是该规则最多应答几次（缺省不限）。发出的请求和它收到的帧记在该步骤的 `answers` 里，照常比较。两侧用的是同一段采集代码。

`cli` 和 `serve` 步骤可以带 `env`：盖在场景环境之上，值为 `null` 表示删掉该变量（例如去掉采集器默认给的 `PILOTDECK_GATEWAY_TOKEN`）。

`serve` 步骤用来比常驻命令（`pilotdeck server`）自己的 stdout / stderr / 退出码，这些在 `"server"` 起的服务上属于不比较的 `serverLog`。它用 CLI 起一个进程，等 stdout 里出现 `until`（默认 `WebSocket: `），依次执行 `then` 里的步骤，然后发 `signal`（默认 `SIGTERM`）并等进程退出；15 秒不退就 SIGKILL，如实记成 `signal: "SIGKILL"`。进程没等到 `until` 就自己退出（启动失败）时不执行 `then`，照样记输出和退出码。这类场景通常写 `"server": false`；`then` 里的 `http` / `connect` 连的是固定端口 38411，所以被起的进程要监听在那里（采集器默认给了 `PILOTDECK_GATEWAY_PORT=38411`）。发信号前客户端若还连着，基线会一直等到它断开，需要比停机时先 `{disconnect: true}`。

字符串里可用 `{ROOT}` `{HOME}` `{PILOT_HOME}` `{PROJECT}` `{MOCK}` `{GATEWAY}` `{TOKEN}`。

## 负控

改动 `diff.mjs`、`run.mjs` 或者怀疑尺子失灵时，做一次：在 Rust 产品代码里故意改坏一处对外输出（例如把某个错误码的字符串改一个字母），重新构建，跑对应场景，确认 `DIFF` 里出现这一处；然后还原。记录见 `state/takeover.md`。

## 边界

- 两侧在同一台机器、同一个 Node / 同一份构建上跑，不防本机被人动手脚，只防实现不一致。
- 假 provider 只有 OpenAI chat completions 形状。别的协议（Anthropic 等）需要时再加。
- 时序相关的行为（超时、并发竞态）不在这个手段的覆盖内，需要单独设计场景或用单元测试。
