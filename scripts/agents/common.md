# 通用说明（pilotdeck-rs 子代理）

> 主会话给每个子代理的任务说明里会要求先读这份文件，再给出本任务的参数：对齐到哪个提交、bbx 槽位（1–4 给子代理，5–8 留给主检出）、scratchpad 子目录、任务本身。这份文件随事故更新；改它的时候在 `state/takeover.md` 记一笔原因。

你在 `pilotdeck-rs` 仓库的一个独立 git worktree 里工作。

## 第一步
对齐到任务说明里给的提交：先试 `git merge --ff-only <提交>`；失败（worktree 分支可能建在别的基点上）且你的分支上没有自己的提交时，`git reset --hard <提交>`。然后读 worktree 里的 `CLAUDE.md`、`scripts/bbx/README.md`、`scripts/bbx/diff.mjs` 开头注释。

## 环境
- 基线（TypeScript 原实现，固定版本）在主检出 `/Users/da/ws/pdrs/pilotdeck-rs/resources/PilotDeck/`：只读，已构建 `dist`，用 Node 22 跑。在你的 worktree 里建符号链接 `resources/PilotDeck -> /Users/da/ws/pdrs/pilotdeck-rs/resources/PilotDeck`（`resources/` 目录本身入库，不要整个替换；链接被 gitignore，不提交）。主检出除 `resources/` 外不要读写。
- **机器只有 10 个核，同时有 5–6 个代理在工作。所有 cargo 命令前加 `CARGO_BUILD_JOBS=4`**（例如 `CARGO_BUILD_JOBS=4 cargo build ...`、`CARGO_BUILD_JOBS=4 cargo nextest run ...`），nextest 另加 `-j 4`。不要跑整个 workspace 的测试；只跑你改动涉及的 crate，而且只在收尾时跑一次。全量 bbx（不带场景名的 `check.mjs`）也只在收尾时跑一次，平时只跑你关心的场景。
- Rust 在 `src/`（Cargo workspace）。gateway 宿主：`src/pilotdeck-gateway/src/native_process/`；CLI：`src/pilotdeck-cli/`。你的 `src/target` 是空的，首次构建要几分钟：`cd src && cargo build -p pilotdeck-cli -p pilotdeck-gateway -p pilotdeck-transport`。
- 行为比对工具 `scripts/bbx`：同一场景先后喂给基线和 Rust，比较 WS 帧、HTTP 响应、CLI 输出、发给假模型 provider 的请求、落盘文件。
  - `node scripts/bbx/check.mjs <场景名>` 跑一个场景（基线 vs Rust）；不带场景名跑全部；加 `--self` 是基线自己跑两次互相比（新场景必须先过这个，证明场景稳定）。
  - **你有自己的并行槽位：所有 bbx 命令前都加任务说明里给的 `BBX_SLOT=<n>`**，这样不用和别的代理排队。不同槽位的产物不要互相比。
  - 它会检查 Rust 二进制是否比源码新，旧了拒跑——先构建。重新构建后先跑一次 `turn-text` 预热（第一次起 Rust 二进制慢，短超时的场景会假失败）。
  - 场景格式、步骤类型（`ws` / `http` / `cli` / `serve` / `connect` / `answer` 等）、假 provider 规则见 README 和 `scripts/bbx/scenarios/` 里的现成例子。
  - 看差异：`node scripts/bbx/diff.mjs _build/bbx/<场景>.ts.json _build/bbx/<场景>.rust.json --max 200`。
  - 机器上同时有多个代理在构建和跑测试，负载高时偶尔会出现与你的改动无关的假失败：Rust 侧事件里出现 `Sidecar handshake timed out`、或某个限时步骤超时。遇到先单独重跑那个场景，重跑一致就不算问题。
  - 需要引用服务端随机生成的值（任务 id、事务 id、会话名等）时：步骤或 provider 规则上写 `"save": {"名字": "正则"}`，之后用 `{saved.名字}` 引用，详见 README。
- **停进程只能按你自己记下的进程号停**（启动时记 `$!`）。不要用 `pkill -f`、`killall` 这类按名字匹配的方式：机器上有别的代理和别的项目的同名进程。
- **后台等待必须有上限。** 需要等某个后台命令结束时，优先直接等它的完成通知；写轮询循环（`until …; do sleep N; done`）时必须带总时限（例如最多等 30 分钟就退出并报告），条件要是一定会成立或能明确失败的；不要用 `pgrep -f <模式>` 这类会匹配到自己或别的代理的条件。收尾前确认自己没有留下还在循环的后台 shell。
- 临时文件放任务说明里给的 scratchpad 子目录。shell 的 cwd 会漂移，每条命令开头 `cd` 到你的 worktree 根目录。

## 什么算事实
基线实际跑出来的输出 > 基线的测试 > 基线源码（结论带文件和行号）> 基线文档。Rust 现有的代码、注释、测试都排在后面，只当线索。拿不准就把基线跑一次看输出，不猜。

不要参考仓库里 `.claude/` 下的技能和 Workflow、`state/` 下除 `takeover.md` 外的记录、`specs/` 下的差异清单、原仓库的 issue / PR——它们是上一轮工作流程的遗留物，结论不可靠。

## 规则
- 目标是 Rust 的对外行为与基线一致。不发明字段、文案、协议、错误格式。读懂基线后自己写，不复制粘贴 `resources/` 的代码（必须逐字一致的文本、名单除外）。Rust 内部结构按 Rust 习惯来，不照搬 TS 的类图。
- 先查 Rust 侧有没有已写好但没接线的实现（上一轮留下很多），能用就接线并用 bbx 验证，不要重写；与基线不符的照基线改。
- **不许占位**：做不完的保持现状并在报告里列出，不要返回看起来像样的假数据，不要为了让场景通过写特判。
- 宁可把少数几个彻底做完，也不要很多个各做一半。按任务说明给的顺序做，每做完一个（场景 + 修到一致）就提交，再做下一个。
- 你认为是基线自身 bug 的（崩溃、挂起、丢数据、与它自己的类型 / 文档 / 测试矛盾、安全问题）：先照基线做（或保持现状），在报告里给出复现和证据，由主会话决定。
- 不改 `scripts/bbx/*.mjs`；需要采集器新能力时在报告里提出。不给场景加 `volatile` / `accepted`。基线自己跑两次就不一样的值，用 `--self` 的输出证明后在报告里列出路径，由主会话决定怎么处理；这类场景先留着差异。
- 场景只读写 bbx 的隔离目录（`{ROOT}` 之下）。不读不写不启停 `~/.pilotdeck`、本机正在运行的 PilotDeck 进程（机器上有别的项目的 `pilotdeck-*` 进程，别碰）、`~/ws/PilotDeck-rust-program-20261009/`。不要让任何进程监听 `0.0.0.0`；不碰端口 18789（bbx 发现它有监听者会拒跑，不要绕过）。
- **任何时候运行两侧的 `pilotdeck` 命令（基线的 `node dist/src/cli/pilotdeck.js` 或 Rust 的 `pilotdeck`，包括 `--version`、`--help` 这类看似无害的），都必须经过 bbx，或者在命令前显式设置隔离的 `HOME` 和 `PILOT_HOME`（指向你 scratchpad 下的临时目录）。** 不带隔离直接运行会读到真实的 `~/.pilotdeck`，这是禁止的。
- **场景和探测不能产生对外的真实网络连接。** 模型 provider 一律指向假 provider；渠道、webhook、MCP、搜索这类会连外部服务的功能，只测「缺凭据 / 没装 / 地址不可用」的表现，或者把地址指到本机的假服务。写渠道配置前先读基线判断「凭据齐全」的条件，别让基线真的去连（2026-11-12 有一次探测让基线向企业微信的服务器发起了连接）。
- **遮住不稳定之前先量两侧。** 一个值或一段次序在对拍里时有时无地不一致时，不要直接声明 `volatile`、也不要改场景去绕开（加延迟、包一层、换输入）。先各跑至少 20 次，分别统计基线自比和 Rust 自比的分布：基线自己不稳，才可以声明最窄的 `volatile`；基线稳定而 Rust 不稳，就是 Rust 的真实差异，要修 Rust 或者如实报告，不能遮。报告里给出两侧的次数。（2026-11-12 有一处用延迟退出的包装遮住了真实差异：基线 40 次次序固定，Rust 40 次里 14 次相反。）
- **改了跨 crate 的东西就跑下游的测试。** 改 sidecar 的帧格式、`pilotdeck-protocol` / `pilotdeck-transport` / `pilotdeck-agent` 里被 gateway 或 CLI 用到的类型和行为时，除了自己改的 crate，还要跑依赖它的 crate 的测试（至少 `pilotdeck-gateway`）。（2026-11-13 有一次只跑了 transport 的测试，gateway 里一个扮演宿主的测试因此失败，到全量才发现。）
- 不联网，不访问真实模型 provider，不用 `.env.local` 里的密钥，不 `pip install` / `npm install`。
- 只读不许动：`resources/`、`vault/raw/`、`notes/`、任何 `hi.md`、`.42cog/`、`docs/plan/`、`specs/`、`state/`。
- 仓库里有一套已停用的旧测试装置（`src/*/tests/fixtures/**/negative_controls.py`、产品代码里的 `mutants::on(...)` 开关）。不用维护它：改产品代码时不必同步那些 `.py`；碰到 `mutants::on` 开关照常保留或随被删的代码一起删。不要新增这类东西。
- 不派子代理。
- 在你的 worktree 分支上本地提交：逻辑单元一个提交，前缀 `feat` / `fix` / `test`，后接中文一句话，尾注 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`；用 `git add <具体路径>`，不要 `git add -A`。**做到一个可用的中间点就提交**（会话可能被中断，未提交的容易丢）。不推送、不开 PR、不动别的分支。收尾前工作区必须干净。

## 完成标准（每个任务都适用）
1. 你新增的场景：`--self` SAME；基线 vs Rust SAME，或剩余差异逐条说明原因。
2. 全量 `node scripts/bbx/check.mjs`（带你的槽位）：SAME 的场景不得比开工时少；给出前后数字和不一致清单。
3. 在 `src/`：`cargo fmt --all --check`、`CARGO_BUILD_JOBS=4 cargo clippy -p <你改动的 crate>... --all-targets -- -D warnings` 通过；跑你改动涉及的 crate 的测试（收尾时一次）：`CARGO_BUILD_JOBS=4 cargo nextest run -p <crate>... -j 4 --no-fail-fast --profile ci --config-file ../.config/nextest.toml`。本机已知失败只有这 3 个：`unsafe_boundary::compiler_enforces_gateway_boundary`、`channel::sms::tests::dual_stack_occupier_is_reported_by_the_fixed_host_bind_probe`、`mcp_https::replays_ts_mcp_https_over_local_tls`（另有限时性能测试 `html_markdown::ten_mib_of_flat_inline_siblings_stays_fast` 在机器负载高时会超时，单独重跑通过即可）。
4. 做一次检查证明比对真的在起作用：故意把你对齐的一处 Rust 输出改错，重建，确认对应场景报出差异，再还原。
5. 改或删的旧测试逐条列出及依据（依据是基线的实际行为）。

## 报告
中文。先结论：任务里每一项是「彻底做完 / 部分 / 没做」，没做完的卡在哪。再：场景结果表（场景、覆盖什么、自比、基线 vs Rust）；全量前后数字；上面第 4 条的结果；改动的旧测试；基线自比不稳的值；疑似基线 bug；worktree 路径、分支名、提交列表；你没验证的和拿不准的事。没做完如实说，不要标完成。
