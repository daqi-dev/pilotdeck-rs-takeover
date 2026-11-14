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
| 四个次序竞争的场景（`turn-steer-terminal`、`abort-model-stream`、`abort-turn-finishing`、`abort-timeout-hook-stop-failure`）。2026-11-13 内置循环已改为进程内运行（`a39b31e00`），但逐帧计时显示宿主调用往返只有 0.1–0.3 ms，**子进程一跳不是原因**，此前「结构问题」的判断是错的；四个场景改动前后分布相同。实际原因是两侧各段的快慢不同：① `turn-steer-terminal`（稳定差 33 处）——基线的触发帧要等运行登记写完才发（拿锁、重读、整体替换，`InProcessGateway.ts:1216-1245`、`RunRegistry.ts:236-263`），这几毫秒里循环已落了 context_budget 与 model_request；Rust 登记写得快，插话在 agent_status_message 之前就到。基线的次序靠慢 I/O 垫出来，不是结构保证的；② `abort-model-stream`——只在调试构建上不一致（请求体构造慢，中止先到），发布构建 10/10 一致；③ `abort-turn-finishing`——assistant_message 到标题解绑之间 Rust 还有一次 budget 调用（约 2.4 ms），中止落进窗口；另外 close 列基线自比只有 6/19、13/19；④ `abort-timeout-hook-stop-failure`——偶发（调试 16/20），与进程无关。下一步：把这类场景改成两侧都确定的触发方式，并且两种先后各有一个场景覆盖（证明两侧在同样的先后下表现相同），不是只留一个放过差异的场景；调试构建的优化档位等 Linux 时序的报告 | loop, session | 进程内化的报告（原始数据在 scratchpad `in-process-loop/`） | 审阅 | 待派（等 Linux 时序的报告回来一起派） |
| `hot-reload-entries` 在调试构建优化档位调到 1 之后稳定差 76 处（macOS 5/5；二分：`7921fd6bd` 档位 0 时 3/3 一致，`9f379c712` 只加档位后 3/3 不一致，与渠道合并无关）。表现：手改配置（`cron.timezone`）后 Rust 的重载让正在跑的一次性 cron 任务那一轮以 `aborted_streaming` 结束，基线那一轮跑完（基线因监视 bug 根本没看到手改，登记在 `config-watch-survives-rewrite`）。以前调试构建慢，重载落在那一轮之外，差异被遮住；发布构建下应当一直如此。要查：基线在显式 `reload_config` 时对正在跑的 cron 轮次怎么处理，Rust 的重载该不该打断它——用一个两侧都会重载的确定场景来量。调高档位后别的场景也可能翻转，等 cb8 上新档位的第一轮全量（v26）出名单 | cron, config | 场景 `hot-reload-entries`；本机二分 | 主会话抽查 | 待派（等 v26 的名单一起） |
| `tool-send-message-close-discard`（新场景）在 cb8 上 v25 单独重跑两遍里一遍差 696 处、一遍一致；本机 3/3 一致 | agents | cb8 `logs/verify-v25.log` | cb8 验收 | 待查 |
| Rust sidecar 在 execute 以抛错结束时，`failed` 帧多带一个基线协议里没有的 `modelState` 字段（`804b8ae7c` 有意加的，用来让 Rust gateway 跨轮次带 token 校准；基线 `agentLoopSidecarServer.ts:358-372` 没有）。外部宿主会看到这个多出来的字段。2026-11-14 主会话定：独立 sidecar 的线上协议不带基线没有的字段；内置循环已在进程内，校准状态改走进程内的途径 | sidecar | 占位第三批的报告 | 占位第三批的报告 | 待派（随占位第四批） |

## 3 边角

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
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
| 代理的残余：`user_dialog` 的 http store 请求与渠道请求仍不走配置的代理（基线走；渠道不在范围）；记忆的模型请求与 token 计数走代理只读了代码、没实测；代理连接超时的错误码按推断设成 `UND_ERR_CONNECT_TIMEOUT`、没实测；旧版 SSE 传输的 MCP 请求头没对；HTTP hook 的连接空闲多久丢弃没照 undici | model, config, ext | `runtime/model/http/dispatcher.rs` | 代理配置的报告 | 待派 |
| 中止遗留里拿不准的两点：「等 fork 走到 SubagentStop 派发再撤事件出口」按派发的第一次 poll 算、不是看到 started 事件，原理上留着很小的竞态（20 次都一致）；无检查段的事件在中止途中照常投递，sidecar 部署下期限到点那一瞬间的行为没专门测。`abort-tool-agent` 偶发不一致（中止在子代理刚起步时到达）。server 停机时还没跑完的 fork 的处理没有场景 | agents, loop | 中止遗留第二轮的报告 | 记为已知（盲审时看） |
| sidecar 外部部署的残余：取消落在工厂准备阶段（plan_todo / get_metadata 挂起时）基线以「Sidecar module call aborted.」终止、Rust 交给循环在步首报 aborted，没有场景、没核实；多个并发 execute 的帧交错只对齐了场景里那一种模式；Rust 独有的三个拒绝码（`UNSUPPORTED_DEADLINE_PROFILE`、`INVALID_OWNER_CLOCK`、`EXECUTION_QUEUE_FULL`）保留着；终止后用同一 operationId、不同 runId 再 execute 时基线整个进程崩溃（Rust 回拒绝，没有场景）；新场景在 Rust 侧没有 20 次的分布 | modules | sidecar 原始协议与执行流两份报告 | sidecar 执行流的报告 | 记为已知（盲审时看） |
| 表单 schema（Draft 2020-12）基线接受、Rust 仍明确拒绝的六类：带 `$id`；`$dynamicRef` / `$dynamicAnchor` / `$recursiveRef`；`formatMaximum` 一族；`unevaluated*` 与 `anyOf` / `oneOf` / `if` / `dependentSchemas` / `patternProperties` / 递归 `$ref` 的组合（Ajv 的求值标记取决于生成代码的写法，不是规范语义）；本机正则引擎表达不了的 pattern；format 名与 `Object.prototype` 成员同名。常见用法已一致（121 个 schema、182 个答案对着基线核过） | tools | `form_dialog_2020.rs:710`；场景 `tool-request-user-form-2020` | 占位缺口修复的报告 | 记为已知（明确报错；盲审时看是否有常见写法落在这里） |
| `sdkSessionConfig` 的两处残余：`plugins` 指向真实目录时基线会为这个会话加载插件（影响 skills、MCP、hooks、output style、agents），Rust 仍回 `CAPABILITY_UNAVAILABLE`，工作量大；`skills` 列表与 `agents` 同时给时仍拒绝（基线给每个子代理单独过滤 skills）。未知键、`plugins` 的两种路径报错、三处自造拒绝已对齐 | config, ext | `native_process/sdk_config.rs` | 占位缺口修复的报告 | 待派（`skills` + `agents` 可做；`plugins` 的会话级加载先评估规模） |
| 配置里的 JS 内建属性名残余：选择串的模型段用内建名且 provider 存在时（`agent.model`、`tokenSaver.judge` / `tiers`、`fallback`、`stats.baselineModel`），基线能加载、到使用时才报错，Rust 仍在加载时输出 TODO 退出码 2；另有两个 `requires_host` 触发点没实测（多个配置源成环且带 `__proto__`；stat 遇到不常见的 IO 错误）。G6（少见的会话记录形状）、G8（少见 YAML 特性）没做 | config, session | `config/sections.rs:143,146`、`graph.rs:344`、`mod.rs:516` | 占位缺口修复的报告 | 待派 |
| 没有场景的热重载边角：停机第一步停掉 cron 之后再来一次重载（Rust 选择跳过）；服务运行中把配置改成非法内容——基线这时半停机（记忆维护里 `loadPilotConfig` 抛的异常没人接，cron 与 always-on 被撤下、之后每轮报 `INVALID_SDK_SESSION_CONFIG`、客户端断开后以 1 退出，疑似基线 bug），Rust 的表现没量 | config, auto | 配置热重载的报告 | 配置热重载的报告 | 待派（先量 Rust；基线那条核实后登记） |
| 中止矩阵里没有场景的格子：上下文准备中、续写 / 恢复间隙（没有可观察的触发帧）；web 工具（采集器没有慢速本机端点）；后台子代理；工具调用参数传到一半。检查点之间的其余段落没有逐段审查 | loop, tools, agents | 矩阵文件 | 中止语义审查（`specs/delivery/abort-matrix.md`） | 记为已知 |
| `modules.skills` / `modelProvider` / `context` 配了外部模块绑定时 server 拒绝启动（G4） | modules | 文案 `Native isolated profile does not yet support configured module bindings.` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| `gateway setup` 里只写本地 yaml、不联网的三步输出 `TODO`（G5） | cli | `gateway_setup.rs:269,307,361` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（联网的三步等人定范围） |
| 会话记录里少见的条目形状读取时报「not implemented (A7.5)」（G6，只核对了 Rust 侧，没逐个读基线的 reducer）；MCP 工具的 `outputSchema` 用 JS 专有正则时报错（G7）；`update_settings` 遇到少见 YAML 特性报错（G8） | session, ext, config | 见盘点文件 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| 循环与工厂里 42 处「原生档案缺口」：模型或宿主回来的值形状异常时整轮失败，文案是 Rust 自造的（G9） | loop, context, config | `*_PROFILE` 错误码、`requires_host` | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派（先各实测几条基线的表现） |
| 盘点没有实测（只读源码）：技能文件 EACCES 等「没测量过的 errno」约 45 行里哪些真机上会撞到；三句「不可达防御」是否真的走不到；`file_history` 的 `backupStorage` 在 server 下是否注入 | * | 盘点文件「没查清的」 | 占位盘点（`specs/delivery/placeholder-inventory.md`） | 待派 |
| 没有场景、只有量测或源码依据的 gw 行为：50 MiB 会话文件、超过 60 秒的过期锁（采集器控制不了 mtime）、文件索引 10 万项上限、TCP 连接超时、握手超时与 HTTP 缺省超时 | gw, session | `surface.json` 的 `gw-behaviors`、`recovery` 里没有 probe 的项 | gw 缺口审查 | 记为已知 |
| 进程内循环的两处残余：循环线程卡死时关管道后最多等 `close_timeout`，等不到就把线程留下，没有上限也没有计数；`--sidecar` / `--sidecar-arg` 对默认部署不再起作用（只用于显式 stdio 部署的缺省命令），是 Rust 侧内部选项，仓库里没有使用者 | loop | `src/pilotdeck-gateway/src/host/in_process.rs` | 进程内化的报告 | 记为已知；卡死线程计数待补 |
| 表面覆盖（2026-11-14 第三批合并后，按现有产物加新场景实跑估算，待 cb8 全量确认）：范围内「没碰到」应为 0（最后 6 项与 `web_fetch.error:tool_aborted` 已标不计，不计清单 109 项，逐项理由在 `scripts/bbx/surface.json` 的 `skip`，生成到 `state/out-of-scope.md`，其中「基线里到不了」「外部触发不了」两类要人过目）；「只被有差异的场景碰到」剩 `recovery:transcript.too-large`（已登记 `oversized-transcript-refused`，登记后场景一致）。没写场景的更长超时：请求体 300 秒、保活 5 秒 | * | `state/status.md` | 覆盖缺口三批的报告 | 等 cb8 全量确认 |
| 占位残余（2026-11-14 第三批合并后，判据命中 62 处范围内、10 处已定范围外）：① G4 的 `modules.modelProvider` 与 `modules.context` 外部 HTTP 模块没做，server 启动时仍拒绝（各估 1–2 天）；② G9 还剩 18 处：sidecar 原始协议下宿主回异常值的 14 处（单工具分派、只有边界的工具结果、工具结果投影、非数组的上下文应答、未知恢复动作、非字符串目标、非数字用量、PlanTodo 边界等；另探到没有 `name` 的工具调用 Rust 报 `Expected a UTF16 string` 而基线照常分派）、顶层 `media_reference`（要 sidecar 读文件）、环境类 3 处（闰秒时区、夏令时空档、期限解析）；③ **21 处改称「internal invariant violated」后不再命中判据，理由是外部造不出来（8 个 trait 缺省方法只有一个实现且全覆盖、宿主只按调用类型应答等），主会话还没逐处核对，留给独立代码审阅**；④ 表单 schema 六类、YAML 孤立代理项与 set / pairs 内部编辑没做；⑤ `session-odd-shapes-turn` 只剩 1 处（含 null 块的会话 Rust 在恢复时就失败，不写检查点）；⑥ 子代理 skills 字符串展开的实际效果只有单元测试；⑦ `placeholder-inventory.md` 是盘点当时的记录 | cli, config, session, ext, loop, sidecar | `specs/delivery/placeholder-inventory.md`、`scripts/bbx/placeholder-out-of-scope.json` | 占位残余三批的报告 | 第四批待派（②④⑤），① 待派，③ 待审 |
| `web_fetch` 的残余：router 自己抛错（如托管模型被拒）的映射只有单元测试；预批准域名的准则文案没有对拍；60 秒超时没测 | tools | web_fetch 的报告 | web_fetch 的报告 | 记为已知 |
| `tool-execute-code` 在 cb8 的 v27 全量里差 1 处：停机退出码基线 1、Rust 0；其余轮次一致，像偶发（基线停机时的退出码不稳？）| tools | cb8 `logs/verify-v27.log` | cb8 验收 | 待查 |

## 4 时序与外观

| 问题 | 功能域 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|---|
| Rust sidecar 每帧单独写，Node 把同一轮里的几次写合并成一块；TS 宿主对一次读到几帧敏感，上游对拍工具里 14 个稳定场景因此记录先后不同（帧内容一致） | modules | `scripts/upstream-tests` 的 parity 组 | 上游 sidecar 测试 | 记为已知 |
| Rust 自定的两个 25 毫秒窗口：`TIMEOUT_REGISTRY_WINDOW`（超时期限到了之后等 25 ms 才把超时状态追加进运行记录，模仿基线写会话记录的 I/O 延迟；基线 20/20 的三行靠它对上）与 `ADVISORY_KILL_SETTLE`（被杀 hook 的完成帧）。基线没有这两个常数，机器慢时可能不够 | loop, hooks | `runtime/turn/` | 超时收尾与中止遗留的报告 | 记为已知（盲审时重点看；找到可等待的事件就换掉） |
| `hook-cancelled`：被中止杀掉的 PreToolUse hook 来不来得及写第一行日志（`hooklog/pre-slow.jsonl`）两侧都不稳，但 Rust 写到的概率更高（基线 5/40，Rust 19/40）——疑似中止送到 hook 进程的链路比基线长 | hooks | cb8 上各 40 次 | Linux 遗留差异的报告 | 待派（先量中止到 SIGTERM 的延迟；基线自己不稳，届时声明最窄的 volatile） |
| 性能（macOS，发布构建，2026-11-14 重量，本机有别的构建在跑）：冷启动 Rust 中位数 25–28 ms、基线约 485–783 ms——此前记的「Rust 650–770 ms」量错了对象，那是 macOS 首次运行一个新链接的二进制时一次性的系统检查（约 1.1 s，同一个二进制拷到新目录首次 exec 也是如此，之后 35–38 ms），不是启动代码慢。新会话 `get_or_create` 12.4 → 5.6 ms（去掉重复的一次插件运行时刷新），一轮 `turn_completed` 58 → 45 ms；前 5 个会话各为内置插件起一个 MCP 子进程（约 21 ms，与基线语义相同）。启动剩下的大头是遥测的 git 探测（约 17 ms，基线同样要付）。Linux 上没重量 | * | 冷启动的报告；脚本在 `scripts/perf/`，数据在 scratchpad 的 `coldstart/` | 性能报告时在 cb8 上用发布构建重量；release 配置不加 strip（首次运行可从约 1.1 s 降到 0.7 s，但 panic 回溯会没有函数名，主会话定：不加） |
| 空闲 CPU（macOS，发布构建）：2026-11-14 把扩展目录监视改成沿用未变目录的列表后，约 2.7–2.9% 核降到 0.9–1.0% 核；基线这次量到 0.1–0.15%。剩下的主要是每 50 ms 逐个查文件状态，以及配置文件每 20 ms 的轮询（没动）。要再降得换成系统事件监视（如 `notify` crate），Linux 上「跟随链接一层」的语义要重新对齐 | ext, config | `runtime/extension_watch.rs` | 冷启动的报告 | 记为已知（性能报告里列出，届时再评估要不要换） |

## 尺子本身的问题

| 问题 | 功能域 | 来源 | 状态 |
|---|---|---|---|
| cb8 上并行跑全量时每轮 5–9 个场景偶发不一致，单独重跑一致（常见：`gap-tool-glob`、`gap-tool-grep`、`gap-permissions-json-rules`、`robust-large-message`、`loop-parallel-tools`、`loop-multi-tool`）。至少一部分的根因是上面的 `Keep-Alive` 差异 | * | cb8 验收 | 在查 |
| cb8 上单槽位重跑偶发不一致（同一轮里另一次重跑与全量都一致）：`hook-post-tool-use-interrupt` 一次差 318 处，`abort-hook-advisory-late-50` 一次差 2 处（后者靠 25 毫秒的收尾窗口）。没有两侧各量 20 次 | hooks, loop | cb8 验收（`2bfa0c7a7`） | 待派（在 cb8 上量；基线稳定而 Rust 不稳就是产品差异） |
| 7 个 `abort-timeout-*` 场景声明了 volatile（超时事件与被中止循环的即时反应在运行记录里的先后，基线自己 19/1 或 11/9；数字写在各场景说明里）；`abort-turn-finishing` 的 close 列基线自己不稳（末尾标题记录 3/20 有）还没声明 | loop | `abort.gen.mjs` 的 `TIMEOUT_ORDER` | 超时收尾与中止遗留的报告 | 记为已知 |
| `abort-tool-bash-disconnect` 在 cb8 的 v20 全量里差 404 处（v19 里一致），没看原因 | loop, tools | cb8 验收（`809bc4cb8`） | 待派（与上面两个中止场景一起在 cb8 上量） |
| `turn-steer-terminal`：场景已改回在 `model_request_started` 触发插话，差异如实露出（macOS 与 cb8 都稳定差 33 处）——原因已量出：基线的触发帧被运行登记的写入拖后几毫秒，不是 sidecar 一跳。见第 2 级那条 | loop | 审阅 | 已转第 2 级 |
| 范围偏宽的放过：四个记忆场景放过整段 `state_json`；`hook-cancelled` 的 `accepted` 放过会话文件第 20–29 条和整份 `projections.json`；TCP 场景整值放过 `moduleInstanceId`（证明不了「新实例」）；子代理场景的用量正则只认字段名；`always-on-fire-git` 一次略过 256 处 | memory, hooks, modules, agents, auto | 审阅 | 在修（收窄） |
| `scripts/upstream-tests/run.mjs` 的缺省运行包含 parity 组，而它在 Rust 上按设计有 14 个不过，所以退出码不能反映「计入的全过」；也还没在 Linux 上跑过 | modules | 主会话 | 待派 |
| 基线停机偶发退出码 1（`gap-tool-grep` 自比 3 次里 2 次，疑似停机时触发未捕获异常的处理，`cli/pilotdeck.ts:266-274`）：频率和条件没量清，是否登记为基线 bug 未定 | gw | Linux 差异 | 待量（尺子第二批又见到 6 次：`gap-tool-glob`、`tool-agent-general`、`tool-execute-code`、`loop-multi-tool`） |
| 基线停机不等运行记录写完（`shutdownCoordinator.ts:13-14`）：锁文件的两种表现已由规则 18 处理；「在途运行记录停在 `running`」只在 `tool-goal-permission` 声明了最窄的 volatile（依据是早先 30 次里 3 次，最近 120 次没复现）；临时文件从没复现，没处理。Rust 一侧 120 次全是终态、无残留 | session, hooks | 尺子第二批的报告 | 记为已知（别的场景出现同样表现时照此声明） |
| cb8 是共用机器：同机别人跑同一套测试时，固定端口的测试（18973、18990、28999）会失败。`6894f3dca` 的全量 nextest 因此 7754/7755，单独重跑通过 | channels | cb8 验收（`6894f3dca`） | 记为已知（第 5 条的 cb8 那一次要挑没人跑的时候，或以接手仓库的 CI 为准） |
| `surface.json` 标为「不计」的项（第一组 28 项，第二组又有若干：TUI 本地命令、QQ 前缀、做不到的错误码等），理由是子代理写的，抽查五项成立；整张表要人过目（交付标准第 2 条） | * | 审阅 | 待整理 |
| 约 35 个没有 `volatile` / `accepted` 的新场景，没有逐条核对说明与步骤是否对得上 | * | 审阅 | 待派 |
| `compact-auto` 的主会话从来不触发压缩（假 provider 报的 `prompt_tokens 12` 让校准后的估算值很低），场景说明与实际不符。要查还有多少场景「一致只是因为没测到它声称测的东西」 | context, * | 任务预算与中止差异 | 待派（场景说明与实际触发行为的核对） |
| `sidecar-raw-stdio-concurrent` 在 cb8 上偶发不一致：v24（`320552a46`）单独重跑两遍里第一遍差 24 处，第二遍和全量里一致；本机 6/6 一致。差异内容没留下（产物被后一遍覆盖） | sidecar | cb8 `logs/verify-v24.log` | cb8 验收 | 待查（下一轮多跑几遍并留下 diff） |
| 采集器没有「本机站点」能力：`web_fetch` 的 7 个场景靠 UserPromptSubmit hook 起一个 127.0.0.1 的 HTTPS 假站点，同一份站点脚本复制进了 7 个场景文件（生成脚本没入库）。应让 `run.mjs` 支持带 content-type 与重定向头的本机站点，再把这 7 个场景改过去；新场景还没归入 `features.json` | tools | `tool-web-fetch-*` 场景 | web_fetch 的报告 | 待派（清理批） |

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
