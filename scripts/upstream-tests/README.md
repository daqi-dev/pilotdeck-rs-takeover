# upstream-tests：来自基线自带测试的 sidecar 验收集

`scripts/bbx` 的场景都是我们自己写的。这里把**基线（`resources/PilotDeck`，`cc7c4e6`）自带的测试**改成指向一个外部的 sidecar 进程，
被测对象换成 Rust 的 `pilotdeck-agent-loop-sidecar` 时，宿主仍是基线的 TS 代码（sidecar 客户端、Session、Gateway），
宿主对收到的每一帧都做协议校验（`src/agent/modules/transport/agentLoopSidecarClient.ts` 的 `validateModuleMessage`）。

## 怎么跑

```sh
cd src && cargo build -p pilotdeck-transport && cd ..
node scripts/upstream-tests/run.mjs                    # 缺省五组，指向 Rust，打印结果表；退出码 0 = 计入的全过
node scripts/upstream-tests/run.mjs --target baseline  # 指向基线自己的 sidecar（外部进程），校准用
node scripts/upstream-tests/run.mjs --group client,tcp,deployment --repeat 10
node scripts/upstream-tests/run.mjs --group stdio      # 附加组，不在缺省里
```

- 基线目录：`PDRS_BASELINE`，缺省找本检出的 `resources/PilotDeck`，再找主检出（`git --git-common-dir` 的上一级）的。不要建软链接。
- Rust 二进制：`PDRS_SIDECAR_BIN`，缺省 `src/target/debug/pilotdeck-agent-loop-sidecar`；比 `src/` 下它依赖的 `.rs` / `Cargo.*` 旧就拒跑（`PDRS_ALLOW_STALE=1` 跳过）。
- 产物：`--out` / `PDRS_UT_OUT`，缺省 `_build/upstream-tests`（不入库）。
- 一轮全部（parity 1 轮）约 6 分钟，其中 parity 约 4 分钟。
- 依赖：Node 22、python3（parity 组）、基线的 `dist` 和 `node_modules`（schema 组用其中的 `ajv@8` 与 `ajv-formats`）。不新增依赖。
- Linux：没有写死 macOS 路径；所有目录从仓库位置、`PDRS_*` 环境变量和输出目录推出来。还没在 Linux 上实际跑过。

## 隔离

每个被跑的 node / python 进程的环境都是新建的：只有 `PATH`、`HOME=<运行目录>/home`、`PILOT_HOME=<HOME>/.pilotdeck`、
`TMPDIR=<运行目录>/tmp`，加上本工具自己的 `PDRS_*` 变量（`lib/common.mjs` 的 `isolatedEnv`）。不继承调用方的
`PILOTDECK_*`、`NODE_OPTIONS` 等。不在 `resources/` 写任何东西：

- 上游用例假定 cwd 是基线仓库根，并会在 `<cwd>/dist/node_modules` 建软链接。这里在输出目录搭一个替身根（`dist/ui` 拷贝、其余条目软链接到基线），`process.cwd()` 改写成它。
- parity 组把 `tools/agent-loop-parity` 拷到输出目录再跑（python 加 `-B` 与 `PYTHONDONTWRITEBYTECODE=1`），只用 `--comparison same-version`。
- 不监听 18789；不起真实外部服务；假模型都在进程内或 mock 上。

## 改写（`lib/generate.mjs`）

从基线 `dist/tests/...` 读原文件、机械改写后写到输出目录；仓库里不放改过的上游文件。每条规则核对命中次数，不符就报错。

| 规则 | 改什么 |
|---|---|
| R1 | 相对 import 改成指向基线 `dist` 的绝对 URL |
| R2 | `import test from "node:test"` 换成 `lib/runtime.mjs` 的包装：按 manifest 跳过整例、记录用例是否碰到被测 sidecar |
| R3 | `loopbackConnection()`（进程内 TS sidecar server + PassThrough）改成外部 sidecar 子进程的生产 stdio 连接；包成与 loopback 同形的普通对象（上游用 `{...conn, send}` 截获帧） |
| R4/R5 | `command: process.execPath, args: [builtSidecarPath()]` 与 TCP 用例的 `spawn(...)` 改成被测 sidecar 的命令 |
| R6 | `createLocalGateway` 的 stdio 部署环境追加 `PILOTDECK_AGENT_LOOP_SIDECAR_COMMAND`（只在指向 Rust 时） |
| R7 | `process.cwd()` 改成隔离的替身根 |
| R8 | （仅 stdio 组）`new AgentLoopSidecarServer(` 改成拉起外部子进程、把 stdin/stdout 接上用例的两条流 |

断言、用例名、用例体其余部分逐字不动。`--target inprocess` 跑上游原样的进程内版本，只作对照。

## 各组

计入的用例清单在 `lib/manifest.mjs` 的 `SIDECAR_TESTS`（由基线外部进程的全绿运行里「碰到 sidecar」的记录得出，每次运行都核对，通过的用例碰没碰与清单不符报「分类不符」）。不在清单里的用例照跑，但不经过被测 sidecar，单列「不经被测 sidecar」不计入。

| 组 | 来源 | 上游 | 不经被测 sidecar | 剔除 | 计入 |
|---|---|---|---|---|---|
| client | `agent/modules/sidecar-client.spec.js` | 52 | 24 | 0 | 28（26 个 loopback + 2 个直接拉起编译产物） |
| tcp | `agent/modules/tcp-sidecar-transport.spec.js` | 10 | 7 | 0 | 3（拉起编译产物监听 TCP） |
| deployment | `cli/local-gateway-agent-loop-deployment.spec.js` | 5 | 4 | 0 | 1（stdio 部署） |
| schema | 上面 client 组转录的帧 | — | — | — | 每一帧 |
| parity | `tools/agent-loop-parity` 的 PilotDeck 场景 | 53 | — | 36 | 17 |
| stdio（附加） | `agent/modules/sidecar.spec.js` | 19 | 0 | 13 | 6 |

「不经被测 sidecar」的用例：client 里是用脚本化的假连接（`connect: () => ({send, receive})`）或直接调宿主 dispatcher 测 TS 宿主自己；
tcp 的 7 个与 deployment 的 4 个 TCP 用例在进程内起 `AgentLoopSidecarTcpServer` 并配自定义执行函数（包着 `createSidecarExecution`
截获模块调用、记 `operationDeadlines` 等 spy，或脚本化的假执行），被断言的正是这些进程内的东西，换成外部进程不成立。

### 校准（指向基线自己的 sidecar，外部进程）

- client / tcp / deployment：10 轮，32 个计入用例每轮全过（`--target baseline --repeat 10`）。上游文档记过 TCP abort 不稳；本组计入的 3 个 TCP 用例 10 轮没有失败过。
- stdio：20 轮。计入的 6 例 20/20；剔除的 13 例 20 轮全红（用例自带 factory 决定的行为在外部进程上不存在，见 manifest 的逐条理由）。
- parity：6 轮基线自比（TS 原生循环 vs 基线 sidecar），只有 17 个场景每轮都过；26 个 6 轮全红，10 个时红时绿。旧记录的「35/53 稳定」与此不符。失败几乎都是 trace 里跨通道事件的先后（`agent.status`、`context.budget` 相对 `model.request` 的位置），由 sidecar 与宿主之间的时序决定：同一个基线 sidecar 中间隔一层转录垫片（`lib/tee-sidecar.mjs`），`pure_text` 就从过变成不过，帧的次序完全一样。剔除清单在 `PARITY_EXCLUSIONS`。

### schema

`docs/pilotdeck-module-protocol-v2.schema.json` 用基线自带的 ajv 8（2020-12 方言，`strict: true`，加 `ajv-formats`）编译，逐帧校验 sidecar 发出的帧和宿主发出的帧。
自检：从合法帧取样各注入一处违规（删必填字段、加顶层 `operation`、未知 module、协议版本 2.1、`streamId` 改名、`final` 无 `outcome`、`sequence` 为负），7 处都必须被判不合法。
`strict: true` 下 schema 里出现 ajv 不认识的关键字会直接编译失败，所以没有被悄悄忽略的关键字；schema 只约束信封与 `hostModules` 等少数 payload 字段，各模块调用的 payload 内容基本不在覆盖内。
帧只来自 client 组（28 个用例约 1200 帧），覆盖到的帧种类见结果里的 `byKind`；TCP、resume/ack/status 等控制帧没有覆盖。

基线 sidecar 自己有一类违规（见下「疑似基线 bug」），只在 `--target baseline` 时豁免。

## 疑似基线 bug

- **module_call 帧里带 `"abortSignal":{}`**：`src/agent/modules/llm/hostModelInvokerPort.ts:101` 等处把 `AbortSignal` 放进调用对象，
  `agentLoopSidecarServer.ts` 的 `callModule` 把调用对象整个展开进帧，序列化成空对象。与基线自己的类型（`protocol.ts` 的 module call 类型没有这个字段）
  和 schema（`moduleCallRequest` 的 `additionalProperties: false`）矛盾。client 组里基线 604 个 module_call 帧有 326 个这样。Rust 不发这个字段；需要人定是否登记为已接受差异。

## 注入错误检查

见报告；做法是改坏 Rust 的一处、重建、跑对应组确认变红、还原。
