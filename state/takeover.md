# 接续记录（`dev/takeover`）

新会话先读这份，再读 `CLAUDE.md` 和 `.42cog/intent.md`，然后看 `git log`。不要往 `state/board.md` 追加；`state/` 下其余文件是上一轮流水线的记录，只当线索。

最后更新：2026-11-11（剩余工作估算）

## 现在由 Rust 承担并有证据的入口

| 入口 | 证据 | 没覆盖的 |
|---|---|---|
| `pilotdeck server` 的启停：参数与环境变量优先级、启动输出、token 文件、停机退出码、`/health`、`/auth/local-token`、落盘 | `server-entry-no-config` / `start-stop` / `port-host` / `token-generate` / `token-sources` 5 个场景 SAME | 停机时有客户端连着；非 git 目录的 stderr；`telemetry.enabled: true` |

当前对拍结果（`node scripts/bbx/check.mjs`，2026-11-11，对拍工具加固并合并工具执行行为分支之后，57 个场景）：基线 vs Rust 6/57 SAME（6 个 `server-entry-*`，在加固后的规则下仍 SAME）。基线自比 55/57 SAME；不稳的是 `tool-subagent`（后台子代理与父轮请求到达先后不定）和 `tool-subagent-input`（场景本身坏了：基线在等一个没人答的权限询问，判 INVALID）。完整结果在 `_tmp/20261110-inventory/build/m1i-bbx.log`。

其余 51 个场景都 DIFF，共同原因是还缺 `subagent`、`send_message` 两个工具的声明（`native_process/tools.rs` 的 `BASELINE_TOOLS_NOT_PORTED`）：每次模型请求少 2 个工具，连带 `content-length`、token 估算各数值、`fileSize` 不同。`turn-text` 的 51 处全属这一类。

去掉这一类之后已知还剩的真实差异：

- JS/TS、HTML、CSV 文件写入后的语法诊断没移植（`tool-write-file-diagnostics`）。
- `web_fetch` 只验证了失败路径：基线总把 http 升级成 https，成功路径需要采集器提供本机 HTTPS 服务并让两侧信任证书。
- `task_output` / `task_wait` / `task_stop` 按真实任务 id 的成功路径没覆盖：需要假 provider 能引用上一步工具结果里的值。`background_tasks` / `background_task_stop` 仍忽略 `projectKey`。
- `survey-empty-params` 34 处、`cli-survey-offline` 17 处（M4 / M6）。

临时分类的做法：`node scripts/bbx/diff.mjs a b --max 100000`，去掉 `tools[name=`、token 数值字段和 `.raw.` 后按路径分组。只用来定下一步，不作验收。

## 验收手段

`scripts/bbx/`（说明见其 README）。同一场景先后喂给基线 `node dist/src/cli/pilotdeck.js` 和 Rust 二进制，比较 WS 帧、HTTP、CLI 输出、发给假 provider 的请求、落盘文件。

- 改完代码：`(cd src && cargo build -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli) && node scripts/bbx/check.mjs`
- 新场景先跑 `--self`（基线自比）确认稳定。
- 注入错误检查的记录：2026-11-10 把 `native_process/methods/tools.rs` 里 `background_task_stop` 的 `sessionKey is required.` 改一个字母，`survey-empty-params` 从 181 变 182 并指出该处，还原后回 181。注入点必须先确认在被调用路径上。

## 正在做

**M1 最小闭环**，分三块：

1. 入口：已提交（`pilotdeck server` 与基线同参数；Linux CI 全绿）。
2. 多项目宿主与落盘布局：已提交。规则集中在 `native_process/projects.rs`：运行时路由是纯词法 `resolve(projectKey ?? pilot home)`；对话准入要求项目已登记（`projects/<id>/.cwd`），pilot home 免登记；gateway 自己从不登记项目。注入错误检查：把缺省项目改成 cwd，`turn-text` 报出 `sessions[0].cwd` 和 4 条落盘路径差异。
3. 发给 provider 的请求与会话记录：权限询问（`permission_request` / `permission_decide`、`ask_user_question`、写工具文件历史）、工具 `agent`（一次性子代理运行时，`agent.subagents` 配置生效）、`execute_code` 都已合并。`turn-text` 到 SAME 只差 `subagent`、`send_message` 两个工具。`execute_code` 次任务的子代理（验证 `bash` / `write_file` / `edit_file` / `task_*` / `web_fetch` 的执行行为，修溢出文件位置）因额度中断，改动在它的 worktree 里，恢复后要先同步主线。
4. 三个分支合并后、M1 收尾前：Rust 风险审查（见「剩余工作估算」）。
5. 尺子复核（2026-11-11，Fable）：已看过 Opus 期间加的两类归一化。随机路径段的规则只认两个固定前缀，`tool-execute-code-truncate` 里两侧溢出路径的真实差异仍被报出。耗时原先保留小数位数，但基线 0.10 会印成 0.1，位数逐次不同，会让自比偶发不稳，已改为整体记成 `<dur>`；耗时的取整与打印格式 bbx 不比，`execute_code` 目前没有专门的单元测试盯这一点，待补。

并行做法：每个子代理一个 git worktree（`.claude/worktrees/`），自己的 `src/target`，在 worktree 里建 `resources/PilotDeck` 符号链接指向主检出的基线；bbx 靠独占锁排队。`negative_anchor_selfcheck` 在多个 worktree 并发时会卡住，让子代理跳过它，合并后在主检出单独跑（2026-11-11 在 `7bdfa0781` 上单独跑：通过，103 秒）。

M1 之后要回头处理的（入口那块发现的）：

- 停机时还有客户端连着：基线一直等到客户端断开再退出 0；Rust 等 5 秒后以 1001 关连接、退出 1。还没定怎么对齐，也没有场景覆盖。
- `validate_profile`：配了 MCP server、plugins、skills、hooks、modules、subagents 时 Rust 拒绝启动，基线没有这个限制。在做到对应功能的里程碑时去掉。
- 基线在非 git 目录启动会往 stderr 漏两行 `fatal: not a git repository`；入口场景用 `DOCKER_CONTAINER=1`、`COMMIT_HASH=bbx` 让基线跳过，等于没覆盖。
- Rust 没有遥测采集器，停机行打印的是全 0 常量；`telemetry.enabled: true` 时与基线不同。
- 基线 server 启动流程里有、Rust 还没有的：全局代理、配置热重载（接了多少未核实）、渠道只有飞书和微信、公共审批与公共 host 的 HTTP 路由、路由事件写 `router/events.jsonl`。
- 多项目改造留下的：sop、always-on、steer、interaction、background tasks、mcp 四控仍只走缺省项目运行时；`project_files_list` / `project_file_read` 缺 key 时回的是 Rust 自造文案；设置的配置存储锚点改成了 cwd，cwd 下有项目级配置时没验证；`--native` 旧入口启动时仍登记自己的项目（`pilotdeck server` 不写）。
- 冻结账本里有条目已不再描述真实差异（`A7.2-NATIVE-ARCHIVE-CWD-001(-R1)`、`A7-W5-NATIVE-SESSION-CONFIG-CAPABILITY-001-R8/R9/R11`），没动账本。`scripts/native-witness/*`、`scripts/gateway-parity/native_evidence.py` 还写旧的 `runs.json` 路径，已不可用。
- 带对话的场景基线自比偶发不稳（首轮 `session_metadata` 与 `compaction_started` 两条记录先后会变），遇到就重跑 `--self`。
- 第三块第一轮留下的：
  - 标题的 `session_metadata` 记录落点在基线里随时序变。Rust 用一个进程级「冷 / 热」标志模仿常见落点（`Recorder::park`）。只影响记录顺序。等尺子能表达「这条记录位置浮动」时改掉。
  - 新接线的工具（`bash`、`edit_file`、`edit_notebook`、`write_file`、`task_*`、`web_fetch`）没验证执行行为，M3 做。`task_*` 没接到 `background_tasks` / `background_task_stop`；`web_fetch` 没接二级模型。
  - 全量摘要压缩（`CompactionEngine`）没接，超阈值时 Rust 回 `skipped`。
  - `permissionMode` / `basePermissionMode` 只用默认值，`set_permission_mode` 之后是否一致没有场景。
  - 插件 Rust 每轮刷新，基线在会话创建时刷新。
  - undici 默认请求头只加在模型请求路径上，MCP HTTP、hooks、web fetch 等没加也没验证。
  - 会话记录里的 `node: v22.22.0` 是 Rust 里的常量。
- 第二轮（三个并行分支）留下的：
  - 工具批次落盘前最多等 2 秒让 `assistant_message` 先落盘（基线没有对应物，是 Rust 的同步手段）。
  - `reload_output_styles` 仍回 unavailable；browser-use 的会话级参数准备没移植（默认布局下不触发）；hooks、modules、subagents 配置仍被 `validate_profile` 拒绝，带 hooks 的插件也拒绝。
  - 内置插件目录缺省找可执行文件上一级或上两级的 `builtin-plugins`；Node 取 `PATH` 上第一个 `node`。发布布局定下来时要重看。
  - `export_session_transcript`、`restore_session_transcript`、`fork_session`、`replace_last_turn`、`finalize_last_turn_replacement` 没做。
  - 投影归约器对畸形记录比基线宽（基线抛错丢投影，Rust 多数情况当不匹配）。
  - 项目级（`<项目>/.pilotdeck/`）的插件、skills、`mcp.json` 没有场景；MCP 工具的权限询问路径没覆盖。
- `execute_code` 合并后（2026-11-11）的剩余差异：基线结果里的耗时（`duration_seconds`、`tool_call_log[].duration_ms`，也嵌在结果文本里）两次跑不同，4 个场景自比都不是 SAME，要先定尺子怎么处理；输出过长时溢出文件的路径不同（基线 `<ROOT>/tmp/pilotdeck-tool-output/<pid>/…`，Rust `<pilot home>/tool-results/…`），第二个溢出文件名也不同；RPC 回调写文件时 Rust 缺 `file_snapshot_recorded` 记录（文件历史，写工具共有的缺口）；`permissionMode` 和工具结果块键顺序归权限询问分支。
- sidecar 找不到时的报错文案是 Rust 自己的，基线没有对应物（基线默认进程内跑循环）。

## 计划

进度不手写，以两条命令的输出为准：`node scripts/bbx/check.mjs`（场景 SAME 数）和 `node scripts/bbx/coverage.mjs`（101 个 gateway method 各有没有正常路径的场景、是不是 SAME）。2026-11-11：场景 57/64 SAME；method 有场景且全 SAME 20，有差异 4，未覆盖 77。

下表只记工作包、依赖和现在谁在做。一个工作包「做完」= 它范围内的入口都有场景且 SAME（或差异已登记为已接受），四条命令通过，范围内的占位和代理路径已删。

| # | 工作包 | 范围 | 依赖 | 状态 |
|---|---|---|---|---|
| P1 | 对话主干 | 一轮对话、28 个工具、权限询问、会话读取、子代理、插件 | — | 主体完成。剩：JS/TS / HTML / CSV 写后语法诊断；`web_fetch` 成功路径；`task_*` 真实 id 路径 |
| P2 | 安全修复 | plan / ask 模式拦截、符号链接、`execute_code` 超时、token 接口 Host / Origin | P1 | 进行中（分支 `worktree-agent-a5b164055e58ead20`） |
| P3 | method：轮次控制与运行记录 | abort / steer / run_* / user_dialog_* / fork / replace / export / restore / rewind 等约 24 个 | P1 | 进行中（`worktree-agent-a77e59246b587d509`，槽位 1） |
| P4 | method：配置、模型、MCP、skills | model_* / settings / thinking / output style / skill_* / mcp_* / project_file* 等约 27 个 | P1 | 进行中（`worktree-agent-a05f5a750360a428d`，槽位 2） |
| P5 | method：存储与自动化 | upload_* / memory_* / snapshot_* / native_archive_* / manager_* / sop_* / cron_* / always_on_* 约 25 个 | P1 | 未开始 |
| P6 | gateway HTTP | 12 条路由（含公共审批、上传下载）、鉴权、SSE | P1；P5 的上传 | 未开始 |
| P7 | CLI | 25 条命令（普查里 13 条未实现）、进程内模式的落盘 | P1；连 gateway 的命令依赖 P3–P5 | 未开始 |
| P8 | 启动流程补齐 | 全局代理、配置热重载、渠道适配器、遥测、hooks / modules 配置、停机时有客户端连着 | P4 | 未开始 |
| P9 | 其余风险审查 | 并发与资源、超时 / 断连 / 重启恢复（做法同安全审查：每条发现附可复现证据，对照基线） | P2–P5 合并后 | 未开始 |
| P10 | 退役 | 删代理模式、未接线代码、旧测试装置；原版 `ui/server` 接 Rust gateway 实测；人参与的真实使用验收 | P1–P9 | 未开始 |
| — | 阶段 B（`ui/server`）、前端 | 推迟，P10 之后再定 | P10 | 推迟 |

并行规则：P3 / P4 / P5 / P6 / P7 互不依赖实现，可同时做；同时最多 3 个 Opus 子代理（额度宽裕时 4 个），各用一个 bbx 槽位（`BBX_SLOT`）；合并由主会话逐个做，合并后在主检出跑全量对拍和四条命令。给子代理的通用说明在会话的 scratchpad（`briefs/common.md`，仓库外；丢了就按 `CLAUDE.md` 和本文件重写）。

速度参考：一个 Opus 子代理一轮 1–1.5 小时、约 40–60 万 token，能做完一个上面这样大小的工作包的大部分。

## 原仓库的新提交与 PR（2026-11-11 查看）

原仓库 `main` 在分叉后又合并了 134 个提交，另有 34 个未合并 PR，几乎都是旧流程的守卫、注入错误检查、账本和评审轮次，验收依据是证明不了 native 实现的 `gateway-parity`。不合并、不派子代理跟踪。做到对应模块时只读查一下它在该模块产品代码上的改动（`git fetch origin` 后 `git log origin/main -- <路径>`），有用就读懂后自己实现、用 bbx 验证。可能有参考价值的 PR：#2862（`export/restore_session_transcript`）、#2902（hooks 注册接线）、#2965（飞书 / 微信并发），以及 `main` 上 Slack 渠道约 770 行改动。不合入的：#2957（比基线更严，改变对外行为）、#2786（+222 万行草稿）。

## 人已定

2026-11-10 人答复「都按你的建议，之后你来决定就行」：

1. 阶段 B（`ui/server`）推迟，不取消。先做完 A、C、D 并让 TS gateway 退役，再回来定 B。在那之前不在 B 上做实现。
2. Node 运行时的 `ExperimentalWarning` 两行 stderr 算已接受差异（`scripts/bbx/diff.mjs` 的 `ACCEPTED`）。
3. 此后这类取舍（已接受差异、阶段顺序、范围内的做法）由我决定，在里程碑汇报里说明决定了什么和依据。我对这句话的理解不包括：推送到原仓库、改接手仓库可见性、对外发布、删除远端资源、改 `intent.md` 的收敛方向，这些仍然先问。

## 做过的决定

| 日期 | 决定 | 依据 |
|---|---|---|
| 2026-11-10 | 新建 `scripts/bbx` 作为验收手段，不再用 `scripts/gateway-parity` 和 `scripts/ui-server-parity` 验收 | 前者比的是透明代理模式，后者的被测件是一次性代理，都证明不了 Rust native 实现的任何事 |
| 2026-11-10 | 保留 Rust native gateway，按对拍差异收敛，不整体重写 | 它能独立跑通一轮对话，69 个 method 的错误路径与基线逐字节一致；缺的是接线和端到端验证 |
| 2026-11-10 | bbx 比较器把带小数的毫秒时间戳归一化成 `<ms.frac>`（与整数的 `<ms>` 区分） | 基线 `createdAt` 用高精度时钟，自比不稳；区分两种记号后「一侧整数一侧小数」仍算差异 |
| 2026-11-10 | 删掉旧负控装置里 11 条针对「单项目入口闸门」的变异和 3 个对应单测 | 闸门本身与基线不符已删除；别的项目是否被正确服务改由 bbx 的 3 个 `project-*` 场景验证 |
| 2026-11-11 | 基线 bug 不照做：`rename_session` / `tag_session` 的记录接着会话记录链写（基线从 `sequence: 1`、`parentEntryId: null` 写起） | 实测基线改名后只要会话重新加载就报 `Session event sequence must increase strictly: received 1 after 1.`，会话永久开不了轮次，属于丢数据类 bug。登记在场景的 `accepted`（`rename-record-chain`），复现留在 `baseline-bug-rename-reload` |
| 2026-11-11 | bbx 比较器：带名字的数组按名字对齐；`requestFingerprint` 归一化；场景可登记带原因的 `accepted` | 少一个工具不再表现为后面全部错位（`turn-text` 544 → 55，差异没有消失）；指纹在请求体逐字节相同时基线自比也不同；已接受差异需要一个每次打印的登记处 |
| 2026-11-11 | Rust 新增环境变量 `PILOTDECK_BUNDLED_PLUGINS_DIR` | 基线的内置插件目录由安装位置决定，Rust 需要自己的打包布局；bbx 用它让 Rust 指向基线那份 |
| 2026-11-11 | bbx 比较器加两类归一化：耗时（`duration_ms` / `duration_seconds` 的数值，保留小数位数）；基线 `execute_code` 的 mkdtemp 随机后缀和溢出目录里的进程号（只认这两个固定前缀） | 基线自比也不稳；只抹数值或随机段，格式和路径其余部分照比。`tool-execute-code-traceback` 自比仍有约 50 处：随机目录名进了 traceback 文本，token 估算随之差几个；`tool-execute-code` 自比剩 `fileSize` 1 处。这两处还没处理 |
| 2026-11-11 | 会话 `.jsonl` 里 provider 标题记录的落点不比（人确认：基线多次运行之间本身不一致）。bbx 比较前把它的内容挪到同一轮开头的固定位置，只允许越过循环开头那几类记录；位置字段不动 | 基线自比的不稳只表现为这条记录与相邻记录换位，`sequence` / `entryId` 随位置不变。注入错误检查：标题内容改一个字报 1 处；落到 `model_request` 之后报 57 处。原先 3 个不稳的自比结果重算后 SAME |
| 2026-11-11 | 关掉 Rust 里模仿标题落点的补丁（`park_title_record: false`） | 上一条之后不再需要；关掉后 `turn-text`、`project-default`、`read-messages-paging`、`tool-todo-write` 过滤后剩余仍为 0。`Recorder::park` 及进程级冷热标志的代码待删 |
| 2026-11-10 | `scripts/parity`、`scripts/cli-parity` 暂留，用到时先做注入错误检查 | 静态看是真对拍，但我没跑过 |

## 停用的旧闸门

| 停了什么 | 它原本想防什么 | 现在由什么替代 |
|---|---|---|
| `.github/workflows/ci.yml`、`pr-gate.yml`（文件留在原处，没删没改；它们只在 PR、push 到 `main`、merge queue 时触发，接手仓库只推 `dev/takeover`，所以不跑） | 四条构建测试命令；另有约 30 步守卫：负控装置自检、CI 配置守卫、冻结账本只追加、评审结果块格式、入口总账记账一致性 | `.github/workflows/takeover.yml` 只跑四条命令。对基线的一致性由本机的 `scripts/bbx` 验收。冻结账本我不改，所以只追加检查暂时没有对象；要动账本时手工跑对应的 `check_frozen.py` |
| `tests/negative_anchor_selfcheck.rs` 的 `negative_fixture_anchors_and_checker_controls`（2026-11-11 标 `#[ignore]`，文件和那些 `.py` 还在） | 用文本匹配把产品语句钉在原位，防「测试假绿」 | `scripts/bbx` 对着基线验收。它让每次改产品代码都要同步 `.py`，并发时还会卡住。确认无影响后连同 `tests/fixtures/native-*/negative_controls.py` 一起删 |

旧 workflow 没删的原因：仓库里有 Python 守卫和至少一个 Rust 测试会读 `ci.yml` 的内容。等 M7 精简测试时一起处理。

## 已知的坑

- bbx 的 `ws` 步骤支持 `answer`：收集帧期间遇到匹配事件就按模板发请求（`{event.字段}` 引用触发事件），用于应答 `permission_request` / `elicitation_request`。说明在 `scripts/bbx/README.md`。
- 重新构建后第一次起 Rust 二进制明显变慢，3 秒超时的场景会假失败。重建后先跑一次 `node scripts/bbx/check.mjs turn-text` 预热再跑全量。
- 基线里第一层子代理的 `subagentDepth` 是 2（`OneShotSubagentPort.ts:155-159` 与 `SubagentRuntimeComposition.ts:59` 各加一次），所以 `maxDepth: 2` 时子代理拿不到 `agent` 工具，要 3 才能嵌套。与工具描述不符，但不属于崩溃或丢数据，Rust 照基线做（`tool-agent-nested`）。
- 子代理会话的写工具记到父会话的文件历史里，权限询问走父会话的应答器（基线子代理的工具注册表是父注册表的视图，`SubagentRuntimeComposition.ts:57-81`）。子代理里触发权限询问没有场景。
- `html_markdown::ten_mib_of_flat_inline_siblings_stays_fast` 是限时的性能测试，机器负载高时全量 nextest 里会超时失败，单独跑通过（约 56 秒）。

- Linux CI（`takeover.yml`）四条命令全绿。这台 macOS 上全量 nextest 有 3 个失败：`unsafe_boundary::compiler_enforces_gateway_boundary`（守卫只支持 Linux）、`channel::sms::tests::dual_stack_occupier_is_reported_by_the_fixed_host_bind_probe`、`mcp_https::replays_ts_mcp_https_over_local_tls`（后两个与 IPv6 有关，稳定复现，根因未查）。
- 全量 nextest 约 12 分钟，`src/target` 约 15 GB。改一行 gateway 后增量构建约 35 秒。
- 基线 CLI 的 `cron` 和默认对话路径会探测写死的 `127.0.0.1:18789`。bbx 在该端口有监听者时拒绝运行，不要绕过。
- bbx 独占固定目录 `/private/tmp/pdrs-bbx` 和端口 38411 / 38412，不能并行跑两个。
- `pilotdeck-gateway` 不带 `--native` 是透明代理（转发给 TS）。验证 Rust 行为时必须走 native。
- native gateway 每个会话拉起 `pilotdeck-agent-loop-sidecar` 子进程；基线默认是进程内。
- 原仓库 `daqi-dev/pilotdeck-rs` 只读。它的其他分支历史里有 18 条 gitleaks 命中（测试夹具里的 key 形状字符串），不在 `dev/takeover` 的历史上；只推 `dev/takeover`。
