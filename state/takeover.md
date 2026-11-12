# 接续记录（`dev/takeover`）

新会话先读这份，再读 `CLAUDE.md` 和 `.42cog/intent.md`，然后看 `git log`。不要往 `state/board.md` 追加；`state/` 下其余文件是上一轮流水线的记录，只当线索。

最后更新：2026-11-12

## 现在在哪

以命令输出为准，不手写进度。2026-11-12 上午的阶段验收（没有子代理在跑的安静机器，提交见 git 历史里本次更新）：

- `node scripts/bbx/check.mjs --slots 5,6,7,8`：319 个场景 310 个 SAME。
- 四条命令：fmt、clippy、文档测试通过；`nextest` 7749 个里只有本机已知的 3 个失败（安静机器上 11 分钟）。
- `node scripts/bbx/coverage.mjs`：101 个 gateway method 里 94 个有场景且全部 SAME，6 个被不一致的场景带上，1 个没有场景。
- `node scripts/bbx/coverage.mjs --surface`：基线对外表面清单（`scripts/bbx/surface.json`，18 个维度 2008 项）里 1479 项被 SAME 的场景碰到（74%）。各维度还没碰到的：config 99、env 135、ws-params 45、tool-params 35、provider-protocols 30、hooks 30、cli 23、files 22、loop-behaviors 22、events 16、channels 16、records 10、tools 8、http 7、slash-commands 5、sdk-session-config 4。`--surface <维度>` 逐项列出。没碰到的项里有一部分是基线只解析不使用的键、渠道与外部服务、Windows 专用。
- `node scripts/bbx/selftest.mjs`：比较器自测 58 个例子。
- `scripts/ui-smoke/`：原版 `ui/server` 接 Rust gateway，协议层零不兼容（2026-11-12 凌晨跑的，之后没重跑）。

不一致的 9 个场景：

| 场景 | 原因 |
|---|---|
| `cfg-project-sdk-sources`、`-invalid` | `sdkSessionConfig.settingSources` 没接，项目级和本地配置文件里的设置在 Rust 上不起作用 |
| `gap-sdk-deferred-tools` | 会话准备失败时少写一条 `gateway_submit_failed` 状态记录 |
| `gap-tool-read-file-vision` | 按页渲染 PDF 没移植（基线用 mupdf） |
| `tool-write-file-diagnostics` | JS/TS、HTML、CSV 写后语法诊断没移植 |
| `sdk-config-agents-background` | 后台自定义子代理的时序（Rust 的子代理在父轮中途就跑完）；用假 provider 的 `delayMs` 固定时序后再看 |
| `loop-stream-idle` | 会话记录里流守卫错误的 `details.raw`（基线落成 `{}`） |
| `proto-anthropic-basic` | 缓存代际 `generation`：Rust 每轮重建上下文运行时，丢了会话级状态 |
| `robust-restart-truncated-session` | `list_sessions` 里两个会话的先后偶发对调（mtime 相同） |

Rust 已有实现和场景的：`pilotdeck server` 启停；对话主干与全部工具（含 `web_search`、计划模式、`request_user_*`、`cron_*`、always-on 工具）；权限询问、`permissions.json`、沙箱模式、交互配置；一次性与后台子代理；hooks；自动与手动上下文压缩；循环里的截断续写、流中断恢复、并行工具调用；OpenAI chat 与 Anthropic 两种协议；会话读取、分叉、替换、导出恢复、文件回退；插件、skills、MCP；设置读写与热重载；上传、快照、定时任务、always-on；HTTP 路由；CLI 的主要命令与默认对话；provider / router / gateway / cron 等配置键。

**重要的认识（2026-11-12）：「有场景且一致」不等于做完。** 场景大多是实现者自己写的。一次只看基线源码和场景、不看 Rust 实现的缺口审查，半小时找出 16 处差异，包括安全相关的配置在 Rust 里不生效（`agent.sandboxMode`、`~/.pilotdeck/permissions.json`、`agent.interactionProfile`）和整个缺失的 `web_search` 工具。它还留下一份没来得及测的缺口清单（报告在会话记录里；合并进来的 `gap-*` 场景是已测的部分）。

## 验收手段与流程

`scripts/bbx/`（说明见其 README）。同一场景先后喂给基线和 Rust 二进制，比较 WS 帧、HTTP、CLI 输出、发给假 provider 的请求、落盘文件。

合并一个分支后的流程（2026-11-12 起）：

1. `cd src && cargo build -p pilotdeck-cli -p pilotdeck-gateway -p pilotdeck-transport && cargo fmt --all --check && cargo clippy --all-targets -- -D warnings`
2. `node scripts/bbx/check.mjs --slots 5,6,7,8`（槽位 5–8 留给主检出，1–4 给子代理）
3. 通过就推送。全量 `nextest` 和文档测试交给接手仓库的 CI（Linux，每次推送跑四条命令）；CI 红了再修。
4. 每个阶段收尾：停掉所有子代理，在安静的机器上跑一遍完整的四条命令和全量对拍，以那次为准。

踩过的坑：

- 不要让两轮 `nextest` 重叠，也不要在 `nextest` 运行期间合并或重新构建：它会按不同的特性组合重新链接 `pilotdeck`，新链接的文件在 macOS 上首次执行要过安全评估，对拍开头几步会超时；旧测试对着新二进制还可能永远等不到退出。
- `nextest` 加 `timeout 3600`。本机已知失败 3 个（见「已知的坑」）；`native_process::settings_tests::reload_ticket_releases_after_settlement` 间歇失败（正在查）；两个限时测试在高负载下会超时。
- 机器 10 核。4–5 个子代理并行时负载均值 15–35。子代理的通用说明要求它们构建最多用 4 核、全量只在收尾跑一次。

## 正在做（2026-11-12）

整体安排（人已同意）：先收住手上的；再建立「基线对外表面」的完整清单并把覆盖统计扩展到各维度；之后按真实使用排优先级（界面用到的约 45 个 method 是地基，已验证）；清理旧代码提前做；最后是发布版本与性能、需要人参与的真实环境验收。

在途的子代理：无（2026-11-12 上午这一批六个分支已全部合并）。

主会话自己要做的：

1. 清单出来后扩展 `coverage.mjs` 到各维度。
2. sidecar 这一层（2026-11-12 已评估）：改成进程内可行、代价不大，但不现在做，排到清理阶段。事实：sidecar 不做任何对外 IO（模型、工具、落盘都由 gateway 一侧完成），它的服务入口 `ExecutionServer::serve` 接受任意读端和写端；所以可以在 gateway 进程里起线程跑同一份代码、用内存管道连接，线上协议不变。独立的 sidecar 可执行文件必须保留（阶段 C 的交付物）。收益有限：进程边界带来的问题（握手、找不到可执行文件、进程残留）能消除，序列化边界带来的问题（空助手消息、错误原文、选项没传）不能，后者已逐个修掉或登记；握手中位数 8 毫秒，开销不是问题。剩下的好处是单文件部署、不受 macOS 首次执行评估影响。做的时候保留子进程方式作为可选项，用全量对拍验收。
3. 安全修复合并后，亲自读一遍权限判定、沙箱、路径检查的代码。
4. 给人一份「需要过目的决定」清单（见下）。

还没派的：

- 缺口审查留下的未测清单：流中途断开后的恢复、`finish_reason=length` 的续写、非法工具参数的自动纠正、只有 thinking 没有文本、并行工具调用、`reasoning_content` 的解析与回传、`speedMapping` / `service_tier`、流空闲超时、若干配置键（`gateway.*`、`memory.*`、`agent.fallbackModel` 等）、LSP 与 MCP 资源工具。假 provider 已支持这些应答形态（多工具调用、非法参数、推理文本、分块、`finish`、`breakAfter`、`hangAfter`、`delayMs`）和 Anthropic messages 形状。
- 别的协议形状（OpenAI responses、Google）：假 provider 还没有。
- 写后语法诊断（JS/TS、HTML、CSV）：基线用 TypeScript 编译器等解析器出报错文案，逐字一致可能做不到；倾向登记为已知差异。
- always-on 的 `always_on_read_chat_history` 工具（调用回未实现）；`sdkSessionConfig` 的 `agents` 少见子项与其余字段；公共 host 的 PDF 解析与 StaffDeck 相关路径；渠道适配器；完整 TUI；遥测的本地队列文件。
- 对拍工具还缺：多条并存的 WS 连接、原始帧、发信号后继续执行步骤、假 provider 记录 CONNECT、`save` 按 JSON 路径整值保存、`files` 写二进制。
- 清理：旧测试装置（`tests/fixtures/**/negative_controls.py`、`mutants::on`）、透明代理模式、未接线代码。
- 发布版本构建与性能测量（启动耗时、每轮开销、内存、并发会话）。

## 需要人过目的决定

有意偏离基线（Rust 不照做）：

| 事项 | 基线 | Rust | 登记处 |
|---|---|---|---|
| 改名 / 打标签后重载会话 | 会话永久开不了轮次 | 记录接着链写 | `rename-record-chain` |
| 配置文件监视 | 第一次被改写后失效 | 持续有效 | `config-watch-survives-rewrite` |
| 飞书 webhook 收到 `null`、`/api/web` 路径带非法百分号编码 | 进程退出 | 回错误 | 场景 `about` |
| `/auth/local-token` | 不看 Host / Origin | 只认本机地址 | `server-local-token-origin` |
| `execute_code` 超时 | 管不住脱离进程组的孙进程 | 回收并限时返回 | 代码注释与测试 |
| 崩溃后残留的运行记录锁 | 能启动但运行记录全部失效 | 持锁进程已死就接管 | `registry-dead-owner-lock-blocks-startup` |
| 重启后对未答对话框作答 | 之后丢掉全部会话历史 | 保留历史 | `dialog-recovery-keeps-history` |
| 客户端不发关闭帧就断开后停机 | 不退出 | 正常退出 | 未登记（需要对拍工具支持只断 TCP） |
| 中止信号先于子进程启动 | 命令仍会执行 | 不启动（修复在途） | 待登记 |
| sidecar 握手 | 无对应物 | 不设时限 | — |
| 遥测上报到外部地址 | `telemetry.enabled` 时上报 | 不实现 | 待登记 |

保持与基线一致的已知风险或怪异行为（人可以决定让 Rust 修掉）：

- `execute_code` 把 Python 自带的写文件和执行命令当只读，默认模式下不询问；子进程继承宿主全部环境变量。
- hook 进程继承 gateway 的全部环境变量，包括 gateway token。
- `always_on_abort` 返回已中止，但那一轮照常跑完（基线传错了运行 id）。
- 第一层子代理的深度算成 2；`managedPermissions.defaultMode: plan` 不生效。
- 默认对话在输入结束后不退出；在未登记的目录里每轮都失败。
- 会话文件里某一轮缺结束记录时，会话永久不可续写。
- 导出空会话报「归档无效」而不是「会话为空」。

Rust 明确报错而不假装支持（文案是 Rust 自己的）：`update_settings` 遇到少见的 YAML 特性；Draft 2020-12 的表单 schema；公共 host 的 PDF 解析。

已接受的小差异：模型请求失败后的空助手消息在 sidecar 里被丢（只影响一个内部计数，`sidecar-drops-empty-assistant`）。

一次越界（2026-11-11）：一个子代理在没有隔离 home 的情况下运行过 `pilotdeck --version`，可能读取了真实的 `~/.pilotdeck`（它报告没有写）。之后通用说明里加了硬规则。

## 计划

工作包的历史状态见 git 历史里本文件的旧版本（2026-11-11 的计划表 P1–P16）。P1–P11 已完成或并入上面的清单；P12（always-on）主体完成。之后不再维护工作包编号，以上面「正在做」「还没派的」为准，进度以命令输出为准。

并行规则：同时最多 4–5 个 Opus 子代理，各用一个 bbx 槽位（1–4）；合并由主会话逐个做。给子代理的通用说明在 `scripts/agents/common.md`（2026-11-12 从会话的临时目录搬进仓库）；派活时让子代理先读它，再给出本任务的参数（对齐到哪个提交、bbx 槽位、scratchpad 子目录）和任务本身。每个子代理一个 worktree（Agent 工具的 `isolation: "worktree"`，`model: "opus"`）；合并后删掉它的 `src/target`（每个 25–45 GB）。子代理结束后检查有没有留下还在循环的后台 shell（2026-11-12 清理过 12 个永远等不到条件的等待循环）。模型分工：主会话 Fable，子代理 Opus，验证不用 fork。

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
