# bbx：黑盒对拍

同样的输入分别喂给基线（`resources/PilotDeck` 的 `dist`，Node 运行）和 Rust 实现，比较对外可观察的输出。接手后（2026-11-10）以它为判定「Rust 与基线一致」的主要手段。

## 它为什么可信

- **被测对象是真进程**：基线是 `node dist/src/cli/pilotdeck.js server --port N`，Rust 是编译出来的 `pilotdeck server --port N`，参数和环境变量两侧相同。采集器不 import 任何一侧的代码。
- **输入完全相同**：两侧先后在同一个固定路径 `/private/tmp/pdrs-bbx/root`（非 macOS 上是等长的 `/tmp/private/pdrs-bbx/root`，让由路径派生的文本与 macOS 等长，避免截断点、token 估算随平台移动；场景里写死的路径、项目目录名 `private-tmp-pdrs-bbx`、百分号编码的写法读入时一并替换）的干净目录里跑，同样的配置文件、同样的初始文件、同样的环境变量、同样的端口、同一个假 provider、同样的步骤。路径和端口因此不需要归一化。
- **记录的是原样输出**：WS 帧（重连前后各条连接上的通知都保留）、HTTP 响应（状态、头、体）、CLI 的 stdout / stderr / 退出码、假 provider 收到的请求（方法、路径、头、体）、结束后 home 和 project 下的全部文件、停机退出码。落盘文件：文本文件比全文和权限；非 UTF-8 文件比长度、sha256 和权限；`.json` / `.jsonl` 比解析结果，另比一份原文指纹（行数、是否以换行结尾、每行排版骨架的哈希计数），缩进、空格、换行、转义写法、数字写法（`1` 与 `1.0`）的差异因此也会报出来。
- **归一化规则都写在 `diff.mjs` 开头**，每条都是「基线自己连跑两次也会变」的东西：UUID、时间戳、进程号、请求指纹、耗时、两处随机路径段、会话文件里标题记录的落点、调试开关 PILOTDECK_DUMP_REQUEST 写的转储文件名、sidecar 的随机身份、sidecar 原始协议应答帧 messageId 的随机后缀（基线 nextId 用过的前缀后面那一段，按出现次序编号）、sidecar 发给宿主的 module_call 的 requestId 与 preparationId 的随机段（宿主各端口的前缀 / 操作名 / 序号照比，随机段按出现次序编号，同一个值在一侧各处编号相同）、记忆检索的随机轨迹 id（`case_trace_` / `trace_` 后的 10 位十六进制，按出现次序编号）。另外，以 `[gateway:turn-timing] ` 开头的整行（基线只在某段耗时达到 200 毫秒时才打的耗时日志）从两侧的全部文本里摘掉，每侧摘了几行在差异报告头部打印。另有两条成对规则：帧里的会话文件大小 `fileSize` 两侧不同时，各自在落盘树里找出「会话文件写到第 K 条记录时正好是这个大小」的 K，K 相等就当作相同（拼不出 K 时退到固定容差：都大于 0、差值不超过最短一条记录的一半且不超过 64 字节）；`runs/registry.json.lock` 只在一侧有时两侧都略过（基线停机可能打断在持锁写入中，30 次留下 9 次），两侧都有时只把内容里的持锁进程号 `"pid":<n>` 抹成 `<pid>`、其余照比。还有一条「各自核对」：工具结果的 `resultBytes` 与发给假 provider 的请求头 `content-length` 是由别的内容算出来的字节数，会随被归一化的耗时的位数差一两个字节；每侧先拿本侧产物核对（`resultBytes` 按基线算法由本侧同一个工具调用的结果原文算出，`content-length` 由采集器在收到请求时对照实际收到的请求体字节数，结果记在请求记录的 `content-length-ok`），两侧都核对通过才不比数值，任一侧没通过照比。三条规则放过的处数每次打印。场景还可以声明 `volatile`（基线自比已证明不稳的路径）和 `accepted`（基线自身的 bug，Rust 不照做），两者每次比较都会打印略过了几处。基线 vs Rust 时，`accepted` 里某条正则一处都没命中会报 `STALE` 并计 1 处差异（过期的豁免让场景变红）；`volatile` 一处没命中只提示 `STALE?`。不比较的字段只有四个（采集侧标记、启动耗时、服务进程日志、失败汇总；失败汇总由 `check.mjs` 单独判定）。
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

产物默认在 `_build/bbx/`：每个场景两侧各一份原始采集 `<场景>.<侧>.json` 和一份差异 `<场景>.diff.txt`。`--out DIR` 可改位置。每个场景的判定是 `SAME`、`DIFF N`、`INVALID`（有非预期的失败，括号里是差异结论和失败位置）、`RUN FAILED`（采集器没跑完，例如锁被占、二进制比源码旧）或 `SKIPPED`（要占 18789 而没设 `BBX_ALLOW_18789=1`，见下）。退出码 0 表示除 SKIPPED 外全部 SAME。

单步使用：

```sh
node scripts/bbx/run.mjs ts   scripts/bbx/scenarios/turn-text.json /tmp/a.json
node scripts/bbx/run.mjs rust scripts/bbx/scenarios/turn-text.json /tmp/b.json
node scripts/bbx/diff.mjs /tmp/a.json /tmp/b.json          # 两份基线互比时加 --self，不做 STALE 检查
```

采集在一个槽位里是独占的（固定目录和端口；缺省槽位用 `/private/tmp/pdrs-bbx` 和 38411–38414），`check.mjs` 遇到锁会排队等。要并行跑，给每个并行方一个不同的槽位：`BBX_SLOT=1 node scripts/bbx/check.mjs`（1–8）。槽位只把目录名和端口换成等长的另一套，场景里写死的同名路径和端口读入时一并替换；两侧在同一槽位里跑，结论不受影响，但不同槽位的采集产物不要互相比。

### 占用 18789 的场景

基线 CLI 的 `cron` 子命令和默认对话（不带子命令的 `pilotdeck`）只探测写死的 `127.0.0.1:18789`（`src/gateway/client/probeServer.ts:16`），不看端口变量；它们连上 server 之后的路径只能把被测 server 起在 18789 上才能对拍。18789 是真实 PilotDeck 的默认端口，所以这要显式选择，两道都要有：

- 场景写 `"gatewayPort": 18789`（只接受这个值）。这时 `"server"` 起的服务、`{GATEWAY}`、采集器给的 `PILOTDECK_GATEWAY_PORT` 都换成 18789；假 provider 仍在槽位端口上。
- 运行时设 `BBX_ALLOW_18789=1`。没设时 `run.mjs` 在拿锁、查端口、起任何进程之前就以退出码 6 退出，不写产物；`check.mjs` 把它记成 `SKIPPED`（不算 SAME 也不算失败），汇总行写出跳过了几个。

设了变量时的保护：仍然先查 18789 有没有监听者，有就拒跑（与其他场景相同）；除了槽位锁，还要拿一把所有槽位共用的锁 `/tmp/pdrs-bbx-18789.lock`（macOS 上是 `/private/tmp/...`），拿不到时退出码 3，`check.mjs` 照常排队，所以并行槽位里的这类场景自动串行；采集结束后把自己起的进程按进程组杀掉，再确认 18789 上没有留下监听者，留下了记一处失败（`release18789`，判 INVALID）并在 stderr 报警。采集器异常退出（未捕获异常、SIGINT / SIGTERM / SIGHUP）时同样按进程组杀掉子进程、释放锁；只有采集器自己被 SIGKILL 时做不到，下一次运行会因 18789 上有监听者而拒跑，需要人按进程号收拾。

服务端令牌：采集器给 server 的令牌在环境变量里，基线这时不写 `server-token` 文件，而探测要求这个文件存在且非空，所以这类场景要在 `files` 里放 `home/.pilotdeck/server-token`。

收尾：基线 CLI 连上 server 后从不关连接，`cli` 步骤只能等超时被 SIGKILL；被杀的客户端只留下半关闭的套接字，基线 server 不处理（`src/gateway/server/websocket.ts:19-20` 只听 `close` / `error`），之后 SIGTERM 停机永远等不完。所以 `cli-cron-online-*` / `cli-default-online-*` 以 `{"stopServer": "SIGKILL"}` 收尾，停机行为不在这些场景里比（这是基线 server 的一处挂起，Rust 的 server 在同样情况下正常退出，两边的差别要另行决定）。

改 `diff.mjs` 之后先跑比较器自测：`node scripts/bbx/selftest.mjs`（也检查上面「没设变量就跳过」的几条）。它给比较器喂成对的小产物，检查每条归一化规则该抹的抹掉了、规则旁边「差一点」的改动仍然报差异。新增归一化规则时同时加这两类例子。

## 进度总表

功能清单的源头是 `features.json`：功能域 → 子能力两层，叶子是 `surface.json` 的表面项和 `scenarios/` 的场景，归属按里面的规则算（表面项取第一条命中的规则，场景取全部命中规则的并集，例外逐项覆盖，都不中的列为「归属不了」）。新增表面项或场景时多数会被现有规则接住；`status.mjs` 的「归属统计」和「归属不了」两处会显示漏网的。

```sh
node scripts/bbx/status.mjs                                  # 读 _build/bbx，写 state/status.md 与 state/out-of-scope.md
node scripts/bbx/status.mjs --out /path/to/round --round-note '哪台机器、哪个检出' --out-label 'cb8:_build/bbx'
node scripts/bbx/status.mjs --csv /tmp/status-csv            # 另导出 domains.csv / caps.csv / gaps.csv（UTF-8 带 BOM，不入库）
node scripts/bbx/status.mjs --stdout --now 2026-01-01T00:00:00Z   # 只打印；--now 固定生成时间
node scripts/bbx/status-selftest.mjs                         # 自测（假产物上的归属、计数、缺口清单、逐字节确定）
```

它只读产物、不跑对拍。一轮全量产物（约 500 MB 的基线采集）上约半分钟到一分钟，几乎全花在 573 个 `observe` 正则扫产物原文上，与 `coverage.mjs --surface` 同样的代价；没有产物时两三秒。除了产物，它还读 `state/issues.md`（「功能域」列）、`state/audits.md`（审阅记录）、`state/check-results.json`（四条命令与上游验收集的结果，手写，写明来源；缺哪项就显示「没有数据」）、`specs/delivery/README.md` 的「允许不一致的清单」，并扫 `src/` 的产品代码数占位和透明代理入口。表面项是否被碰到的判定与 `coverage.mjs --surface` 共用 `lib/surface.mjs`，同一产物目录上两者的数字相同（总表末尾有按维度的对照表）。

## 场景格式

`scenarios/*.json`：

| 字段 | 含义 |
|---|---|
| `name` / `about` | 名字和它想证明什么 |
| `config` | `pilotdeck.yaml` 的内容；省略用默认（指向假 provider）；`null` 表示不写配置文件 |
| `files` | 初始文件，键是相对根目录的路径（`home/.pilotdeck/...`、`proj/...`），值是字符串或 JSON。值是只有一个键 `$file` 的对象时按写法生成：`{"$file": {"prefix", "text", "repeat", "suffix", "mode"}}` 写 `prefix + text×repeat + suffix`（各段替换占位符，与 `raw` 的 `send` 同一写法，用来种几十 MiB 的大文件而不把内容写进场景），`mode` 是八进制权限（如 `"755"`，给假可执行文件） |
| `pathDir` | 相对根目录的目录，排在子进程（被测服务、`cli` / `serve` / `sidecar` 步骤）PATH 的最前面，放场景自带的假可执行文件（用 `files` 的 `$file` 加 `mode` 种进去；常见写法是把收到的 argv 和指定的环境变量写到 `proj/` 下的文件，落盘照常比较）。被测进程本身在此之前已解析（基线的 `node` 也按这个 PATH 找，所以目录里不要放 `node`）；场景 `env` 里显式写的 `PATH` 盖过它 |
| `symlinks` | 种完 `files` 之后建的符号链接：键是相对根目录的链接路径，值是链接内容（可用 `{ROOT}` 等占位符；链接本身必须在根目录之下，指向哪里不限） |
| `env` | 额外环境变量 |
| `provider` | 假 provider 的应答规则，按顺序匹配请求体文本：`when` 全部包含、`unless` 全部不包含；`reply` 可以是 `{text}`、`{tool:{name,arguments}}`、`{status,body}`。规则里的字符串（含工具参数）可用下面的 `{ROOT}` 等占位符。请求按命中的规则名分组记录 |
| `server` | `false` 表示不起服务（纯 CLI 场景） |
| `steps` | 依次执行：`{ws, params, answer}`（`answer` 见下）、`{http: "GET /path", headers, body}`、`{cli: [参数...], stdin, cwd, env}`（`stdin` 是数组时分块写：每块写之前等被测进程打出新的 stdout 并静止 100 毫秒，用来逐个回答交互式提问；写完关闭 stdin）、`{serve: [参数...]}`（见下）、`{connect: true}`（重连；也可以给一个完整的 hello 帧）、`{disconnect: true}`（客户端发关闭帧断开；不发关闭帧的几种见下）、`{sleep: 毫秒}`、`{sidecar: "start" / "stop"}` 与 `{tcpProxy: "start" / "cut"}`、`{stopServer: 信号}`、`{proxyLog: true}`（见下） |
| 步骤的 `timeout` | `ws` / `http` / `cli` / `serve` 等待的毫秒数，缺省 30000 |
| 步骤的 `expectTimeout` | `true` 表示这一步在基线上本来就会超时（如等一个采集器不回答的询问、命令常驻不退出）。只有声明了的超时不判 `INVALID`；连不上不能这样声明 |
| `expectStartFailure` | `true` 表示 `"server"` 起的服务预期启动失败。`serve` / `cli` 步骤里进程自己退出是被比较的输出，不需要声明 |
| `settleMs` | 最后一步之后、收尾前等待的毫秒数，缺省 300 |
| `httpProxies` | 假 HTTP 代理，见下面「假 HTTP 代理」 |
| `httpsPort` | 本机 HTTPS 前端的端口（如 `38419`，随槽位替换）：TLS 解密后原样转给假 provider，请求照常按规则应答和记录。证书是仓库里一次性的测试 CA 与 `127.0.0.1` 叶子（`src/pilotdeck-gateway/tests/fixtures/model-stream/tls.json`，不是凭据）；CA 写到 `{HTTPS_CA}`（根目录下、比较的两棵树之外），场景要在 `env` 里写 `"NODE_EXTRA_CA_CERTS": "{HTTPS_CA}"` 让两侧都信任它；`{HTTPS}` 是 `https://127.0.0.1:<端口>`（见 `abort-tool-web-fetch`） |
| `gatewayPort` | 只能是 `18789`：把被测 server 起在 18789 上，要求 `BBX_ALLOW_18789=1`，否则 SKIPPED（见上面「占用 18789 的场景」） |
| `volatile` | 该场景里确认不稳定的差异路径（正则）。会被略过并打印。用之前先用 `--self` 证明它确实不稳定 |
| `accepted` | 已接受差异 `[{id, why, paths: [正则]}]`，只用于基线自身的 bug（Rust 有意不照做）。命中的差异略过并打印条数；基线 vs Rust 时某条正则一处都没命中报 `STALE`，计 1 处差异 |

`ws` 步骤可以带 `answer`，用来在这一步等待期间应答服务端发来的询问（权限询问、提问）：`"answer": [{"on": {"type": "permission_request"}, "ws": "permission_decide", "params": {"sessionKey": "s", "requestId": "{event.requestId}", "decision": "allow"}, "count": 1}]`。收集该步骤的帧时，每遇到一个属于本步骤、且 `on` 里各字段都与事件相等的事件帧，就用第一条还有余量的规则发一个请求；`params` 里的 `{event.字段}` 换成触发事件的字段值，`count` 是该规则最多应答几次（缺省不限）。规则上写 `delayMs` 时等这么久再发（例如工具开始后过一会儿再中止）。发出的请求和它收到的帧记在该步骤的 `answers` 里，照常比较。两侧用的是同一段采集代码。

`cli` 步骤可以带 `until` 与 `then`：stdout 或 stderr 里出现 `until` 时（进程还在不在都行）依次执行 `then` 里的步骤，然后照常等进程退出或超时；没等到 `until` 进程就结束了时 `then` 不执行，记成空数组。用来在一个不退出的 CLI 进程还连着 server 时做事，例如停掉 server。

`{"stopServer": "SIGTERM"}`（值是信号名）给 `"server"` 起的被测服务发信号，等它退出，记下 `exitCode` 与 `signal`；15 秒不退就 SIGKILL 整个进程组，如实记成 SIGKILL。之后收尾不再停机。假 provider 的 `reply` 上写 `"killServer": "SIGKILL"` 则在命中这条规则时给 server 发这个信号（只发不等），用来让 server 在一轮进行中消失；同时写 `"killServerDelayMs": n` 则收到请求 n 毫秒后再发（配合更长的 `delayMs`，让 server 在闲着等模型应答时消失，避开它正在写落盘的那一刻）。

`cli` 和 `serve` 步骤可以带 `env`：盖在场景环境之上，值为 `null` 表示删掉该变量（例如去掉采集器默认给的 `PILOTDECK_GATEWAY_TOKEN`）。

`serve` 步骤用来比常驻命令（`pilotdeck server`）自己的 stdout / stderr / 退出码，这些在 `"server"` 起的服务上属于不比较的 `serverLog`。它用 CLI 起一个进程，等 stdout 里出现 `until`（默认 `WebSocket: `），依次执行 `then` 里的步骤，然后发 `signal`（默认 `SIGTERM`）并等进程退出；15 秒不退就 SIGKILL，如实记成 `signal: "SIGKILL"`。进程没等到 `until` 就自己退出（启动失败）时不执行 `then`，照样记输出和退出码。这类场景通常写 `"server": false`；`then` 里的 `http` / `connect` 连的是固定端口 38411，所以被起的进程要监听在那里（采集器默认给了 `PILOTDECK_GATEWAY_PORT=38411`）。发信号前客户端若还连着，基线会一直等到它断开，需要比停机时先 `{disconnect: true}`。`then` 里的步骤同样可以 `save` 和引用 `{saved.名字}`。

不发 WS 关闭帧的断开：`{disconnect: "destroy"}` 直接关掉底层 TCP（FIN，模拟客户端进程被杀），`{disconnect: "reset"}` 以 RST 断开（SO_LINGER 0），`{disconnect: "end"}` 只关写方向（半关闭：发 FIN 但照常接收服务端的帧，之后可以再 `destroy`；在那之前不要 `connect` 新连接，两条连接的帧会混在一起）。`{vanish: N}` 另建 N 条 WS 连接（与场景的主连接无关，逐条依次建），每条 hello 并等到第一帧非通知的应答后直接关掉 TCP（FIN），模拟 N 个客户端进程被杀；结果是 `{opened, replies}`（`replies` 按应答帧的 `type` 计数，`__timeout` 计为超时失败，`connectFailed` 计为连不上），这些连接上的帧不进步骤结果和通知。前两种结果里的 `close` 是客户端随后看到的关闭（1006）。采集器只在建 WS 连接期间、只对连向 gateway 端口的那次 `net.connect` 记下套接字，别的连接不受影响。注意基线对只收到 FIN 的连接不做任何处理（`src/gateway/server/websocket.ts:17-21` 不听 `end`），服务端此后没再写过它时停机会挂住，用 `serve` 起服务才能把这一点比进去。

`raw` 步骤自己开一条 TCP 连接写原始字节，用来发浏览器式客户端发不出的输入（非法帧、超大帧、不升级的握手、半截请求行）：`{"raw": {"port": 38411, "ws": true | {"path", "method", "headers"}, "hello": true, "send": [...], "readMs": 1000}}`。`port` 缺省是 gateway 端口；`ws` 先发 WS 升级请求（头可以覆盖，值为 `null` 表示不发这个头），`hello` 在 101 之后发缺省 hello 并等第一帧。`send` 里依次是 `{"text": "…"}`（可带 `prefix` / `repeat` / `suffix` 拼大串）、`{"hex": "…"}`、`{"frame": {opcode, text | json | hex, prefix, repeat, suffix, fin, rsv, mask, length}}`（客户端帧，缺省 FIN=1、加固定掩码；`mask: false` 不加掩码，`length` 改声明的长度）、`{"wait": 毫秒}`；`{"pauseRead": true}` / `{"resumeRead": true}`（停止 / 恢复从套接字读：字节留在内核缓冲里，服务端写不出去时在它那边排队，用来模拟慢读客户端）；同一项可带 `times: n`（同样的字节连写 n 遍、一次写出）；任一项可带 `splitAt: n`（先写前 n 字节，停 200 毫秒再写其余）和 `end: true`（写完半关闭）。写完后读到服务端关闭或 `readMs` 到时为止。结果：`upgrade`（状态行和头，去掉 `date`）、升级了的记 `frames`（每帧的 fin / rsv / opcode / masked，文本能解析成 JSON 的记解析结果，超过 4 KiB 的文本只记长度和 sha256，关闭帧记 code / reason），`countFrames: true` 时 `frames` 只记总数和按「类型:id」（关闭帧按码与原因）分组的计数，用于帧数极多的场景；没升级的记 `http`（状态行；头名转成小写、去掉 `date`；正文：`transfer-encoding: chunked` 的解码后记录并标 `chunked: true`（没收完标 `"incomplete"`），有 `content-length` 的按长度截，其余到连接结束；同一连接上的后续应答依次放进 `next`，多出来的字节记 `trailing`）。头名大小写和 chunk 长度的写法是 HTTP 语义等价的传输细节，按这条尺子规则不比；规则在 `lib/rawhttp.mjs`，自测 `node scripts/bbx/rawhttp-selftest.mjs`，`serverClosed`（`fin` / `reset` / `close`，没关是 `null`，只收到 FIN 是 `fin-half`），连不上记 `refused`。对方说的是按行分隔的 JSON（agent loop sidecar 的原始协议，`sidecar` 步骤起的 TCP 端口）时写 `"lines": true`：收到的字节按换行切开记在 `lines`（每行能解析成 JSON 的记解析结果，否则记 `{text}`），没有换行结尾的最后一段记 `partial`，不按 HTTP / WS 解析，收尾不发关闭帧（见 `sidecar-raw-protocol`）。这一步不产生 `failures`：连不上、服务端没应答都是被比较的输出，所以服务进程挂掉之后的探测用它，不用 `http`。

`flood` 步骤同时另开很多条 WS 连接：`{"flood": N}` 并发建 N 条连接，等 101 后各发 hello，等每条拿到第一帧或被关（5 秒），结果按结局计数（`tally`：`hello_ok`、关闭帧以外的 `closed-before-upgrade:fin` 之类）。连接一直保持到 `{"flood": "release"}` 或场景结束（停机前统一关掉），这期间的其他步骤照常跑，用来比连接很多时服务端是否还接受、还应答。

`sidecar` 步骤起停一个外部常驻的 agent loop sidecar（TCP 部署用）：基线是 `node dist/src/cli/pilotdeck-agent-loop-sidecar.js`，Rust 是 `pilotdeck-agent-loop-sidecar`。`{"sidecar": "start", "port": 38414, "env": {...}}` 用场景环境盖上步骤 `env` 起进程，等它在 `port` 上开始监听（进程没等到监听就退出时记下退出码、信号、stdout、stderr）；`{"sidecar": "stop", "signal": "SIGKILL"}` 给最近起的那个发信号（缺省 SIGTERM），等它退出并记下退出码、信号、stdout、stderr（15 秒不退就 SIGKILL）。场景结束时还在跑的由收尾按进程组杀掉，不记输出。sidecar 是否监听 TCP 由环境里的 `PILOTDECK_AGENT_LOOP_TCP_PORT` 决定，和 gateway 用同一套变量。

stdio 部署用 `{"sidecar": "stdio", "env": {...}, "send": [...], "readMs": 1000, "signal": "SIGTERM"}`：起同一个程序（环境里不要有 `PILOTDECK_AGENT_LOOP_TCP_PORT`），按 `send` 往它的 stdin 写，stdout 按行记在 `lines`（同 `raw` 的 `lines: true`：每行能解析成 JSON 的记解析结果，否则 `{text}`，没有换行结尾的最后一段记 `partial`），stderr 原样记。字节项上写 `end: true` 关闭 stdin，不写就一直开着。写完后等到进程退出或 `readMs` 到时；还没退出的记 `exited: false`，再发 `signal`（15 秒不退就 SIGKILL），最后记下 `exitCode` 与 `signal`。进程先退出时后面的项不再写（EPIPE 不算采集失败）。这一步与 `raw` 一样不产生 `failures`。

`raw` 与 `sidecar: "stdio"` 的 `send` 是同一套发送脚本：除了 `text` / `hex` / `frame` / `wait` / `pauseRead` / `resumeRead`，还有 `{"json": 值}`（写 `JSON.stringify(值)` 加一个换行，免得手写转义）和 `{"until": {"lines": N} | {"text": "…"}, "timeout": 毫秒, "save": {...}}`：等对方的输出里出现第 N 个完整行（从头数），或者在上一次 `until` 停下的位置之后（`"fromStart": true` 时从头找，用于几条执行并发、对方先发哪条不定的时候按内容认）出现这段文字、且那一行已经完整，再往下走；`save` 的正则在那一整行的原文上匹配，存下来的值后面的项用 `{saved.名字}` 引用（每一项在用到时才替换）。这样场景能扮演宿主应答 sidecar 发来的 `module_call`：等到 `"method":"module_call"` 那一行，存下它的 `messageId` 和 `requestId`，下一项写 `inReplyTo` / `requestId` 引用它们的应答（见 `sidecar-raw-stdio-model`）。`timeout`（缺省 10000）内没等到时后面的项不再执行，结果里记 `untilMissed`（这一项在 `send` 里的下标）。

`tcpProxy` 步骤在采集器里起一个透明的 TCP 转发，用来在 sidecar 不退出的情况下断开它与 gateway 之间的连接：`{"tcpProxy": "start", "port": 38413, "target": 38414}`；`{"tcpProxy": "cut"}` 立刻断开现有的全部连接，结果里的 `live` 是断开前还连着的连接数（代理继续监听）。要在一轮进行中打断，在假 provider 的 `reply` 上写 `"cutProxy": true`（断开代理上的连接）或 `"killSidecar": "SIGKILL"`（给当前 sidecar 发信号）：命中这条规则时先做这件事，再照常应答（可配 `delayMs`）。

假 HTTP 代理：场景写 `"httpProxies": {"A": 38415, "B": {"port": 38418, "status": 407}}`，采集器在被测进程之前于 `127.0.0.1` 上各起一个 HTTP 代理（端口照常随槽位替换）。每个收到的请求记一条 `{method, target, headers}`：`CONNECT` 的 `target` 是请求行里的 `host:port`，绝对 URI 请求是整个 URL；头名是小写，键的先后照收到的顺序。它只转发到本机假 provider：目标主机是 `[::1]`、`0.0.0.0`、`127.0.0.1`、`localhost` 时不论端口，`CONNECT` 回 `200` 后把隧道接到 `127.0.0.1` 上的假 provider，绝对 URI 请求同样转给它；别的目标回 `502`，不外连。写了 `status` 的代理对 `CONNECT` 一律回这个状态。端口不限是为了让不同种类的请求用不同的目标端口（如模型 `http://[::1]:38416/v1`、hook `http://[::1]:38417/...`），各自一条隧道，记录里分得清谁走了代理；`[::1]` 上没有人监听这些端口，没走代理的请求直连时立刻被拒（被测进程对 `127.0.0.1` / `localhost` 固定直连，所以目标不能用它们）。`{"proxyLog": true}` 步骤取出各代理自上次取出以来的新记录（按 JSON 文本排序：并发请求到达代理的先后不稳定；条数照比，隧道复用了几次因此也在比较之内），收尾时剩下的记进产物的 `proxies`（见 `cfg-proxy-outbound`、`cfg-proxy-routing`、`hot-reload-proxy`）。

假 provider 的 `reply` 除了 `{text}`、`{tool}`、`{status, body}` 之外还可以组合：`tools`（多个工具调用，可与 `text` 同时给）、工具上的 `rawArguments`（原样当参数字符串，用来回非法 JSON）、`reasoning`（推理文本）、`chunks`（文本分多块流式发出）、`finish`（覆盖结束原因，如 `length`）、`usage`、`delayMs`（先等一会儿再回）、`breakAfter: n`（流里发出 n 块后断开连接）、`hangAfter: n`（发出 n 块后挂住，`hangMs` 后断开）、`endAfter: n`（发出 n 块后正常结束响应，流里没有结束原因 / `[DONE]` / `message_stop`）。请求路径按协议分流：以 `/messages` 结尾按 Anthropic messages 的形状应答；以 `/responses` 结尾按 OpenAI Responses（`finish` 是响应的 `status`：`completed` / `incomplete` / `failed`，`failed` 的错误对象取 `reply.error`）；`…/models/<模型>:generateContent` 或 `:streamGenerateContent` 按 Google Gemini（`finish` 是 `finishReason`，缺省 `STOP`；推理文本是带 `thoughtSignature` 的 `thought` 部分；工具调用只有显式写了 `id` 才带 `functionCall.id`）；其余按 OpenAI chat completions。`usage` 一律按 chat 形状写（可带 `prompt_tokens_details.cached_tokens`、`completion_tokens_details.reasoning_tokens`），各协议自行换算。

引用前面跑出来的值（服务端随机生成的 id 之类）：步骤或 provider 规则上写 `"save": {"名字": "正则"}`，正则在该步骤结果的 JSON 文本上（provider 规则则是在命中它的那次模型请求的原文上）匹配，第一个捕获组存成这个名字；之后的步骤参数和 provider 应答里用 `{saved.名字}` 引用。存下来的值不进产物，两侧各存各的。

字符串里可用 `{ROOT}` `{HOME}` `{PILOT_HOME}` `{PROJECT}` `{MOCK}` `{GATEWAY}` `{TOKEN}`，以及 `{PATH}`（采集器自己的 PATH，不含 `pathDir`；两侧相同，但随机器变，只放进不落盘、不进输出的地方，例如场景 `env` 里 `"PATH": "{ROOT}/other:{PATH}"`）。另有 `{SIDECAR}`：本侧 agent loop sidecar 的启动命令行（shell 写法，基线是 `node …/pilotdeck-agent-loop-sidecar.js`，Rust 是 `pilotdeck-agent-loop-sidecar`），两侧的值不同，只能放进不落盘、不进输出的地方，例如场景 `env` 里 `"BBX_SIDECAR": "{SIDECAR}"`，再让 `modules.agentLoop` 的 `module-stdio-v2` 绑定以 `command: /bin/sh`、`args: [-c, 'eval "exec $BBX_SIDECAR"']` 起它（见 `modules-tools-batch-sidecar-binding`）。

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
- **归一化抹掉的写法**：ISO 时间整体换成 `<iso>`，小数位数（毫秒 / 微秒）、`Z` 与 `+00:00` 的差异看不到；耗时只比有无不比数值和位数；UUID 只比「同一个 id」的关系，不比大小写和版本位。会话文件大小 `fileSize` 只看「写到第几条记录」，字节数本身的差由落盘树里那些记录的比较负责；CLI 输出文本里的 `"fileSize": N` 不在这条规则里。`resultBytes` 与发给 provider 的 `content-length` 在两侧都核对通过时只比类型不比数值：数值算错（与本侧原文 / 请求体对不上）会报出来，「两侧原文本身一样长」由原文的比较负责；找不到本侧原文（或同一个调用 id 有几份大小不同、分不出轮次的原文）时照比数值。运行记录文件锁只在两侧都有时比内容（进程号不比），有没有不比。
- **原文指纹的粒度**：只比每行骨架的计数，不比行的先后（先后由解析结果比）；字符串里只看转义序列，不看普通字符；文件里的字符串长度和数字的位数不在指纹里（它们的值由解析结果比）。
- **大文件**：落盘里超过 16 MiB 的文件只比长度（`largeBytes`）、全文 sha256 和末尾 64 KiB 里的完整行（`tail`，能解析成 JSON 的按解析结果比；被测进程追加在大文件末尾的记录在这里比；`.jsonl` 的末尾行全能解析时改放在 `jsonl` 下并标 `tailOnly`，会话记录的归一规则照样适用），不比其余内容的解析结果和原文指纹。命名管道、套接字这类不是普通文件的只记种类（`special`），不读内容。
- **落盘范围**：只采 `home` 和 `proj` 两棵树；`TMPDIR`（`{ROOT}/tmp`）和其他位置的文件不比。`serverLog`（`"server"` 起的服务进程自己的输出）不比。
- **未声明的失败只判 INVALID，不判谁对**：一侧超时一侧不超时时，差异照常在 `steps` 里报，但场景整体是 INVALID，要先修场景（或确认是 Rust 的挂起）再看差异。
