# bbx：黑盒对拍

同样的输入分别喂给基线（`resources/PilotDeck` 的 `dist`，Node 运行）和 Rust 实现，比较对外可观察的输出。接手后（2026-11-10）以它为判定「Rust 与基线一致」的主要手段。

## 它为什么可信

- **被测对象是真进程**：基线是 `node dist/src/cli/pilotdeck.js server --port N`，Rust 是编译出来的 `pilotdeck server --port N`，参数和环境变量两侧相同。采集器不 import 任何一侧的代码。
- **输入完全相同**：两侧先后在同一个隔离路径 `<BBX_TMP_ROOT 或系统 tmp>/pdrs-bbx/root` 的干净目录里跑，同样的配置文件、同样的初始文件、同样的环境变量、同样的端口、同一个假 provider、同样的步骤。路径和端口因此不需要归一化。
- **记录的是原样输出**：WS 帧（重连前后各条连接上的通知都保留）、HTTP 响应（状态、头、体）、CLI 的 stdout / stderr / 退出码、假 provider 收到的请求（方法、路径、头、体）、结束后 home 和 project 下的全部文件、停机退出码。落盘文件：文本文件比全文和权限；非 UTF-8 文件比长度、sha256 和权限；`.json` / `.jsonl` 比解析结果，另比一份原文指纹（行数、是否以换行结尾、每行排版骨架的哈希计数），缩进、空格、换行、转义写法、数字写法（`1` 与 `1.0`）的差异因此也会报出来。
- **归一化规则都写在 `diff.mjs` 开头**，每条都是「基线自己连跑两次也会变」的东西：UUID、时间戳、进程号、请求指纹、耗时、两处随机路径段、会话文件里标题记录的落点。场景还可以声明 `volatile`（基线自比已证明不稳的路径）和 `accepted`（基线自身的 bug，Rust 不照做），两者每次比较都会打印略过了几处。基线 vs Rust 时，`accepted` 里某条正则一处都没命中会报 `STALE` 并计 1 处差异（过期的豁免让场景变红）；`volatile` 一处没命中只提示 `STALE?`。不比较的字段只有四个（采集侧标记、启动耗时、服务进程日志、失败汇总；失败汇总由 `check.mjs` 单独判定）。
- **失败不算一致**：启动失败、步骤超时（含被采集器杀掉的 `cli` / `serve`、`disconnect` 等不到关闭）、连不上、停机等不到退出，都记进产物的 `failures`。只要有未声明为预期的失败，`check.mjs` 就判 `INVALID`，两侧失败得一模一样也不行。
- **二进制是新的**：跑 Rust 侧之前，`run.mjs` 检查 `src/target/debug/pilotdeck`（和 `pilotdeck-agent-loop-sidecar`）不比 `src/` 下（不含 `target/`）的 `Cargo.toml`、`Cargo.lock` 和它依赖的 `.rs`（cargo 依赖清单 `<二进制>.d` 里列的；没有清单就看全部 `.rs`）旧，旧了拒跑。`BBX_ALLOW_STALE=1` 跳过，`BBX_SRC` 可指定源码目录（默认 `BBX_TARGET` 往上两级）。
- **自检**：`check.mjs --self` 让基线跑两次互相比。必须 SAME，否则该场景不稳定，结论不能用。
- **能抓错**：见下面「注入错误检查」。

采集、比较、完整运行清单与覆盖统计均使用 Node 内置库。

## 用法

在仓库根目录：

```sh
cargo build --manifest-path src/Cargo.toml -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli

node scripts/bbx/check.mjs --self            # 基线自比，应全部 SAME
node scripts/bbx/check.mjs                   # 基线 vs Rust，全部场景
node scripts/bbx/check.mjs turn-text         # 只跑一个场景
```

每次运行创建一个全新目录，默认在 `_build/bbx/<时间-随机 ID>/`：每个场景两侧各一份原始采集 `<场景>.<侧>.json` 和一份差异 `<场景>.diff.txt`。`--out DIR` 可指定尚不存在的新目录；目录已存在直接失败，不覆盖旧报告。`manifest.json` 保存本次完整结果，失败运行也保留，后续重跑必须使用另一个目录。每个场景的判定是 `SAME`、`DIFF N`、`INVALID`（有非预期的失败，括号里是差异结论和失败位置）或 `RUN FAILED`（采集器没跑完，例如锁被占、二进制比源码旧）。退出码 0 表示全部 SAME。

单步使用：

```sh
node scripts/bbx/run.mjs ts   scripts/bbx/scenarios/turn-text.json /tmp/a.json
node scripts/bbx/run.mjs rust scripts/bbx/scenarios/turn-text.json /tmp/b.json
node scripts/bbx/diff.mjs /tmp/a.json /tmp/b.json          # 两份基线互比时加 --self，不做 STALE 检查
```

采集在一个槽位里是独占的（固定目录和端口；缺省槽位用 `<BBX_TMP_ROOT 或系统 tmp>/pdrs-bbx` 和 38411–38414），`check.mjs` 遇到锁会排队等。要并行跑，给每个并行方一个不同的槽位：`BBX_SLOT=1 node scripts/bbx/check.mjs`（1–8）。槽位只把目录名和端口换成等长的另一套，场景里写死的同名路径和端口读入时一并替换；两侧在同一槽位里跑，结论不受影响，但不同槽位的采集产物不要互相比。

改 `diff.mjs` 之后先跑比较器自测：`node scripts/bbx/selftest.mjs`。它给比较器喂成对的小产物，检查每条归一化规则该抹的抹掉了、规则旁边「差一点」的改动仍然报差异。新增归一化规则时同时加这两类例子。

## 场景格式

`scenarios/*.json`：

| 字段 | 含义 |
|---|---|
| `name` / `about` | 名字和它想证明什么 |
| `config` | `pilotdeck.yaml` 的内容；省略用默认（指向假 provider）；`null` 表示不写配置文件 |
| `files` | 初始文件，键是相对根目录的路径（`home/.pilotdeck/...`、`proj/...`），值是字符串或 JSON；单键对象 `{"$sqlite": ["CREATE TABLE ...", "INSERT ..."]}` 在指定路径以事务创建 SQLite fixture，SQL 也支持路径占位符 |
| `env` | 额外环境变量 |
| `provider` | 假 provider 的应答规则，按顺序匹配请求体文本：`when` 全部包含、`unless` 全部不包含；`reply` 可以是 `{text}`、`{tool:{name,arguments}}`、`{status,body}`。规则里的字符串（含工具参数）可用下面的 `{ROOT}` 等占位符。请求按命中的规则名分组记录 |
| `server` | `false` 表示不起服务（纯 CLI 场景） |
| `steps` | 依次执行：`{ws, params, answer}`（`answer` 见下）、`{http: "GET /path", headers, body}`、`{cli: [参数...], stdin, cwd, env}`、`{serve: [参数...]}`（见下）、`{connect: true}`（重连；也可以给一个完整的 hello 帧）、`{disconnect: true}`（客户端主动断开）、`{sleep: 毫秒}` |
| 步骤的 `timeout` | `ws` / `http` / `cli` / `serve` 等待的毫秒数，缺省 30000 |
| 步骤的 `expectTimeout` | `true` 表示这一步在基线上本来就会超时（如等一个采集器不回答的询问、命令常驻不退出）。只有声明了的超时不判 `INVALID`；连不上不能这样声明 |
| `expectStartFailure` | `true` 表示 `"server"` 起的服务预期启动失败。`serve` / `cli` 步骤里进程自己退出是被比较的输出，不需要声明 |
| `settleMs` | 最后一步之后、收尾前等待的毫秒数，缺省 300 |
| `volatile` | 该场景里确认不稳定的差异路径（正则）。会被略过并打印。用之前先用 `--self` 证明它确实不稳定 |
| `accepted` | 已接受差异 `[{id, why, paths: [正则], assertions: [断言 ID]}]`，只用于基线自身的 bug（Rust 有意不照做）。命中的差异略过并打印条数；基线 vs Rust 时某条正则一处都没命中报 `STALE`，计 1 处差异 |

`ws` 步骤可以带 `answer`，用来在这一步等待期间应答服务端发来的询问（权限询问、提问）：`"answer": [{"on": {"type": "permission_request"}, "ws": "permission_decide", "params": {"sessionKey": "s", "requestId": "{event.requestId}", "decision": "allow"}, "count": 1}]`。收集该步骤的帧时，每遇到一个属于本步骤、且 `on` 里各字段都与事件相等的事件帧，就用第一条还有余量的规则发一个请求；`params` 里的 `{event.字段}` 换成触发事件的字段值，`count` 是该规则最多应答几次（缺省不限）。发出的请求和它收到的帧记在该步骤的 `answers` 里，照常比较。两侧用的是同一段采集代码。

`cli` 和 `serve` 步骤可以带 `env`：盖在场景环境之上，值为 `null` 表示删掉该变量（例如去掉采集器默认给的 `PILOTDECK_GATEWAY_TOKEN`）。

`serve` 步骤用来比常驻命令（`pilotdeck server`）自己的 stdout / stderr / 退出码，这些在 `"server"` 起的服务上属于不比较的 `serverLog`。它用 CLI 起一个进程，等 stdout 里出现 `until`（默认 `WebSocket: `），依次执行 `then` 里的步骤，然后发 `signal`（默认 `SIGTERM`）并等进程退出；15 秒不退就 SIGKILL，如实记成 `signal: "SIGKILL"`。进程没等到 `until` 就自己退出（启动失败）时不执行 `then`，照样记输出和退出码。这类场景通常写 `"server": false`；`then` 里的 `http` / `connect` 连的是固定端口 38411，所以被起的进程要监听在那里（采集器默认给了 `PILOTDECK_GATEWAY_PORT=38411`）。发信号前客户端若还连着，基线会一直等到它断开，需要比停机时先 `{disconnect: true}`。

假 provider 的 `reply` 除了 `{text}`、`{tool}`、`{status, body}` 之外还可以组合：`tools`（多个工具调用，可与 `text` 同时给）、工具上的 `rawArguments`（原样当参数字符串，用来回非法 JSON）、`reasoning`（推理文本）、`chunks`（文本分多块流式发出）、`finish`（覆盖结束原因，如 `length`）、`usage`、`delayMs`（先等一会儿再回）、`breakAfter: n`（流里发出 n 块后断开连接）、`hangAfter: n`（发出 n 块后挂住，`hangMs` 后断开）。请求路径以 `/messages` 结尾时按 Anthropic messages 的形状应答，其余按 OpenAI chat completions 的形状。

引用前面跑出来的值（服务端随机生成的 id 之类）：步骤或 provider 规则上写 `"save": {"名字": "正则"}`，正则在该步骤结果的 JSON 文本上（provider 规则则是在命中它的那次模型请求的原文上）匹配，第一个捕获组存成这个名字；之后的步骤参数和 provider 应答里用 `{saved.名字}` 引用。存下来的值不进产物，两侧各存各的。

字符串里可用 `{ROOT}` `{HOME}` `{PILOT_HOME}` `{PROJECT}` `{MOCK}` `{GATEWAY}` `{TOKEN}`。

## 注入错误检查

改动 `diff.mjs`、`run.mjs` 或者怀疑尺子失灵时，做一次：在 Rust 产品代码里故意改坏一处对外输出（例如把某个错误码的字符串改一个字母），重新构建，跑对应场景，确认 `DIFF` 里出现这一处；然后还原。记录见 `state/takeover.md`。

不动 Rust 也能验证采集器本身：`BBX_BASELINE` 指向一个假的基线目录（只放一个 `dist/src/cli/pilotdeck.js`），两次用不同的假脚本跑同一个 `"server": false` 的场景，例如分别写出等长不同内容的二进制文件、或内容相同排版不同的 JSON，确认 `diff.mjs` 报出差异。

## 边界

- 两侧在同一台机器、同一个 Node / 同一份构建上跑，不防本机被人动手脚，只防实现不一致。
- 假 provider 有 OpenAI chat completions 和 Anthropic messages 两种形状；OpenAI responses、Google 两种协议形状还没有。

落盘的 sqlite 数据库按内容比：每张表的建表语句和全部行（行排序后比），不比文件字节。

## 已知不覆盖

- **协议帧和 HTTP 体按解析结果比，不比原始字节**：WS 帧、HTTP 响应体、发给 provider 的请求体都先 `JSON.parse` 再比，键的顺序会比，但空格、转义写法、数字写法（`1` 与 `1.0`）看不到。原文指纹只做在落盘的 `.json` / `.jsonl` 上。
- **时序与并发**：只比每一步结束后收集到的内容，帧之间的时间间隔、超时的具体时长、并发请求的交错、竞态都不在覆盖内；`expectTimeout` 只说明「两侧这一步都在等」，不说明等多久会怎样。需要单独设计场景或用单元测试。
- **归一化抹掉的写法**：ISO 时间整体换成 `<iso>`，小数位数（毫秒 / 微秒）、`Z` 与 `+00:00` 的差异看不到；耗时只比有无不比数值和位数；UUID 只比「同一个 id」的关系，不比大小写和版本位。
- **原文指纹的粒度**：只比每行骨架的计数，不比行的先后（先后由解析结果比）；字符串里只看转义序列，不看普通字符；文件里的字符串长度和数字的位数不在指纹里（它们的值由解析结果比）。
- **落盘范围**：只采 `home` 和 `proj` 两棵树；`TMPDIR`（`{ROOT}/tmp`）和其他位置的文件不比。`serverLog`（`"server"` 起的服务进程自己的输出）不比。
- **未声明的失败只判 INVALID，不判谁对**：一侧超时一侧不超时时，差异照常在 `steps` 里报，但场景整体是 INVALID，要先修场景（或确认是 Rust 的挂起）再看差异。

## 完整运行证据（2026-11-13）

`check.mjs` 逐一核对选择的场景集合；子工作者非零退出、信号退出、无输出、缺失、重复或多余场景均失败。每侧产物必须有相同运行 ID、场景 SHA-256、源码身份、二进制 SHA-256、TS 构建树哈希、工具与平台、装置哈希。运行结束再次检查输入，运行期间源码、二进制或场景变化会使整次结果无效。比较器不比较这些采集元数据，由入口独立严格核验；双方 `failures` 均参与失败判定。

覆盖统计必须明确指定本次目录：

```sh
node scripts/bbx/coverage.mjs --out _build/bbx/<本次目录>
node scripts/bbx/coverage.mjs --out _build/bbx/<本次目录> --surface
node --test scripts/bbx/evidence.test.mjs scripts/bbx/check.test.mjs
node scripts/bbx/selftest.mjs
```

`coverage.mjs` 不再把零散或陈旧 `.diff.txt` 当结果。它读取已完成的 `manifest.json`，检查报告及原始产物哈希、当前场景与输入身份，再报告通过、失败、跳过、阻塞。旧产物需重新运行；新的局部运行仅证明所选场景，其余显示未跑。

场景可声明 `assertions`，每项必须有唯一 `id`、指向最终原始采集产物的 JSON Pointer `path`，以及 `equals` / `exists` / `includes` / `matches` 中至少一个谓词。可选 `sides: ["rust"]` 或 `["ts"]` 明确限制侧别；省略则两侧都执行。例如：

```json
{
  "assertions": [
    {"id": "version-exit", "path": "/steps/0/cli/exitCode", "equals": 0},
    {"id": "rust-fixed-output", "sides": ["rust"], "path": "/steps/0/cli/stdout", "includes": "expected text"}
  ],
  "accepted": [{"id": "documented-bug", "why": "见差异证据", "paths": ["steps"], "assertions": ["rust-fixed-output"]}]
}
```

必需断言未实际执行或失败均使结果 INVALID。每项 `accepted` 必须引用已执行且通过的 Rust 正向断言；旧场景仅有忽略路径的做法不再足以通过（当前 8 个历史场景待补证，不自行更改它们的产品期望）。

`BBX_TMP_ROOT` 只指定临时目录的父目录，实际清理由采集器自身的槽位目录承担；Linux 无需创建 `/private/tmp`。`BBX_COLLECT_TIMEOUT_MS` 为单次采集/比较进程的最长等待时间，默认 600000 ms；超过后终止该进程并按失败记录。CLI 子进程使用独立进程组，超时和收尾只清理本次创建的进程。

固定 18789 场景必须在 Docker 私有网络内运行，采集器的占用保护保持启用。容器不映射宿主端口、不挂载真实用户 home，使用非特权 UID；该结果不能替代 Seatbelt 等宿主专项测试。

原生 PDF 场景以 `BBX_PDF_TOOL` 指定已从固定源码构建的组件，场景的 `env.PILOTDECK_PDF_TOOL` 可引用 `{PDF_TOOL}`。运行身份记录该路径及组件 SHA-256；缺失也明确记为 null，运行期间替换组件使证据失效。未指定时占位符解析为明确不存在的测试路径，不借用宿主其他安装。HTTP `Date` 的时间波动由各场景显式声明 `volatile`，相应正向断言仍验证响应头存在且格式正确，不全局忽略任意 Date 字段。

T1 八个固定写后诊断场景另有 `syntaxDiagnostics` 正向谓词，验证真实工具结果、error/精确夹具位置/非空诊断、好文件无诊断及坏好文件字节。对应 `accepted.policy: "syntaxDiagnostics"` 必须绑定完全相同 config 的正向断言，且不能携带路径正则。`syntax-policy.mjs` 逐值核验后只接受已验证诊断串及其重算的 UTF-8 长度、JSON 请求长度、o200k token 和 calibration 派生值；具体边界见 `specs/gateway-parity/diffs/T1-SYNTAX-NATIVE-001-R1.md`。比较报告仍逐项打印 raw a/b 与接受原因，未接受的字段照常比较。该策略不改写旧运行证据，不是通用语法诊断忽略开关。

`backend-core-channel-recovery` 使用显式 `volatilePolicies: [{"policy":"coreChannelPollAfter"}]`，只处理 TS 双轮已证实波动的 BlueBubbles 启动轮询 `after` 秒值。独立合同核验固定两阶段、请求数量及次序、URL 唯一查询字段、实际 durable input 与时间范围；两侧 URL 仅替换该十进制值后仍须完全相同，其他 URL 字节、headers、body 和请求序号继续比较。比较器逐条打印原值和理由，身份验证同时要求声明与原场景完全相同并执行合同，不能仅添加路径豁免。旧的七处 DIFF 和 verifier 字节错误仍保留；新规则需要固定候选重新运行，不能改写旧运行结果。

`agent03-sdk-fork` 的 `sdkAgent03DerivedValues` 只处理已取证的子代理随机 UUID/耗时报告引起的 token 与文件大小派生值。单侧合同使用固定哈希的 o200k 实现重算完整 canonical 请求，并逐条验证 calibration、对外事件和持久化投影；双侧只接受由验证后输入推导出的精确数值路径。JSONL 的采集见证位于 `evidence.persistedFiles`，直接记录实际文件字节的长度和 SHA-256；canonical 重构只有两者均匹配才可用于文件大小正判。缺见证、业务内容变化、无法重算或投影错配都拒绝，不忽略任意 token/byte 字段。原 0e 的 DIFF90 与合成见证诊断保持独立，正式规则需新采集验证。

依赖身份另记录实际加载的 Sharp JS 文件/addon/libvips 共享库、MuPDF 入口/loader/WASM、诊断策略实际使用的 js-tiktoken lite 入口和 o200k 编码表，并记录包 manifest 与软链接真实路径。Sharp 探测使用隔离 HOME 的新子进程，避免 require 缓存藏住替换。`BBX_IMAGE_TOOL` / `{IMAGE_TOOL}` 指定原生图像组件；身份包含组件、相邻发行 libvips 库，以及 Linux 动态链接器实际解析的 libvips 路径和哈希。缺失文件记录 null，不以固定 lockfile 代替已安装依赖的实际哈希；构建或替换这些依赖期间不得把旧 run 当作有效覆盖证据。

二进制输入夹具使用严格单键对象 `{"$binaryBase64":"..."}`，仅接受规范 base64，写入解码后的原始字节；多键对象仍按普通 JSON 写入。图像正向断言可用 `imageAttachments: {count, mime, maxBytes, bytes?, sha256?}` 检查所选 provider messages 下的全部 `image_url`：要求精确数量、MIME、规范 data URL、非空内容、预算上限，以及可选的精确长度和 SHA-256。`count: 0` 用于坏图像确实未附加的证明，所选路径仍必须存在。该谓词不改变比较输出，也不忽略图像 payload；缺失执行记录、错误 MIME、超出预算和 payload 被改写均失败。

### SDK 真实信号取消的具名 bug 策略

`c1-sdk-cancel-resume` 仅在同 binary 的完整八路原始 wire 证据重验成功后，使用 `sdkSignalAbortOrder` 对第1 serve 的 stdout footer/stderr 逐值接受已授权差异。运行前必须设置绝对 `BBX_SDK_SIGNAL_WIRE_DIR` 和 `BBX_SDK_SIGNAL_WIRE_RECEIPT`；`inputIdentity` 保存原始文件及收据哈希，缺证据为 INVALID。TS 自比仍保留原 DIFF，其他字段不忽略。完整命令、两层证据边界与严格负控见 [SDK signal 策略](sdk-signal-policy.md)。

### 已批准的后台 observer 生命周期窗口与 memory 两个有限子组

2026-11-13 用户明确批准三组精确边界：observer started/completion 的两个已验证窗口、memory 四处请求字节派生值、memory 六处采集 headers 对象键序。它们不是新校准的随机 volatile，也不代表全部 SDK 子代理差异已接受。规则逐叶打印 `COMPATIBILITY-VALUE`、实际路径、两侧原值及理由；原 capture 不改。

`observer-lifecycle-policy.mjs` 仅接受固定 SHA 的 `agent03-sdk-fork` 和 memory lifecycle 原场景。先核同 worker/reviewer、完整四次 assistant 活动摘要/最终报告、实际 provider 参数、child/parent durable 链、projection、真实 WS/registry/history 及 run 内连续 seq。reviewer started 只能位于 agent tool_result 前，或紧随该 tool_result 与 step_completed 后，不能越过其他事件；completion 的 text/block/completed 必须完整且有序，属于首轮 W0 或同 session 紧接 W1 的真实 drain 窗口。固定源码的 session buffer 链与同 producer 的 durable/terminal/新 run/原始帧共同作证，不伪造隐藏 tap；没有同 exact TS 反向自比实证的限制仍保留。

这项规则只配对已经证明的实体和实际路径，不排序或删除原事件数组。非目标值继续比较，不能把整个 array/record 标作已接受。若某个已验证跨窗口记录在普通逐下标报告里表现为整对象多出，比较器只在此具名合同内将它展开为 `.<type>`、对象键全集/次序、数组长度与每个叶值，再逐叶核对唯一实际 counterpart；打印 `LIFECYCLE-LEAF-EXPANSION`，未证明的未知字段、类型或叶仍报 DIFF。原整记录差异仍可从原件复算。已有标题 FLOAT_OVER 规则不扩大；仅六类强 UUID 实体用于目标证明，其他首次出现别名连锁仍保留。参见 `specs/gateway-parity/diffs/H1-OBSERVER-LIFECYCLE-WINDOWS-001.md`。

`observer-memory-policy.mjs` 仅接受原 memory 具名场景的四个已验证长度/bytes 叶，以及 `memory-gate[0..2]`、`memory-selector[0..2]` 六个 headers 对象的键序叶。请求真实 body 的 UTF8 字节数、唯一 agent 报告 duration、headers 全部键值、请求身份与数量均须通过；不忽略 body、任意 header 或原始 wire。参见 `D1-SDK-MEMORY-DERIVED-001.md` 与 `D1-SDK-MEMORY-HEADER-ORDER-001.md`。fork 原 `sdkAgent03DerivedValues` 仍按原范围独立执行，memory trace/hash、其他数值、provider 请求键序和 model_selection 键序均未因此批准。

`fixtures/observer-lifecycle-policy/paired-contract.json` 是明确删减无关大字段的合成结构测试数据，不是实际采集、构建来源或正式通过证明。正式实现还独立复用了原两场完整 capture 控制；旧 8a/ccc 的真实缺最后 assistant 摘要原件仍在专属摘要门被拒，不能用新生命周期规则遮住旧产品 bug。

`observer-uuid-policy.mjs` 是同一完整 UUID 引用关系的归一实现修复，不是新增差异许可。仅上述两个固定场景在完整 lifecycle 验证后，配对八个真实实体：parent W0/W1、second session run、worker/reviewer、reviewer block，以及由 actual delta/block、run/timeline、durable assistant_message 和 terminal.finalMessage 共同证明的 parent 两轮最终 block。UUID 与实体双向唯一；当前 raw、normalized view 和实际 aliases 表的哈希、强锚位置和全引用次数在一次调用内绑定，不接受外部拼装的 proof。伪造表、字面 alias 欺骗、多对一或新 token 碰撞均 INVALID。

这一步只在 normalized 副本中重标八个已证别名。未证明的编号不重新分配，所有数组位置、非 UUID 值和对象键插入顺序保持原样；未知 entry/operation 等继续差异。逐项打印 `UUID-ENTITY` 便于审查实际关系，不把新可见的 provider/context/history/存储指纹差异包进来。私有原型与旧正式263/214结果保留，正式冻结后另记新比较。

比较器末尾使用 `process.exitCode` 自然排空 stdout/stderr pipe，保持0/1退出语义。大型逐叶报告的真实spawn+pipe控制要求超过128KiB且完整保留最后 `DIFF N`；它只证明报告收口，不替代任何产品、工具供给或实际副作用断言。旧缺末尾verdict或工具缺失证据不得据此回算通过。

### 2026-11-13 已批准的 A/B/C 精确补充（公共候选）

本节按 `state/decisions-20261113.md:59`（A）、`:71`（B）、`:83`（C）补充上述边界。批准不等于正式重判通过；旧 raw、首 FAIL、旧 DIFF 均保留，公共接线独审和新 harness 冻结后另记成绩。watch、parent、cron、Original53 和 memory tree 键序没有由此获得许可。

`normalization.mjs` 只提取原 `diff.mjs` 的 normalizer/load/title/compare 逻辑，规则与对象/数组顺序不变；提取前后自测61/61，六份实际 raw 与边界样例的 views、aliases、比较结果逐项等价。正式策略可直接导入，不能动态 eval 比较器源码或接受 caller 拼装的 view/Map/passed 收据。

A 组仅以下有限合同：

- `observer-provenance.mjs`、`observer-supplement-policy.mjs` 内部绑定原两场的同 worker/reviewer、完整业务、canonical 请求、固定 tokenizer/calibration、WS/durable/registry/history/projection及真实producer。provider 只允许 parent-after/observer 相邻两项反序；context 只允许重算的八个数值叶和最终 usage三叶（含明确新增的 memory block支持）。两 run counters、两 history sequence、registry skeleton 必须完整成员、终态、producer及逐侧全部 byte/skeleton 来源联合成立。其他值继续比较。
- `observer-id-links-policy.mjs` 补齐既有同 ID 关系的准确 producer/normalized-slot/projection，每个原叶须有唯一真实 counterpart。它不将159 UUID表全局重标，不对未知对象任意递归授予许可，也不排序tree。未获许可的 memory tree四键组顺序继续 DIFF。
- `observer-memory-witness.mjs` 仅三L0精确formula和schema/唯一业务行、严格case/trace/step引用图与非目标payload、collector有限行序、12项原ISO规则，以及62parent全文/bytes/SHA/stat重建后输出七个精确路径。未采Math.random不被假称已逆算；其它hash/field/size不接受。
- `duration-file-size-policy.mjs` 只 long-tool-chain step3/frame0、permissions step10/frame0 的唯一 stat；必须为实际成功 read_session_messages/s-gap，完整JSONL byte/SHA与该侧stat相同、其余capture原比较全相同，总差严格等枚举durationMs十进制位数贡献。同移step0、同改method/session仍拒。
- AO完整prompt→1200UTF16残片规则仅有限增加 always-on-fire-execfail；仍使用完整run/accepted/provider/state/projection/anchor和无歧义prefix的原WeakMap descriptor，不将firstPrompt/lastPrompt整字段volatile。

B 组 `feishu-stop-policy.mjs` 只非self的具名stop场景，内部重读sealed原文件并核源码/场景/原字节身份、完整receipt复制、exact旧WS关闭因果、全部budget/calibration/durable字节来源和完整普通差异。62因果路径与2固定SDK日志路径使用不同理由；固定SDK1.65.0的3组startup info和1组not.registered warn须各一次且原阶段顺序。Node SQLite仍仅使用旧warning规则。失败为INVALID，不以无Pong/timeout当dead，TSself不适用。

C 组 `diagnostics-policy.mjs` 只处理具名诊断场景中的三个固定文件；TS self不应用该Rust差异合同，从raw内部重建views/完整all、固定source/fixture/真实诊断及所有UTF8/token/durable/rawPrint派生。完整具名合同不成立就INVALID；不接受外部counter/view/Map或所有诊断文本。

正式输入身份新增记录 `normalization.mjs`、全部上述runtime策略和constants/contract JSON的SHA。ledger仅新增对应 `diffs/` 文件，不改aggregate/FROZEN。两侧原type/键全集/键序/数组长度和非目标内容始终由原比较器保留。

真实原件控制通过显式测试清单提供，避免公共代码依赖开发机 `_tmp`：`BBX_OBSERVER_ACTUAL_MANIFEST` 指向 `{schema:1, observer:{memory:[TS1,TS2,Rust],fork:[TS1,TS2,Rust]}, tail:{场景:{"ts-1":路径,"ts-2":路径,"rust":路径}}, ao:[TS1,TS2,Rust], rawSha256:{路径:原SHA}}`。`BBX_BASELINE` 指定固定TS包，public tests核每份原件SHA后只在内存变异，不重写原件。不提供该清单时只运行明确注册的便携控制，不把缺失原件算作真实正例通过。Feishu原件测试另用 `FEISHU_STOP_ARTIFACT_ROOT`；各结果分母与实际模式须明确报告。
