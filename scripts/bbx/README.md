# bbx：黑盒对拍

同样的输入分别喂给基线（`resources/PilotDeck` 的 `dist`，Node 运行）和 Rust 实现，比较对外可观察的输出。接手后（2026-11-10）以它为判定「Rust 与基线一致」的主要手段。

## 它为什么可信

- **被测对象是真进程**：基线是 `node dist/src/cli/pilotdeck.js server --port N`，Rust 是编译出来的 `pilotdeck server --port N`，参数和环境变量两侧相同。采集器不 import 任何一侧的代码。
- **输入完全相同**：两侧先后在同一个固定路径 `/private/tmp/pdrs-bbx/root`（非 macOS 上是等长的 `/tmp/private/pdrs-bbx/root`，让由路径派生的文本与 macOS 等长，避免截断点、token 估算随平台移动；场景里写死的路径、项目目录名 `private-tmp-pdrs-bbx`、百分号编码的写法读入时一并替换）的干净目录里跑，同样的配置文件、同样的初始文件、同样的环境变量、同样的端口、同一个假 provider、同样的步骤。路径和端口因此不需要归一化。
- **记录的是原样输出**：WS 帧（重连前后各条连接上的通知都保留）、HTTP 响应（状态、头、体）、CLI 的 stdout / stderr / 退出码、假 provider 收到的请求（方法、路径、头、体）、结束后 home 和 project 下的全部文件、停机退出码。落盘文件：文本文件比全文和权限；非 UTF-8 文件比长度、sha256 和权限；`.json` / `.jsonl` 比解析结果，另比一份原文指纹（行数、是否以换行结尾、每行排版骨架的哈希计数），缩进、空格、换行、转义写法、数字写法（`1` 与 `1.0`）的差异因此也会报出来。
- **归一化规则都写在 `diff.mjs` 开头**，每条都是「基线自己连跑两次也会变」的东西：UUID、时间戳、进程号、请求指纹、耗时、两处随机路径段、会话文件里标题记录的落点、调试开关 PILOTDECK_DUMP_REQUEST 写的转储文件名。场景还可以声明 `volatile`（基线自比已证明不稳的路径）和 `accepted`（基线自身的 bug，Rust 不照做），两者每次比较都会打印略过了几处。基线 vs Rust 时，`accepted` 里某条正则一处都没命中会报 `STALE` 并计 1 处差异（过期的豁免让场景变红）；`volatile` 一处没命中只提示 `STALE?`。不比较的字段只有四个（采集侧标记、启动耗时、服务进程日志、失败汇总；失败汇总由 `check.mjs` 单独判定）。
- **失败不算一致**：启动失败、步骤超时（含被采集器杀掉的 `cli` / `serve`、`disconnect` 等不到关闭）、连不上、停机等不到退出，都记进产物的 `failures`。只要有未声明为预期的失败，`check.mjs` 就判 `INVALID`，两侧失败得一模一样也不行。
- **二进制是新的**：跑 Rust 侧之前，`run.mjs` 检查 `src/target/debug/pilotdeck`（和 `pilotdeck-agent-loop-sidecar`）不比 `src/` 下（不含 `target/`）的 `Cargo.toml`、`Cargo.lock` 和它依赖的 `.rs`（cargo 依赖清单 `<二进制>.d` 里列的；没有清单就看全部 `.rs`）旧，旧了拒跑。`BBX_ALLOW_STALE=1` 跳过，`BBX_SRC` 可指定源码目录（默认 `BBX_TARGET` 往上两级）。
- **自检**：`check.mjs --self` 让基线跑两次互相比。必须 SAME，否则该场景不稳定，结论不能用。
- **能抓错**：见下面「注入错误检查」。

总共三个脚本、不到 700 行，没有依赖。

## 用法

在仓库根目录：

```sh
cargo build --manifest-path src/Cargo.toml -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli

node scripts/bbx/check.mjs --self            # 基线自比，应全部 SAME
node scripts/bbx/check.mjs                   # 基线 vs Rust，全部场景
node scripts/bbx/check.mjs turn-text         # 只跑一个场景
```

产物默认在 `_build/bbx/`：每个场景两侧各一份原始采集 `<场景>.<侧>.json` 和一份差异 `<场景>.diff.txt`。`--out DIR` 可改位置。每个场景的判定是 `SAME`、`DIFF N`、`INVALID`（有非预期的失败，括号里是差异结论和失败位置）或 `RUN FAILED`（采集器没跑完，例如锁被占、二进制比源码旧）。退出码 0 表示全部 SAME。

单步使用：

```sh
node scripts/bbx/run.mjs ts   scripts/bbx/scenarios/turn-text.json /tmp/a.json
node scripts/bbx/run.mjs rust scripts/bbx/scenarios/turn-text.json /tmp/b.json
node scripts/bbx/diff.mjs /tmp/a.json /tmp/b.json          # 两份基线互比时加 --self，不做 STALE 检查
```

采集在一个槽位里是独占的（固定目录和端口；缺省槽位用 `/private/tmp/pdrs-bbx` 和 38411–38414），`check.mjs` 遇到锁会排队等。要并行跑，给每个并行方一个不同的槽位：`BBX_SLOT=1 node scripts/bbx/check.mjs`（1–8）。槽位只把目录名和端口换成等长的另一套，场景里写死的同名路径和端口读入时一并替换；两侧在同一槽位里跑，结论不受影响，但不同槽位的采集产物不要互相比。

改 `diff.mjs` 之后先跑比较器自测：`node scripts/bbx/selftest.mjs`。它给比较器喂成对的小产物，检查每条归一化规则该抹的抹掉了、规则旁边「差一点」的改动仍然报差异。新增归一化规则时同时加这两类例子。

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
| `steps` | 依次执行：`{ws, params, answer}`（`answer` 见下）、`{http: "GET /path", headers, body}`、`{cli: [参数...], stdin, cwd, env}`、`{serve: [参数...]}`（见下）、`{connect: true}`（重连；也可以给一个完整的 hello 帧）、`{disconnect: true}`（客户端发关闭帧断开；不发关闭帧的几种见下）、`{sleep: 毫秒}`、`{sidecar: "start" / "stop"}` 与 `{tcpProxy: "start" / "cut"}`（见下） |
| 步骤的 `timeout` | `ws` / `http` / `cli` / `serve` 等待的毫秒数，缺省 30000 |
| 步骤的 `expectTimeout` | `true` 表示这一步在基线上本来就会超时（如等一个采集器不回答的询问、命令常驻不退出）。只有声明了的超时不判 `INVALID`；连不上不能这样声明 |
| `expectStartFailure` | `true` 表示 `"server"` 起的服务预期启动失败。`serve` / `cli` 步骤里进程自己退出是被比较的输出，不需要声明 |
| `settleMs` | 最后一步之后、收尾前等待的毫秒数，缺省 300 |
| `volatile` | 该场景里确认不稳定的差异路径（正则）。会被略过并打印。用之前先用 `--self` 证明它确实不稳定 |
| `accepted` | 已接受差异 `[{id, why, paths: [正则]}]`，只用于基线自身的 bug（Rust 有意不照做）。命中的差异略过并打印条数；基线 vs Rust 时某条正则一处都没命中报 `STALE`，计 1 处差异 |

`ws` 步骤可以带 `answer`，用来在这一步等待期间应答服务端发来的询问（权限询问、提问）：`"answer": [{"on": {"type": "permission_request"}, "ws": "permission_decide", "params": {"sessionKey": "s", "requestId": "{event.requestId}", "decision": "allow"}, "count": 1}]`。收集该步骤的帧时，每遇到一个属于本步骤、且 `on` 里各字段都与事件相等的事件帧，就用第一条还有余量的规则发一个请求；`params` 里的 `{event.字段}` 换成触发事件的字段值，`count` 是该规则最多应答几次（缺省不限）。发出的请求和它收到的帧记在该步骤的 `answers` 里，照常比较。两侧用的是同一段采集代码。

`cli` 和 `serve` 步骤可以带 `env`：盖在场景环境之上，值为 `null` 表示删掉该变量（例如去掉采集器默认给的 `PILOTDECK_GATEWAY_TOKEN`）。

`serve` 步骤用来比常驻命令（`pilotdeck server`）自己的 stdout / stderr / 退出码，这些在 `"server"` 起的服务上属于不比较的 `serverLog`。它用 CLI 起一个进程，等 stdout 里出现 `until`（默认 `WebSocket: `），依次执行 `then` 里的步骤，然后发 `signal`（默认 `SIGTERM`）并等进程退出；15 秒不退就 SIGKILL，如实记成 `signal: "SIGKILL"`。进程没等到 `until` 就自己退出（启动失败）时不执行 `then`，照样记输出和退出码。这类场景通常写 `"server": false`；`then` 里的 `http` / `connect` 连的是固定端口 38411，所以被起的进程要监听在那里（采集器默认给了 `PILOTDECK_GATEWAY_PORT=38411`）。发信号前客户端若还连着，基线会一直等到它断开，需要比停机时先 `{disconnect: true}`。`then` 里的步骤同样可以 `save` 和引用 `{saved.名字}`。

不发 WS 关闭帧的断开：`{disconnect: "destroy"}` 直接关掉底层 TCP（FIN，模拟客户端进程被杀），`{disconnect: "reset"}` 以 RST 断开（SO_LINGER 0），`{disconnect: "end"}` 只关写方向（半关闭：发 FIN 但照常接收服务端的帧，之后可以再 `destroy`；在那之前不要 `connect` 新连接，两条连接的帧会混在一起）。`{vanish: N}` 另建 N 条 WS 连接（与场景的主连接无关，逐条依次建），每条 hello 并等到第一帧非通知的应答后直接关掉 TCP（FIN），模拟 N 个客户端进程被杀；结果是 `{opened, replies}`（`replies` 按应答帧的 `type` 计数，`__timeout` 计为超时失败，`connectFailed` 计为连不上），这些连接上的帧不进步骤结果和通知。前两种结果里的 `close` 是客户端随后看到的关闭（1006）。采集器只在建 WS 连接期间、只对连向 gateway 端口的那次 `net.connect` 记下套接字，别的连接不受影响。注意基线对只收到 FIN 的连接不做任何处理（`src/gateway/server/websocket.ts:17-21` 不听 `end`），服务端此后没再写过它时停机会挂住，用 `serve` 起服务才能把这一点比进去。

`sidecar` 步骤起停一个外部常驻的 agent loop sidecar（TCP 部署用）：基线是 `node dist/src/cli/pilotdeck-agent-loop-sidecar.js`，Rust 是 `pilotdeck-agent-loop-sidecar`。`{"sidecar": "start", "port": 38414, "env": {...}}` 用场景环境盖上步骤 `env` 起进程，等它在 `port` 上开始监听（进程没等到监听就退出时记下退出码、信号、stdout、stderr）；`{"sidecar": "stop", "signal": "SIGKILL"}` 给最近起的那个发信号（缺省 SIGTERM），等它退出并记下退出码、信号、stdout、stderr（15 秒不退就 SIGKILL）。场景结束时还在跑的由收尾按进程组杀掉，不记输出。sidecar 是否监听 TCP 由环境里的 `PILOTDECK_AGENT_LOOP_TCP_PORT` 决定，和 gateway 用同一套变量。

`tcpProxy` 步骤在采集器里起一个透明的 TCP 转发，用来在 sidecar 不退出的情况下断开它与 gateway 之间的连接：`{"tcpProxy": "start", "port": 38413, "target": 38414}`；`{"tcpProxy": "cut"}` 立刻断开现有的全部连接，结果里的 `live` 是断开前还连着的连接数（代理继续监听）。要在一轮进行中打断，在假 provider 的 `reply` 上写 `"cutProxy": true`（断开代理上的连接）或 `"killSidecar": "SIGKILL"`（给当前 sidecar 发信号）：命中这条规则时先做这件事，再照常应答（可配 `delayMs`）。

假 provider 的 `reply` 除了 `{text}`、`{tool}`、`{status, body}` 之外还可以组合：`tools`（多个工具调用，可与 `text` 同时给）、工具上的 `rawArguments`（原样当参数字符串，用来回非法 JSON）、`reasoning`（推理文本）、`chunks`（文本分多块流式发出）、`finish`（覆盖结束原因，如 `length`）、`usage`、`delayMs`（先等一会儿再回）、`breakAfter: n`（流里发出 n 块后断开连接）、`hangAfter: n`（发出 n 块后挂住，`hangMs` 后断开）、`endAfter: n`（发出 n 块后正常结束响应，流里没有结束原因 / `[DONE]` / `message_stop`）。请求路径按协议分流：以 `/messages` 结尾按 Anthropic messages 的形状应答；以 `/responses` 结尾按 OpenAI Responses（`finish` 是响应的 `status`：`completed` / `incomplete` / `failed`，`failed` 的错误对象取 `reply.error`）；`…/models/<模型>:generateContent` 或 `:streamGenerateContent` 按 Google Gemini（`finish` 是 `finishReason`，缺省 `STOP`；推理文本是带 `thoughtSignature` 的 `thought` 部分；工具调用只有显式写了 `id` 才带 `functionCall.id`）；其余按 OpenAI chat completions。`usage` 一律按 chat 形状写（可带 `prompt_tokens_details.cached_tokens`、`completion_tokens_details.reasoning_tokens`），各协议自行换算。

引用前面跑出来的值（服务端随机生成的 id 之类）：步骤或 provider 规则上写 `"save": {"名字": "正则"}`，正则在该步骤结果的 JSON 文本上（provider 规则则是在命中它的那次模型请求的原文上）匹配，第一个捕获组存成这个名字；之后的步骤参数和 provider 应答里用 `{saved.名字}` 引用。存下来的值不进产物，两侧各存各的。

字符串里可用 `{ROOT}` `{HOME}` `{PILOT_HOME}` `{PROJECT}` `{MOCK}` `{GATEWAY}` `{TOKEN}`。另有 `{SIDECAR}`：本侧 agent loop sidecar 的启动命令行（shell 写法，基线是 `node …/pilotdeck-agent-loop-sidecar.js`，Rust 是 `pilotdeck-agent-loop-sidecar`），两侧的值不同，只能放进不落盘、不进输出的地方，例如场景 `env` 里 `"BBX_SIDECAR": "{SIDECAR}"`，再让 `modules.agentLoop` 的 `module-stdio-v2` 绑定以 `command: /bin/sh`、`args: [-c, 'eval "exec $BBX_SIDECAR"']` 起它（见 `modules-tools-batch-sidecar-binding`）。

## 注入错误检查

改动 `diff.mjs`、`run.mjs` 或者怀疑尺子失灵时，做一次：在 Rust 产品代码里故意改坏一处对外输出（例如把某个错误码的字符串改一个字母），重新构建，跑对应场景，确认 `DIFF` 里出现这一处；然后还原。记录见 `state/takeover.md`。

不动 Rust 也能验证采集器本身：`BBX_BASELINE` 指向一个假的基线目录（只放一个 `dist/src/cli/pilotdeck.js`），两次用不同的假脚本跑同一个 `"server": false` 的场景，例如分别写出等长不同内容的二进制文件、或内容相同排版不同的 JSON，确认 `diff.mjs` 报出差异。

## 边界

- 两侧在同一台机器、同一个 Node / 同一份构建上跑，不防本机被人动手脚，只防实现不一致。
- 假 provider 有 OpenAI chat completions、Anthropic messages、OpenAI Responses、Google Gemini 四种形状（基线 `src/model/providers/` 下的全部协议）。Responses 只发 `response.failed`，不发顶层 `error` 事件；Google 只发 SSE，不发数组形式的流。

落盘的 sqlite 数据库按内容比：每张表的建表语句和全部行（行排序后比），不比文件字节。

## 已知不覆盖

- **协议帧和 HTTP 体按解析结果比，不比原始字节**：WS 帧、HTTP 响应体、发给 provider 的请求体都先 `JSON.parse` 再比，键的顺序会比，但空格、转义写法、数字写法（`1` 与 `1.0`）看不到。原文指纹只做在落盘的 `.json` / `.jsonl` 上。
- **时序与并发**：只比每一步结束后收集到的内容，帧之间的时间间隔、超时的具体时长、并发请求的交错、竞态都不在覆盖内；`expectTimeout` 只说明「两侧这一步都在等」，不说明等多久会怎样。需要单独设计场景或用单元测试。
- **归一化抹掉的写法**：ISO 时间整体换成 `<iso>`，小数位数（毫秒 / 微秒）、`Z` 与 `+00:00` 的差异看不到；耗时只比有无不比数值和位数；UUID 只比「同一个 id」的关系，不比大小写和版本位。
- **原文指纹的粒度**：只比每行骨架的计数，不比行的先后（先后由解析结果比）；字符串里只看转义序列，不看普通字符；文件里的字符串长度和数字的位数不在指纹里（它们的值由解析结果比）。
- **落盘范围**：只采 `home` 和 `proj` 两棵树；`TMPDIR`（`{ROOT}/tmp`）和其他位置的文件不比。`serverLog`（`"server"` 起的服务进程自己的输出）不比。
- **未声明的失败只判 INVALID，不判谁对**：一侧超时一侧不超时时，差异照常在 `steps` 里报，但场景整体是 INVALID，要先修场景（或确认是 Rust 的挂起）再看差异。
