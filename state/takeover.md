# 接续记录（`dev/takeover`）

新会话先读这份，再读 `CLAUDE.md` 和 `.42cog/intent.md`，然后看 `git log`。不要往 `state/board.md` 追加；`state/` 下其余文件是上一轮流水线的记录，只当线索。

最后更新：2026-11-11（剩余工作估算）

## 现在由 Rust 承担并有证据的入口

| 入口 | 证据 | 没覆盖的 |
|---|---|---|
| `pilotdeck server` 的启停：参数与环境变量优先级、启动输出、token 文件、停机退出码、`/health`、`/auth/local-token`、落盘 | `server-entry-no-config` / `start-stop` / `port-host` / `token-generate` / `token-sources` 5 个场景 SAME | 停机时有客户端连着；非 git 目录的 stderr；`telemetry.enabled: true` |

当前对拍结果（`node scripts/bbx/check.mjs`，2026-11-11，三个并行分支合并后，30 个场景）：基线自比 29/30 SAME（`read-messages-paging` 撞标题记录竞态）；基线 vs Rust 6/30 SAME（6 个 `server-entry-*`）。

其余场景的差异绝大部分是同一个原因：5 个工具没有 Rust 实现（`agent`、`subagent`、`send_message`、`ask_user_question`、`execute_code`，清单在 `native_process/tools.rs` 的 `BASELINE_TOOLS_NOT_PORTED`），每次模型请求少 5 个工具声明，连带 `content-length`、`context_budget` 各数值、`fileSize` 不同。`turn-text` 的 55 处全部属于这一类。

去掉这一类之后还剩的（下一轮要做的）：

- 默认权限模式下的权限询问：基线发 `permission_request` 等用户答复，Rust 直接拒绝（`tool-goal-permission`、`tool-send-attachment-outside`）。bbx 采集器还不能在一步里应答请求。
- `model_request` 记录里工具结果块的键顺序（`isError`）；`permissionDenials` 的键顺序。
- `set_permission_mode` 之后会话记录里的 `metadata.permissionMode`。
- 读文件失败时的事件类型和错误码（`read-messages-tools`：Rust `file_not_found`，基线 `permission_denied`）。
- `plugins-user-level` 里会话记录有两条错位。
- `survey-empty-params` 34 处、`cli-survey-offline` 15 处（M4 / M6）。

已做完并有场景的：7 个工具（`todo_write`、`create_goal`、`get_goal`、`update_goal`、`read_skill`、`structured_output`、`send_attachment`，只在 `bypassPermissions` 下验证了成功路径）；工具轮的落盘记录；13 个投影与 `read_session_messages` / `read_subagent_messages`；内置插件加载、funasr 的 MCP 工具、用户级 skills / MCP server / 插件。

临时分类脚本的做法：`node scripts/bbx/diff.mjs a b --max 100000`，去掉 `tools[name=` 和 token 数值字段后按路径分组。只用来定下一步，不作验收。

## 验收手段

`scripts/bbx/`（说明见其 README）。同一场景先后喂给基线 `node dist/src/cli/pilotdeck.js` 和 Rust 二进制，比较 WS 帧、HTTP、CLI 输出、发给假 provider 的请求、落盘文件。

- 改完代码：`(cd src && cargo build -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli) && node scripts/bbx/check.mjs`
- 新场景先跑 `--self`（基线自比）确认稳定。
- 负控记录：2026-11-10 把 `native_process/methods/tools.rs` 里 `background_task_stop` 的 `sessionKey is required.` 改一个字母，`survey-empty-params` 从 181 变 182 并指出该处，还原后回 181。注入点必须先确认在被调用路径上。

## 正在做

**M1 最小闭环**，分三块：

1. 入口：已提交（`pilotdeck server` 与基线同参数；Linux CI 全绿）。
2. 多项目宿主与落盘布局：已提交。规则集中在 `native_process/projects.rs`：运行时路由是纯词法 `resolve(projectKey ?? pilot home)`；对话准入要求项目已登记（`projects/<id>/.cwd`），pilot home 免登记；gateway 自己从不登记项目。负控：把缺省项目改成 cwd，`turn-text` 报出 `sessions[0].cwd` 和 4 条落盘路径差异。
3. 发给 provider 的请求与会话记录：两轮已提交。第三批并行三件：宿主层权限询问 + 零星差异 + `ask_user_question`（7 个提交，未合并）；子代理运行时（`agent`、`subagent`、`send_message`，未提交）；`execute_code`（主任务已合并，见下；次任务刚开始）。三件都因额度中断过，改动在各自 worktree 里。
4. 三个分支合并后、M1 收尾前：Rust 风险审查（见「剩余工作估算」）。
5. 复核尺子：2026-11-11 Opus 5.5 主持期间给 `diff.mjs` 加了耗时归一化和 `execute_code` 随机路径段归一化。下一次由 Fable 主持时先独立复核：在 Rust 侧故意改坏耗时的小数位数和溢出路径前缀，确认对拍仍报出，再还原。

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

## 剩余工作估算（2026-11-11）

单位：**轮** = 一个子代理做一个连贯模块（约 1 小时实跑，加我复核合并）；**窗口** = 一个 5 小时额度窗口。实测速度：11-10 到 11-11 共 8 轮（入口、多项目、请求与记录、三个并行 ×2 批里已回报的 4 个半），三次撞额度（两次 5 小时上限、一次 Fable 周额度）；一个窗口大约撑 3 个并行子代理各 1 轮，即 **约 3 轮 / 窗口**。下表的轮数含复核返工，不含人的真实使用验收。

| 范围 | 已完成 | 剩余 | 主要风险 | 估计 | 把握 |
|---|---|---|---|---|---|
| M1+M3 一轮对话与工具回路（进行中） | 入口 6 场景 SAME；多项目；请求头、能力列表、会话记录、投影、读模型；内置插件与 MCP；24/28 工具（含 `execute_code`） | 子代理运行时 3 个工具；权限询问分支收尾合并；文件历史快照记录（`file_snapshot_recorded`）；工具输出溢出文件路径；`bash` / 写改文件 / `web_fetch` / `task_*` 执行行为；全量摘要压缩；`scripts/parity` 53 场景重跑与负控 | 子代理运行时规模未知；基线耗时字段让工具场景自比不稳，需要先定尺子怎么处理 | 8–12 轮，3–4 窗口 | 中（有实测速度，子代理运行时是推测） |
| Rust 风险审查（人 2026-11-11 同意） | — | 只审接手以来新写和新接线的代码（`git diff 9c0c5f9b8..HEAD`）：安全（`execute_code` 子进程与沙箱、权限询问放行、路径检查）、并发与资源（每会话 sidecar 泄漏、落盘前等 2 秒、冷 / 热标志、会话锁）、超时 / 断连 / 重启恢复。审查者只拿代码和基线对应位置，不看实现动机。放在本轮三个分支合并之后、M1 收尾之前 | 不用旧流水线的 `pdrs-review`（人 2026-11-11 明确：旧流程工具是噪声来源）。做法：每类风险一个普通子代理，每条发现必须附可复现证据（失败的测试、复现命令或对拍场景），只有观点没有复现的不进修复清单；由我逐条复现后再修 | 审 1–2 轮 + 修 1–2 轮，约 1 窗口 | 中 |
| M2 会话域 | 列出、读、改名、打标、删除、关闭、读模型 | `export_session_transcript`、`restore_session_transcript`、`fork_session`、`replace_last_turn`、`finalize_last_turn_replacement`；多轮续接与重启恢复场景 | 归档格式要和基线字节一致 | 2–3 轮，约 1 窗口 | 中 |
| M4 其余 method 族 | 空参数普查 101 个 method 里 82 个一致 | 空参数仍不一致的 19 个（`submit_turn`、`run_get`、`run_reattach`、`sop_*`、`record_agent_status_message`、`project_files_list` / `project_file_read`、`apply_flag_settings`、`update_settings`、`reload_output_styles`、`always_on_*` 三个及 M2 那 5 个）；其余族的正常路径只验证过空参数：设置与热重载、模型目录、MCP 四控、skills、cron、上传、快照、memory、后台任务、steer / interaction；server 启动流程缺口（全局代理、渠道、公共审批路由、`router/events.jsonl`、遥测、停机时有客户端连着） | 各族正常路径没有场景，实际差异量未知 | 8–14 轮，3–5 窗口 | 低（推测） |
| M5 gateway HTTP | `/health`、`/auth/local-token` 已对拍 | 其余 10 条路由（`/api/module-host/*` 4 条现在 404）、静态资源、鉴权 | — | 2–3 轮，约 1 窗口 | 低 |
| M6 CLI | 离线普查 25 条里 20 条一致 | 无参数默认对话、未知命令、`cron` 三条；`run` / `resume` / `sessions` / `settings` 对着 Rust server 的正常路径；`tui`（PTY）、`gateway setup`、`update`（mock） | `tui` 需要 PTY 采集，bbx 没有 | 4–6 轮，约 2 窗口 | 低 |
| M7 退役 | — | 删代理模式与上游转发、删未接线代码、精简测试与 CI（旧 workflow 被 Python 守卫和 Rust 测试读取）、原版 `ui/server` 接 Rust gateway 的真实使用验收 | gateway 约 60 万行，删除面大；真实使用验收要人参与 | 3–5 轮，1–2 窗口，另加人的验收 | 低 |
| 阶段 B `ui/server`（推迟） | — | 基线约 3.6 万行，245 处路由，WS 三类，SSE | 依赖 M7；规模大 | 15–30 轮，5–10 窗口 | 很低 |
| 前端（Leptos，按计划 004） | `ui-rs/` 只有探针 | 全部业务页面与交互迁移、桌面壳 smoke、真人会话 | 未盘点；依赖 B | 未估（需要单独盘点） | — |

合计（不含 B 和前端）：约 28–45 轮，**10–15 个窗口**。按每天 2–3 个窗口算约 4–7 天，前提是额度不再被周上限截断。最大的不确定是 M4：现在只有空参数普查，各族正常路径差多少要等写了场景才知道。

## 之后

M2 会话域 → M3 工具回路（同时重跑 `scripts/parity` 的 53 场景并做负控）→ M4 其余 method 族 → M5 gateway HTTP → M6 CLI → M7 退役（删代理模式、删未接线代码、原版 `ui/server` 接 Rust gateway 实测）。完整计划和盘点见 `_tmp/20261110-inventory/REPORT.md`（不入库；丢了就按本文件重做盘点的相应部分）。

## 原仓库的新提交与 PR（2026-11-11 查看）

原仓库 `main` 在分叉后又合并了 134 个提交，另有 34 个未合并 PR，几乎都是旧流程的守卫、负控、账本和评审轮次，验收依据是证明不了 native 实现的 `gateway-parity`。不合并、不派子代理跟踪。做到对应模块时只读查一下它在该模块产品代码上的改动（`git fetch origin` 后 `git log origin/main -- <路径>`），有用就读懂后自己实现、用 bbx 验证。可能有参考价值的 PR：#2862（`export/restore_session_transcript`）、#2902（hooks 注册接线）、#2965（飞书 / 微信并发），以及 `main` 上 Slack 渠道约 770 行改动。不合入的：#2957（比基线更严，改变对外行为）、#2786（+222 万行草稿）。

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
| 2026-11-10 | `scripts/parity`、`scripts/cli-parity` 暂留，用到时先做负控 | 静态看是真对拍，但我没跑过 |

## 停用的旧闸门

| 停了什么 | 它原本想防什么 | 现在由什么替代 |
|---|---|---|
| `.github/workflows/ci.yml`、`pr-gate.yml`（文件留在原处，没删没改；它们只在 PR、push 到 `main`、merge queue 时触发，接手仓库只推 `dev/takeover`，所以不跑） | 四条构建测试命令；另有约 30 步守卫：负控装置自检、CI 配置守卫、冻结账本只追加、评审结果块格式、入口总账记账一致性 | `.github/workflows/takeover.yml` 只跑四条命令。对基线的一致性由本机的 `scripts/bbx` 验收。冻结账本我不改，所以只追加检查暂时没有对象；要动账本时手工跑对应的 `check_frozen.py` |

旧 workflow 没删的原因：仓库里有 Python 守卫和至少一个 Rust 测试会读 `ci.yml` 的内容。等 M7 精简测试时一起处理。

## 已知的坑

- Linux CI（`takeover.yml`）四条命令全绿。这台 macOS 上全量 nextest 有 3 个失败：`unsafe_boundary::compiler_enforces_gateway_boundary`（守卫只支持 Linux）、`channel::sms::tests::dual_stack_occupier_is_reported_by_the_fixed_host_bind_probe`、`mcp_https::replays_ts_mcp_https_over_local_tls`（后两个与 IPv6 有关，稳定复现，根因未查）。
- 全量 nextest 约 12 分钟，`src/target` 约 15 GB。改一行 gateway 后增量构建约 35 秒。
- 基线 CLI 的 `cron` 和默认对话路径会探测写死的 `127.0.0.1:18789`。bbx 在该端口有监听者时拒绝运行，不要绕过。
- bbx 独占固定目录 `/private/tmp/pdrs-bbx` 和端口 38411 / 38412，不能并行跑两个。
- `pilotdeck-gateway` 不带 `--native` 是透明代理（转发给 TS）。验证 Rust 行为时必须走 native。
- native gateway 每个会话拉起 `pilotdeck-agent-loop-sidecar` 子进程；基线默认是进程内。
- 原仓库 `daqi-dev/pilotdeck-rs` 只读。它的其他分支历史里有 18 条 gitleaks 命中（测试夹具里的 key 形状字符串），不在 `dev/takeover` 的历史上；只推 `dev/takeover`。
