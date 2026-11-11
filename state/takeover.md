# 接续记录（`dev/takeover`）

新会话先读这份，再读 `CLAUDE.md` 和 `.42cog/intent.md`，然后看 `git log`。不要往 `state/board.md` 追加；`state/` 下其余文件是上一轮流水线的记录，只当线索。

最后更新：2026-11-10

## 现在由 Rust 承担并有证据的入口

| 入口 | 证据 | 没覆盖的 |
|---|---|---|
| `pilotdeck server` 的启停：参数与环境变量优先级、启动输出、token 文件、停机退出码、`/health`、`/auth/local-token`、落盘 | `server-entry-no-config` / `start-stop` / `port-host` / `token-generate` / `token-sources` 5 个场景 SAME | 停机时有客户端连着；非 git 目录的 stderr；`telemetry.enabled: true` |

当前对拍结果（`node scripts/bbx/check.mjs`，2026-11-10，M1 第三块第一轮后）：基线 vs Rust 6/12 SAME。

| 场景 | 结果 |
|---|---|
| `server-entry-*` 6 个（含握手、能力列表 53 项） | SAME |
| `turn-text` | DIFF 469 |
| `project-default` / `project-keys` / `project-session-lifecycle` | DIFF 864 / 1140 / 1698 |
| `survey-empty-params` | DIFF 38 |
| `cli-survey-offline` | DIFF 15 |

带对话的 4 个场景，会话 `.jsonl` 的记录类型、顺序、字段、请求头、`context_budget` 字段集已一致。剩余差异的原因（数字是 `turn-text` 的，用 `diff.mjs --max 100000` 统计）：

- 工具声明 15 对 28，数组按下标比整体错位（392）。13 个工具没有 Rust 实现：`agent`、`ask_user_question`、`create_goal`、`execute_code`、`get_goal`、`read_skill`、`send_attachment`、`send_message`、`structured_output`、`subagent`、`todo_write`、`update_goal`、`mcp__funasr__transcribe_audio`（清单在 `native_process/tools.rs` 的 `BASELINE_TOOLS_NOT_PORTED`）。
- 系统提示词缺内置插件带来的两行 skills（2）。基线从 `dist/src/extension/plugins/builtin/` 加载 browser-use、windows-skills、funasr。
- 上两项派生的数值：`content-length`、`context_budget` 数值、`requestFingerprint`、`fileSize`（24）。
- `read_session_messages` 未实现（39，含 uuid 编号错位）；`.projections.json` 13 个投影只写了 2 个（12）。两者都依赖基线 `session/projection/` 的 reducer。
- `rename_session` 追加的 `session_metadata` 记录：基线 `sequence: 1`、`parentEntryId: null`，Rust 接着编号（只在 `project-keys`）。

能力列表照基线列出但 Rust 还不能（完全）服务的，清单在 `native_process/capabilities.rs` 的 `ADVERTISED_NOT_SERVED`。

## 验收手段

`scripts/bbx/`（说明见其 README）。同一场景先后喂给基线 `node dist/src/cli/pilotdeck.js` 和 Rust 二进制，比较 WS 帧、HTTP、CLI 输出、发给假 provider 的请求、落盘文件。

- 改完代码：`(cd src && cargo build -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli) && node scripts/bbx/check.mjs`
- 新场景先跑 `--self`（基线自比）确认稳定。
- 负控记录：2026-11-10 把 `native_process/methods/tools.rs` 里 `background_task_stop` 的 `sessionKey is required.` 改一个字母，`survey-empty-params` 从 181 变 182 并指出该处，还原后回 181。注入点必须先确认在被调用路径上。

## 正在做

**M1 最小闭环**，分三块：

1. 入口：已提交（`pilotdeck server` 与基线同参数；Linux CI 全绿）。
2. 多项目宿主与落盘布局：已提交。规则集中在 `native_process/projects.rs`：运行时路由是纯词法 `resolve(projectKey ?? pilot home)`；对话准入要求项目已登记（`projects/<id>/.cwd`），pilot home 免登记；gateway 自己从不登记项目。负控：把缺省项目改成 cwd，`turn-text` 报出 `sessions[0].cwd` 和 4 条落盘路径差异。
3. 发给 provider 的请求与会话记录：第一轮已提交（能力列表、请求头、会话 `.jsonl`、`context_budget` 字段、10 个工具接线）。`turn-text` 还到不了 SAME，剩下三件并行做（各自在 git worktree 里，用 `BBX_TARGET` / `BBX_BASELINE` 指向自己的构建和主检出的基线）：移植 12 个工具；会话读模型（投影 + `read_session_messages` + `rename_session` 记录）；内置插件与 funasr 的 MCP 工具。

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
- sidecar 找不到时的报错文案是 Rust 自己的，基线没有对应物（基线默认进程内跑循环）。

## 之后

M2 会话域 → M3 工具回路（同时重跑 `scripts/parity` 的 53 场景并做负控）→ M4 其余 method 族 → M5 gateway HTTP → M6 CLI → M7 退役（删代理模式、删未接线代码、原版 `ui/server` 接 Rust gateway 实测）。完整计划和盘点见 `_tmp/20261110-inventory/REPORT.md`（不入库；丢了就按本文件重做盘点的相应部分）。

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
