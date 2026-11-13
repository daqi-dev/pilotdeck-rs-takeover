# 问题台账（`dev/takeover`）

分级见 `specs/delivery/README.md`。只记还开着的；修掉并在 cb8 上验证后删行（历史在 git 里）。「来源」里的「审阅」指 2026-11-13 的独立代码审阅（只读、没有运行，结论是线索，修之前先复现）。

「功能域」列填 `scripts/bbx/features.json` 里功能域的短标识（`domains[].id`），一个问题涉及几个域就用逗号分开，横跨全部的填 `*`；`scripts/bbx/status.mjs` 按这一列统计各域开着的问题（状态为 `—` 的占位行不计）。

最后更新：2026-11-14（合并 gw 缺口审查、cron 在线路径、尺子修复、占位盘点、中止语义审查之后；cb8 的 v18 验收结果还没取回）

## 1 安全与数据

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| （无开着的）半开连接占名额已在 `6299bcece` 修掉：对端 EOF 时立即归还名额、保留写端；场景 `robust-vanished-clients-free-slots`，cb8 上 `5b1aec8cd`、`6894f3dca` 两轮全量一致 | gw | — | — | — |

## 2 主路径

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| `turn-steer-terminal`：轮次进行中插话（steer）后，会话文件里插话记录与本轮记录的先后与基线不同（Rust 的循环在 sidecar 里，落盘经宿主一跳）。macOS 与 cb8 都稳定差 33 处 | loop, session | 场景 `turn-steer-terminal`；cb8 验收（`6894f3dca`） | 审阅 | 待派（与 sidecar 进程内化一起评估） |
| 子代理的 SubagentStart hook 被中止杀掉时，Rust 不写子代理自己的会话文件；基线照样写出 `turn_started` / descriptor / `accepted_input` / `turn_result`。`abort-hook-subagent-start`（稳定差 254 处）与 `abort-timeout-hook-subagent-start` 因此不一致 | agents, session | 场景同名 | 超时收尾与中止遗留的报告 | 在修（2026-11-14 派） |
| 超时后 pump 停下时，Rust 把在途的工具 future 一起取消；基线在途的子代理继续在后台跑完、SubagentStop 照常开始（`abort-timeout-tool-agent`，基线 20/20） | agents, loop | 场景同名 | 超时收尾与中止遗留的报告 | 在修（同上一个子代理） |
| 配置里的 `proxy.url` 对 Rust 的模型、hook 等请求不生效：这些请求只读环境变量里的代理（`ProxyConfig::from_settings(None)`），启动时配置里写的代理也没用上；基线启动时与热重载时都会安装全局代理并打 `[proxy] Global fetch proxy → URL (noProxy: …)` / `[proxy] Global fetch proxy removed` | model, config | 无场景 | 配置热重载的报告 | 待派（要一个进程级、可替换的代理状态；启动与热重载两条入口一起做） |
| 渠道的配置热重载没接：Rust 的 `NativeChannels::reload` 只管 feishu 与 weixin，没有订阅配置变化；基线重载后打 `[pilotdeck] Adapter hot-reload complete: …`（只有 `adapters.` 开头的子路径触发，整段新增不触发）。还没量 | channels, config | 基线 `ChannelAdapterBundle.ts:96` | 配置热重载的报告 | 待派（与「渠道配齐凭据时拒绝启动」的取舍一起，等人定之后做） |
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
| sidecar 收到「请求方取消」恰好落在 factory 前或准备阶段的毫秒级窗口时：Rust 给干净的中止结果，基线 TS sidecar 给缺结果的 `cancelled` 终止帧（只在显式外部部署下可见） | modules | `pilotdeck-transport/src/server.rs`、`default_factory.rs` | 任务预算与中止差异 | 记为已知 |
| 审阅第 3 级修复没做完的：子代理场景的用量正则没有加位置限定（只认字段名）；Google 协议分支的重试日志行没有对拍覆盖 | agents, model | `c6a53a052` 的报告 | 审阅 | 待派 |
| 没有场景的热重载边角：停机第一步停掉 cron 之后再来一次重载（Rust 选择跳过）；服务运行中把配置改成非法内容——基线这时半停机（记忆维护里 `loadPilotConfig` 抛的异常没人接，cron 与 always-on 被撤下、之后每轮报 `INVALID_SDK_SESSION_CONFIG`、客户端断开后以 1 退出，疑似基线 bug），Rust 的表现没量 | config, auto | 配置热重载的报告 | 配置热重载的报告 | 待派（先量 Rust；基线那条核实后登记） |
| 直接对 sidecar 说原始协议时的差异（基线自比 5/5 稳定，对 Rust 稳定差 28 处）：应答的 messageId 前缀不同（基线 `parse-` / `validation-` / `hello-` / `cancel-` / `status-` / `module-call-` 加随机串，Rust 是 `error-` / `response-<连接代>-<序号>`）；status 与 module_call 应答里 `requestId` 和 `ok` 的键序相反；客户端发半行后半关闭时基线抛未处理异常、不回，Rust 回 `INVALID_JSON`（疑似基线 bug） | modules | 尺子第二批的报告（场景 `sidecar-raw-protocol`） | 待派（前两处照基线改；第三处先核实再登记） |
| 中止矩阵里没有场景的格子：上下文准备中、续写 / 恢复间隙（没有可观察的触发帧）；web 工具（采集器没有慢速本机端点）；后台子代理；工具调用参数传到一半。检查点之间的其余段落没有逐段审查 | loop, tools, agents | 矩阵文件 | 中止语义审查（`specs/delivery/abort-matrix.md`） | 记为已知 |
| `sdkSessionConfig` 的三种合法写法被拒（G2）：`plugins`、基线会忽略的未知键、同时给 `skills` 列表和 `agents`；请求带基线没有的 `sdkConfig` 字段、畸形的 `uploadedAttachments` 引用、method 名含孤立代理项时回 Rust 自造的拒绝（G11） | config, loop, session | `sdk_config.rs:1365-1391` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| `modules.skills` / `modelProvider` / `context` 配了外部模块绑定时 server 拒绝启动（G4） | modules | 文案 `Native isolated profile does not yet support configured module bindings.` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| `gateway setup` 里只写本地 yaml、不联网的三步输出 `TODO`（G5） | cli | `gateway_setup.rs:269,307,361` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（联网的三步等人定范围） |
| 会话记录里少见的条目形状读取时报「not implemented (A7.5)」（G6，只核对了 Rust 侧，没逐个读基线的 reducer）；MCP 工具的 `outputSchema` 用 JS 专有正则时报错（G7）；`update_settings` 遇到少见 YAML 特性报错（G8） | session, ext, config | 见盘点文件 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| 循环与工厂里 42 处「原生档案缺口」：模型或宿主回来的值形状异常时整轮失败，文案是 Rust 自造的（G9）；配置里用 `constructor`、`toString` 这类 JS 内建属性名时 CLI 输出 `TODO config requiring JavaScript host support`（G10） | loop, context, config | `*_PROFILE` 错误码、`requires_host` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（先各实测几条基线的表现） |
| 盘点没有实测（只读源码）：技能文件 EACCES 等「没测量过的 errno」约 45 行里哪些真机上会撞到；三句「不可达防御」是否真的走不到；`file_history` 的 `backupStorage` 在 server 下是否注入 | * | 盘点文件「没查清的」 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| 没有场景、只有量测或源码依据的 gw 行为：50 MiB 会话文件、超过 60 秒的过期锁（采集器控制不了 mtime）、文件索引 10 万项上限、TCP 连接超时、握手超时与 HTTP 缺省超时 | gw, session | `surface.json` 的 `gw-behaviors`、`recovery` 里没有 probe 的项 | gw 缺口审查 | 记为已知 |

## 4 时序与外观

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| Rust sidecar 每帧单独写，Node 把同一轮里的几次写合并成一块；TS 宿主对一次读到几帧敏感，上游对拍工具里 14 个稳定场景因此记录先后不同（帧内容一致） | modules | `scripts/upstream-tests` 的 parity 组 | 上游 sidecar 测试 | 记为已知 |
| Rust 比基线慢造成的次序差异（都不是靠延时能修的，要加快 Rust）：`abort-timeout-hook-stop-failure` 第二次重试越过 1500 ms 的超时（基线 20/20 在超时之前，Rust 运行记录 18/20、帧 13/20）；`abort-turn-finishing` 里中止在标题解绑之前就到了，标题被取消、下一轮重生成（基线 20/20 首轮写入，Rust 20/20 相反）；`abort-model-stream` 里 `tool_call_delta` 是否在中止前落盘（基线 20/20，Rust 18/20）；被中止的会话整体慢约 2.6 秒（场景因此改成每个会话紧跟着读，避开 30 秒的保留流期限） | loop, session, * | 场景同名 | 超时收尾与中止遗留的报告 | 部分在修：固定延迟修复合并后 `abort-model-stream` 变成稳定不一致（Rust 稳定少落一条记录），`abort-turn-finishing` 查明是中止时报 `Sidecar terminal payload does not contain an AgentLoop result`、不是快慢问题——两项都交给了中止遗留的子代理；`abort-timeout-hook-stop-failure` 待重量 |
| Rust 自定的两个 25 毫秒窗口：`TIMEOUT_REGISTRY_WINDOW`（超时期限到了之后等 25 ms 才把超时状态追加进运行记录，模仿基线写会话记录的 I/O 延迟；基线 20/20 的三行靠它对上）与 `ADVISORY_KILL_SETTLE`（被杀 hook 的完成帧）。基线没有这两个常数，机器慢时可能不够 | loop, hooks | `runtime/turn/` | 超时收尾与中止遗留的报告 | 记为已知（盲审时重点看；找到可等待的事件就换掉） |
| `hook-cancelled`：被中止杀掉的 PreToolUse hook 来不来得及写第一行日志（`hooklog/pre-slow.jsonl`）两侧都不稳，但 Rust 写到的概率更高（基线 5/40，Rust 19/40）——疑似中止送到 hook 进程的链路比基线长 | hooks | cb8 上各 40 次 | Linux 遗留差异的报告 | 待派（先量中止到 SIGTERM 的延迟；基线自己不稳，届时声明最窄的 volatile） |
| HTTP 传输字节，hyper 1.7.0 内部决定、应用代码碰不到的三处：400 / 431 应答多带 `content-length: 0`（`role.rs:463-481`）；HTTP/1.0 请求回 `HTTP/1.0` 状态行（`conn.rs:681-688`）；请求头写一半就半关闭时基线回 400、Rust 不回。`gw-http-raw-requests` 因此一直不一致（DIFF 4）。可行的办法是在 hyper 前加一层预读自己写原始字节（`user_dialog/http_store.rs` 的 HeaderGuard 已这样处理 431），改动面不小 | gw | 场景 `gw-http-raw-requests` | gw 缺口审查 | 等人定（加预读层对齐，或经人同意列入允许不一致的清单） |
| 性能（macOS，20 轮中位数，假 provider）：修掉四处固定延迟后，一轮 `turn_completed` 基线约 45 ms、Rust 发布构建约 55 ms、调试构建约 248 ms（修前 81 / 298）。发布构建还多出来的：新会话 `get_or_create` 约 30–48 ms（其中插件 / 技能重新加载两次约 8–18 ms）、sidecar 内 execute 到 `model_request_started` 约 15 ms、每轮起一个 sidecar 子进程约 3 + 2 ms。冷启动：基线约 270 ms，Rust 发布约 650–770 ms。Linux 上没重量 | * | 固定延迟排查的报告；数据在 scratchpad 的 `latency/` | 性能报告时在 cb8 上用发布构建重量 |
| 空闲 CPU：Rust 约 2.7% 核，基线约 0.6%。来源是扩展目录监视每 50 ms 整棵遍历（约 168 项），另有配置文件每 20 ms 轮询；基线用 `fs.watch`（FSEvents / inotify）。去掉轮询要引入原生监视（如 `notify` crate），Linux 上「跟随链接一层」的语义要重新对齐 | ext, config | `runtime/extension_watch.rs` | 固定延迟排查的报告 | 记为已知（主会话定：先不换，性能报告出来后再评估） |
| 对拍用的调试构建一轮比发布构建慢约 4.5 倍（未优化代码的 CPU：tiktoken 计数、JSON 解析与序列化、UTF-16 转换），时间余量紧的场景在忙机器上因此偶发不一致 | * | 固定延迟排查的报告 | 待派（评估给 dev profile 设 `opt-level`：量构建时间与一轮耗时的变化，再定） |

## 尺子本身的问题

| 问题 | 功能域 | 来源 | 状态 |
|---|---|---|---|
| cb8 上并行跑全量时每轮 5–9 个场景偶发不一致，单独重跑一致（常见：`gap-tool-glob`、`gap-tool-grep`、`gap-permissions-json-rules`、`robust-large-message`、`loop-parallel-tools`、`loop-multi-tool`）。至少一部分的根因是上面的 `Keep-Alive` 差异 | * | cb8 验收 | 在查 |
| cb8 上单槽位重跑偶发不一致（同一轮里另一次重跑与全量都一致）：`hook-post-tool-use-interrupt` 一次差 318 处，`abort-hook-advisory-late-50` 一次差 2 处（后者靠 25 毫秒的收尾窗口）。没有两侧各量 20 次 | hooks, loop | cb8 验收（`2bfa0c7a7`） | 待派（在 cb8 上量；基线稳定而 Rust 不稳就是产品差异） |
| `[gateway:turn-timing]` 日志行两侧都是「耗时达到 200 毫秒才打」（基线 `InProcessGateway.ts:1629、1820`），所以它出不出现取决于机器快慢：`cli-entry-flags` 在 cb8 全量里 Rust 多打一行（`releaseMs` 218）。规则已定：比较器把这类行从 stdout 里摘出来不比（两侧各记条数供性能报告用） | cli, gw | cb8 验收（`809bc4cb8`） | 待派（下一批尺子修复） |
| 7 个 `abort-timeout-*` 场景声明了 volatile（超时事件与被中止循环的即时反应在运行记录里的先后，基线自己 19/1 或 11/9；数字写在各场景说明里）；`abort-turn-finishing` 的 close 列基线自己不稳（末尾标题记录 3/20 有）还没声明 | loop | `abort.gen.mjs` 的 `TIMEOUT_ORDER` | 超时收尾与中止遗留的报告 | 记为已知 |
| `abort-tool-bash-disconnect` 在 cb8 的 v20 全量里差 404 处（v19 里一致），没看原因 | loop, tools | cb8 验收（`809bc4cb8`） | 待派（与上面两个中止场景一起在 cb8 上量） |
| `turn-steer-terminal`：场景已改回在 `model_request_started` 触发插话，差异如实露出（macOS 与 cb8 都稳定差 33 处）——Rust 经 sidecar 一跳落盘，插话记录与这一轮记录的先后与基线不同。属产品差异，见第 2 级 | loop | 审阅 | 已转第 2 级 |
| 范围偏宽的放过：四个记忆场景放过整段 `state_json`；`hook-cancelled` 的 `accepted` 放过会话文件第 20–29 条和整份 `projections.json`；TCP 场景整值放过 `moduleInstanceId`（证明不了「新实例」）；子代理场景的用量正则只认字段名；`always-on-fire-git` 一次略过 256 处 | memory, hooks, modules, agents, auto | 审阅 | 在修（收窄） |
| `scripts/upstream-tests/run.mjs` 的缺省运行包含 parity 组，而它在 Rust 上按设计有 14 个不过，所以退出码不能反映「计入的全过」；也还没在 Linux 上跑过 | modules | 主会话 | 待派 |
| 基线停机偶发退出码 1（`gap-tool-grep` 自比 3 次里 2 次，疑似停机时触发未捕获异常的处理，`cli/pilotdeck.ts:266-274`）：频率和条件没量清，是否登记为基线 bug 未定 | gw | Linux 差异 | 待量（尺子第二批又见到 6 次：`gap-tool-glob`、`tool-agent-general`、`tool-execute-code`、`loop-multi-tool`） |
| 基线停机不等运行记录写完（`shutdownCoordinator.ts:13-14`）：锁文件的两种表现已由规则 18 处理；「在途运行记录停在 `running`」只在 `tool-goal-permission` 声明了最窄的 volatile（依据是早先 30 次里 3 次，最近 120 次没复现）；临时文件从没复现，没处理。Rust 一侧 120 次全是终态、无残留 | session, hooks | 尺子第二批的报告 | 记为已知（别的场景出现同样表现时照此声明） |
| cb8 是共用机器：同机别人跑同一套测试时，固定端口的测试（18973、18990、28999）会失败。`6894f3dca` 的全量 nextest 因此 7754/7755，单独重跑通过 | channels | cb8 验收（`6894f3dca`） | 记为已知（第 5 条的 cb8 那一次要挑没人跑的时候，或以接手仓库的 CI 为准） |
| Rust 调试构建的一轮比基线慢 4–8 倍（压缩一步约 0.5–0.8 秒），时间余量紧的场景在慢机器上会翻红 | * | Linux 差异 | 性能报告时看发布构建 |
| `surface.json` 标为「不计」的项（第一组 28 项，第二组又有若干：TUI 本地命令、QQ 前缀、做不到的错误码等），理由是子代理写的，抽查五项成立；整张表要人过目（交付标准第 2 条） | * | 审阅 | 待整理 |
| 约 35 个没有 `volatile` / `accepted` 的新场景，没有逐条核对说明与步骤是否对得上 | * | 审阅 | 待派 |
| `compact-auto` 的主会话从来不触发压缩（假 provider 报的 `prompt_tokens 12` 让校准后的估算值很低），场景说明与实际不符。要查还有多少场景「一致只是因为没测到它声称测的东西」 | context, * | 任务预算与中止差异 | 待派（场景说明与实际触发行为的核对） |

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
- `gw-http-raw-requests` 的三处 HTTP 传输字节差异（见第 4 级）：在 hyper 前加预读层去对齐，还是列入允许不一致的清单。
- 公开 agent 评测的两侧对比（人说之后再决定）。
