# 进度总表（`dev/takeover`）

> 本文件由 `node scripts/bbx/status.mjs` 生成，不要手改。改功能清单改 `scripts/bbx/features.json`，改问题改 `state/issues.md`，改审阅记录改 `state/audits.md`，改检查结果改 `state/check-results.json`，然后重新生成。

- 生成时间：2026-11-14T08:10:32Z
- 仓库提交：`34260db33`（场景、表面清单、功能清单、台账都取自这个提交的工作区）
- 对拍产物：`_build/bbx`
- 产物来历（人给的说明）：cb8（Linux aarch64）上 verify.sh v27 的一轮全量对拍（槽位 5–8 并行），被测提交 34260db3
- 产物里的场景结果：636 个 `.diff.txt`；修改时间 2026-11-14T07:23:11.053Z 至 2026-11-14T08:04:15.278Z（UTC）；按小时：2026-11-14T07:00Z 604 个，2026-11-14T08:00Z 32 个
- 采集平台（从 diff 文件头里的产物路径判断）：Linux 636 个
- 产物里有、当前提交没有的场景 0 个；当前提交有、产物里没有的（记为「未跑」）0 个
- 产物不记录被测提交号，也不记录是哪一轮；修改时间跨度大或分成几段时，可能混合了多轮。

## 八条标准

| 条 | 标准 | 现在（由数据算出） |
|---|---|---|
| 1 | 全量对拍稳定一致 | 场景 636 个：SAME 622，DIFF 14，INVALID 0，未跑 0。不是 SAME 的里属于允许清单 2 个、登记了 accepted 1 个、其余 11 个（逐个见下）。连续 3 轮稳定：需要连续 3 轮的产物，未提供 |
| 2 | 表面覆盖完整 | 表面项 2113 个，范围内 2004 个：被 SAME 场景碰到 2001，只被有差异的场景碰到 3，没碰到 0；标为不计 109（理由表在 `state/out-of-scope.md`，待人过目） |
| 3 | 盲审收敛 | 审阅记录 10 次，其中类型为「盲审」的 1 次；16 个功能域里从没有过盲审或缺口审查的 0 个。各功能域最近一次见下表。「两次连续盲审覆盖全部区域且无 1、2 级新发现」：不满足 |
| 4 | 原版界面可用 | 没有数据：需要 `scripts/ui-smoke` 的当次结果和人亲手使用的记录 |
| 5 | 构建与测试全绿，没有占位 | 四条命令：2026-11-14 在 cb8（Linux aarch64） 上、提交 `d0e857162`：fmt pass，clippy pass，nextest pass（7784/7784），文档测试 pass。`src/` 产品代码里范围内的占位：`unimplemented!` 0 处、`todo!` 0 处、`NOT_IMPLEMENTED` 0 处、`Failure::Todo` 6 处、自报未实现文案 13 处、宿主边界哨兵 25 处、原生档案缺口 42 处、范围外不支持却没列进清单的 0 处（共 86，逐条见下）；已定范围外 10 处；范围外清单里失效的 0 条（`CAPABILITY_UNAVAILABLE`、返回固定空结果、空 match 分支没有机械判据，未统计，逐处定性见 `specs/delivery/placeholder-inventory.md`） |
| 6 | TS gateway 退役 | 透明代理入口的机械判据命中 0 处。进程树里没有 Node gateway 的证据：没有数据（需要全量对拍与界面冒烟期间的进程列表） |
| 7 | 性能报告 | 没有数据：需要发布构建下两侧的启动耗时、每轮开销、内存、并发会话的测量 |
| 8 | 真实使用 | 没有数据：需要人在最终组合上完成的真实任务记录 |

上游 sidecar 验收集（`scripts/upstream-tests`，挂在 `context.compaction`、`loop.continuation`、`modules.sidecar-stdio`、`modules.sidecar-tcp`）：2026-11-13 在 cb8（Linux aarch64）；macOS 上同样的结果见 3d7c9b2ef 上、提交 `9a5fb723b`：client 计入 28 个用例，指向 Rust 全过（Linux 1 轮；macOS 10 轮）；tcp 计入 3 个用例，全过；deployment 计入 1 个用例，全过；schema Rust sidecar 发出的 1204 帧全部合法，宿主的 715 帧全部合法；parity 基线自比稳定的 17 个场景里过 3 个（其余 14 个是写合并造成的记录先后，见台账第 4 级）。来源：cb8 上 ~/ws/pdrs-takeover/logs/upstream-v15.log

### 第 1 条：不是 SAME 的场景

| 场景 | 结论 | 归类 | 子能力 |
|---|---|---|---|
| `abort-model-stream` | DIFF | 其余 | `loop.steer-abort` |
| `abort-timeout-tool-mcp` | DIFF | 其余 | `loop.steer-abort` |
| `abort-tool-bash` | DIFF | 其余 | `loop.steer-abort` |
| `abort-tool-web-fetch` | DIFF | 其余 | `loop.steer-abort` |
| `abort-turn-finishing` | DIFF | 其余 | `loop.steer-abort` |
| `gap-tool-read-file-vision` | DIFF | 允许不一致的清单 | `tools.files` |
| `hot-reload-entries` | DIFF | 场景登记了 accepted（登记以外仍有差异） | 归属不了 |
| `loop-parallel-tools` | DIFF | 其余 | `loop.tool-dispatch` |
| `modules-tools-result-unknown-sidecar-binding` | DIFF | 其余 | `modules.tools` |
| `session-odd-shapes-turn` | DIFF | 其余 | 归属不了 |
| `tool-execute-code` | DIFF | 其余 | `tools.execute-code` |
| `tool-write-file-diagnostics` | DIFF | 允许不一致的清单 | `tools.files` |
| `turn-abort-before-request-status` | DIFF | 其余 | `loop.steer-abort` |
| `turn-steer-terminal` | DIFF | 其余 | `loop.steer-abort` |

### 第 5 条：占位的逐处位置

判据：`src/` 下的 `.rs`，跳过 `target`、`tests`、`benches`、`examples`、`fixtures` 目录和 `tests.rs`、`*_tests.rs`、`test_*.rs` 一类文件名，跳过整行注释和顶层的 `#[cfg(test)] mod x { … }` 块；HTTP 状态码 `StatusCode::NOT_IMPLEMENTED`（501）不算。`Failure::Todo` 只数构造处（`Err(Failure::Todo(what))` 一类匹配分支不算）；自报未实现文案按已知措辞列举（见 `status.mjs` 的 `PLACEHOLDERS`），新措辞要补进去；宿主边界哨兵与原生档案缺口数的是命中行（含构造与转交处）。各处是缺口、一致还是环境性，见 `specs/delivery/placeholder-inventory.md`。

已定范围外的命中按 `scripts/bbx/placeholder-out-of-scope.json` 逐处认定（文件全等、命中行含原文、每条只认一处），不在清单里的命中都算范围内的占位。

| 判据 | 位置 | 归类 | 已定范围外的哪一项 |
|---|---|---|---|
| `Failure::Todo` | `pilotdeck-cli/src/config.rs:26` | 范围内的占位 | — |
| `Failure::Todo` | `pilotdeck-cli/src/main.rs:144` | 范围内的占位 | — |
| `Failure::Todo` | `pilotdeck-cli/src/paths.rs:65` | 范围内的占位 | — |
| `Failure::Todo` | `pilotdeck-cli/src/skills.rs:29` | 范围内的占位 | — |
| `Failure::Todo` | `pilotdeck-cli/src/skills.rs:256` | 范围内的占位 | — |
| `Failure::Todo` | `pilotdeck-cli/src/skills_migrate.rs:123` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-agent/src/turn.rs:4154` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/native_process/assembly.rs:2494` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/native_process/management.rs:33` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/native_process/methods/metadata.rs:70` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/native_process/methods/sop.rs:133` | 已定范围外 | staffdeck |
| `自报未实现文案` | `pilotdeck-gateway/src/runtime/mcp/output_schema.rs:471` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/runtime/mcp/output_schema.rs:2157` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/runtime/mcp/output_schema.rs:2940` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/runtime/project/factory.rs:644` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/runtime/settings/local_settings.rs:342` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/runtime/tools/builtin/form_dialog_2020.rs:710` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/storage/transcript_adapter.rs:490` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-gateway/src/storage/transcript_adapter.rs:1628` | 范围内的占位 | — |
| `自报未实现文案` | `pilotdeck-transport/src/default_factory.rs:1175` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/binding.rs:27` | 已定范围外 | staffdeck |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/binding.rs:28` | 已定范围外 | staffdeck |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/graph.rs:344` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/graph.rs:345` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/mod.rs:516` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/mod.rs:517` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/sections.rs:143` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/config/sections.rs:146` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/native_archive.rs:285` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/native_archive.rs:293` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/native_archive.rs:574` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/project_files.rs:82` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/project_files.rs:836` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/project_files.rs:965` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/runtime/memory/edgeclaw/service/mod.rs:192` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/runtime/memory/mod.rs:118` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/runtime/memory/mod.rs:120` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/runtime/snapshot.rs:104` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/runtime/snapshot.rs:107` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/session_model.rs:333` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/session_model.rs:334` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/session_model.rs:345` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/session_model.rs:346` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/skills/error.rs:163` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/skills/error.rs:164` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/skills/error.rs:173` | 范围内的占位 | — |
| `宿主边界哨兵` | `pilotdeck-gateway/src/skills/error.rs:174` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/request_assembly.rs:96` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/request_assembly.rs:955` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/tool_context_policy.rs:125` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:148` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:154` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:161` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:168` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:176` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:228` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:243` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:254` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:1095` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:1179` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:1786` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:1875` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:1986` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:2064` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:2077` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:2085` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:2410` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:2556` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:2860` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:3270` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:3295` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:3442` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:3505` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:4269` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:4290` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:4598` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:4626` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:4765` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-agent/src/turn.rs:4920` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:764` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:766` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:787` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:792` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:925` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:927` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/default_factory.rs:941` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/server.rs:596` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/system_clock.rs:371` | 范围内的占位 | — |
| `原生档案缺口` | `pilotdeck-transport/src/system_clock.rs:432` | 范围内的占位 | — |
| `范围外不支持` | `pilotdeck-cli/src/config.rs:19` | 已定范围外 | staffdeck |
| `范围外不支持` | `pilotdeck-cli/src/gateway_setup.rs:266` | 已定范围外 | gateway setup 的联网三步 |
| `范围外不支持` | `pilotdeck-cli/src/gateway_setup.rs:295` | 已定范围外 | gateway setup 的联网三步 |
| `范围外不支持` | `pilotdeck-cli/src/gateway_setup.rs:354` | 已定范围外 | gateway setup 的联网三步 |
| `范围外不支持` | `pilotdeck-cli/src/main.rs:192` | 已定范围外 | pilotdeck tui |
| `范围外不支持` | `pilotdeck-gateway/src/module_host/runtime.rs:623` | 已定范围外 | 公共 host file_parse 的 PDF 抽文本 |
| `范围外不支持` | `pilotdeck-transport/src/bin/pilotdeck-agent-loop-sidecar.rs:29` | 已定范围外 | sidecar 的 PILOTDECK_AGENT_LOOP_FACTORY |

范围外清单里失效的条目（0）：无

### 第 6 条：透明代理的判据

判据（可机械检查，与第 5 条同样的文件范围）：读取透明代理的配置项（`var` / `var_os` 读 `PILOTDECK_PROXY_UPSTREAM`、`PILOTDECK_PROXY_LISTEN`、`PILOTDECK_GATEWAY_MODE`）、引用基线 gateway 入口脚本 `dist/src/cli/pilotdeck.js`、定义 `struct Proxy`。命中 0 处只说明旧入口不在，不说明运行时没有 Node 进程。

（命中 0 处）

## 按功能域

「问题」按台账的级别 1 / 2 / 3 / 4 / 尺子分列；台账里功能域填 `*` 的横跨全部，单列在表末，不计入各域。

| 功能域 | 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 问题 1/2/3/4/尺子 | 最近盲审 / 缺口审查 | 最近代码审阅 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `loop` 对话主干与 agent 循环 | 7 | 97 | 89 | 195 | 185 | 0 | 0 | 10 | 0/1/8/1/4 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `model` 模型协议与路由 | 9 | 57 | 57 | 271 | 270 | 0 | 0 | 1 | 0/0/3/0/0 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `tools` 工具 | 9 | 75 | 72 | 295 | 286 | 3 | 0 | 6 | 0/1/3/0/2 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `perm` 权限、沙箱、路径检查 | 6 | 65 | 65 | 55 | 53 | 0 | 0 | 2 | 0/0/1/0/0 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `session` 会话存储、分叉、恢复、导出 | 8 | 54 | 54 | 206 | 200 | 0 | 0 | 6 | 0/1/4/0/1 | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） |
| `context` 上下文、压缩与缓存 | 5 | 36 | 36 | 61 | 59 | 0 | 0 | 2 | 0/0/1/0/1 | 2026-11-13（缺口审查） | — |
| `agents` 子代理与后台任务 | 4 | 32 | 32 | 91 | 90 | 0 | 0 | 1 | 0/1/4/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `hooks` hooks | 5 | 34 | 34 | 99 | 99 | 0 | 0 | 0 | 0/0/0/2/3 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `ext` 插件、skills、MCP | 5 | 25 | 25 | 146 | 134 | 0 | 0 | 12 | 0/0/4/1/1 | 2026-11-11（盲审） | 2026-11-11（代码审阅） |
| `memory` 记忆 | 4 | 9 | 9 | 35 | 34 | 0 | 0 | 1 | 0/0/2/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `auto` cron 与 always-on | 5 | 24 | 24 | 123 | 123 | 0 | 0 | 0 | 0/0/2/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `config` 设置、配置、热重载 | 7 | 24 | 24 | 86 | 83 | 0 | 0 | 3 | 0/1/8/1/0 | 2026-11-12（缺口审查） | — |
| `modules` 外部能力模块与 sidecar 部署 | 7 | 45 | 44 | 117 | 105 | 0 | 0 | 12 | 0/0/4/1/2 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `gw` gateway 连接层 | 5 | 55 | 55 | 125 | 125 | 0 | 0 | 0 | 0/0/3/0/1 | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） |
| `channels` 渠道（只要求启动表现） | 4 | 13 | 13 | 125 | 83 | 0 | 0 | 42 | 0/0/0/0/1 | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） |
| `cli` CLI | 6 | 24 | 24 | 83 | 72 | 0 | 0 | 11 | 0/0/2/0/0 | 2026-11-12（缺口审查） | — |
| 横跨（`*`） | | | | | | | | | 0/0/2/1/4 | | |
| 归属不了 | | 66 | 64 | 0 | 0 | 0 | 0 | 0 | | | |

一个场景可以属于几个子能力，所以各域场景数之和大于场景总数；表面项每项只属于一个子能力。

台账里认不出的功能域：
- 「`hot-reload-entries` 在调试构建优化档位…」的功能域 cron
- 「`sidecar-raw-stdio-concurrent`…」的功能域 sidecar
- 「`sidecar-raw-stdio-concurrent`…」没有填功能域

## 按子能力

### `loop` 对话主干与 agent 循环

一轮对话从 submit_turn 受理到 turn_completed：循环的步进、终止与恢复，工具调用的调度，对客户端的事件流，运行记录。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `loop.submit` 轮次受理与每轮参数 | 8 | 8 | 34 | 34 | 0 | 0 | 0 | — | — | 2026-11-13（代码审阅） | `src/agent/turn/` `src/gateway/client/InProcessGateway.ts` |
| `loop.steer-abort` 插话与中止 | 58 | 51 | 30 | 27 | 0 | 0 | 3 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/agent/session/SteerMailbox.ts` `src/agent/loop/AgentLoop.ts` |
| `loop.continuation` 步进与终止 | 3 | 3 | 43 | 38 | 0 | 0 | 5 | parity | 2026-11-11（盲审） | — | `src/agent/loop/decideLoopContinuation.ts` `src/agent/policy/FailureGuard.ts` |
| `loop.recovery` 循环层的异常恢复 | 21 | 21 | 28 | 27 | 0 | 0 | 1 | — | 2026-11-13（缺口审查） | — | `src/agent/loop/AgentLoop.ts` `src/agent/loop/outputTokenRetry.ts` `src/agent/loop/LargeFileRepair.ts` |
| `loop.tool-dispatch` 工具调用的调度 | 4 | 3 | 6 | 6 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/tool/scheduler/` `src/agent/loop/collectToolCalls.ts` `src/agent/loop/ensureToolResultPairing.ts` `src/agent/modules/capability/hostToolPort.ts` |
| `loop.events` 事件流与轮次快照 | 4 | 4 | 28 | 27 | 0 | 0 | 1 | — | — | — | `src/gateway/client/GatewayAgentEventProjector.ts` `src/status/agentStatus.ts` |
| `loop.runs` 运行记录 | 2 | 2 | 26 | 26 | 0 | 0 | 0 | — | — | — | `src/gateway/run/` |

### `model` 模型协议与路由

发给模型 provider 的请求与流解析（四种协议），公共请求层，模型目录与会话模型选择，路由，用量与调用记录。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `model.openai-chat` OpenAI chat completions | 2 | 2 | 25 | 25 | 0 | 0 | 0 | — | — | — | `src/model/providers/openai/` |
| `model.anthropic` Anthropic messages | 3 | 3 | 25 | 25 | 0 | 0 | 0 | — | — | — | `src/model/providers/anthropic/` |
| `model.openai-responses` OpenAI Responses | 2 | 2 | 28 | 28 | 0 | 0 | 0 | — | — | — | `src/model/providers/openai-responses/` |
| `model.google` Google Gemini | 3 | 3 | 13 | 13 | 0 | 0 | 0 | — | — | 2026-11-13（代码审阅） | `src/model/providers/google/` |
| `model.transport` 公共请求层与错误 | 13 | 13 | 51 | 51 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/model/streaming/` `src/model/providerEndpoint.ts` `src/model/errors/` `src/model/policy/` |
| `model.catalog` 模型目录与会话模型 | 17 | 17 | 66 | 65 | 0 | 0 | 1 | — | 2026-11-13（缺口审查） | — | `src/model/config/` `src/model/catalog/` `src/model/thinking/` `src/gateway/dialog/modelCatalog.ts` |
| `model.router` 路由 | 14 | 14 | 50 | 50 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/router/` |
| `model.usage` 用量 | 2 | 2 | 7 | 7 | 0 | 0 | 0 | — | — | — | `src/model/response/normalizeUsage.ts` `src/gateway/client/InProcessGateway.ts` |
| `model.invocations` 模型调用记录 | 4 | 4 | 6 | 6 | 0 | 0 | 0 | — | — | — | `src/storage/invocationStorage.ts` |

### `tools` 工具

内置工具（文件、搜索、shell、execute_code、web、用户交互、计划与目标等）和工具运行时的公共部分。子代理与后台任务工具在 agents，权限与沙箱在 perm。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `tools.files` 文件工具 | 13 | 11 | 43 | 43 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/tool/builtin/readFile.ts` `src/tool/builtin/writeFile.ts` `src/tool/builtin/editFile.ts` `src/tool/builtin/sendAttachment.ts` |
| `tools.search` 搜索工具 | 4 | 4 | 24 | 24 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/tool/builtin/glob.ts` `src/tool/builtin/grep.ts` |
| `tools.shell` bash | 4 | 4 | 15 | 14 | 0 | 0 | 1 | — | — | 2026-11-12（代码审阅） | `src/tool/builtin/` `src/runtime/commandShell.ts` |
| `tools.execute-code` execute_code | 8 | 7 | 19 | 12 | 3 | 0 | 4 | — | — | 2026-11-11（代码审阅） | `src/tool/builtin/executeCode.ts` |
| `tools.web` web 工具 | 13 | 13 | 37 | 36 | 0 | 0 | 1 | — | 2026-11-11（盲审） | — | `src/tool/builtin/webSearch.ts` `src/tool/builtin/additionalWebSearch.ts` `src/tool/builtin/webFetch.ts` |
| `tools.interaction` 向用户提问与对话框 | 16 | 16 | 79 | 79 | 0 | 0 | 0 | — | — | 2026-11-11（代码审阅） | `src/tool/elicitation/` `src/tool/dialog/` `src/gateway/elicitation/` `src/gateway/user-dialog/` |
| `tools.state` 计划、待办与目标 | 9 | 9 | 48 | 48 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/plan-todo/` `src/goal/` `src/tool/builtin/` |
| `tools.misc` 其他内建工具 | 4 | 4 | 9 | 9 | 0 | 0 | 0 | — | 2026-11-11（盲审） | 2026-11-13（代码审阅） | `src/tool/builtin/getCurrentTime.ts` `src/tool/builtin/lsp.ts` `src/lsp/` |
| `tools.runtime` 工具运行时公共部分 | 5 | 5 | 21 | 21 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/tool/registry/` `src/tool/execution/` `src/tool/protocol/` |

### `perm` 权限、沙箱、路径检查

权限判定与询问、permissions.json、运行模式的工具约束、沙箱模式、路径检查、交互 profile 与交互绑定。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `perm.decision` 权限判定与询问 | 22 | 22 | 27 | 25 | 0 | 0 | 2 | — | — | 2026-11-13（代码审阅） | `src/permission/` `src/gateway/permission/` |
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
| `session.transcript` 会话记录格式 | 11 | 11 | 53 | 51 | 0 | 0 | 2 | — | 2026-11-13（缺口审查） | — | `src/session/transcript/` `src/session/events/` `src/session/persistence/` `src/session/title/` |
| `session.read` 读会话 | 5 | 5 | 21 | 21 | 0 | 0 | 0 | — | — | — | `src/session/history/` |
| `session.fork-replace` 分叉与末轮替换 | 10 | 10 | 17 | 17 | 0 | 0 | 0 | — | — | — | `src/session/fork/` `src/session/replacement/` |
| `session.export` 导出、恢复与归档 | 5 | 5 | 26 | 26 | 0 | 0 | 0 | — | — | — | `src/session/artifacts/` `src/gateway/storage/` |
| `session.files` 文件历史、回退与快照 | 5 | 5 | 33 | 33 | 0 | 0 | 0 | — | — | — | `src/session/filesystem/` `src/storage/workspaceSnapshot.ts` |
| `session.uploads` 上传与附件 | 5 | 5 | 15 | 15 | 0 | 0 | 0 | — | 2026-11-11（盲审） | — | `src/context/attachments/` `src/cli/GatewayUploadedAttachmentBundle.ts` |
| `session.recovery` 崩溃与重启后的恢复 | 10 | 10 | 13 | 13 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/session/resume/` `src/gateway/run/` |

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
| `agents.oneshot` 一次性子代理 | 14 | 14 | 32 | 31 | 0 | 0 | 1 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/agent/sub/OneShotSubagentPort.ts` `src/agent/sub/builtinSubagentTypes.ts` `src/agent/sub/SubAgentSession.ts` |
| `agents.background` 可续跑与后台子代理 | 8 | 8 | 7 | 7 | 0 | 0 | 0 | — | — | — | `src/agent/sub/BackgroundSubagentRuntime.ts` `src/agent/sub/SubagentContinuationManager.ts` |
| `agents.sdk-defs` SDK 自定义子代理 | 5 | 5 | 18 | 18 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/cli/SessionAgentConfigBundle.ts` `src/agent/sub/SubagentRuntimeComposition.ts` |
| `agents.tasks` 后台 shell 任务 | 6 | 6 | 34 | 34 | 0 | 0 | 0 | — | — | — | `src/task/` `src/tool/builtin/taskTools.ts` |

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
| `ext.plugins` 插件 | 4 | 4 | 30 | 19 | 0 | 0 | 11 | — | 2026-11-11（盲审） | — | `src/extension/plugins/` |
| `ext.skills` skills | 11 | 11 | 49 | 49 | 0 | 0 | 0 | — | — | — | `src/extension/skills/` |
| `ext.mcp` MCP | 11 | 11 | 37 | 36 | 0 | 0 | 1 | — | — | 2026-11-11（代码审阅） | `src/mcp/` |
| `ext.commands` 命令 | 3 | 3 | 27 | 27 | 0 | 0 | 0 | — | — | — | `src/extension/contributions/CommandContribution.ts` |
| `ext.reload` 扩展热更新 | 5 | 5 | 3 | 3 | 0 | 0 | 0 | — | — | — | `src/cli/ExtensionWatchManager.ts` `src/cli/GatewayRuntimeRefreshBundle.ts` |

### `memory` 记忆

memory 段、轮次中的捕获与检索、轮后维护、memory_* method。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `memory.config` 记忆配置与模型 | 4 | 4 | 18 | 17 | 0 | 0 | 1 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/context/memory/createEdgeClawMemoryProviderFromConfig.ts` `src/context/memory/edgeclaw-memory-core/` |
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
| `config.env-paths` 环境变量与目录 | 6 | 6 | 19 | 17 | 0 | 0 | 2 | — | 2026-11-12（缺口审查） | — | `src/pilot/paths.ts` `src/cli/proxy.ts` `src/pilot/writablePaths.ts` |
| `config.telemetry` 遥测 | 2 | 2 | 19 | 19 | 0 | 0 | 0 | — | — | — | `src/telemetry/` |

### `modules` 外部能力模块与 sidecar 部署

modules 段的外部绑定、外部工具与知识库、agent 循环 sidecar 的 stdio 与 TCP 部署、公共 host HTTP、SOP。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `modules.binding` modules 段与绑定 | 5 | 5 | 31 | 31 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/pilot/config/parseModulesConfig.ts` `src/composition/` |
| `modules.tools` 外部工具模块 | 13 | 12 | 3 | 3 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/composition/runtimePorts.ts` `src/composition/HttpModuleClient.ts` `src/agent/modules/capability/hostToolPort.ts` |
| `modules.knowledge` 知识库模块 | 2 | 2 | 5 | 5 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/composition/domainPorts.ts` |
| `modules.sidecar-stdio` sidecar stdio 部署 | 10 | 10 | 14 | 14 | 0 | 0 | 0 | client deployment schema parity stdio | 2026-11-13（缺口审查） | — | `src/agent/modules/transport/` `src/cli/AgentLoopDeploymentProfile.ts` `src/cli/pilotdeck-agent-loop-sidecar.ts` |
| `modules.sidecar-tcp` sidecar TCP 部署 | 13 | 13 | 9 | 9 | 0 | 0 | 0 | tcp | 2026-11-13（缺口审查） | 2026-11-13（代码审阅） | `src/agent/modules/transport/` `src/cli/pilotdeck-agent-loop-sidecar.ts` |
| `modules.host-http` 公共 host 与审批 HTTP | 4 | 4 | 6 | 6 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/composition/publicHostHttpTransport.ts` `src/composition/publicApprovalHttpTransport.ts` `src/composition/activeRuntimeModelPorts.ts` |
| `modules.sop` SOP | 4 | 4 | 49 | 37 | 0 | 0 | 12 | — | 2026-11-13（缺口审查） | — | `src/sop/` `src/composition/nativeSopAuthority.ts` |

### `gw` gateway 连接层

pilotdeck server 的启停、WS 握手与帧、HTTP 路由、连接的健壮性、项目路由与描述。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `gw.server-entry` 服务启停 | 12 | 12 | 23 | 23 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/cli/pilotdeckServer.ts` `src/cli/PilotDeckServerShutdownBundle.ts` `src/gateway/server/authToken.ts` |
| `gw.ws-protocol` WS 握手与帧 | 6 | 6 | 36 | 36 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/gateway/server/GatewayWsConnection.ts` `src/gateway/protocol/` |
| `gw.http` HTTP 路由 | 8 | 8 | 27 | 27 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/gateway/server/GatewayServer.ts` `src/gateway/server/staticAssets.ts` `src/adapters/web/` |
| `gw.connections` 连接健壮性 | 24 | 24 | 14 | 14 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | 2026-11-11（代码审阅） | `src/gateway/server/websocket.ts` `src/gateway/server/GatewayServer.ts` |
| `gw.projects` 项目路由与描述 | 7 | 7 | 25 | 25 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/cli/ProjectRuntimeRegistry.ts` `src/gateway/GatewaySessionCatalog.ts` |

### `channels` 渠道（只要求启动表现）

各渠道启用后的启动表现、渠道自带的 HTTP 入口、微信登录、渠道命令。按范围只要求与基线启动表现一致。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `channels.startup` 启动表现 | 6 | 6 | 107 | 67 | 0 | 0 | 40 | — | — | 2026-11-13（代码审阅） | `src/adapters/channel/loadEnabledChannels.ts` `src/cli/ChannelAdapterBundle.ts` |
| `channels.http-endpoints` 渠道 HTTP 入口 | 6 | 6 | 13 | 13 | 0 | 0 | 0 | — | — | — | `src/adapters/channel/` |
| `channels.weixin-login` 微信登录 | 1 | 1 | 2 | 2 | 0 | 0 | 0 | — | 2026-11-13（缺口审查） | — | `src/adapters/channel/weixin/` `src/cli/pilotdeck.ts` |
| `channels.commands` 渠道命令 | 0 | 0 | 3 | 1 | 0 | 0 | 2 | — | — | — | `src/adapters/channel/` |

### `cli` CLI

pilotdeck 的子命令（server 归 gw.server-entry，sidecar 可执行文件归 modules）。

| 子能力 | 场景 | 一致 | 表面项 | 被碰到 | 只被有差异的碰到 | 没碰到 | 不计 | 上游验收集 | 最近盲审 / 缺口审查 | 最近代码审阅 | 基线位置 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `cli.entry` 入口与帮助 | 5 | 5 | 1 | 1 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/pilotdeck.ts` |
| `cli.default-chat` 默认对话与 tui | 2 | 2 | 17 | 6 | 0 | 0 | 11 | — | 2026-11-12（缺口审查） | — | `src/adapters/channel/cli/` `src/adapters/channel/tui/` |
| `cli.sdk-commands` run / resume / sessions / settings | 9 | 9 | 27 | 27 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/sdkCli.ts` |
| `cli.cron` cron 子命令 | 1 | 1 | 17 | 17 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/pilotdeck.ts` |
| `cli.skills-chat` skills migrate 与 chat search | 4 | 4 | 19 | 19 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/extension/skills/migrateSkills.ts` `src/cli/commands/chatSearch.ts` |
| `cli.setup-update` gateway setup 与 update | 4 | 4 | 2 | 2 | 0 | 0 | 0 | — | 2026-11-12（缺口审查） | — | `src/cli/commands/gatewaySetup.ts` `src/runtime/updateCommand.ts` |

## 缺口清单

这一节是「接下来派什么活」的来源。

### 没被一致的场景碰到的表面项

「没碰到」是没有场景碰到；「只被有差异的碰到」是碰到它的场景都不是 SAME（括号里是那些场景）。

#### `tools` 工具（3）

- `tools.execute-code`
  - 只被有差异的碰到（`tool-execute-code`）：`env` `env:PYTHONDONTWRITEBYTECODE` — src/tool/builtin/executeCode.ts:821
  - 只被有差异的碰到（`tool-execute-code`）：`tool-params` `execute_code.description` — src/tool/builtin/executeCode.ts:202
  - 只被有差异的碰到（`tool-execute-code`）：`tool-params` `execute_code.timeout_seconds` — src/tool/builtin/executeCode.ts:206

### 没有任何场景的子能力

- `channels.commands` 渠道命令

### 没审过的功能域

没有任何盲审或缺口审查记录的（0）：无

没有任何代码审阅记录的（3）：`context`、`config`、`cli`

### 归属不了的场景

- `cfg-builtin-property-keys`（SAME）
- `cfg-proxy-outbound`（SAME）
- `cfg-proxy-routing`（SAME）
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
- `env-browser-use-launch-args`（SAME）
- `env-path-fake-executables`（SAME）
- `gw-crash-frame-length-overflow`（SAME）
- `hook-advisory-abort-after-turn`（SAME）
- `hot-reload-adapters`（SAME）
- `hot-reload-adapters-connecting`（SAME）
- `hot-reload-always-on`（SAME）
- `hot-reload-cron`（SAME）
- `hot-reload-edge`（SAME）
- `hot-reload-entries`（DIFF）
- `hot-reload-proxy`（SAME）
- `loop-task-budget`（SAME）
- `loop-task-budget-invalid`（SAME）
- `loop-task-budget-project`（SAME）
- `model-complete-retry-stderr`（SAME）
- `modules-skills-external`（SAME）
- `params-submit-malformed-refs`（SAME）
- `params-submit-turn-cwd-type-tools`（SAME）
- `sdk-config-plugins-errors`（SAME）
- `sdk-config-plugins-hooks`（SAME）
- `sdk-config-plugins-mcp`（SAME）
- `sdk-config-plugins-output-style`（SAME）
- `sdk-config-plugins-skills`（SAME）
- `sdk-config-skills-agents`（SAME）
- `sdk-config-unknown-keys`（SAME）
- `session-odd-entry-shapes`（SAME）
- `session-odd-shapes-turn`（DIFF）
- `session-two-writers-server-cli`（SAME）
- `session-two-writers-servers`（SAME）
- `settings-yaml-tag-directives`（SAME）
- `settings-yaml11-document`（SAME）
- `sidecar-raw-connections`（SAME）
- `sidecar-raw-execute-terminal`（SAME）
- `sidecar-raw-framing`（SAME）
- `sidecar-raw-half-line-eof`（SAME）
- `sidecar-raw-protocol`（SAME）
- `sidecar-raw-stdio-cancel-pending`（SAME）
- `sidecar-raw-stdio-concurrent`（SAME）
- `sidecar-raw-stdio-eof-pending`（SAME）
- `sidecar-raw-stdio-execute-terminal`（SAME）
- `sidecar-raw-stdio-factory-failure`（SAME）
- `sidecar-raw-stdio-framing`（SAME）
- `sidecar-raw-stdio-half-line-eof`（SAME）
- `sidecar-raw-stdio-model`（SAME）
- `sidecar-raw-stdio-model-metadata`（SAME）
- `sidecar-raw-stdio-timeout-cancel`（SAME）
- `sidecar-raw-stdio-validation`（SAME）
- `sidecar-raw-tcp-disconnect-pending`（SAME）
- `sidecar-raw-validation`（SAME）
- `tool-request-user-form-2020`（SAME）

## 归属统计

- 表面项 2113 个：按规则归属 2112，例外 1，归属不了 0。
- 场景 636 个：按规则归属 413，例外 157，归属不了 66。

## 与 `coverage.mjs --surface` 的对照

两者用同一个判定（`scripts/bbx/lib/surface.mjs`）。对同一产物目录运行 `node scripts/bbx/coverage.mjs --surface --out <产物目录>` 应打印与下表相同的数字（场景 622/636 SAME）。

| 维度 | 项数 | 被SAME场景碰到 | 只被有差异的场景碰到 | 没碰到 | 不计 |
|---|---|---|---|---|---|
| config | 275 | 275 | 0 | 0 | 0 |
| project-config | 12 | 12 | 0 | 0 | 0 |
| files | 84 | 72 | 0 | 0 | 12 |
| env | 192 | 130 | 1 | 0 | 61 |
| tools | 54 | 53 | 0 | 0 | 1 |
| tool-params | 260 | 254 | 2 | 0 | 4 |
| events | 88 | 81 | 0 | 0 | 7 |
| records | 82 | 79 | 0 | 0 | 3 |
| provider-protocols | 105 | 105 | 0 | 0 | 0 |
| cli | 69 | 69 | 0 | 0 | 0 |
| http | 23 | 23 | 0 | 0 | 0 |
| ws-methods | 101 | 101 | 0 | 0 | 0 |
| ws-params | 337 | 337 | 0 | 0 | 0 |
| loop-behaviors | 116 | 108 | 0 | 0 | 8 |
| hooks | 74 | 74 | 0 | 0 | 0 |
| channels | 22 | 22 | 0 | 0 | 0 |
| sdk-session-config | 75 | 75 | 0 | 0 | 0 |
| slash-commands | 37 | 25 | 0 | 0 | 12 |
| gw-behaviors | 75 | 75 | 0 | 0 | 0 |
| recovery-behaviors | 13 | 13 | 0 | 0 | 0 |
| module-behaviors | 19 | 18 | 0 | 0 | 1 |
| 合计 | 2113 | 2001 | 3 | 0 | 109 |
