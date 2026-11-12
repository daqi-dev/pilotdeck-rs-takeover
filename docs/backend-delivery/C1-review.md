# C1 非作者审查

状态：本地非作者审查已冻结；E0、集成全量与发行验收未闭合。审查者：`/root/review`。起点：`50cfd7c96633de7dbfdf8159d3934eefd9d55262`；固定 TS：`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`。本记录只报告本轮检查，不能替代受测提交的五层交付证据。

## 起点审计

固定 TS `src/cli/pilotdeck.ts:30-35,285-348` 的非 TUI 命令集合与 Rust `src/pilotdeck-cli/src/main.rs:158-184` 对应：run/resume、sessions、settings、help、server、gateway setup、cron、update、skills migrate、chat search、默认 argv/stdin。基线没有专门的 `--version` 或补全命令，不新增。第二个 bin 的 sidecar 生命周期属于 R1/R2/D1 交叉验收。

| 发现 | 起点证据 | 处置与复审重点 |
|---|---|---|
| 在线 cron、默认会话、update 必需分支仍占位 | Rust main.rs:200,224；cli_channel.rs:47。TS pilotdeck.ts:351-423；CliChannel.ts:36-45 | 由对应作者实现；检查正式顶层接线、WS 失败回退、输出和退出码 |
| run/resume 无 project 的 model 选项提前返回 Todo | Rust sdk/turn.rs:23-28；TS sdkCli.ts:43-68、packages/sdk/src/client.ts:2002 | 在已发生的 control/query RPC 后保留基线 validation_error 文案；不能提前改变请求顺序 |
| SDK 错误路径保留 WS 导致挂起 | Rust sdk/turn.rs:150-164；sdk/transport.rs:427-442 | 已授权修复挂起；须登记明确差异，并正向断言双连接释放、错误仅打印一次、exit 1 |
| 默认 CLI 无条件永久等待 | Rust cli_channel.rs:62-71；TS CliChannel.ts:25-33 返回后主入口未 dispose 本地 gateway | 已通知作者按挂起修复处理；单轮、stdin EOF、错误、取消均须验证释放 |
| 三渠道 setup 成功与保存路径占位 | Rust gateway_setup.rs:259,269,281,307,314,361；TS gatewaySetup.ts:153-171,273-478,500-608 | 检查失败默认取消、WeCom 别名及 groups 保留、空 DM allowlist 降级、其他配置键保留、凭据与 YAML 分步落盘 |
| health 探针仅检查状态行 | Rust probe.rs:41-60；TS probeServer.ts:20 | 半截 200 不能算成功；fetch 默认重定向、完整 headers 与 200ms 总 deadline 需兼容；已通知 transport 作者 |
| HOME 未设置的系统回退仍占位 | Rust paths.rs:65、skills_migrate.rs:123；TS pilot/paths.ts:98-105、extension/skills/migrateSkills.ts:187 | 范围内可达路径；测试必须使用 mock OS home 或隔离用户容器，不能落入真实 home |
| 启动环境失败和 ICU 加载错误用 Todo | Rust main.rs:139、skills.rs:29,256 | 启动 cwd 失败交顶层作者；系统/内置数据错误不应当作未移植业务，需逐项说明实际可达性 |
| 遥测输出有固定假快照 | Rust telemetry.rs:6-11；server.rs:12-18。TS pilotdeck.ts:41-44、telemetry/collector.ts:121-151、sender.ts:79-139 | 交 O1/主协调者；C1 不能因没有 Todo 而宣称启用遥测后的 server 行为完成 |
| JS 配置宿主与 TUI | Rust config.rs:20、main.rs:181 | 旧 JS 函数加载与 TUI 本轮排除；JS 配置应有清晰不支持错误，由 B1 确认 |

表中 TS 简写路径均相对于固定基线 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`；Rust 简写路径均相对于 `src/pilotdeck-cli/src`。起点未发现显式转发至 TS 的 CLI 业务兜底。空分支中路径归一化的 CurDir、忽略非目标 WS 帧、输出事件过滤属于有条件语义，不能单凭空分支认定占位。

## 验证与复审

已执行源码、入口和占位检索；审查者未执行构建或正式产品验证。以下运行结果来自独立阅读主协调保存的本轮日志，旧 fixture 成绩不作本轮通过证据。最近已读提交为 `c8f8df2e`；probe/fetch 与 update 取消修复均已提交，固定 HEAD 的 update 模块、workspace fmt 与 CLI all-targets clippy 均已独立核对通过。本记录区分提交前冻结候选的测试与固定 HEAD 验证。

产品复审结论：

- SDK `turn.rs` 和正式入口 `c1_sdk_lifecycle.rs`：model guard 位于 query resume 之后，保留 control 错误优先；Retain 分支有界关闭 control/query，错误只由 main 打印一次。未发现静态阻断问题；本轮三项测试的 16 格全部通过，包含 hello timeout 两格。受影响旧 signal 正式重放与固定提交验证仍须单列。
- cron `cron.rs`：四命令、探测先于 usage、`argv[1]` 优先、JS parseInt、插入字段顺序与固定源码一致。27 格 oracle 是执行源码的 VM mock，不能替代固定端口正式入口；目前未发现静态阻断问题。
- YAML `StringifyOptions`：默认 80/None 的表达式与起点一致；零宽覆盖 flow/plain/quoted/block，禁 single quote 也作用于旧 scalar。核对固定依赖 `yaml/dist/stringify/{stringifyString,foldFlowLines}.js`；70 格包含真实默认 oracle。`_build/20261113-024901-yaml/output.log` 的现有 stringify 与新 stringify_options 均通过；不把两项测试表述为全套 YAML 验证。
- skills/paths：共享 Node collator 保留 process-default 的 fr-CA 修正，Latn 仍应用 backward-secondary，Cyrl 不套该修正；新增三种 locale 的正式入口样本。HOME 在迁移 report 中统一解析，已去掉后续 expect 的潜在 panic。未知 exe 跳过两个相对候选后使用原 cwd/skills fallback，override 仍优先；ICU baked data 全部加载失败明确返回错误，非正常业务占位。
- setup `gateway_setup.rs:226-343,347-657` 与 persistence.rs：Feishu 严格 code=0/token truthy 且不依赖 HTTP status；WeCom 复用 aliases/nullish/policy/list/groups；新空 DM allowlist 降 disabled；微信先凭据再 YAML，第二步失败保留凭据。`replace_adapter` 重新序列化整份配置且保留其他键、数组 named property 不落盘与 primitive 写入错误符合固定源码。作者自报 Feishu JSON null 的属性错误已修，新增 null/0 无 token/空 msg 断言。审查另发现 scode 孤立 surrogate 被 lossy 转换后错误进入 poll，固定 TS `encodeURIComponent` 应报 URI malformed；作者已用固定源码 fetchImpl 实测确认仅 1 请求并回手动模式，证据 `_tmp/20261113-c1-setup/wecom-surrogate.json`。现于 encode/poll 前检查原 JsString（含 array String 转换），已独立复审无阻断，新增两项边界测试已包含在后续 15 项 setup 回归通过结果中。网络 trait mock 不替代正式 HTTP 请求验证。
- fetch/probe：新 `runtime/model/http/fetch.rs` 复用既有 redirect、Node wire headers 与 Hyper decoder，不复制底层规则；probe 的总 200ms 包含 runtime 构建，完整 headers 后即返回而不等待 body。初始 9 格真实 TCP oracle 在 CLI 模块测试通过。追加 unsolicited 100 后 200 后，`_build/20261113-030422-probe-12/output.log` 实证 Rust true/TS false；现每 hop 安装 informational callback，仅 100 置 AtomicBool/Notify 拒绝，biased select 加 post-send 检查覆盖 100+200 同 poll，102/103不变。独立静态复审无竞态阻断，`_build/20261113-030747-fetch-100/output.log` 的 100-only 无 final 响应测试通过（0.01s、peer EOF），证明不等待 CLI 200ms timer。`_build/20261113-031203-probe-13-retest/output.log` 的 CLI 主 bin 与嵌入 sdk_signal_exit 两处均通过；每处为 1 个 Rust 测试循环比较全部 13 格的 bool、请求数和完整响应数，分母未减少。identity.json 记录 exit0、jobs4，受测工作树随后提交为 `5e2d1af5`；该项本地回归已闭合，不代表固定 18789 正式 CLI 或全量检查通过。
- SDK 六格接受差异比较器 `tests/common/signal_replay.rs:1071-1243`：白名单精确匹配原 6 ID；校验原 TS timeout/cleanup 形状后只投影 exit1 与两条 1005 noncleanup close；RPC 逐帧比较保留连接、方向、method/params，仅绑定生成 UUID，其他输出、退出、落盘和 listener removal 仍严格。新增负控覆盖范围扩大、错误退出、丢/重复/倒序 close 及 RPC 关联破坏；静态未发现放宽其他格。
- setup 网络预算复审：首版 `network.rs` 的整链 300s 与固定 CLI 不符。已独立核对 TS `pilotdeck.ts:28`、`proxy.ts:149-159`、`model/streaming/streamModel.ts:56`：headers/body 均为 600000ms，响应头预算逐请求设置。现 `SetupTransport::send` 每 hop 600s，body 每 frame 600s，新增虚拟时间“两 hop 各 400s 成功 / 单 hop 601s 失败”断言；静态复审通过，`_build/20261113-030525-setup-retest/output.log` 的 15 项 setup 回归全部通过（包含两项预算及 null/surrogate 边界）。既有连接阶段与 undici timer 粒度不在这项测试的证明范围内。

本轮发现与修复状态：

| 级别 | 实现位置 | 问题与当前处置 |
|---|---|---|
| P1（已修） | cli_channel.rs:80-90 / cli_channel/remote.rs:70-113 | 同步等 stdin 时 current-thread runtime 不驱动 WS。现常驻 reader 与一个 worker runtime 已独立复审；两轮间同步等待 Ping→Pong→下一轮的 `idle_interactive_connection_answers_ping_before_the_next_prompt` 本轮通过 |
| P2（已修） | cli_channel/remote.rs:236-260 | 首版 hello 期 Close 错报通用 closed。现保留 `Gateway closed during hello: reason/code N`，已连接期仍用原通用错误；`hello_rejection_keeps_reason_or_close_code` 本轮通过 |
| P2（实现已修，本地回归通过） | update.rs:293-345（首版89-141） | 每 8192 字节独立 UTF-8 替换会损坏跨块中文。现各 pipe 独立增量解码；模块 decoder、正式 8192/非法尾字节与双流 Unicode 均通过。原 30s watchdog 首败保留，后续 hardlink 复用只读 inode 降低反复冷加载；确切 OS 延迟原因未证明，不将归因写成产品解码死锁。差异 `D1-UPDATE-UTF8-CHUNKS-001` 已核对落账 |

update 保留已获主协调确认的可选 Git Web 管理脚本契约，不属于默认 native 核心运行依赖。updater 自身信号退出只证明退出码映射，新增 4 格另行验证 CLI PID/前台组取消后的子进程与连接释放，两类测试没有混用。

已核对新增 `D1-SETUP-WEIXIN-HOME-001` 的实际 TS 丢失登录凭据证据及精确落盘判据。默认 CLI/cron 正常返回后主动 dispose 的差异仍待正式 TS 挂起证据与登记；online 的 CLI PID SIGINT、终端前台组信号、自然 EOF 须分别验收，不能互相代替。

正式入口需要 E0 调度：固定 18789 在线 cron/default 使用 Docker 私有网络；SDK 可用随机端口本地 mock；setup 外部请求和 update 子进程使用本地 mock。所有入口均隔离 HOME/PILOT_HOME。新场景须先 TS 自比和故意错误检出，再比较当前 Rust 候选，保留首次失败。

cron 正式采集器 `tests/fixtures/c1-cron/capture.mjs` 已独立静态审查：隔离声明及 `/.dockerenv` 双守卫在端口/文件/产品启动前；清理只发往 detached 自有 child 进程组；原 stdout/stderr/RPC/hello/close 与清理信号保留，TS observed-hang 明确 `productPassed=false`，Rust 必须自然退出且有 client close。14 格仍是待固定入口执行的候选断言；采集器自测不能当产品通过。复审建议已落实：强制 Node 22.22.0、规范小写 UUIDv4、精确 scene×side 多重集覆盖及 fatal 留档。改 UUID 后自测抓到自写 WS parser 的 16-bit payload=127 误进 64-bit 分支，已改 `else if` 并保留首次失败；最终作者报告 7 自测通过。

online 正式采集器 `tests/fixtures/c1-online` 已完成非作者静态审查，8 格待 E0 固定入口执行。首版空 bundled skills fixture 会产生基线迁移 warning，现两侧 seed 相同 `completed\n` marker，未放宽 stderr。多轮 UUID 唯一、用户信号必须仅一条且实际发送到已登记 CLI PID、Linux startTicks 验证、残留子孙进程断言均已补；作者报告 15 自测通过。`files-before/after` 仅采集原始落盘证据，当前 verifyCase 不证明复杂持久化语义一致；前台进程组取消也不在这 8 格证明范围内。

SDK 正式 TS 重放驱动 `tests/fixtures/c1-sdk/capture.mjs` 固定源码/构建 SHA 与 Node，独占新目录、raw stream 哈希、精确 10/2 格及严格请求 UUID 自比已审。发现 cleanup 首步异常会跳过 socket/server 释放后，作者改为每步独立 attempt 聚合错误，补 kill false / child close timeout / already exited 三种故障注入；复审通过。已独立读取 `final-errors-1`、`final-errors-2`、`final-hello-1` 三套实际记录（相对于 `_tmp/20261113-c1-sdk`），分别 10/10/2 格 verified 且 cleanup 后零连接；逐项核对 result.json 与 summary、原始 stdout/stderr SHA 和字节。两轮 errors 的完整请求、退出、关闭及输出比较视图 10/10 相同，仅合法 UUID 与精确 SQLite 启动警告按驱动规则处理；原件未改写。

update 取消实现候选已完成静态复审：新进程组仅归本 CLI，waitid NOWAIT 保持 leader 可等待直至最后组信号，reap 前撤销 PGID，独立监督线程可在 stdout 阻塞时完成取消并保留父信号退出状态。`D1-UPDATE-CANCEL-OWNERSHIP-001` 已核对正式 TS 两轮父 PID 取消残留证据及自有组/移交 guardian 边界，4 格 PID与前台组×INT/TERM已在串行、并行两轮正式子进程测试通过。已反馈的测试问题均静态闭合：accepted socket 显式清除 nonblocking 并读完整 header；CancelCleanup 在断言失败时核验 helper 自报 PID/PGID/birth 后释放（command 留证，exec 可改变它），成功断言前不清理；signal receipt 单次写入避免 KILL 碰上重复 truncate。普通 9 格 watchdog 保留 30s 判失败边界，之后先 TERM CLI 并留 9s 回收窗口，再按 mock shell 自报及现场复核的 PID/PGID/birth 清理独立 updater 组，最后 Child kill/reap。未沿用“CLI 组包含 updater”的旧假设，失败 fixture/字节计数仍保留；13 正式子进程测试已在串行、并行两轮通过。

`_build/20261113-025233-cli-tests/output.log` 首批已结束，整体失败，不能报全批通过。模块测试 104 passed / 3 ignored、setup 正式入口 5 格、online transport 5 项、migration locale 正式入口和 SDK 新 16 格通过；update 为 8 passed / 1 failed，双流 Unicode 用例在 30s 超时（日志 525-539）。`sdk_signal_exit` 的嵌入 probe 测试在 mock 读取请求时遭遇 macOS WouldBlock（probe.rs:134，日志 1044-1062），该 target 为 133 passed / 1 failed / 113 ignored；随后已修正测试 socket 的继承 nonblocking，产品 200ms 预算没有扩大，13 格双入口重测通过。两项首次失败产物保留。

update 冻结候选的两轮结果已独立核对 identity 与原始日志：`_build/20261113-031804-update-final-serial` 明确 `--test-threads=1`，13 passed / 0 failed / 1 ignored（helper 为子进程入口，由取消测试调用），29.65s；`_build/20261113-031859-update-final-parallel` 默认并发，分母相同，16.20s。两轮均 exit0、jobs4，涵盖原 9 项与新增 4 项取消，随后产品提交为 `c8f8df2e`。`_build/20261113-031916-update-final-unit` 在编译阶段被根主动 SIGINT 终止，identity exit=-2、无 test result；这项属于调度中止，不能计产品失败或通过，已由下述固定 HEAD 顺序重跑替代。

固定产品提交 `c8f8df2ec03edced01f716287ec45b7d75e0c0ad` 的最终本地检查已独立核对 command、sourceSha、sourceDiff 与 exitCode；这三次 sourceDiff 均只有交付/审查文档和 handoff，没有产品修改：

- `_build/20261113-032023-update-fixed-unit`：`cargo test -p pilotdeck-cli --bin pilotdeck update::tests`，2 passed / 0 failed，exit0。
- `_build/20261113-032154-c1-fixed-fmt`：`cargo fmt --all --check`，exit0；这是 workspace 格式检查。
- `_build/20261113-032204-c1-fixed-clippy`：`cargo clippy -p pilotdeck-cli --all-targets -- -D warnings`，exit0，8.72s；这是 CLI crate all-targets 检查，不表述为 workspace clippy。

`c1_entry_setup.rs` 核对 source/cases/oracle/capture SHA，逐 prompt 写输入，对比精确 stdout/落盘树并断言 exit0 和空 stderr；这 5 格均为离线 setup，不声称覆盖外部成功登录。

二次占位检索：`Failure::Todo` 的产品构造只余 `config.rs:20`（B1 的 JS 宿主边界）、`main.rs:141`（cwd 读取失败，仍待主协调处理）与 `main.rs:183`（排除的 TUI）。其他 Todo 命中为错误映射或测试。`update.rs` 是唯一 Node 子进程用途，已限定为获准的可选 Git Web 管理组件；未发现 native gateway/agent 业务转发 TS。空 match 中 Pong/非目标帧过滤、CurDir、普通未修改 skill、chmod EINVAL 和 setup 数组 named properties 均有明确基线语义。

未闭合：集成候选的要求全量检查（workspace clippy/nextest/doc 等）、正式入口接受差异正向断言；E0 固定 18789 cron/default 与 setup HTTPS 正式入口、SDK 精确六格旧重放、locale 七项编译变异、发行布局。默认 CLI/cron dispose 差异、main 的 cwd Todo 及 B1/O1 交叉归属须按主协调当前证据闭合。不得据本次静态审查或采集器自测关闭 C1。
