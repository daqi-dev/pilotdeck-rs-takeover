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
| 会话写入锁目录 `pilotdeck-ledger-locks-<uid>/` 建在共享临时目录（基线没有跨进程写入栅栏）：别的用户可抢先建同名目录让会话写入失败；`TMPDIR` 为空串时落到当前目录。这道栅栏是 Rust 独有的对外行为（拿不到锁时报 `Session operation ledger is locked.`），是否站得住取决于基线在两个写者下会不会损坏会话。两个转储场景因它各剩 1 处差异 | session | `storage/transcript.rs:460-607`；场景 `env-dump-request-tmpdir`、`proto-dump-request` | 任务预算与中止差异 | 在修（2026-11-13 派）。规则已定：先量基线（两个 server 共用 home；server 与离线 CLI 同写一个会话）。基线不损坏→删掉跨进程栅栏；基线损坏→保留并登记，锁目录优先 `XDG_RUNTIME_DIR`、目录准备不出来时退回无栅栏。不挪到 `PILOT_HOME`（会让每个场景的落盘树多一个目录） |
| 中止落在 hook 执行中时的差异（基线每次都稳定）：`UserPromptSubmit` 中止，Rust 少发 `turn_aborted` 状态、少落一条会话记录；`Stop` 中止，基线以 `completed` 结束、Rust 发 `turn_failed` 和 `error`；`SubagentStart` / `SubagentStop` 中止，Rust 缺 `subagent_started` / `subagent_completed`，且 `SubagentStop` 的完成帧次序不稳 | hooks, loop, agents | 临时场景在会话 scratchpad 的 `hook-order/sc/aw-*.json`，未入库 | hook 帧次序 | 在修（中止语义整块审查） |
| `skills-extension-watch-symlink` 在 Linux 上稳定差 2 处：基线比 Rust 多打「Extensions changed」日志行（子代理对 Linux 上 `fs.watch` 的推断不对） | ext | cb8 验收（`9a5fb723b`）两次单独重跑与全量 | cb8 验收 | 在修（Linux 遗留子代理） |
| `turn-steer-terminal`：轮次进行中插话（steer）后，会话文件里插话记录与本轮记录的先后与基线不同（Rust 的循环在 sidecar 里，落盘经宿主一跳）。macOS 与 cb8 都稳定差 33 处 | loop, session | 场景 `turn-steer-terminal`；cb8 验收（`6894f3dca`） | 审阅 | 待派（与 sidecar 进程内化一起评估） |
| WS 与 HTTP 共用 64 条连接上限（Rust 独有，基线 1000 条同时在线全部正常、RSS 只多约 8 MB）：100 条并发时 37 条被重置，期间 `/health` 与 `/api/web` 也连不上 | gw | `native_ws.rs`；场景 `gw-conn-many-clients`；旧记录 `A7.2-NATIVE-WS-AGGREGATE-001`（未批准） | gw 缺口审查 | 在修（2026-11-13 定：去掉上限；先量两侧在描述符耗尽时的表现，Rust 不能比基线差） |
| 配置热重载的子系统部分整块没有（G1）：服务运行中改了 `cron` / `alwaysOn` / `proxy` / `adapters` / `telemetry` 的配置（含界面改设置），基线重启对应子系统并打日志，Rust 静默不生效，要重启。`assembly/automation.rs` 的 `reload` 已写好但只有测试调用 | config, auto, channels | `native_process/assembly.rs:1998`；基线 `pilotdeck.ts:132-206`；无场景 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 在修（2026-11-13 派） |
| `request_user_form` 遇到带 Draft 2020-12 声明的表单 schema 时工具报 Rust 自造的「not supported by this gateway」（G3）；模型写 schema 时常带这一行 | tools | `form_dialog.rs:160-164` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（先实测基线对这类 schema 的处理） |

## 3 边角

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
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
| `sdkSessionConfig` 的三种合法写法被拒（G2）：`plugins`、基线会忽略的未知键、同时给 `skills` 列表和 `agents`；请求带基线没有的 `sdkConfig` 字段、畸形的 `uploadedAttachments` 引用、method 名含孤立代理项时回 Rust 自造的拒绝（G11） | config, loop, session | `sdk_config.rs:1365-1391` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| `modules.skills` / `modelProvider` / `context` 配了外部模块绑定时 server 拒绝启动（G4） | modules | 文案 `Native isolated profile does not yet support configured module bindings.` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| `gateway setup` 里只写本地 yaml、不联网的三步输出 `TODO`（G5） | cli | `gateway_setup.rs:269,307,361` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（联网的三步等人定范围） |
| 会话记录里少见的条目形状读取时报「not implemented (A7.5)」（G6，只核对了 Rust 侧，没逐个读基线的 reducer）；MCP 工具的 `outputSchema` 用 JS 专有正则时报错（G7）；`update_settings` 遇到少见 YAML 特性报错（G8） | session, ext, config | 见盘点文件 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| 循环与工厂里 42 处「原生档案缺口」：模型或宿主回来的值形状异常时整轮失败，文案是 Rust 自造的（G9）；配置里用 `constructor`、`toString` 这类 JS 内建属性名时 CLI 输出 `TODO config requiring JavaScript host support`（G10） | loop, context, config | `*_PROFILE` 错误码、`requires_host` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（先各实测几条基线的表现） |
| 盘点没有实测（只读源码）：技能文件 EACCES 等「没测量过的 errno」约 45 行里哪些真机上会撞到；三句「不可达防御」是否真的走不到；`file_history` 的 `backupStorage` 在 server 下是否注入 | * | 盘点文件「没查清的」 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| 慢读客户端：Rust 在输出队列满 64 帧后以 1009 关闭连接（只送达 687 帧），基线 20001 帧全部送达 | gw | `native_ws.rs` 的输出队列；只有量测 | gw 缺口审查 | 在修（2026-11-13 定：帧数上限改成每连接 64 MiB 的字节上限，作为有意偏离登记） |
| `runs/registry.json` 写坏时两侧都以 1 退出，但基线先打印监听地址与 telemetry 行、Rust 只打一行解析错误 | gw, session | 场景 `recovery-runs-registry-corrupt` | gw 缺口审查 | 在修（照基线对齐） |
| 经项目内的符号链接写到项目外：基线只做字面检查（`projectFiles.ts:238`，只读了源码没运行）；Rust 的表现没量。Rust 也能写出去的话是第 1 级 | gw | 无场景 | gw 缺口审查 | 在修（先实测两侧；Rust 拒绝解析后越界的写入并登记） |
| sidecar 原始协议（非 JSON 行、校验码、cancel / status 等）没有直接场景：应答的 messageId 带随机后缀，比较器还不归一化 | modules | `surface.json` 的 `modules` 两项没碰到 | gw 缺口审查 | 在修（有余力时；比较器按出现次序编号） |
| 没有场景、只有量测或源码依据的 gw 行为：50 MiB 会话文件、超过 60 秒的过期锁（采集器控制不了 mtime）、文件索引 10 万项上限、TCP 连接超时、握手超时与 HTTP 缺省超时 | gw, session | `surface.json` 的 `gw-behaviors`、`recovery` 里没有 probe 的项 | gw 缺口审查 | 记为已知 |

## 4 时序与外观

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| Rust sidecar 每帧单独写，Node 把同一轮里的几次写合并成一块；TS 宿主对一次读到几帧敏感，上游对拍工具里 14 个稳定场景因此记录先后不同（帧内容一致） | modules | `scripts/upstream-tests` 的 parity 组 | 上游 sidecar 测试 | 记为已知 |
| HTTP 传输字节（hyper 与 Node 的差别）：400 / 431 应答多带 `content-length: 0`；HTTP/1.0 请求回 `HTTP/1.0` 状态行；请求头写一半就半关闭时基线回 400、Rust 不回。头名大小写与 chunk 长度的十六进制大小写按语义等价由采集器归一化 | gw | 场景 `gw-http-raw-requests`、`gw-conn-large-frame` | gw 缺口审查 | 在修（尽量在 Rust 里对齐；hyper 做不到的留着差异问人） |
| 性能待查（调试构建）：`submit_turn` 约 300ms 对基线约 47ms；空闲 CPU 约 3% 对 0%；扩展监视在 3 万个文件的树下空闲 5.4% 对 1.9% | * | 审阅修复的测量脚本 | 审阅修复 | 性能报告时用发布构建重量 |

## 尺子本身的问题

| 问题 | 功能域 | 来源 | 状态 |
|---|---|---|---|
| cb8 上并行跑全量时每轮 5–9 个场景偶发不一致，单独重跑一致（常见：`gap-tool-glob`、`gap-tool-grep`、`gap-permissions-json-rules`、`robust-large-message`、`loop-parallel-tools`、`loop-multi-tool`）。至少一部分的根因是上面的 `Keep-Alive` 差异 | * | cb8 验收 | 在查 |
| `turn-steer-terminal`：场景已改回在 `model_request_started` 触发插话，差异如实露出（macOS 与 cb8 都稳定差 33 处）——Rust 经 sidecar 一跳落盘，插话记录与这一轮记录的先后与基线不同。属产品差异，见第 2 级 | loop | 审阅 | 已转第 2 级 |
| 范围偏宽的放过：四个记忆场景放过整段 `state_json`；`hook-cancelled` 的 `accepted` 放过会话文件第 20–29 条和整份 `projections.json`；TCP 场景整值放过 `moduleInstanceId`（证明不了「新实例」）；子代理场景的用量正则只认字段名；`always-on-fire-git` 一次略过 256 处 | memory, hooks, modules, agents, auto | 审阅 | 在修（收窄） |
| `scripts/upstream-tests/run.mjs` 的缺省运行包含 parity 组，而它在 Rust 上按设计有 14 个不过，所以退出码不能反映「计入的全过」；也还没在 Linux 上跑过 | modules | 主会话 | 待派 |
| 耗时的位数漏进字节数：帧里的 `fileSize` 已由比较器规则 17 处理（按「文件写到第几条记录」对齐）。同一毛病还在工具结果的 `resultBytes` 与发给 provider 的 `content-length` 上（`loop-multi-tool` 5 次里 2 次因此差 4 处）；另有一批只为 `fileSize` 声明的 `volatile` 现在多余（`gap-tool-glob`、`gap-tool-grep`、`gap-agent-sandbox-workspace-write`、`sdk-config-forward-subagent-text`、`tool-agent-child-stream`，以及约 15 个长正则里的 `fileSize` 一项） | * | Linux 差异；尺子修复的报告 | 待派（规则已定：`resultBytes` 与 `content-length` 各自核对「等于本侧原文的字节数」，成立就不比数值；多余的声明先做基线自比再删） |
| 基线停机偶发退出码 1（`gap-tool-grep` 自比 3 次里 2 次，疑似停机时触发未捕获异常的处理，`cli/pilotdeck.ts:266-274`）：频率和条件没量清，是否登记为基线 bug 未定 | gw | Linux 差异 | 待量 |
| 基线停机时运行记录有时停在 `running`（`tool-goal-permission` 基线连跑 30 次里 3 次：停机抢在终态写入之前），该场景因此偶发不一致 | session | 尺子修复的报告 | 待派（基线自己不稳，声明最窄的 volatile） |
| 三处远程输入让基线崩溃或挂死（请求目标 `//[`、不加掩码的帧、`/api/web/projects/%E0/…`；前两处不需要鉴权）：基线 bug，Rust 不照做，三个 `gw-crash-*` 场景待登记 accepted。单帧超过 100 MiB 时 Rust 以 1009 关闭、基线接受但内存随帧长平方增长（300 MiB 一帧 60 秒无应答）：Rust 的上限保留，待登记为有意偏离 | gw | `GatewayServer.ts:95`、`websocket.ts:133`、`httpRouter.ts:60` | gw 缺口审查 | 在修（登记最窄路径；登记后列入「需要人过目的决定」） |
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
- **渠道配齐凭据时 Rust 的 server 整个拒绝启动**（盘点 P1）：`qq`、`wecom`、`mattermost`、`signal`、`whatsapp`、`homeassistant` 这几个基线会真的去连外部服务的渠道，Rust 报「未迁移到 Rust，需继续使用 TS gateway」并退出，与「TS gateway 退役」矛盾。要定：照常启动并报一行该渠道不可用，还是继续拒绝启动（换文案）。
- 范围待定的六项（盘点 P2–P7，建议都归范围外并登记）：`pilotdeck tui`；`gateway setup` 的联网三步（飞书凭据校验、微信与企业微信扫码）；`update` 的 Git Web 更新；遥测上报（现在静默不上报）；公共 host `file_parse` 的 PDF 抽文本；sidecar 的 `PILOTDECK_AGENT_LOOP_FACTORY`（加载用户的 JS 模块）。
- 公开 agent 评测的两侧对比（人说之后再决定）。
