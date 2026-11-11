# 接续记录（`dev/takeover`）

新会话先读这份，再读 `CLAUDE.md` 和 `.42cog/intent.md`，然后看 `git log`。不要往 `state/board.md` 追加；`state/` 下其余文件是上一轮流水线的记录，只当线索。

最后更新：2026-11-11（剩余工作估算）

## 现在由 Rust 承担并有证据的入口

以两条命令的输出为准（2026-11-11 晚，提交 `22305e9bb`）：

- `node scripts/bbx/check.mjs`：135 个场景，全量一次跑出 123 个 SAME；另有 3 个（`tool-send-message`、`tool-agent-general`、`cron-tools-run`）单独重跑 SAME（前两个是高负载下的时序抖动）。
- `node scripts/bbx/coverage.mjs`：101 个 gateway method 里 95 个有正常路径的场景且全部 SAME，5 个有场景但有差异（`submit_turn`、`set_permission_mode`、`read_session_messages`、`memory_list`、`memory_wipe`——前三个是被下面那几个不一致的场景带上的），1 个没有场景（`prepare_weixin_login`，需要外部服务）。

按入口分：`pilotdeck server` 启停与握手；一轮对话与 28 个常规工具、计划模式两个工具、`request_user_*` 四个工具、`cron_*` 六个工具；权限询问；子代理（一次性与后台可续跑）；会话读取、分叉、替换最后一轮、导出与恢复、文件回退；插件、skills、MCP；设置读写与热重载；上传、快照、定时任务；HTTP 的通用行为、`/api/web/*`、静态托管、飞书 webhook；CLI 的帮助与用法、`run` / `resume`、`sessions`、`chat search`、`skills migrate`、`gateway setup` 的不联网部分。

仍不一致的场景（9 个）：

| 场景 | 原因 | 状态 |
|---|---|---|
| `cli-run-permission-mode`（91 处） | `submit_turn` 的本轮 `mode` 没用到权限决定和提示词上 | 进行中（提交选项工作包） |
| `llm-invocations-retry`（81 处） | 没发 `retry_progress` 事件、没写 `router/events.jsonl` | 进行中（同上） |
| `survey-empty-params`（2 处）、`cli-sessions-settings`（1 处，配置版本号） | 小差异；后者高负载下不确定 | 进行中 |
| `cli-entry-default`、`cli-survey-offline` | 不带子命令的默认对话没做（基线这条路径自身会挂住且每轮失败，怎么处理未定） | 未派 |
| `http-module-host`（11 处） | 公共 host 的 6 个操作没实现 | 未派 |
| `tool-write-file-diagnostics`（297 处） | JS/TS、HTML、CSV 写后语法诊断没移植 | 未派 |
| `memory-enabled`（2 处） | sqlite 文件只比了哈希，基线自比也不稳；需要 bbx 按表导出再比 | 未派 |

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

| # | 工作包 | 状态 |
|---|---|---|
| P1 | 对话主干、28 个工具、权限、会话读取、子代理、插件 | 完成（剩语法诊断，见 P13） |
| P2 | 安全修复（plan / ask 模式拦截、符号链接、`execute_code` 超时、local-token 的 Host / Origin） | 完成 |
| P3–P5 | gateway method 三个族（轮次控制与运行记录；配置、模型、MCP、skills；存储与自动化） | 完成 |
| P6 | gateway HTTP | 主体完成。剩公共 host 的 6 个操作（`list_general_skills`、`list_model_catalog`、`model_prepare`、`model_stream`、`file_parse`、`read_sop_authority`）与审批桥的 StaffDeck 外部认证 |
| P7 | CLI | 主体完成。剩默认对话、`cron` 探测成功后的路径（基线端口写死 18789，隔离环境比不了）、`gateway setup` 写回配置与联网登录、TUI |
| P8a | SDK 会话配置、用户对话、计划模式工具 | 完成 9 个字段；`sdkSessionConfig` 另 17 个字段未做（带它们的配置明确回不可用） |
| P8b | 定时任务轮次、LLM 调用记录 | 完成（google 协议与非流式路径的调用记录未做） |
| P8c | 会话编辑、导出、文件回退、按 id 的后台任务 | 完成 |
| P9 | 可靠性：sidecar 握手超时、会话记录顺序、配置版本号、sidecar 泄漏 | 进行中（`worktree-agent-acc1387970b6815a3`，槽位 4） |
| P10 | 每轮提交选项、模型路由事件、启动时的配置通知 | 进行中（`worktree-agent-a0b2fe4f56bb9ec37`，槽位 1） |
| P11 | hooks 执行器、`modules` 配置（现在带 hooks / modules 的配置被拒绝启动） | 进行中（`worktree-agent-a65c52e3be0d8a4a1`，槽位 2） |
| P12 | always-on 完整流程（基线约 6000 行：discovery / workspace / execute / report / apply） | 未开始 |
| P13 | 写后语法诊断（JS/TS、HTML、CSV）；`memory` 的 sqlite 比对；公共 host 6 个操作；`sdkSessionConfig` 其余字段 | 未开始 |
| P14 | 启动流程补齐：全局代理、渠道适配器（只接了飞书和微信）、遥测、停机时有客户端连着 | 未开始 |
| P15 | 其余风险审查（超时 / 断连 / 重启恢复；做法同安全审查） | P9 之后 |
| P16 | 退役：删代理模式、未接线代码、旧测试装置；原版 `ui/server` 接 Rust gateway 实测；人参与的真实使用验收 | 最后 |
| — | 阶段 B（`ui/server`）、前端 | 推迟，P16 之后再定 |

并行规则：P3 / P4 / P5 / P6 / P7 互不依赖实现，可同时做；同时最多 4 个 Opus 子代理（人 2026-11-11 同意；撞额度上限就降回 2–3 个）。4 个并行加主检出的全量测试时负载均值到过 27–35，会制造假失败（握手超时、限时测试超时、时序场景抖动），遇到先单独重跑，各用一个 bbx 槽位（`BBX_SLOT`）；合并由主会话逐个做，合并后在主检出跑全量对拍和四条命令。给子代理的通用说明在会话的 scratchpad（`briefs/common.md`，仓库外；丢了就按 `CLAUDE.md` 和本文件重写）。

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
| 2026-11-11 | 安全审查的处理：`execute_code` 的「只读」判定保持与基线一致（记为已知风险）；`execute_code` 超时回收孙进程、`/auth/local-token` 检查 Host / Origin 两处有意偏离基线 | 前者改了会明显改变日常体验；后两者是可被利用或会挂死的问题，修了不影响正常使用（前端经 `ui/server` 连 gateway，不走这个接口） |
| 2026-11-11 | 基线会崩溃的两种请求 Rust 不照做：飞书 webhook 收到 `null` 请求体、`/api/web` 路径里非法的百分号编码 | 基线进程直接退出（前者未鉴权即可触发）；Rust 正常回错误 |
| 2026-11-11 | 配置文件监视在被 `update_settings` 改写后继续有效（基线第一次原子替换后就失效） | 基线 bug：之后手工改配置不再热重载。登记在 `settings-update` 的 `accepted` |
| 2026-11-11 | `update_settings` 遇到少见的 YAML 特性（自定义标签指令、YAML 1.1 特殊类型）时报写入失败；新版规范（Draft 2020-12）的表单 schema 被拒绝 | Rust 没有对应的序列化 / 校验能力，选择明确报错而不是冒险写坏或假装支持。两处文案是 Rust 自己的 |
| 2026-11-11 | 基线第一层子代理的深度算成 2（配 `maxDepth: 2` 时子代理拿不到 `agent` 工具）照基线做；`managedPermissions.defaultMode: plan` 不生效照基线做 | 与基线文档不符但不属于崩溃或丢数据，改了会改变对外行为 |
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
