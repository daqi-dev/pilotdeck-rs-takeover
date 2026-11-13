# 问题台账（`dev/takeover`）

分级见 `specs/delivery/README.md`。只记还开着的；修掉并在 cb8 上验证后删行（历史在 git 里）。「来源」里的「审阅」指 2026-11-13 的独立代码审阅（只读、没有运行，结论是线索，修之前先复现）。

「功能域」列填 `scripts/bbx/features.json` 里功能域的短标识（`domains[].id`），一个问题涉及几个域就用逗号分开，横跨全部的填 `*`；`scripts/bbx/status.mjs` 按这一列统计各域开着的问题（状态为 `—` 的占位行不计）。

最后更新：2026-11-13（cb8 上 `6894f3dca` 的 v17 验收之后）

## 1 安全与数据

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| （无开着的）半开连接占名额已在 `6299bcece` 修掉：对端 EOF 时立即归还名额、保留写端；场景 `robust-vanished-clients-free-slots`，cb8 上 `5b1aec8cd`、`6894f3dca` 两轮全量一致 | gw | — | — | — |

## 2 主路径

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| Linux 上稳定不一致：`skills-session-snapshot-at-create`（Rust 的系统提示词多约 40 个 token）。其余 5 个已在 `275ce32dc` 修掉（对拍根目录改成等长的 `/tmp/private`、场景余量），待 cb8 验收确认 | ext | cb8 全量对拍 | cb8 验收 | 在修（Linux 遗留子代理，2026-11-13 派） |
| 用户中止时，基线让在途的工具自己结束、跑完 `PostToolUseFailure` hook（`isInterrupt: true`）再发 `turn_completed`；Rust 直接丢掉在途的工具调用，hook 不跑 | hooks, loop | 场景 `hook-post-tool-use-interrupt`；基线 `ToolRuntime.ts:418-423` | 表面缺口 | 在修（中止语义整块审查） |
| 会话写入锁目录 `pilotdeck-ledger-locks-<uid>/` 建在共享临时目录（基线没有跨进程写入栅栏）：别的用户可抢先建同名目录让会话写入失败；`TMPDIR` 为空串时落到当前目录。要挪到 `PILOT_HOME` 下。两个转储场景因它各剩 1 处差异 | session | `storage/transcript.rs:530-578`；场景 `env-dump-request-tmpdir`、`proto-dump-request` | 任务预算与中止差异 | 待派 |
| 中止落在 hook 执行中时的差异（基线每次都稳定）：`UserPromptSubmit` 中止，Rust 少发 `turn_aborted` 状态、少落一条会话记录；`Stop` 中止，基线以 `completed` 结束、Rust 发 `turn_failed` 和 `error`；`SubagentStart` / `SubagentStop` 中止，Rust 缺 `subagent_started` / `subagent_completed`，且 `SubagentStop` 的完成帧次序不稳 | hooks, loop, agents | 临时场景在会话 scratchpad 的 `hook-order/sc/aw-*.json`，未入库 | hook 帧次序 | 在修（中止语义整块审查） |
| `skills-extension-watch-symlink` 在 Linux 上稳定差 2 处：基线比 Rust 多打「Extensions changed」日志行（子代理对 Linux 上 `fs.watch` 的推断不对） | ext | cb8 验收（`9a5fb723b`）两次单独重跑与全量 | cb8 验收 | 在修（Linux 遗留子代理） |
| `turn-steer-terminal`：轮次进行中插话（steer）后，会话文件里插话记录与本轮记录的先后与基线不同（Rust 的循环在 sidecar 里，落盘经宿主一跳）。macOS 与 cb8 都稳定差 33 处 | loop, session | 场景 `turn-steer-terminal`；cb8 验收（`6894f3dca`） | 审阅 | 待派（与 sidecar 进程内化一起评估） |

## 3 边角

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| WS 连接上限 64 条（HTTP 与 WS 共用，满了直接关新连接，不回 503）是 Rust 独有的，基线没有上限；旧记录 `A7.2-NATIVE-WS-AGGREGATE-001` 标着「未批准」。主会话倾向去掉，先评估它与输入预算等内存保护的关系 | gw | `native_ws.rs` 的 `MAX_CONNECTIONS`（`4fb6c17d1` 引入） | 审阅修复 | 待派 |
| sidecar 被取消时不立即释放挂起的模块调用：基线立刻拒绝挂起的调用、以 `cancelled` 结束；Rust 再等宿主回应最多 5 秒，等不到以 `result_unknown` 结束（宿主会回应时两边一样） | modules | `scripts/upstream-tests` 的 stdio 组有 1 例因此全红；基线 `agentLoopSidecarServer.ts` | 上游 sidecar 测试 | 待派 |
| 非字符串 `workspaceCwd`：只在场景覆盖的两处模拟了基线的 TypeError，工具实际仍在项目根执行。场景没有工具调用所以看不到 | loop | `host/modules/lifecycle.rs:130-132`、`session/factory.rs:1309-1344`、`native_process/assembly.rs:835-846` | 审阅 | 文案已在 `c6a53a052` 对齐；「工具实际仍在项目根执行」这一半没有场景，待派 |
| 进程本地时区的建模只对场景里的 11 个 `TZ` 值、只在 macOS 上有依据：`GMT` 的单点特判；POSIX 规则串不建模；`/etc/localtime` 是普通文件或多级链接；绝对路径 `TZ`；cron 缓存键用 Unicode 小写 | auto, tools | `pilotdeck-transport/src/system_clock.rs:220-310`、`runtime/cron/schedule.rs:161` | 审阅 | 待派（在 cb8 上量 Node） |
| 客户端只发 FIN 后，这一轮在第几帧写失败而中止取决于时序（基线约十来帧，Rust 约三四帧） | gw | 无场景 | 断开后续跑 | 记为已知 |
| 显式 stdio 部署下 sidecar 在模块调用期间死掉；`module-stdio-v2` 绑定无场景；Rust sidecar 的 TCP 启动错误文案与基线不同；连接超时无场景 | modules | — | sidecar TCP | 待派 |
| 项目根目录的祖先建不出来时的报错文案：基线 `EACCES: permission denied, mkdir '/private'`，Rust 报 `open '<root>'` | gw | `api_web/files.rs` 的写入路径；无场景 | Linux 差异 | 待派 |
| 全局开启记忆时子代理 `memory: "disabled"` 不挂记忆、`seed_read_state` 带子代理字符串形式 MCP 引用：没有场景 | agents, memory | `native_process/sdk_config.rs`、`subagents.rs` | 表面缺口 | 待派 |
| 记忆维护的实际抽取请求没有场景 | memory | `runtime/memory/maintenance.rs` | 循环缺口 / 审阅 | 待派 |
| 表面清单里没做完的：`agent_execution_rejected`、`agent_turn_discarded`、`permission_failed`、`agent_transcript_error`（后者判断做不到） | loop, perm | `coverage.mjs --surface` | 表面缺口 | 待派 |
| 任务预算没做的：`projectRetentionMs` 到期清理无场景；组织策略的 `maxTaskBudgetUsd` 上限没接；预算与子代理、压缩摘要费用的关系没查 | loop, config | `native_process/task_budget.rs`；基线 `ProjectRuntimeRegistry.ts:1058-1072` | 任务预算 | 待派 |
| 请求转储在带 `extraBody` 或 Google 协议时，Rust 转储的是实际发出的请求体，与基线不同；无场景 | model | `runtime/model/http.rs` | 任务预算与中止差异 | 待派 |
| 被杀的 advisory hook 收到 SIGTERM 后若还活不到 1 秒，Rust 会把它的完成帧放上流，基线可能已结束这一轮；1 秒上限是 Rust 自定的，无场景，这个修复也没单独做注入错误检查 | hooks | `runtime/turn/coordinator` 的 `ADVISORY_KILL_SETTLE` | hook 帧次序 | 待派 |
| sidecar 收到「请求方取消」恰好落在 factory 前或准备阶段的毫秒级窗口时：Rust 给干净的中止结果，基线 TS sidecar 给缺结果的 `cancelled` 终止帧（只在显式外部部署下可见） | modules | `pilotdeck-transport/src/server.rs`、`default_factory.rs` | 任务预算与中止差异 | 记为已知 |
| 审阅第 3 级修复没做完的：子代理场景的用量正则没有加位置限定（只认字段名）；Google 协议分支的重试日志行没有对拍覆盖 | agents, model | `c6a53a052` 的报告 | 审阅 | 待派 |

## 4 时序与外观

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| Rust sidecar 每帧单独写，Node 把同一轮里的几次写合并成一块；TS 宿主对一次读到几帧敏感，上游对拍工具里 14 个稳定场景因此记录先后不同（帧内容一致） | modules | `scripts/upstream-tests` 的 parity 组 | 上游 sidecar 测试 | 记为已知 |
| 性能待查（调试构建）：`submit_turn` 约 300ms 对基线约 47ms；空闲 CPU 约 3% 对 0%；扩展监视在 3 万个文件的树下空闲 5.4% 对 1.9% | * | 审阅修复的测量脚本 | 审阅修复 | 性能报告时用发布构建重量 |

## 尺子本身的问题

| 问题 | 功能域 | 来源 | 状态 |
|---|---|---|---|
| cb8 上并行跑全量时每轮 5–9 个场景偶发不一致，单独重跑一致（常见：`gap-tool-glob`、`gap-tool-grep`、`gap-permissions-json-rules`、`robust-large-message`、`loop-parallel-tools`、`loop-multi-tool`）。至少一部分的根因是上面的 `Keep-Alive` 差异 | * | cb8 验收 | 在查 |
| `turn-steer-terminal`：场景已改回在 `model_request_started` 触发插话，差异如实露出（macOS 与 cb8 都稳定差 33 处）——Rust 经 sidecar 一跳落盘，插话记录与这一轮记录的先后与基线不同。属产品差异，见第 2 级 | loop | 审阅 | 已转第 2 级 |
| 范围偏宽的放过：四个记忆场景放过整段 `state_json`；`hook-cancelled` 的 `accepted` 放过会话文件第 20–29 条和整份 `projections.json`；TCP 场景整值放过 `moduleInstanceId`（证明不了「新实例」）；子代理场景的用量正则只认字段名；`always-on-fire-git` 一次略过 256 处 | memory, hooks, modules, agents, auto | 审阅 | 在修（收窄） |
| `scripts/upstream-tests/run.mjs` 的缺省运行包含 parity 组，而它在 Rust 上按设计有 14 个不过，所以退出码不能反映「计入的全过」；也还没在 Linux 上跑过 | modules | 主会话 | 待派 |
| 耗时（`durationMs`）的位数漏进会话文件大小：`fileSize`、`resultBytes`、`content-length` 随之差几个字节。`gap-tool-grep` / `gap-tool-glob` 已各自声明 `volatile`，`gap-permissions-json-rules` 基线自比稳定所以不能声明、在慢机器上稳定差 11 字节。要在比较器层面定一条通用规则 | * | Linux 差异 | 在修（尺子子代理，2026-11-13 派）。规则已定：`fileSize` 两侧之差不超过会话文件里最短一条记录的一半且不超过 64 字节时放过，超过照常报 |
| 基线停机偶发退出码 1（`gap-tool-grep` 自比 3 次里 2 次，疑似停机时触发未捕获异常的处理，`cli/pilotdeck.ts:266-274`）：频率和条件没量清，是否登记为基线 bug 未定 | gw | Linux 差异 | 待量 |
| 落盘树里 `runs/registry.json.lock` 基线时有时无（`tool-goal-permission`、`tool-send-attachment-outside`、`loop-multi-tool` 偶发只差这 1 处） | session | cb8 验收 | 在修（尺子子代理：先量基线自比 30 次，再定是否列为不比较；同时查 Rust 与基线进程共用同一 home 时的互操作） |
| 规则 15（sqlite 的 `*_json` 列先解析再比）让旧的 `volatile` 路径匹配不上：`slash-memory-capture-skip` 差 36 处（`caseId`、`traceId` 等标识）；别的场景可能也有匹配不到任何东西的死声明 | memory, * | cb8 验收（`6894f3dca`） | 在修（尺子子代理：标识按出现次序编号，列出死声明） |
| `turn-abort-before-request-sidecar-stdio`：外置 sidecar 下请求前中止，基线把运行记录永远留在 `running`（基线 bug，Rust 落终态），待登记为已接受差异 | loop, modules | 任务预算与中止差异 | 在修（尺子子代理：先核实再登记最窄路径） |
| cb8 是共用机器：同机别人跑同一套测试时，固定端口的测试（18973、18990、28999）会失败。`6894f3dca` 的全量 nextest 因此 7754/7755，单独重跑通过 | channels | cb8 验收（`6894f3dca`） | 记为已知（第 5 条的 cb8 那一次要挑没人跑的时候，或以接手仓库的 CI 为准） |
| Rust 调试构建的一轮比基线慢 4–8 倍（压缩一步约 0.5–0.8 秒），时间余量紧的场景在慢机器上会翻红 | * | Linux 差异 | 性能报告时看发布构建 |
| `surface.json` 标为「不计」的项（第一组 28 项，第二组又有若干：TUI 本地命令、QQ 前缀、做不到的错误码等），理由是子代理写的，抽查五项成立；整张表要人过目（交付标准第 2 条） | * | 审阅 | 待整理 |
| 约 35 个没有 `volatile` / `accepted` 的新场景，没有逐条核对说明与步骤是否对得上 | * | 审阅 | 待派 |
| `compact-auto` 的主会话从来不触发压缩（假 provider 报的 `prompt_tokens 12` 让校准后的估算值很低），场景说明与实际不符。要查还有多少场景「一致只是因为没测到它声称测的东西」 | context, * | 任务预算与中止差异 | 待派（场景说明与实际触发行为的核对） |
| cb8 上的遗留（macOS 上一致）：两个 `hook-*-server-stop` 单槽位重跑稳定差 1 处（6 次），四槽位全量里有时一致、有时差 178 处（基线 10 帧、Rust 6 帧）；`cli-run-resume` 连续四轮 INVALID（基线一侧第 16 步连接失败）；`hook-cancelled` 登记范围之外还剩 `hooklog/pre-slow.jsonl`（基线没有、Rust 有）；`modules-tools-result-unknown-sidecar-binding` 标题记录与另一条记录的先后偶发相反 | hooks, cli, modules | cb8 验收（`6894f3dca`） | 在修（Linux 遗留子代理：先各量 20 次分清是谁不稳） |

## 审阅读过、未发现问题的

外部工具模块不走宿主权限判定（与基线一样宽，含子代理会话）；`modules.agentLoop` 绑定；sidecar TCP 的监听地址与鉴权；`browser_use.rs` 的路径；权限询问与事件转发不跨会话；`read_file` 路径检查；hook 进程环境变量；`execute_code` 只读判定；Google 请求；记忆请求头；渠道放行条件。

审阅没来得及读的：`channel/tests.rs`、`runtime/memory/maintenance.rs`、`pilotdeck-agent/src/turn.rs` 与 `default_factory.rs` 的基线对照、`runtime/turn/timeline.rs`、`host/modules/context.rs`、`pilotdeck-cli/src/chat_search.rs`、`runtime/cron/migration.rs`、`skills/external.rs`、`tests/` 下的集成测试改动。

## 等人定的

- 冻结表里登记的旧对拍脚本（`scripts/gateway-parity`、`ui-server-parity`、`exp008`、`a5-19`）是否退役。
- BlueBubbles 渠道的生产实现（配了凭据会真的轮询，与基线一致）是否保留。
- StaffDeck 的审批接口与项目绑定（设了环境变量才生效）是否收缩。
- 请求转储（`PILOTDECK_DUMP_REQUEST`）：Rust 的文件名带进程号和序号、独占创建、权限 600，是上一轮登记的安全偏离（标着「AI 代批，待人复核」）；内容已与基线一致。场景 `env-dump-request-tmpdir`、`proto-dump-request` 现在不一致，是否登记由「任务预算与中止差异」子代理先用基线实测判断，再由主会话定。
- `cli-update-usage`：Rust 的安装要不要带网页更新脚本（发布构建阶段定）。
- 公开 agent 评测的两侧对比（人说之后再决定）。
