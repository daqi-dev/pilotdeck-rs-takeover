# 进度总表（`dev/takeover`）

> 本文件由 `node scripts/bbx/status.mjs` 生成，不要手改。改功能清单改 `scripts/bbx/features.json`，改问题改 `state/issues.md`，改审阅记录改 `state/audits.md`，改检查结果改 `state/check-results.json`，然后重新生成。

- 生成时间：2026-11-13T16:25:30Z
- 仓库提交：`809bc4cb8`（场景、表面清单、功能清单、台账都取自这个提交的工作区）
- 对拍产物：`_build/bbx`
- 产物来历（人给的说明）：cb8（Linux aarch64）上 verify.sh v20 的一轮全量对拍（槽位 5–8 并行），被测提交 809bc4cb8
- 产物里的场景结果：575 个 `.diff.txt`；修改时间 2026-11-13T15:34:17.871Z 至 2026-11-13T16:17:56.905Z（UTC）；按小时：2026-11-13T15:00Z 326 个，2026-11-13T16:00Z 249 个
- 采集平台（从 diff 文件头里的产物路径判断）：Linux 575 个
- 产物里有、当前提交没有的场景 0 个；当前提交有、产物里没有的（记为「未跑」）0 个
- 产物不记录被测提交号，也不记录是哪一轮；修改时间跨度大或分成几段时，可能混合了多轮。

## 八条标准

| 条 | 标准 | 现在（由数据算出） |
|---|---|---|
| 1 | 全量对拍稳定一致 | 场景 575 个：SAME 551，DIFF 24，INVALID 0，未跑 0。不是 SAME 的里属于允许清单 2 个、登记了 accepted 0 个、其余 22 个（逐个见下）。连续 3 轮稳定：需要连续 3 轮的产物，未提供 |
| 2 | 表面覆盖完整 | 表面项 2113 个，范围内 2084 个：被 SAME 场景碰到 1975，只被有差异的场景碰到 7，没碰到 102；标为不计 29（理由表在 `state/out-of-scope.md`，待人过目） |
| 3 | 盲审收敛 | 审阅记录 10 次，其中类型为「盲审」的 1 次；16 个功能域里从没有过盲审或缺口审查的 0 个。各功能域最近一次见下表。「两次连续盲审覆盖全部区域且无 1、2 级新发现」：不满足 |
| 4 | 原版界面可用 | 没有数据：需要 `scripts/ui-smoke` 的当次结果和人亲手使用的记录 |
| 5 | 构建与测试全绿，没有占位 | 四条命令：2026-11-13 在 cb8（Linux aarch64） 上、提交 `2bfa0c7a7`：fmt pass，clippy pass，nextest pass（7763/7763），文档测试 pass。`src/` 产品代码里 `unimplemented!` 0 处、`todo!` 0 处、`NOT_IMPLEMENTED` 0 处、`Failure::Todo` 15 处、自报未实现文案 18 处、宿主边界哨兵 27 处、原生档案缺口 42 处（共 102，逐条见下；`CAPABILITY_UNAVAILABLE`、返回固定空结果、空 match 分支没有机械判据，未统计，逐处定性见 `specs/delivery/placeholder-inventory.md`） |
| 6 | TS gateway 退役 | 透明代理入口的机械判据命中 0 处。进程树里没有 Node gateway 的证据：没有数据（需要全量对拍与界面冒烟期间的进程列表） |
| 7 | 性能报告 | 没有数据：需要发布构建下两侧的启动耗时、每轮开销、内存、并发会话的测量 |
| 8 | 真实使用 | 没有数据：需要人在最终组合上完成的真实任务记录 |

上游 sidecar 验收集（`scripts/upstream-tests`，挂在 `context.compaction`、`loop.continuation`、`modules.sidecar-stdio`、`modules.sidecar-tcp`）：2026-11-13 在 cb8（Linux aarch64）；macOS 上同样的结果见 3d7c9b2ef 上、提交 `9a5fb723b`：client 计入 28 个用例，指向 Rust 全过（Linux 1 轮；macOS 10 轮）；tcp 计入 3 个用例，全过；deployment 计入 1 个用例，全过；schema Rust sidecar 发出的 1204 帧全部合法，宿主的 715 帧全部合法；parity 基线自比稳定的 17 个场景里过 3 个（其余 14 个是写合并造成的记录先后，见台账第 4 级）。来源：cb8 上 ~/ws/pdrs-takeover/logs/upstream-v15.log

### 第 1 条：不是 SAME 的场景

| 场景 | 结论 | 归类 | 子能力 |
|---|---|---|---|
| `abort-hook-pre-tool` | DIFF | 其余 | `loop.steer-abort` |
| `abort-hook-subagent-start` | DIFF | 其余 | `loop.steer-abort` |
| `abort-model-stream` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-post-tool` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-post-tool-failure` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-pre-tool` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-stop` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-stop-failure` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-subagent-start` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-subagent-stop` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-hook-user-prompt` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-model-stream` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-tool-agent` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-tool-execute-code` | DIFF | 其余 | `loop.steer-abort` |
| `abort-tool-bash-disconnect` | DIFF | 其余 | `loop.steer-abort` |
| `abort-turn-finishing` | DIFF | 其余 | `loop.steer-abort` |
| `cli-entry-flags` | DIFF | 其余 | `cli.entry` |
| `cli-update-usage` | DIFF | 其余 | `cli.setup-update` |
| `gap-tool-read-file-vision` | DIFF | 允许不一致的清单 | `tools.files` |
| `gw-http-raw-requests` | DIFF | 其余 | `gw.http` |
| `modules-tools-result-unknown-sidecar-binding` | DIFF | 其余 | `modules.tools` |
| `tool-agent-general` | DIFF | 其余 | `agents.oneshot` |
| `tool-write-file-diagnostics` | DIFF | 允许不一致的清单 | `tools.files` |
| `turn-steer-terminal` | DIFF | 其余 | `loop.steer-abort` |

### 第 5 条：占位的逐处位置

判据：`src/` 下的 `.rs`，跳过 `target`、`tests`、`benches`、`examples`、`fixtures` 目录和 `tests.rs`、`*_tests.rs`、`test_*.rs` 一类文件名，跳过整行注释和顶层的 `#[cfg(test)] mod x { … }` 块；HTTP 状态码 `StatusCode::NOT_IMPLEMENTED`（501）不算。`Failure::Todo` 只数构造处（`Err(Failure::Todo(what))` 一类匹配分支不算）；自报未实现文案按已知措辞列举（见 `status.mjs` 的 `PLACEHOLDERS`），新措辞要补进去；宿主边界哨兵与原生档案缺口数的是命中行（含构造与转交处）。各处是缺口、一致还是环境性，见 `specs/delivery/placeholder-inventory.md`。

- `Failure::Todo`：`pilotdeck-cli/src/config.rs:19`
- `Failure::Todo`：`pilotdeck-cli/src/config.rs:26`
- `Failure::Todo`：`pilotdeck-cli/src/gateway_setup.rs:259`
- `Failure::Todo`：`pilotdeck-cli/src/gateway_setup.rs:269`
- `Failure::Todo`：`pilotdeck-cli/src/gateway_setup.rs:281`
- `Failure::Todo`：`pilotdeck-cli/src/gateway_setup.rs:307`
- `Failure::Todo`：`pilotdeck-cli/src/gateway_setup.rs:314`
- `Failure::Todo`：`pilotdeck-cli/src/gateway_setup.rs:361`
- `Failure::Todo`：`pilotdeck-cli/src/main.rs:141`
- `Failure::Todo`：`pilotdeck-cli/src/main.rs:183`
- `Failure::Todo`：`pilotdeck-cli/src/main.rs:202`
- `Failure::Todo`：`pilotdeck-cli/src/paths.rs:65`
- `Failure::Todo`：`pilotdeck-cli/src/skills.rs:29`
- `Failure::Todo`：`pilotdeck-cli/src/skills.rs:256`
- `Failure::Todo`：`pilotdeck-cli/src/skills_migrate.rs:123`
- `自报未实现文案`：`pilotdeck-agent/src/turn.rs:4136`
- `自报未实现文案`：`pilotdeck-gateway/src/channel/loader.rs:146`
- `自报未实现文案`：`pilotdeck-gateway/src/channel/loader.rs:464`
- `自报未实现文案`：`pilotdeck-gateway/src/native_process/assembly.rs:2373`
- `自报未实现文案`：`pilotdeck-gateway/src/native_process/management.rs:33`
- `自报未实现文案`：`pilotdeck-gateway/src/native_process/methods/metadata.rs:70`
- `自报未实现文案`：`pilotdeck-gateway/src/native_process/methods/sop.rs:133`
- `自报未实现文案`：`pilotdeck-gateway/src/native_process/services.rs:1161`
- `自报未实现文案`：`pilotdeck-gateway/src/runtime/mcp/output_schema.rs:450`
- `自报未实现文案`：`pilotdeck-gateway/src/runtime/mcp/output_schema.rs:2131`
- `自报未实现文案`：`pilotdeck-gateway/src/runtime/mcp/output_schema.rs:2914`
- `自报未实现文案`：`pilotdeck-gateway/src/runtime/project/factory.rs:644`
- `自报未实现文案`：`pilotdeck-gateway/src/runtime/settings/local_settings.rs:342`
- `自报未实现文案`：`pilotdeck-gateway/src/runtime/tools/builtin/form_dialog.rs:162`
- `自报未实现文案`：`pilotdeck-gateway/src/storage/transcript_adapter.rs:490`
- `自报未实现文案`：`pilotdeck-gateway/src/storage/transcript_adapter.rs:1564`
- `自报未实现文案`：`pilotdeck-transport/src/bin/pilotdeck-agent-loop-sidecar.rs:33`
- `自报未实现文案`：`pilotdeck-transport/src/default_factory.rs:1149`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/binding.rs:27`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/binding.rs:28`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/graph.rs:344`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/graph.rs:345`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/mod.rs:349`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/mod.rs:352`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/mod.rs:510`
- `宿主边界哨兵`：`pilotdeck-gateway/src/config/mod.rs:511`
- `宿主边界哨兵`：`pilotdeck-gateway/src/native_archive.rs:285`
- `宿主边界哨兵`：`pilotdeck-gateway/src/native_archive.rs:293`
- `宿主边界哨兵`：`pilotdeck-gateway/src/native_archive.rs:574`
- `宿主边界哨兵`：`pilotdeck-gateway/src/project_files.rs:82`
- `宿主边界哨兵`：`pilotdeck-gateway/src/project_files.rs:836`
- `宿主边界哨兵`：`pilotdeck-gateway/src/project_files.rs:965`
- `宿主边界哨兵`：`pilotdeck-gateway/src/runtime/memory/edgeclaw/service/mod.rs:192`
- `宿主边界哨兵`：`pilotdeck-gateway/src/runtime/memory/mod.rs:118`
- `宿主边界哨兵`：`pilotdeck-gateway/src/runtime/memory/mod.rs:120`
- `宿主边界哨兵`：`pilotdeck-gateway/src/runtime/snapshot.rs:104`
- `宿主边界哨兵`：`pilotdeck-gateway/src/runtime/snapshot.rs:107`
- `宿主边界哨兵`：`pilotdeck-gateway/src/session_model.rs:333`
- `宿主边界哨兵`：`pilotdeck-gateway/src/session_model.rs:334`
- `宿主边界哨兵`：`pilotdeck-gateway/src/session_model.rs:345`
- `宿主边界哨兵`：`pilotdeck-gateway/src/session_model.rs:346`
- `宿主边界哨兵`：`pilotdeck-gateway/src/skills/error.rs:163`
- `宿主边界哨兵`：`pilotdeck-gateway/src/skills/error.rs:164`
- `宿主边界哨兵`：`pilotdeck-gateway/src/skills/error.rs:173`
- `宿主边界哨兵`：`pilotdeck-gateway/src/skills/error.rs:174`
- `原生档案缺口`：`pilotdeck-agent/src/request_assembly.rs:96`
- `原生档案缺口`：`pilotdeck-agent/src/request_assembly.rs:955`
- `原生档案缺口`：`pilotdeck-agent/src/tool_context_policy.rs:125`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:148`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:154`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:161`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:168`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:176`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:228`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:243`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:254`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:1090`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:1174`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:1776`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:1865`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:1976`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:2054`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:2067`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:2075`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:2400`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:2546`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:2850`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:3260`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:3285`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:3432`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:3495`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:4251`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:4272`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:4580`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:4608`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:4747`
- `原生档案缺口`：`pilotdeck-agent/src/turn.rs:4902`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:750`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:752`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:773`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:778`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:911`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:913`
- `原生档案缺口`：`pilotdeck-transport/src/default_factory.rs:927`
- `原生档案缺口`：`pilotdeck-transport/src/server.rs:449`
- `原生档案缺口`：`pilotdeck-transport/src/system_clock.rs:371`
- `原生档案缺口`：`pilotdeck-transport/src/system_clock.rs:432`

### 第 6 条：透明代理的判据

判据（可机械检查，与第 5 条同样的文件范围）：读取透明代理的配置项（`var` / `var_os` 读 `PILOTDECK_PROXY_UPSTREAM`、`PILOTDECK_PROXY_LISTEN`、`PILOTDECK_GATEWAY_MODE`）、引用基线 gateway 入口脚本 `dist/src/cli/pilotdeck.js`、定义 `struct Proxy`。命中 0 处只说明旧入口不在，不说明运行时没有 Node 进程。

（命中 0 处）

## 按功能域

「问题」按台账的级别 1 / 2 / 3 / 4 / 尺子分列；台账里功能域填 `*` 的横跨全部，单列在表末，不计入各域。

| 功能域 | 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 问题 1/2/3/4/尺子 | 最近盲审 / 缺口审查 | 最近代码审阅 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `loop` 对话主干与 agent 循环 | 7 | 91 | 74 | 195 | 182 | 2 | 2 | 9 | 0/3/6/1/2 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `model` 模型协议与路由 | 9 | 56 | 56 | 271 | 269 | 0 | 2 | 0 | 0/1/2/0/0 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `tools` 工具 | 9 | 75 | 73 | 295 | 286 | 0 | 8 | 1 | 0/1/2/0/0 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `perm` 权限、沙箱、路径检查 | 6 | 65 | 65 | 55 | 53 | 0 | 2 | 0 | 0/0/1/0/0 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `session` 会话存储、分叉、恢复、导出 | 8 | 53 | 53 | 206 | 198 | 0 | 2 | 6 | 0/4/3/1/1 | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） |
| `context` 上下文、压缩与缓存 | 5 | 36 | 36 | 61 | 59 | 0 | 0 | 2 | 0/0/1/0/1 | 2026-11-13（缺口审查） | — |
| `agents` 子代理与后台任务 | 4 | 30 | 29 | 91 | 88 | 0 | 3 | 0 | 0/0/3/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `hooks` hooks | 5 | 34 | 34 | 99 | 99 | 0 | 0 | 0 | 0/1/0/1/4 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `ext` 插件、skills、MCP | 5 | 25 | 25 | 146 | 127 | 0 | 18 | 1 | 0/0/1/0/0 | 2026-11-11（盲审） | 2026-11-11（代码审阅） |
| `memory` 记忆 | 4 | 8 | 8 | 35 | 34 | 0 | 0 | 1 | 0/0/2/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `auto` cron 与 always-on | 5 | 24 | 24 | 123 | 123 | 0 | 0 | 0 | 0/1/1/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `config` 设置、配置、热重载 | 7 | 24 | 24 | 86 | 82 | 0 | 2 | 2 | 0/1/4/0/0 | 2026-11-12（缺口审查） | — |
| `modules` 外部能力模块与 sidecar 部署 | 7 | 44 | 43 | 117 | 103 | 0 | 10 | 4 | 0/1/5/1/2 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `gw` gateway 连接层 | 5 | 52 | 51 | 125 | 117 | 4 | 4 | 0 | 0/0/3/1/1 | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） |
| `channels` 渠道（只要求启动表现） | 4 | 12 | 12 | 125 | 84 | 0 | 38 | 3 | 0/1/0/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `cli` CLI | 6 | 23 | 21 | 83 | 71 | 1 | 11 | 0 | 0/0/1/0/0 | 2026-11-12（缺口审查） | — |
| 横跨（`*`） | | | | | | | | | 0/0/1/1/6 | | |
| 归属不了 | | 22 | 22 | 0 | 0 | 0 | 0 | 0 | | | |

一个场景可以属于几个子能力，所以各域场景数之和大于场景总数；表面项每项只属于一个子能力。

## 按子能力

### `loop` 对话主干与 agent 循环

一轮对话从 submit_turn 受理到 turn_completed：循环的步进、终止与恢复，工具调用的调度，对客户端的事件流，运行记录。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `loop.submit` 轮次受理与每轮参数 | 8 | 8 | 34 | 34 | 0 | 0 | 0 | — | — | 2026-11-13（代码审阅） | `src/agent/turn/` `src/gateway/client/InProcessGateway.ts` |
| `loop.steer-abort` 插话与中止 | 52 | 35 | 30 | 25 | 2 | 0 | 3 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/agent/session/SteerMailbox.ts` `src/agent/loop/AgentLoop.ts` |
| `loop.continuation` 步进与终止 | 3 | 3 | 43 | 37 | 0 | 2 | 4 | parity | 2026-11-11（盲审） | — | `src/agent/loop/decideLoopContinuation.ts` `src/agent/policy/FailureGuard.ts` |
| `loop.recovery` 循环层的异常恢复 | 21 | 21 | 28 | 27 | 0 | 0 | 1 | — | 2026-11-13（缺口审查） | — | `src/agent/loop/AgentLoop.ts` `src/agent/loop/outputTokenRetry.ts` `src/agent/loop/LargeFileRepair.ts` |
| `loop.tool-dispatch` 工具调用的调度 | 4 | 4 | 6 | 6 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/tool/scheduler/` `src/agent/loop/collectToolCalls.ts` `src/agent/loop/ensureToolResultPairing.ts` `src/agent/modules/capability/hostToolPort.ts` |
| `loop.events` 事件流与轮次快照 | 4 | 4 | 28 | 27 | 0 | 0 | 1 | — | — | — | `src/gateway/client/GatewayAgentEventProjector.ts` `src/status/agentStatus.ts` |
| `loop.runs` 运行记录 | 2 | 2 | 26 | 26 | 0 | 0 | 0 | — | — | — | `src/gateway/run/` |

### `model` 模型协议与路由

发给模型 provider 的请求与流解析（四种协议），公共请求层，模型目录与会话模型选择，路由，用量与调用记录。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `model.openai-chat` OpenAI chat completions | 2 | 2 | 25 | 25 | 0 | 0 | 0 | — | — | — | `src/model/providers/openai/` |
| `model.anthropic` Anthropic messages | 3 | 3 | 25 | 25 | 0 | 0 | 0 | — | — | — | `src/model/providers/anthropic/` |
| `model.openai-responses` OpenAI Responses | 2 | 2 | 28 | 28 | 0 | 0 | 0 | — | — | — | `src/model/providers/openai-responses/` |
| `model.google` Google Gemini | 2 | 2 | 13 | 12 | 0 | 1 | 0 | — | — | 2026-11-13（代码审阅） | `src/model/providers/google/` |
| `model.transport` 公共请求层与错误 | 13 | 13 | 51 | 51 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/model/streaming/` `src/model/providerEndpoint.ts` `src/model/errors/` `src/model/policy/` |
| `model.catalog` 模型目录与会话模型 | 17 | 17 | 66 | 65 | 0 | 1 | 0 | — | 2026-11-13（缺口审查） | — | `src/model/config/` `src/model/catalog/` `src/model/thinking/` `src/gateway/dialog/modelCatalog.ts` |
| `model.router` 路由 | 14 | 14 | 50 | 50 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/router/` |
| `model.usage` 用量 | 2 | 2 | 7 | 7 | 0 | 0 | 0 | — | — | — | `src/model/response/normalizeUsage.ts` `src/gateway/client/InProcessGateway.ts` |
| `model.invocations` 模型调用记录 | 4 | 4 | 6 | 6 | 0 | 0 | 0 | — | — | — | `src/storage/invocationStorage.ts` |

### `tools` 工具

内置工具（文件、搜索、shell、execute_code、web、用户交互、计划与目标等）和工具运行时的公共部分。子代理与后台任务工具在 agents，权限与沙箱在 perm。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `tools.files` 文件工具 | 13 | 11 | 43 | 43 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/tool/builtin/readFile.ts` `src/tool/builtin/writeFile.ts` `src/tool/builtin/editFile.ts` `src/tool/builtin/sendAttachment.ts` |
| `tools.search` 搜索工具 | 4 | 4 | 24 | 23 | 0 | 1 | 0 | — | 2026-11-11（盲审） | — | `src/tool/builtin/glob.ts` `src/tool/builtin/grep.ts` |
| `tools.shell` bash | 4 | 4 | 15 | 13 | 0 | 2 | 0 | — | — | 2026-11-12（代码审阅） | `src/tool/builtin/` `src/runtime/commandShell.ts` |
| `tools.execute-code` execute_code | 8 | 8 | 19 | 15 | 0 | 3 | 1 | — | — | 2026-11-11（代码审阅） | `src/tool/builtin/executeCode.ts` |
| `tools.web` web 工具 | 13 | 13 | 37 | 36 | 0 | 1 | 0 | — | 2026-11-11（盲审） | — | `src/tool/builtin/webSearch.ts` `src/tool/builtin/additionalWebSearch.ts` `src/tool/builtin/webFetch.ts` |
| `tools.interaction` 向用户提问与对话框 | 16 | 16 | 79 | 79 | 0 | 0 | 0 | — | — | 2026-11-11（代码审阅） | `src/tool/elicitation/` `src/tool/dialog/` `src/gateway/elicitation/` `src/gateway/user-dialog/` |
| `tools.state` 计划、待办与目标 | 9 | 9 | 48 | 48 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/plan-todo/` `src/goal/` `src/tool/builtin/` |
| `tools.misc` 其他内建工具 | 4 | 4 | 9 | 9 | 0 | 0 | 0 | — | 2026-11-11（盲审） | 2026-11-13（代码审阅） | `src/tool/builtin/getCurrentTime.ts` `src/tool/builtin/lsp.ts` `src/lsp/` |
| `tools.runtime` 工具运行时公共部分 | 5 | 5 | 21 | 20 | 0 | 1 | 0 | — | 2026-11-13（缺口审查） | — | `src/tool/registry/` `src/tool/execution/` `src/tool/protocol/` |

### `perm` 权限、沙箱、路径检查

权限判定与询问、permissions.json、运行模式的工具约束、沙箱模式、路径检查、交互 profile 与交互绑定。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `perm.decision` 权限判定与询问 | 22 | 22 | 27 | 25 | 0 | 2 | 0 | — | — | 2026-11-13（代码审阅） | `src/permission/` `src/gateway/permission/` |
| `perm.rules-file` permissions.json 与托管权限 | 3 | 3 | 6 | 6 | 0 | 0 | 0 | — | 2026-11-11（盲审） | 2026-11-13（代码审阅） | `src/permission/settings.ts` `src/agent/turn/permissionOverrides.ts` |
| `perm.modes` 运行模式约束 | 7 | 7 | 7 | 7 | 0 | 0 | 0 | — | — | 2026-11-13（代码审阅） | `src/tool/planModeConstraints.ts` `src/tool/askModeConstraints.ts` |
| `perm.sandbox` 沙箱模式 | 14 | 14 | 6 | 6 | 0 | 0 | 0 | — | 2026-11-11（盲审） | 2026-11-13（代码审阅） | `src/sandbox/` `src/tool/execution-world/` |
| `perm.paths` 路径检查 | 7 | 7 | 4 | 4 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/tool/builtin/filesystem/pathSafety.ts` `src/pilot/writablePaths.ts` |
| `perm.interaction` 交互 profile 与交互绑定 | 15 | 15 | 5 | 5 | 0 | 0 | 0 | — | 2026-11-11（盲审） | 2026-11-13（代码审阅） | `src/interaction/` |

### `session` 会话存储、分叉、恢复、导出

会话的生命周期、落盘格式、读取、分叉与替换、导出与归档、文件历史与快照、上传与附件、崩溃后的恢复。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `session.lifecycle` 会话生命周期 | 10 | 10 | 28 | 24 | 0 | 0 | 4 | — | — | — | `src/session/catalog/` `src/gateway/SessionRouter.ts` `src/gateway/GatewaySessionCatalog.ts` |
| `session.transcript` 会话记录格式 | 11 | 11 | 53 | 50 | 0 | 1 | 2 | — | 2026-11-13（缺口审查） | — | `src/session/transcript/` `src/session/events/` `src/session/persistence/` `src/session/title/` |
| `session.read` 读会话 | 5 | 5 | 21 | 21 | 0 | 0 | 0 | — | — | — | `src/session/history/` |
| `session.fork-replace` 分叉与末轮替换 | 10 | 10 | 17 | 17 | 0 | 0 | 0 | — | — | — | `src/session/fork/` `src/session/replacement/` |
| `session.export` 导出、恢复与归档 | 5 | 5 | 26 | 26 | 0 | 0 | 0 | — | — | — | `src/session/artifacts/` `src/gateway/storage/` |
| `session.files` 文件历史、回退与快照 | 5 | 5 | 33 | 33 | 0 | 0 | 0 | — | — | — | `src/session/filesystem/` `src/storage/workspaceSnapshot.ts` |
| `session.uploads` 上传与附件 | 5 | 5 | 15 | 15 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/context/attachments/` `src/cli/GatewayUploadedAttachmentBundle.ts` |
| `session.recovery` 崩溃与重启后的恢复 | 9 | 9 | 13 | 12 | 0 | 1 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/session/resume/` `src/gateway/run/` |

### `context` 上下文、压缩与缓存

系统提示词的组装、上下文预算、压缩与溢出恢复、提示缓存、读状态。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `context.prompt` 系统提示词组装 | 5 | 5 | 21 | 21 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/context/prompt/` `src/context/instructions/` `src/context/RuntimeContextSurface.ts` |
| `context.compaction` 压缩 | 18 | 18 | 24 | 23 | 0 | 0 | 1 | parity | 2026-11-13（缺口审查） | — | `src/context/compaction/` `src/context/recovery/` |
| `context.budget` 上下文预算 | 4 | 4 | 7 | 6 | 0 | 0 | 1 | — | — | — | `src/context/budget/` |
| `context.cache` 提示缓存 | 6 | 6 | 1 | 1 | 0 | 0 | 0 | — | — | — | `src/context/cache/` |
| `context.read-state` 读状态 | 3 | 3 | 8 | 8 | 0 | 0 | 0 | — | — | — | `src/agent/loop/seedReadState.ts` |

### `agents` 子代理与后台任务

一次性子代理、可续跑的后台子代理、SDK 自定义子代理、后台 shell 任务。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `agents.oneshot` 一次性子代理 | 14 | 13 | 32 | 30 | 0 | 2 | 0 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/agent/sub/OneShotSubagentPort.ts` `src/agent/sub/builtinSubagentTypes.ts` `src/agent/sub/SubAgentSession.ts` |
| `agents.background` 可续跑与后台子代理 | 7 | 7 | 7 | 7 | 0 | 0 | 0 | — | — | — | `src/agent/sub/BackgroundSubagentRuntime.ts` `src/agent/sub/SubagentContinuationManager.ts` |
| `agents.sdk-defs` SDK 自定义子代理 | 4 | 4 | 18 | 18 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/cli/SessionAgentConfigBundle.ts` `src/agent/sub/SubagentRuntimeComposition.ts` |
| `agents.tasks` 后台 shell 任务 | 6 | 6 | 34 | 33 | 0 | 1 | 0 | — | — | — | `src/task/` `src/tool/builtin/taskTools.ts` |

### `hooks` hooks

hook 事件的派发时机、种类与来源、输出字段的处理、执行（异步、取消、超时、进程环境）。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `hooks.lifecycle-events` 会话与轮次事件 | 15 | 15 | 25 | 25 | 0 | 0 | 0 | — | — | — | `src/extension/hooks/events/` `src/extension/hooks/protocol/events.ts` |
| `hooks.tool-events` 工具相关事件 | 11 | 11 | 10 | 10 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-12（代码审阅） | `src/tool/execution/ToolRuntime.ts` `src/extension/hooks/events/` |
| `hooks.types-sources` 种类、来源与配置 | 6 | 6 | 21 | 21 | 0 | 0 | 0 | — | — | — | `src/extension/hooks/config/` `src/extension/contributions/HookContribution.ts` |
| `hooks.output` 输出字段的处理 | 5 | 5 | 23 | 23 | 0 | 0 | 0 | — | — | — | `src/extension/hooks/protocol/` |
| `hooks.execution` 执行 | 13 | 13 | 20 | 20 | 0 | 0 | 0 | — | — | 2026-11-13（代码审阅） | `src/extension/hooks/execution/` `src/gateway/hooks/` |

### `ext` 插件、skills、MCP

扩展的发现、加载与热更新：插件（含内置的 browser-use、funasr）、skills、MCP、命令。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `ext.plugins` 插件 | 4 | 4 | 30 | 12 | 0 | 17 | 1 | — | 2026-11-11（盲审） | — | `src/extension/plugins/` |
| `ext.skills` skills | 11 | 11 | 49 | 49 | 0 | 0 | 0 | — | — | — | `src/extension/skills/` |
| `ext.mcp` MCP | 11 | 11 | 37 | 36 | 0 | 1 | 0 | — | — | 2026-11-11（代码审阅） | `src/mcp/` |
| `ext.commands` 命令 | 3 | 3 | 27 | 27 | 0 | 0 | 0 | — | — | — | `src/extension/contributions/CommandContribution.ts` |
| `ext.reload` 扩展热更新 | 5 | 5 | 3 | 3 | 0 | 0 | 0 | — | — | — | `src/cli/ExtensionWatchManager.ts` `src/cli/GatewayRuntimeRefreshBundle.ts` |

### `memory` 记忆

memory 段、轮次中的捕获与检索、轮后维护、memory_* method。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `memory.config` 记忆配置与模型 | 3 | 3 | 18 | 17 | 0 | 0 | 1 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/context/memory/createEdgeClawMemoryProviderFromConfig.ts` `src/context/memory/edgeclaw-memory-core/` |
| `memory.capture-recall` 捕获与检索 | 2 | 2 | 5 | 5 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/context/memory/MemoryResolver.ts` `src/context/memory/MemoryAttachmentBuilder.ts` |
| `memory.maintenance` 轮后维护 | 1 | 1 | 5 | 5 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/ProjectMemoryMaintenanceController.ts` |
| `memory.methods` 记忆 method | 3 | 3 | 7 | 7 | 0 | 0 | 0 | — | — | — | `src/gateway/memoryDiagnostics.ts` `src/gateway/client/InProcessGateway.ts` |

### `auto` cron 与 always-on

定时任务与 always-on 的配置、接口、调度触发。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `auto.cron-config` cron 配置与时区 | 3 | 3 | 5 | 5 | 0 | 0 | 0 | — | — | 2026-11-13（代码审阅） | `src/cron/config/` `src/cron/CronTimezone.ts` |
| `auto.cron-api` cron 接口 | 7 | 7 | 61 | 61 | 0 | 0 | 0 | — | — | — | `src/cron/runtime/` `src/cron/tool/` |
| `auto.cron-fire` cron 触发 | 3 | 3 | 2 | 2 | 0 | 0 | 0 | — | — | — | `src/cron/runtime/` |
| `auto.always-on-cycle` always-on 调度与一轮 | 6 | 6 | 27 | 27 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/always-on/runtime/` `src/always-on/config/` |
| `auto.always-on-api` always-on 接口与工具 | 7 | 7 | 28 | 28 | 0 | 0 | 0 | — | — | — | `src/always-on/tool/` `src/always-on/protocol/` |

### `config` 设置、配置、热重载

pilotdeck.yaml 的解析与诊断、项目级配置、设置 method、热重载、SDK 会话配置的校验、环境变量与目录、遥测。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `config.load` 配置解析与诊断 | 7 | 7 | 13 | 13 | 0 | 0 | 0 | — | — | — | `src/pilot/config/loadPilotConfig.ts` |
| `config.project` 项目级配置与设置来源 | 6 | 6 | 23 | 23 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/pilot/config/sdkSessionSettings.ts` `src/pilot/config/loadPilotConfig.ts` |
| `config.settings-api` 设置 method | 2 | 2 | 9 | 9 | 0 | 0 | 0 | — | — | — | `src/gateway/client/InProcessGateway.ts` `src/pilot/config/` |
| `config.hot-reload` 热重载 | 2 | 2 | 2 | 1 | 0 | 0 | 1 | — | — | — | `src/pilot/config/PilotConfigStore.ts` |
| `config.sdk-session` SDK 会话配置的校验 | 2 | 2 | 1 | 1 | 0 | 0 | 0 | — | — | — | `src/gateway/protocol/types.ts` `src/gateway/client/InProcessGateway.ts` |
| `config.env-paths` 环境变量与目录 | 6 | 6 | 19 | 16 | 0 | 2 | 1 | — | 2026-11-12（缺口审查） | — | `src/pilot/paths.ts` `src/cli/proxy.ts` `src/pilot/writablePaths.ts` |
| `config.telemetry` 遥测 | 2 | 2 | 19 | 19 | 0 | 0 | 0 | — | — | — | `src/telemetry/` |

### `modules` 外部能力模块与 sidecar 部署

modules 段的外部绑定、外部工具与知识库、agent 循环 sidecar 的 stdio 与 TCP 部署、公共 host HTTP、SOP。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `modules.binding` modules 段与绑定 | 5 | 5 | 31 | 31 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/pilot/config/parseModulesConfig.ts` `src/composition/` |
| `modules.tools` 外部工具模块 | 13 | 12 | 3 | 3 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/composition/runtimePorts.ts` `src/composition/HttpModuleClient.ts` `src/agent/modules/capability/hostToolPort.ts` |
| `modules.knowledge` 知识库模块 | 2 | 2 | 5 | 5 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/composition/domainPorts.ts` |
| `modules.sidecar-stdio` sidecar stdio 部署 | 9 | 9 | 14 | 12 | 0 | 2 | 0 | client deployment schema parity stdio | 2026-11-13（缺口审查） | — | `src/agent/modules/transport/` `src/cli/AgentLoopDeploymentProfile.ts` `src/cli/pilotdeck-agent-loop-sidecar.ts` |
| `modules.sidecar-tcp` sidecar TCP 部署 | 13 | 13 | 9 | 9 | 0 | 0 | 0 | tcp | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/agent/modules/transport/` `src/cli/pilotdeck-agent-loop-sidecar.ts` |
| `modules.host-http` 公共 host 与审批 HTTP | 4 | 4 | 6 | 6 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/composition/publicHostHttpTransport.ts` `src/composition/publicApprovalHttpTransport.ts` `src/composition/activeRuntimeModelPorts.ts` |
| `modules.sop` SOP | 4 | 4 | 49 | 37 | 0 | 8 | 4 | — | 2026-11-13（缺口审查） | — | `src/sop/` `src/composition/nativeSopAuthority.ts` |

### `gw` gateway 连接层

pilotdeck server 的启停、WS 握手与帧、HTTP 路由、连接的健壮性、项目路由与描述。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `gw.server-entry` 服务启停 | 12 | 12 | 23 | 22 | 0 | 1 | 0 | — | 2026-11-13（缺口审查） | — | `src/cli/pilotdeckServer.ts` `src/cli/PilotDeckServerShutdownBundle.ts` `src/gateway/server/authToken.ts` |
| `gw.ws-protocol` WS 握手与帧 | 6 | 6 | 36 | 36 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/gateway/server/GatewayWsConnection.ts` `src/gateway/protocol/` |
| `gw.http` HTTP 路由 | 8 | 7 | 27 | 26 | 1 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/gateway/server/GatewayServer.ts` `src/gateway/server/staticAssets.ts` `src/adapters/web/` |
| `gw.connections` 连接健壮性 | 22 | 22 | 14 | 9 | 3 | 2 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/gateway/server/websocket.ts` `src/gateway/server/GatewayServer.ts` |
| `gw.projects` 项目路由与描述 | 6 | 6 | 25 | 24 | 0 | 1 | 0 | — | 2026-11-13（缺口审查） | — | `src/cli/ProjectRuntimeRegistry.ts` `src/gateway/GatewaySessionCatalog.ts` |

### `channels` 渠道（只要求启动表现）

各渠道启用后的启动表现、渠道自带的 HTTP 入口、微信登录、渠道命令。按范围只要求与基线启动表现一致。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `channels.startup` 启动表现 | 5 | 5 | 107 | 68 | 0 | 36 | 3 | — | — | 2026-11-13（代码审阅） | `src/adapters/channel/loadEnabledChannels.ts` `src/cli/ChannelAdapterBundle.ts` |
| `channels.http-endpoints` 渠道 HTTP 入口 | 6 | 6 | 13 | 13 | 0 | 0 | 0 | — | — | — | `src/adapters/channel/` |
| `channels.weixin-login` 微信登录 | 1 | 1 | 2 | 2 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/adapters/channel/weixin/` `src/cli/pilotdeck.ts` |
| `channels.commands` 渠道命令 | 0 | 0 | 3 | 1 | 0 | 2 | 0 | — | — | — | `src/adapters/channel/` |

### `cli` CLI

pilotdeck 的子命令（server 归 gw.server-entry，sidecar 可执行文件归 modules）。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `cli.entry` 入口与帮助 | 5 | 4 | 1 | 1 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/pilotdeck.ts` |
| `cli.default-chat` 默认对话与 tui | 2 | 2 | 17 | 6 | 0 | 11 | 0 | — | 2026-11-12（缺口审查） | — | `src/adapters/channel/cli/` `src/adapters/channel/tui/` |
| `cli.sdk-commands` run / resume / sessions / settings | 9 | 9 | 27 | 27 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/sdkCli.ts` |
| `cli.cron` cron 子命令 | 1 | 1 | 17 | 17 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/pilotdeck.ts` |
| `cli.skills-chat` skills migrate 与 chat search | 4 | 4 | 19 | 19 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/extension/skills/migrateSkills.ts` `src/cli/commands/chatSearch.ts` |
| `cli.setup-update` gateway setup 与 update | 3 | 2 | 2 | 1 | 1 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/commands/gatewaySetup.ts` `src/runtime/updateCommand.ts` |

## 缺口清单

这一节是「接下来派什么活」的来源。

### 没被一致的场景碰到的表面项

「没碰到」是没有场景碰到；「只被有差异的碰到」是碰到它的场景都不是 SAME（括号里是那些场景）。

#### `loop` 对话主干与 agent 循环（4）

- `loop.continuation`
  - 没碰到：`loop-behaviors` `loop:agentError:agent_execution_rejected` — src/agent/protocol/errors.ts:12
  - 没碰到：`loop-behaviors` `loop:agentError:agent_transcript_error` — src/agent/protocol/errors.ts:11
- `loop.steer-abort`
  - 只被有差异的碰到（`turn-steer-terminal`）：`events` `event:steer_unapplied` — src/gateway/protocol/types.ts:575
  - 只被有差异的碰到（`turn-steer-terminal`）：`loop-behaviors` `loop:turn_continued:user_steer` — src/agent/protocol/state.ts:11

#### `model` 模型协议与路由（2）

- `model.catalog`
  - 没碰到：`env` `env:STAFFDECK_SOP_MANAGEMENT_API_KEY` — src/pilot/staffdeck/projectBinding.ts:245
- `model.google`
  - 没碰到：`provider-protocols` `google.body.config.automaticFunctionCalling` — src/model/providers/google/request.ts:37

#### `tools` 工具（8）

- `tools.execute-code`
  - 没碰到：`env` `env:PILOTDECK_RPC_PORT` — src/tool/builtin/executeCode.ts:818
  - 没碰到：`env` `env:PILOTDECK_RPC_TOKEN` — src/tool/builtin/executeCode.ts:819
  - 没碰到：`tool-params` `execute_code.error:invalid_rpc_token` — src/tool/builtin/executeCode.ts:595
- `tools.runtime`
  - 没碰到：`tool-params` `*.error:tool_aborted` — src/tool/execution/errorRecovery.ts:176
- `tools.search`
  - 没碰到：`tool-params` `<ripgrep>.error:tool_aborted` — src/tool/builtin/filesystem/ripgrep.ts:49
- `tools.shell`
  - 没碰到：`env` `env:LD_LIBRARY_PATH` — src/sandbox/nsjail/NsjailProvider.ts:496
  - 没碰到：`env` `env:PATH` — src/sandbox/nsjail/NsjailProvider.ts:256
- `tools.web`
  - 没碰到：`tool-params` `web_fetch.error:tool_aborted` — src/tool/builtin/webFetch.ts:401

#### `perm` 权限、沙箱、路径检查（2）

- `perm.decision`
  - 没碰到：`records` `transcript:permission_failed` — src/session/transcript/TranscriptEntry.ts:47
  - 没碰到：`tool-params` `*.error:permission_cancelled` — src/tool/execution/ToolRuntime.ts:346

#### `session` 会话存储、分叉、恢复、导出（2）

- `session.recovery`
  - 没碰到：`recovery-behaviors` `recovery:transcript.too-large` — src/session/transcript/TranscriptReader.ts:19
- `session.transcript`
  - 没碰到：`records` `transcript:agent_turn_discarded` — src/session/transcript/TranscriptEntry.ts:28

#### `agents` 子代理与后台任务（3）

- `agents.oneshot`
  - 没碰到：`tool-params` `agent.error:subagent_launch_failed` — src/tool/builtin/agent.ts:492
  - 没碰到：`tool-params` `agent.error:tool_aborted` — src/tool/builtin/agent.ts:408
- `agents.tasks`
  - 没碰到：`tool-params` `task_create\|task_list\|task_output\|task_stop\|task_wait.error:tool_aborted` — src/tool/builtin/taskTools.ts:444

#### `ext` 插件、skills、MCP（18）

- `ext.mcp`
  - 没碰到：`env` `env:USERPROFILE` — src/mcp/config/expandPlaceholders.ts:12
- `ext.plugins`
  - 没碰到：`env` `env:APPDATA` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:50
  - 没碰到：`env` `env:LOCALAPPDATA` — src/runtime/commandShell.ts:78
  - 没碰到：`env` `env:PILOTDECK_BROWSER_ACTION_TIMEOUT_MS` — src/cli/createLocalGateway.ts:1742
  - 没碰到：`env` `env:PILOTDECK_BROWSER_NAVIGATION_TIMEOUT_MS` — src/cli/createLocalGateway.ts:1751
  - 没碰到：`env` `env:PILOTDECK_BROWSER_PROXY_BYPASS` — src/cli/createLocalGateway.ts:1804
  - 没碰到：`env` `env:PILOTDECK_BROWSER_PROXY_FROM_ENV` — src/cli/createLocalGateway.ts:1785
  - 没碰到：`env` `env:PILOTDECK_BROWSER_PROXY_SERVER` — src/cli/createLocalGateway.ts:1780
  - 没碰到：`env` `env:PILOTDECK_BROWSER_TIMEOUT_ACTION_MS` — src/cli/createLocalGateway.ts:1741
  - 没碰到：`env` `env:PILOTDECK_BROWSER_TIMEOUT_NAVIGATION_MS` — src/cli/createLocalGateway.ts:1750
  - 没碰到：`env` `env:PILOTDECK_DESKTOP_DOWNLOAD_MIRROR` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:80
  - 没碰到：`env` `env:PILOTDECK_DESKTOP_PLAYWRIGHT_ARCHIVE_DIR` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:77
  - 没碰到：`env` `env:PILOTDECK_DESKTOP_PLAYWRIGHT_BROWSER_SET` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:84
  - 没碰到：`env` `env:PILOTDECK_DESKTOP_PLAYWRIGHT_CHROME_BASE_URL` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:109
  - 没碰到：`env` `env:PILOTDECK_DESKTOP_PLAYWRIGHT_MIRROR` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:78
  - 没碰到：`env` `env:PILOTDECK_SESSION_BROWSER_PROXY` — src/sandbox/egress/SessionEgressProxy.ts:283
  - 没碰到：`env` `env:PLAYWRIGHT_BROWSERS_PATH` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:45
  - 没碰到：`tools` `tool:<extension contribution>` — src/tool/registry/registerExtensionToolContributions.ts:17

#### `config` 设置、配置、热重载（2）

- `config.env-paths`
  - 没碰到：`env` `env:PILOTDECK_RUNTIME_PROXY` — src/sandbox/nsjail/NsjailProvider.ts:571
  - 没碰到：`env` `env:PILOTDECK_RUNTIME_ROOT` — src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:16

#### `modules` 外部能力模块与 sidecar 部署（10）

- `modules.sidecar-stdio`
  - 没碰到：`module-behaviors` `mod:sidecar.proto-control` — src/agent/modules/transport/agentLoopSidecarServer.ts:146
  - 没碰到：`module-behaviors` `mod:sidecar.proto-errors` — src/agent/modules/transport/agentLoopSidecarServer.ts:112
- `modules.sop`
  - 没碰到：`env` `env:DATABASE_PATH` — src/cli/createLocalGateway.ts:778
  - 没碰到：`env` `env:STAFFDECK_APPROVAL_USER_ID` — src/cli/createLocalGateway.ts:977
  - 没碰到：`env` `env:STAFFDECK_COPY_ACTOR_USER_ID` — src/composition/nativeApprovalSessionResolver.ts:75
  - 没碰到：`env` `env:STAFFDECK_COPY_USER_TOKEN` — src/composition/nativeApprovalSessionResolver.ts:77
  - 没碰到：`env` `env:STAFFDECK_FORMAL_API_ORIGIN` — src/cli/createLocalGateway.ts:977
  - 没碰到：`env` `env:STAFFDECK_PROJECT_BINDING_ENABLED` — src/cli/createLocalGateway.ts:965
  - 没碰到：`env` `env:STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID` — src/composition/nativeApprovalSessionResolver.ts:76
  - 没碰到：`env` `env:STAFFDECK_SOP_RUNTIME_ORIGIN` — src/pilot/staffdeck/projectBinding.ts:162

#### `gw` gateway 连接层（8）

- `gw.connections`
  - 只被有差异的碰到（`gw-http-raw-requests`）：`gw-behaviors` `gw:conn.header-too-large` — node:http 缺省 maxHeaderSize
  - 没碰到：`gw-behaviors` `gw:conn.http-timeouts` — src/gateway/server/GatewayServer.ts:64
  - 只被有差异的碰到（`gw-http-raw-requests`）：`gw-behaviors` `gw:conn.keepalive-pipelining` — node:http
  - 只被有差异的碰到（`gw-http-raw-requests`）：`gw-behaviors` `gw:conn.malformed-request` — node:http clientError 缺省处理
  - 没碰到：`gw-behaviors` `gw:conn.no-handshake-timeout` — src/gateway/server/websocket.ts:17
- `gw.http`
  - 只被有差异的碰到（`gw-http-raw-requests`）：`gw-behaviors` `gw:http.url-parse` — src/gateway/server/GatewayServer.ts:95
- `gw.projects`
  - 没碰到：`gw-behaviors` `gw:projects.files-list-limits` — src/gateway/dialog/projectFiles.ts:71
- `gw.server-entry`
  - 没碰到：`env` `env:PILOTDECK_MEMORY_DIAGNOSTICS` — src/gateway/memoryDiagnostics.ts:26

#### `channels` 渠道（只要求启动表现）（38）

- `channels.commands`
  - 没碰到：`slash-commands` `channel-command:/ask` — src/adapters/channel/qq/QQChannel.ts:19
  - 没碰到：`slash-commands` `channel-command:/chat` — src/adapters/channel/qq/QQChannel.ts:19
- `channels.startup`
  - 没碰到：`env` `env:BLUEBUBBLES_PASSWORD` — src/adapters/channel/bluebubbles/BlueBubblesChannel.ts:55
  - 没碰到：`env` `env:DINGTALK_CLIENT_ID` — src/adapters/channel/dingtalk/DingTalkChannel.ts:48
  - 没碰到：`env` `env:DINGTALK_CLIENT_SECRET` — src/adapters/channel/dingtalk/DingTalkChannel.ts:49
  - 没碰到：`env` `env:DINGTALK_STREAM_DEBUG` — src/adapters/channel/dingtalk/DingTalkChannel.ts:69
  - 没碰到：`env` `env:DISCORD_BOT_TOKEN` — src/adapters/channel/discord/DiscordChannel.ts:41
  - 没碰到：`env` `env:EMAIL_ADDRESS` — src/adapters/channel/email/EmailChannel.ts:66
  - 没碰到：`env` `env:EMAIL_PASSWORD` — src/adapters/channel/email/EmailChannel.ts:67
  - 没碰到：`env` `env:HASS_URL` — src/adapters/channel/homeassistant/HomeAssistantChannel.ts:66
  - 没碰到：`env` `env:IMAP_HOST` — src/adapters/channel/email/EmailChannel.ts:68
  - 没碰到：`env` `env:IMAP_PORT` — src/adapters/channel/email/EmailChannel.ts:70
  - 没碰到：`env` `env:MATRIX_ACCESS_TOKEN` — src/adapters/channel/matrix/MatrixChannel.ts:50
  - 没碰到：`env` `env:MATRIX_HOMESERVER` — src/adapters/channel/matrix/MatrixChannel.ts:51
  - 没碰到：`env` `env:MATRIX_USER_ID` — src/adapters/channel/matrix/MatrixChannel.ts:52
  - 没碰到：`env` `env:MATTERMOST_TEAM_ID` — src/adapters/channel/mattermost/MattermostChannel.ts:50
  - 没碰到：`env` `env:MATTERMOST_TOKEN` — src/adapters/channel/mattermost/MattermostChannel.ts:48
  - 没碰到：`env` `env:MATTERMOST_URL` — src/adapters/channel/mattermost/MattermostChannel.ts:49
  - 没碰到：`env` `env:QQ_ALLOW_GROUPS` — src/adapters/channel/qq/QQChannel.ts:44
  - 没碰到：`env` `env:SIGNAL_ACCOUNT` — src/adapters/channel/signal/SignalChannel.ts:86
  - 没碰到：`env` `env:SIGNAL_HTTP_URL` — src/adapters/channel/signal/SignalChannel.ts:84
  - 没碰到：`env` `env:SLACK_APP_TOKEN` — src/adapters/channel/slack/SlackChannel.ts:42
  - 没碰到：`env` `env:SMTP_HOST` — src/adapters/channel/email/EmailChannel.ts:69
  - 没碰到：`env` `env:SMTP_PORT` — src/adapters/channel/email/EmailChannel.ts:71
  - 没碰到：`env` `env:TELEGRAM_BOT_TOKEN` — src/adapters/channel/telegram/TelegramChannel.ts:42
  - 没碰到：`env` `env:TWILIO_ACCOUNT_SID` — src/adapters/channel/sms/SmsChannel.ts:66
  - 没碰到：`env` `env:TWILIO_AUTH_TOKEN` — src/adapters/channel/sms/SmsChannel.ts:68
  - 没碰到：`env` `env:TWILIO_PHONE_NUMBER` — src/adapters/channel/sms/SmsChannel.ts:70
  - 没碰到：`env` `env:TWILIO_WEBHOOK_PORT` — src/adapters/channel/sms/SmsChannel.ts:72
  - 没碰到：`env` `env:WECOM_AGENT_ID` — src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:101
  - 没碰到：`env` `env:WECOM_ALLOWED_USERS` — src/adapters/channel/wecom/WeComChannel.ts:215
  - 没碰到：`env` `env:WECOM_CB_PORT` — src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:105
  - 没碰到：`env` `env:WECOM_CB_SECRET` — src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:102
  - 没碰到：`env` `env:WECOM_DM_POLICY` — src/adapters/channel/wecom/WeComChannel.ts:214
  - 没碰到：`env` `env:WECOM_GROUP_POLICY` — src/adapters/channel/wecom/WeComChannel.ts:216
  - 没碰到：`env` `env:WECOM_WEBSOCKET_URL` — src/adapters/channel/wecom/WeComChannel.ts:208
  - 没碰到：`env` `env:WHATSAPP_BRIDGE_PATH` — src/adapters/channel/whatsapp/WhatsAppChannel.ts:51
  - 没碰到：`env` `env:WHATSAPP_BRIDGE_URL` — src/adapters/channel/whatsapp/WhatsAppChannel.ts:53

#### `cli` CLI（12）

- `cli.default-chat`
  - 没碰到：`env` `env:PAGER` — src/adapters/channel/tui/TuiChannel.ts:44
  - 没碰到：`slash-commands` `tui-command:/clear` — src/adapters/channel/tui/app/TuiApp.tsx:806
  - 没碰到：`slash-commands` `tui-command:/dashboard` — src/adapters/channel/tui/app/TuiApp.tsx:689
  - 没碰到：`slash-commands` `tui-command:/exit` — src/adapters/channel/tui/app/TuiApp.tsx:812
  - 没碰到：`slash-commands` `tui-command:/help` — src/adapters/channel/tui/app/TuiApp.tsx:809
  - 没碰到：`slash-commands` `tui-command:/mode` — src/adapters/channel/tui/app/TuiApp.tsx:765
  - 没碰到：`slash-commands` `tui-command:/new` — src/adapters/channel/tui/app/TuiApp.tsx:678
  - 没碰到：`slash-commands` `tui-command:/permissions` — src/adapters/channel/tui/app/TuiApp.tsx:706
  - 没碰到：`slash-commands` `tui-command:/sessions` — src/adapters/channel/tui/app/TuiApp.tsx:687
  - 没碰到：`slash-commands` `tui-command:/switch` — src/adapters/channel/tui/app/TuiApp.tsx:688
  - 没碰到：`slash-commands` `tui-command:/view` — src/adapters/channel/tui/app/TuiApp.tsx:787
- `cli.setup-update`
  - 只被有差异的碰到（`cli-update-usage`）：`cli` `cli:update` — src/cli/pilotdeck.ts:302

### 没有任何场景的子能力

- `channels.commands` 渠道命令

### 没审过的功能域

没有任何盲审或缺口审查记录的（0）：无

没有任何代码审阅记录的（3）：`context`、`config`、`cli`

### 归属不了的场景

- `cli-cron-online-auth`（SAME）
- `cli-cron-online-create`（SAME）
- `cli-cron-online-delete-stop`（SAME）
- `cli-cron-online-disabled`（SAME）
- `cli-cron-online-gone-error`（SAME）
- `cli-cron-online-gone-ok`（SAME）
- `cli-cron-online-gone-usage`（SAME）
- `cli-cron-online-list`（SAME）
- `cli-cron-online-usage`（SAME）
- `cli-default-online-auth`（SAME）
- `cli-default-online-gone`（SAME）
- `cli-default-online-rebind`（SAME）
- `cli-default-online-turns`（SAME）
- `gw-crash-frame-length-overflow`（SAME）
- `hook-advisory-abort-after-turn`（SAME）
- `loop-task-budget`（SAME）
- `loop-task-budget-invalid`（SAME）
- `loop-task-budget-project`（SAME）
- `model-complete-retry-stderr`（SAME）
- `params-submit-turn-cwd-type-tools`（SAME）
- `session-two-writers-server-cli`（SAME）
- `session-two-writers-servers`（SAME）

## 归属统计

- 表面项 2113 个：按规则归属 2112，例外 1，归属不了 0。
- 场景 575 个：按规则归属 396，例外 157，归属不了 22。

## 与 `coverage.mjs --surface` 的对照

两者用同一个判定（`scripts/bbx/lib/surface.mjs`）。对同一产物目录运行 `node scripts/bbx/coverage.mjs --surface --out <产物目录>` 应打印与下表相同的数字（场景 551/575 SAME）。

| 维度 | 项数 | 被SAME场景碰到 | 只被有差异的场景碰到 | 没碰到 | 不计 |
|---|---|---|---|---|---|
| config | 275 | 275 | 0 | 0 | 0 |
| project-config | 12 | 12 | 0 | 0 | 0 |
| files | 84 | 72 | 0 | 0 | 12 |
| env | 192 | 122 | 0 | 70 | 0 |
| tools | 54 | 53 | 0 | 1 | 0 |
| tool-params | 260 | 252 | 0 | 8 | 0 |
| events | 88 | 80 | 1 | 0 | 7 |
| records | 82 | 78 | 0 | 2 | 2 |
| provider-protocols | 105 | 104 | 0 | 1 | 0 |
| cli | 69 | 68 | 1 | 0 | 0 |
| http | 23 | 23 | 0 | 0 | 0 |
| ws-methods | 101 | 101 | 0 | 0 | 0 |
| ws-params | 337 | 337 | 0 | 0 | 0 |
| loop-behaviors | 116 | 106 | 1 | 2 | 7 |
| hooks | 74 | 74 | 0 | 0 | 0 |
| channels | 22 | 22 | 0 | 0 | 0 |
| sdk-session-config | 75 | 75 | 0 | 0 | 0 |
| slash-commands | 37 | 25 | 0 | 12 | 0 |
| gw-behaviors | 75 | 68 | 4 | 3 | 0 |
| recovery-behaviors | 13 | 12 | 0 | 1 | 0 |
| module-behaviors | 19 | 16 | 0 | 2 | 1 |
| 合计 | 2113 | 1975 | 7 | 102 | 29 |
