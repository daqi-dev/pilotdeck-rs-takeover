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

端口（`UI_SMOKE_PORT_BASE`，默认 39100）：39100 假 provider，39101 gateway，39102 转发器，39103 ui/server，全部只监听 127.0.0.1；18789 有监听者时拒跑（`lsof` 之外再直接连一次 127.0.0.1:18789，Linux 上 `lsof` 看不到别的用户的进程）。

其他环境变量：`UI_SMOKE_BASELINE`（基线目录，缺省 `resources/PilotDeck`；worktree 里没有它时指向主克隆的）、`UI_SMOKE_TARGET`（Rust 二进制目录，缺省 `src/target/debug`）、`UI_SMOKE_AUDIT`（非 macOS 上缺省 `strace`，`off` 关掉，见下）。

### Linux（验收机 cb8）

Linux 上没有 `sandbox-exec`；cb8 也不许非特权用户建 user namespace（`kernel.apparmor_restrict_unprivileged_userns=1`，`bwrap` / `unshare -rn` 都报权限错误），所以那里 gateway 与 ui/server 不能被当场拦住。隔离靠 `HOME` / `PILOT_HOME` / `TMPDIR` 都指到根目录下，再加默认开着的事后检查：每组的 gateway（含中途重启的 `gateway2`）与 ui/server 都套在 `strace -f -y` 下，系统调用记到 `<前缀>.<名>.strace`；跑完 `run.mjs` 自动调 `audit.mjs`，结果写 `<前缀>.audit.txt`，并打印最后一行 `[ui-smoke] audit: 越界 0 处`。不为 0 时这一组判失败（退出码 1）。`UI_SMOKE_AUDIT=off` 可关掉，只用于排查，关掉的组不算第 4 条的证据。

与 macOS 的区别：macOS 的 `sandbox-exec` 是**当场拦截**，越界的系统调用直接失败，被测进程拿到的是错误；Linux 的 strace 检查是**事后核对**，越界的调用会真的发生（真的连出去、真的写了文件），只在跑完后被报出来。所以 Linux 上的「越界 0 处」证明的是「这一次实际没有越界」，不能证明「越界了也伤不到机器」。检查的范围：连接看全部尝试（含非阻塞 `connect` 的 `EINPROGRESS`），只许 127.0.0.0/8、`::1` 与 Unix 套接字；写类调用（写方式打开、创建、改名、删除、建目录、改权限等）只看成功的，只许根目录、`/dev`、`/proc/<自己>`；读类只查是否碰了真实 `~/.pilotdeck`。`*at` 调用的相对路径按 `-y` 给出的目录 fd 还原，`AT_FDCWD` 的相对路径列为「待看」不计数。审计器用手写的越界样例确认过三类都能报出来。

```sh
. ~/ws/pdrs-takeover/env.sh                     # Node v22.22.0、LANG=en_US.UTF-8、umask 022
cd ~/ws/pdrs-takeover/wt-tz                     # 任一检出
export UI_SMOKE_BASELINE=~/ws/pdrs-takeover/pilotdeck-rs/resources/PilotDeck
export UI_SMOKE_ROOT=~/ws/pdrs-takeover/tmp-ui/root
O=~/ws/pdrs-takeover/tmp-ui/out
node scripts/ui-smoke/run.mjs ts   $O                 # 约 85–90 秒（strace 下；中途重启与收尾各有一次基线 gateway 等满 15 秒后 SIGKILL）
node scripts/ui-smoke/run.mjs ts   $O ts2             # 同上
node scripts/ui-smoke/proctree.mjs --prefix src/target/debug --out $O/rust.proctree.json -- \
  node scripts/ui-smoke/run.mjs rust $O               # 约 70 秒；同时采样 Rust 进程树（第 6 条证据，见下）
node scripts/ui-smoke/compare.mjs $O ts ts2           # ui 3 处、calls 6 处、provider SAME（全是「已知不稳」那一处）
node scripts/ui-smoke/compare.mjs $O ts rust          # 同样只应有那一处；每组日志末尾应有「越界 0 处」
```

中途重启 gateway 时，ui/server 的 bridge 连接还开着，基线和 Rust 的 gateway 都在 SIGTERM 后 15 秒内没有退出、被 SIGKILL（`<前缀>.stop.json` 里的 `gateway` 一项）；收尾时 ui/server 已先停，Rust 正常退出、基线仍被 SIGKILL。停机结果不参与比较。

判读：`ts vs rust` 的差异必须是 `ts vs ts2` 里出现过的路径；数值落在基线自己的波动范围外时，按「已知不稳」里的办法用基线分词器核对。

## 进程证据（交付标准第 6 条）

`proctree.mjs` 有两种用法：包住一条命令（下面界面冒烟和 bbx 的例子），或 `--watch` 常驻（全量对拍用，见「全量对拍期间」）。两种都每 50 毫秒扫一遍进程表（Linux 读 `/proc`，其他平台用 `ps`），把「可执行文件在 `--prefix` 目录下的进程」当根，记录根及其全部后代、以及与根同一会话的进程的每一个命令行；一个进程一旦被认作后代就一直算（父进程先退、被 init 收养也不丢）。不需要事先知道任何进程号，四个槽位并行、server 不断起停时逐个认出来。结果写 `<out>.json` 与 `<out>.txt`：每行是一种命令行（路径归一后：UUID、bbx 槽位目录、mkdtemp 随机后缀、端口、长数字）的进程数、被采到的次数、与根的关系、是否 node / 是否 Node gateway；头部有采样次数、实际最大间隔、采样器自己的 CPU 占用，以及一条机器可读的结论行（stdout 也打印）：

```
PROCTREE_RESULT {"nodeGatewayUnderRust":0,"ok":true,"rustRoots":…,"processes":…,"kinds":…,"ticks":…,"intervalMs":50,"maxGapMs":…,"durationMs":…,"samplerCpuPercent":…}
```

基线一侧的 gateway（`node dist/src/cli/pilotdeck.js server`）由采集器直接起，不是 Rust 进程的后代，不计入。Node gateway 的判定：命令行含基线 gateway 入口（`dist/src/cli/pilotdeck.js`、`src/cli/pilotdeck.ts`、`dist/src/gateway/`、`src/gateway/server/`），或 node 进程参数里有 ` server`。原版 ui/server 不是 Rust 进程的后代，不在清单里（它本来就该是 Node）。

界面冒烟用上面的命令；bbx 的 Rust 一侧同样包一层，例如：

```sh
export BBX_SLOT=3 BBX_BASELINE=$UI_SMOKE_BASELINE
node scripts/ui-smoke/proctree.mjs --prefix src/target/debug --out $O/bbx.proctree.json -- \
  bash -c 'for s in tool-bash hook-pre-tool-use mcp-session-servers; do node scripts/bbx/run.mjs rust scripts/bbx/scenarios/$s.json /tmp/x/$s.rust.json; done'
```

2026-11-14 在 cb8 上的结果（原文在 `~/ws/pdrs-takeover/logs/process-evidence-20261114.txt`）：界面冒烟 Rust 组 39 秒、774 次采样，6 个进程：Rust `pilotdeck server` 和 5 个基线自带 funasr 插件的 MCP 服务（node）；bbx 30 个场景（工具、子代理、MCP、hook、sidecar）的 Rust 一侧 195 秒、3892 次采样，168 个进程，node 进程只有 MCP 服务（funasr、场景的 `mcp/*.mjs`、`@playwright/mcp`）和 hook 脚本 `hook.mjs`。两段都是 `node-gateway=0`。

### 全量对拍期间

验收脚本里在对拍前后台起、对拍后停（`--pidfile` 在结果写完后删掉，用它等结束）：

```sh
PT=$LOG/proctree-full                                  # 结果前缀（.json / .txt / .out）
node scripts/ui-smoke/proctree.mjs --watch --prefix "$PWD/src/target/debug" --out $PT.json --pidfile $PT.pid > $PT.out 2>&1 &
node scripts/bbx/check.mjs --slots 1,2,3,4 …          # 全量对拍，照常
kill "$(cat $PT.pid)"; while [ -e $PT.pid ]; do sleep 0.2; done
grep '^PROCTREE_RESULT' $PT.out                        # nodeGatewayUnderRust 必须是 0、ok 必须是 true
```

开销（cb8，20 核 aarch64，约 550–620 个进程）：空闲 10 秒占一个核的 4.6%；`--watch` 下跑 10 个 bbx 场景两侧（115 秒，12 个 Rust server 先后起停，基线 gateway 同时在跑）3.1%；包住界面冒烟 Rust 组（68 秒）3.2%；只跑几秒的短命令会到 8–10%（主要是 Node 自己的启动）。折合 20 核整机都不到 0.5%。每轮读一次 `/proc` 目录，只给新出现的进程读 `stat` 与 `exe`，出现不到 2 秒的进程每轮重读 `exe`（刚 fork 还没 exec 的），命令行只读跟踪集合里的。

判定能抓到：用一个不入库的反例场景（bash 工具执行 `sh -c 'sleep 1; true' /fake/PilotDeck/dist/src/cli/pilotdeck.js server`）在 `--watch` 下各跑一侧：基线一侧 `nodeGatewayUnderRust=0`（基线 gateway 和它起的同样命令行都不是 Rust 的后代）；Rust 一侧报 `nodeGatewayUnderRust=1`、`ok=false`，清单里那一行标 `NODE-GATEWAY`。

`--prefix` 要指向这次被测的那份构建，机器上别的检出跑的 Rust 进程因此不会混进来；同一份构建的两个采样不要同时跑（会互相算进对方的根）。局限：活不到 50 毫秒的进程可能漏采（`maxGap` 是实际最大间隔）；命令行里 `key=` / `token=` / `sk-…` 形式的值抹掉，环境变量不记。

## 组成

| 文件 | 作用 |
|---|---|
| `run.mjs` | 建隔离目录（`HOME`、`PILOT_HOME`、`TMPDIR` 都在根目录下，预写 `pilotdeck.yaml` 与固定的 `server-token`），按序起 provider → gateway → 转发器 → ui/server，跑 `drive.mjs`（drive 经 IPC 请求时中途重启 gateway），最后按进程号逐个停（停机结果写 `<侧>.stop.json`） |
| `provider.mjs` | 假模型（OpenAI chat completions），按最后一条消息里的标记回文本 / read_file / write_file（含「总是允许」用的绝对路径写入）/ ask_user_question / 挂住 |
| `proxy.mjs` | ui/server 与 gateway 之间的透明转发器，WS 帧与 HTTP 请求逐条记到 `<侧>.gateway.jsonl`（去掉 `Sec-WebSocket-Extensions` 以免压缩） |
| `drive.mjs` | 扮演前端：HTTP API + `/ws` 聊天 WebSocket 的典型流程，再按 `routes.mjs` 把其余路由各打一次 |
| `routes.mjs` | ui/server 的完整路由清单（带源码位置），不打的写明原因 |
| `compare.mjs` | 归一化两侧产物并用 `scripts/bbx/diff.mjs` 比较 ui 输出、gateway 流量、provider 请求；生成 method 清单 `<侧>.inventory.json` |
| `audit.mjs` | Linux 上替代沙箱的事后检查：读 `UI_SMOKE_AUDIT=strace` 记下的系统调用，列出连 localhost 以外、写根目录与 `/dev` 以外、读真实 `~/.pilotdeck` 的调用 |
| `proctree.mjs` | 进程树采样（交付标准第 6 条的证据），见「进程证据」 |
| `halfopen-shutdown.mjs` | 复现：WS 客户端不发 close 帧就断开后 gateway 能否响应 SIGTERM 退出 |

ui/server 的起法与生产一致（`ui/server/webRuntimeSupervisor.js:27-38`）：`cwd=resources/PilotDeck/ui`，`node --import tsx server/index.js`，它从源码 import 基线的 gateway 客户端（`ui/server/pilotdeck-bridge.js:60`）。用到的环境变量：`SERVER_PORT`、`HOST=127.0.0.1`（缺省 `0.0.0.0`，`index.js:3584`）、`PILOTDECK_GATEWAY_URL`（缺省 `ws://127.0.0.1:18789/ws`，`pilotdeck-bridge.js:77`）、`PILOTDECK_SKIP_BROWSER_OPEN=1`（否则会打开浏览器，`index.js:3695`）、`PILOT_HOME`（token 文件 `$PILOT_HOME/server-token`，`pilotdeck-bridge.js:79`；数据库 `$PILOT_HOME/auth.db`，`load-env.js:50`）。本机登录默认关闭（`constants/config.js:16`），不需要取 token。

macOS 上 gateway 与 ui/server 都在 `sandbox-exec` 里跑：只许连 localhost、只许写根目录与 `/dev`、读真实 `~/.pilotdeck` 被拒。跑完各进程日志里没有一条被拒的操作，说明这套流程不联网、不写根目录之外。Linux 上用 `UI_SMOKE_AUDIT=strace` + `audit.mjs` 事后检查（见「Linux」）；2026-11-14 在 cb8 上两侧各跑一次，gateway 与 ui/server 都是 clean（审计器本身用手写的越界样例确认过能报出外连、根目录外写入、读真实 `~/.pilotdeck` 三类）。

## 比较规则（compare.mjs 开头有完整说明）

- ui 侧：去掉 ui/server 异步广播帧（`loading_progress` / `projects_updated` / `session-activity`）和 ui/server 自己生成的随机值（memory dream id、API key、VAPID 公钥、导出文件名时间、下载 zip）。
- gateway 侧：去掉请求 id；按 method 分组、组内按发出顺序比；项目快照轮询（`list_projects` / `list_sessions` / `describe_project`）只比去重后的 (method, params) 集合。
- provider 侧：按命中规则分组、组内按到达顺序比请求的方法、路径、头、体。
- 之后交给 `scripts/bbx/diff.mjs --self`（UUID、时间戳、毫秒数、进程号归一化）。

## 已知不稳（基线自比也有）

带附件那一轮（`turn-with-attachment`）的 `context_budget` 事件里，消息部分的本地 token 估算随上传目录里的随机 UUID 变几个（`submit_turn[N].events[3]` 的 used / displayUsed / localEstimateTokens / ratio / breakdown.total / breakdown.messages 6 处，ui 侧 tokenBudget 的 used / displayUsed / ratio 3 处；N 随流程步数变）。

依据：
- 算法：基线的本地估算对每个文本块调 `countTokens`（`src/context/budget/TokenBudgetManager.ts:106-108`，`estimateForBlock` 在 `:124` 起），`countTokens` 是 `src/context/budget/tokenizer.ts` 里的 tiktoken 分词。上传目录名里有 `crypto.randomUUID()` 生成的 UUID，出现在发给模型的附件消息里；同样 36 个字符，十六进制串的组合不同，BPE 切出的 token 数就不同。
- 实测（2026-11-14，cb8）：三次运行的这一轮各取 provider 收到的请求，把带 UUID 的消息用基线自己的 `dist/src/context/budget/tokenizer.js` 的 `countTokens` 重算：ts 96、ts2 93、rust 97；对应事件里的 `breakdown.messages` 是 497 / 494 / 498，差值（-3 / +1）完全一致。其余部分（系统提示、工具定义）三次相同。
- 两侧的 provider 请求在 UUID 归一化后完全相同（`provider.cmp` SAME），所以这一处与 Rust 无关；偶尔两次的 UUID 碰巧切出同样多的 token，这一处就不报（cb8 上 5 次 ts vs rust 里有 1 次全 SAME）。

核对办法（数值超出基线自比见过的范围时）：

```sh
node --input-type=module -e '
import fs from "fs"; const { countTokens } = await import(process.env.UI_SMOKE_BASELINE + "/dist/src/context/budget/tokenizer.js");
for (const s of ["ts", "ts2", "rust"]) {
  const r = fs.readFileSync(`${process.argv[1]}/${s}.provider.jsonl`, "utf8").trim().split("\n").map(JSON.parse)
    .find((x) => x.body?.stream && JSON.stringify(x.body).includes("please look at the attachment"));
  let n = 0; for (const m of r.body.messages) { const c = typeof m.content === "string" ? m.content : (m.content ?? []).map((p) => p.text ?? "").join("");
    if (/[0-9a-f]{8}-[0-9a-f]{4}-/.test(c)) n += countTokens(c); }
  console.log(s, n);
}' "$O"
```

所以这一轮之后的步骤不再读会话历史或 token 用量（读了会把这个数带进更多处）；「会话续写」那组步骤因此放在附件那一轮之前。

其他：基线 gateway 收到 SIGTERM 后 15 秒内不退出，`run.mjs` 照设计 SIGKILL（`ts.stop.json` 里记为 `SIGKILL after 15s SIGTERM`），Rust 正常退出；停机结果不参与比较。

## 不覆盖

- `routes.mjs` 里标了 skip 的路由（联网、起 claude / task-master CLI、弹 GUI、让 ui/server 退出）；`/shell` 与 `/plugin-ws/*` 两个 WS 路径。
- StaffDeck / 知识库 / SOP 绑定未配置，`/api/module-host/*`（ui/server 走 HTTP 调 gateway 的那一路）这次没有被触发。
- 回合进行中重启 gateway、并发多标签页、多个浏览器同时连接。（聊天 WS 重连后续写、gateway 重启后续写已覆盖，见下表。）

## 交付标准第 4 条三样的覆盖

| 人要亲手试的 | 流程里对应的步骤 | 没覆盖的 |
|---|---|---|
| 对话与工具 | `turn-text-new-session`（新会话纯文本）、`turn-tool-read`（read_file，不问权限）、`turn-permission-allow`（write_file）、`turn-plan-mode`、`regenerate-last-message`、`turn-after-model-switch`、`turn-with-attachment`、`turn-general`（不带项目）、`turn-abort` / `turn-queue-then-abort`（中止与排队） | bash / 子代理 / MCP / skill 调用没有在界面路径上走（由 bbx 场景覆盖 gateway 一侧）；真实模型的流式分段、思考块、长输出的渲染；前端 JS 本身（只扮演前端发的请求，不跑浏览器） |
| 权限询问 | `turn-permission-allow`（批准）、`turn-permission-deny`（拒绝带理由）、`turn-ask-user`（ask_user_question 回答）、`turn-always-allow` → `turn-after-always-allow`（点「总是允许」：`permission-response` 带 `rememberEntry: write_file:<项目>/*`，规则同时进入之后每轮的 `toolsSettings.allowedTools`；下一轮同目录写入不再询问，两侧都是 0 次询问）、`get-pending-permissions`、`session-permission-grant` | 权限询问挂着时刷新页面再回答；「总是允许」跨 gateway 重启是否仍生效（规则在前端 localStorage，由每轮命令带上） |
| 会话续写与历史 | 同一 WS 上 `resume: true` 连续多轮；`reconnect-active-sessions` → `reopen-sessions` → `reopen-session-messages` → `turn-resume-after-reconnect` → `session-messages-after-resume`（断开聊天 WS 再连、从侧栏重开会话、读历史、接着发一轮、再读历史）；`gateway-restart` → `restart-sessions` → `restart-session-messages` → `turn-resume-after-gateway-restart` → `session-messages-after-restart-resume`（`run.mjs` 经 IPC 停掉 gateway 再在同一端口起一个，ui/server 不动，之后重连、重开会话、读历史、续写、再读历史）；`session-messages-page`（分页）、`session-rename`、`session-fork`、`session-delete` | 第二个浏览器连接上同时看同一会话；回合进行中重启 gateway |
