# H1 共享渠道命令

状态：进行中。2026-11-13，根工作区独立新模块 `src/pilotdeck-gateway/src/channel/commands/`；没有改共享 `channel/mod.rs`、Cargo 或 assembly，也没有运行 Cargo、TS 采集或产品。用户范围包含全部渠道；不能把该模块本身称作渠道接线或完整交付。

固定 TS `src/adapters/channel/protocol/ChannelCommandRegistry.ts:67-275` 为行为来源：八项按 new/plan/projects/switch-project/update/status/search/help 顺序；`resolveCommand` 以 JS trim、仅 ASCII 空格分开命令与参数，名称大小写和别名匹配；`new` 无 handler、`plan` 非 system，均返回 false。projects 与 switch 顺序、update 两次回复、status 格式、search 能力缺失和总错误回复按该源实现。WeCom 调用在 `WeComChannel.ts:531-543`，mapper 在 `:721`；Weixin `WeixinChannel.ts:482`，Feishu `FeishuChannel.ts:488`。各渠道入口调用次序仍由各作者据实迁移，不能把所有渠道不加区分地拦截所有 slash。

公共接口：`execute(input: &JsString, host: &dyn CommandHost, ctx: &dyn CommandContext) -> Result<bool, CommandError>`；`resolve` 返回名称与参数。Host：`list_projects`、`run_update(check)`、`status`、`search(raw_args,project)`，异步结果均经 `CommandFuture`；Context：`reply`、`current_project`、`bind_project`、`reset_session`。search 返回 None 仅表示基线缺少 sessionSearch/pilotHome；有能力时由真实共享搜索服务按 TS 的 `ChatSearchArgs.ts` 解析、执行并按中文格式化，不能直接返回固定字符串。传 `JsString` 保留孤立 UTF-16 单元。Project 包含 name/project_key/full_path，UpdateResult 包含 code/output，Status 包含 branch/commit/uptime_seconds/node/platform。每个 adapter 实现自己 chat 的 scope 和发送方式。

已核复用：gateway `metadata.rs:733` 的 `list_projects` 可作为真实项目清单来源；历史搜索实现位于 CLI 私有 `src/pilotdeck-cli/src/chat_search.rs` 及子模块，现不可由 gateway 直接复用；更新入口 `src/pilotdeck-cli/src/update.rs` 私有且不提供捕获输出的公共进程端口。上述搜索/更新需由主协调抽取为共享宿主服务或正确依赖注入，不能调用 TS 或返回假结果。status 需要真实 Git、进程运行时间和基线可观察 Node/platform 信息；默认 native 不依赖 Node 的限制与基线 status 输出如何兼容须实测并交主协调处理，不能凭常量伪造。此次没有改变架构或批准差异。

测试：`commands/tests.rs` 是4个纯 mock 测试，覆盖分发、别名和空格、项目精确优先与绑定顺序、更新多条回复、status、搜索传参和 help，**尚未编译运行**。fixture `h1-channel-commands/capture.mjs` 为固定 TS dist + 假 gateway/mapper/search/reply 的14格采集，`node --check` 通过；由于 E0 采集槽占满，未执行。它只含无副作用的无效 update，未覆盖执行有效 update/status 子进程；dist/source 对应尚待核。必须两轮新目录执行、加入实际故障注入，核对源码身份后再声称 oracle。Rust `/search` parser/formatter、更新/status 宿主以及渠道真实入口、cancel/reload 和错误路径的完整行为必须在集成后单列验证。

暂定操作序列：核实 TS dist/source → 14格双轮及产品路径负控 → 主调度固定源码编译测试 → 对照 capture → 主接真 host 与各 adapter → mock 正式入口/失败/恢复 → 在同一集成 SHA 验收。不能用局部 mock 测试或本说明代替对拍。

## 固定 TS 模块采集（2026-11-13）

经主协调释放轻量槽，隔离 `HOME` / `PILOT_HOME`，Node `v22.22.0` 真实加载固定基线 dist registry 和独立 fake gateway、mapper、search 两轮采集：`_tmp/20261113-h1-commands-oracle/{first,second}.json`，14格完整 case 精确相同，`verify.mjs` exit0。使用真实 registry 执行但将 listProjects mock 改为空的实际路径负控 `negative.json`，原 verifier exit1，实际“暂无项目”被原项目列表断言拒绝。并非只改结果 JSON；属于受控模块级故障注入，不替代真实 server 和 Rust 对拍。有效 update/status 为避免真实进程副作用不含在14格中，需另建 fake exec 和适用基线入口证据。

ES2022 + ESNext 模块选项下 TypeScript 5.9.3 分别转译 registry、`ChatSearchArgs.ts`、`formatChatHistorySearch.ts`，去除 sourcemap 行/首尾空白后与对应固定 dist 字节一致。最初探查误用 NodeNext 默认模块模式，三个比较均 false；更正实际 ESM 选项后均 true，不抹去首次误配观察。该检查只覆盖上述三个文件，不证明整个 dist。两次采集与负控使用同一只读基线，无真实账号、Gateway 或网络。

补充 `/status` 实际模块探针：`status.mjs` 以隔离 PATH 下可控 fake git 调用固定 TS handler，success 回复包含 mock 分支/提交、真实该 Node 进程 uptime/version/platform；git exit3 的失败回复保留命令和 stderr。证据 `_tmp/20261113-h1-commands-oracle/status-{ok,fail}.json`。这是每项一轮模块探针，不是正式渠道验收；没有执行真实 git。

`status` success/failure 第二轮与首轮各自逐字节相同。额外有效 `/update` 四格以 Node 官方 `syncBuiltinESMExports` 替换 `child_process.spawn` 为内存 mock，实际运行固定 TS registry→`runUpdateProcess`，未执行 update 脚本：成功0、已最新2、失败1、spawn error 两轮一致，核对 `--restart`/`--check`、两条回复和退出码对应输出；产物 `update-{first,second}.json`。`runtime/updateCommand.ts` 与对应 dist 的正确 ESM 转译一致。此仍是 mock 子进程模块证据，真实 Rust 进程生命周期待测。

## 原生宿主候选与共享搜索迁移

已新增 `commands/host.rs`、`update.rs`、`search/`：真实 gateway Metadata 枚举，CLI 原有 JSONL 搜索 matcher/title 核心移入 gateway 并让 CLI 保留原 argv/英文格式、转调用同一核心；原 CLI 私有 matcher/title 文件随迁移删除。host 在阻塞池执行真实 metadata/历史扫描，支持 search flags 和中文输出，UTF-16 查询与会话 ID 保留；无 Node 的 native status 保留 `Node: not used (native Rust)`，详 `H1-STATUS-NATIVE-RUNTIME-001`。可选 Git Web updater 仅在找到安装的 `scripts/update-web.mjs` 时使用 Node 启动真实进程；缺脚本按固定基线明确报错，不发明 Rust 自更新。O1 正在接线 `set_command_host`；尚未实际编译或产品入口验证。

新增6项 commands 测试包括隔离真实 JSONL、role/regex错误、孤立代理项/BOM、fake git/node 状态与 updater 成功、exit2、失败和缺脚本；`rustfmt` 与 `git diff --check` 已执行，**未运行 Cargo**。CLI 搬迁必须对照既有39格正式测试、原 matcher/title 回归、中文搜索格式及 Node 真实 fixture。TS 的 status `process.uptime` 从进程起点计算，O1 当前传 assembly Instant，需改成产品入口起点或明确接受/验证差异；目前不声称精确一致。已知 project path 含孤立 UTF-16 仍报错、非法 regex 错误文案转换有损，尚未闭合。updater 新 host 实现目前与 CLI 现有 updater 进程代码独立，尚未完成共享抽取；不能据此报告最终去重或交付。正式 WeCom/其他适用渠道、失败/取消及真实 mock 集成均待固定源码集中验证。

第5批候选窄修：状态真实入口在 CLI main 第一行、gateway 同步 main 构造 Tokio runtime 前登记 `Instant`；embedding 未登记时报错，不把装配时间冒称进程起点。gateway 原 `#[tokio::main]` 展开为相同 multi-thread/enable-all runtime；O1 的装配使用 `process_start()`。status 的 git child 设置 kill-on-drop；新增隔离子进程中 fake git 阻塞后取消 status、确认该明确 PID 已退出的测试。fake git/node 另记录并核对完整 argv、工作目录，缺 updater 测试改用无脚本的 sibling root，避免误命中原夹具脚本。非法 createdAt 现在保留原始 UTF-16；孤立项目路径经隔离 Node filesystem/sha1 实测按 U+FFFD 编码，并在 native 历史查询中加真实文件正向测试。所有测试待集中执行；缺省 Node 字段差异已单独登记并通过冻结检查。进程 earliest-main 仍可能与 OS 实际启动相差装载时间，时间边界需正式对拍。CLI 与 host updater 进程代码仍待抽取为唯一实现；正则语法错误包含孤立代理项的文案仍有损，不能宣称该边界 parity。
