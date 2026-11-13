# 问题台账（`dev/takeover`）

分级见 `specs/delivery/README.md`。只记还开着的；修掉并在 cb8 上验证后删行（历史在 git 里）。「来源」里的「审阅」指 2026-11-13 的独立代码审阅（只读、没有运行，结论是线索，修之前先复现）。

最后更新：2026-11-13（合并 Linux 差异、清理透明代理、表面缺口第二组之后）

## 1 安全与数据

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| （无开着的）半开连接占名额已在 `6299bcece` 修掉：对端 EOF 时立即归还名额、保留写端；场景 `robust-vanished-clients-free-slots`，待 cb8 验收确认 | — | — | — |

## 2 主路径

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| 请求发出前中止不发 `turn_aborted` 状态；压缩进行中中止时摘要请求不取消 | 场景 `turn-abort-before-request-status`、`compact-auto-aborted` | 表面缺口 | 在修 |
| `submit_turn` 带 `taskBudget` 直接回 `CAPABILITY_UNAVAILABLE`（存储层有、没接线） | 场景 `loop-budget` 的说明 | 表面缺口 | 在修 |
| Linux 上稳定不一致：`skills-session-snapshot-at-create`（Rust 的系统提示词多约 40 个 token）。其余 5 个已在 `275ce32dc` 修掉（对拍根目录改成等长的 `/tmp/private`、场景余量），待 cb8 验收确认 | cb8 全量对拍 | cb8 验收 | 待派 |
| 用户中止时，基线让在途的工具自己结束、跑完 `PostToolUseFailure` hook（`isInterrupt: true`）再发 `turn_completed`；Rust 直接丢掉在途的工具调用，hook 不跑 | 场景 `hook-post-tool-use-interrupt`；基线 `ToolRuntime.ts:418-423` | 表面缺口 | 待派（与中止差异一起） |

## 3 边角

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| WS 连接上限 64 条（HTTP 与 WS 共用，满了直接关新连接，不回 503）是 Rust 独有的，基线没有上限；旧记录 `A7.2-NATIVE-WS-AGGREGATE-001` 标着「未批准」。主会话倾向去掉，先评估它与输入预算等内存保护的关系 | `native_ws.rs` 的 `MAX_CONNECTIONS`（`4fb6c17d1` 引入） | 审阅修复 | 待派 |
| sidecar 被取消时不立即释放挂起的模块调用：基线立刻拒绝挂起的调用、以 `cancelled` 结束；Rust 再等宿主回应最多 5 秒，等不到以 `result_unknown` 结束（宿主会回应时两边一样） | `scripts/upstream-tests` 的 stdio 组有 1 例因此全红；基线 `agentLoopSidecarServer.ts` | 上游 sidecar 测试 | 待派 |
| 轮次结束后再 `abort_turn`，上一轮还在跑的 advisory hook 基线会杀、Rust 不杀 | `runtime/turn/live.rs:806-817`；基线 `AgentSession.ts:279-282` | 审阅 | 在修 |
| 外部工具调用进行中用户中止，Rust 不打断那次 HTTP 请求 | `native_process/module_tools.rs:193-197`；基线 `hostToolPort.ts:192,244`、`HttpModuleClient.ts:107-113` | 审阅 | 在修 |
| 子代理工具运行时的 `warning` 事件没有转发到父会话流 | `native_process/subagents.rs:796`；基线 `ToolRuntime.ts:591` | 审阅 | 在修 |
| 非字符串 `workspaceCwd`：只在场景覆盖的两处模拟了基线的 TypeError，工具实际仍在项目根执行。场景没有工具调用所以看不到 | `host/modules/lifecycle.rs:130-132`、`session/factory.rs:1309-1344`、`native_process/assembly.rs:835-846` | 审阅 | 在修 |
| 「循环是否外部模块」有三套判定，`module-stdio-v2` 绑定指向 Rust 自己的 sidecar 且没设环境变量时 gateway 与 sidecar 判断相反 | `native_process/tools.rs:1065-1072`、`assembly.rs:1300-1309`、`pilotdeck-transport/src/factory_payload.rs:1018-1027` | 审阅 | 在修 |
| 进程本地时区的建模只对场景里的 11 个 `TZ` 值、只在 macOS 上有依据：`GMT` 的单点特判；POSIX 规则串不建模；`/etc/localtime` 是普通文件或多级链接；绝对路径 `TZ`；cron 缓存键用 Unicode 小写 | `pilotdeck-transport/src/system_clock.rs:220-310`、`runtime/cron/schedule.rs:161` | 审阅 | 待派（在 cb8 上量 Node） |
| 客户端只发 FIN 后，这一轮在第几帧写失败而中止取决于时序（基线约十来帧，Rust 约三四帧） | 无场景 | 断开后续跑 | 记为已知 |
| 显式 stdio 部署下 sidecar 在模块调用期间死掉；`module-stdio-v2` 绑定无场景；Rust sidecar 的 TCP 启动错误文案与基线不同；连接超时无场景 | — | sidecar TCP | 待派 |
| 项目根目录的祖先建不出来时的报错文案：基线 `EACCES: permission denied, mkdir '/private'`，Rust 报 `open '<root>'` | `api_web/files.rs` 的写入路径；无场景 | Linux 差异 | 待派 |
| 全局开启记忆时子代理 `memory: "disabled"` 不挂记忆、`seed_read_state` 带子代理字符串形式 MCP 引用：没有场景 | `native_process/sdk_config.rs`、`subagents.rs` | 表面缺口 | 待派 |
| 记忆维护的实际抽取请求没有场景 | `runtime/memory/maintenance.rs` | 循环缺口 / 审阅 | 待派 |
| 表面清单里没做完的：`agent_execution_rejected`、`agent_turn_discarded`、`permission_failed`、`agent_transcript_error`（后者判断做不到） | `coverage.mjs --surface` | 表面缺口 | 待派 |

## 4 时序与外观

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| 被取消的 hook 的完成帧与截断的 assistant_block 的先后：基线 40 次固定，Rust 40 次里 14 次相反 | `hooks.gen.mjs` 的统计注释 | 断开后续跑 | 在修 |
| `complete()` 重试时基线向 stderr 打一行，Rust 不打 | 基线 `streamModel.ts:108-111` | 审阅 | 在修 |
| Rust sidecar 每帧单独写，Node 把同一轮里的几次写合并成一块；TS 宿主对一次读到几帧敏感，上游对拍工具里 14 个稳定场景因此记录先后不同（帧内容一致） | `scripts/upstream-tests` 的 parity 组 | 上游 sidecar 测试 | 记为已知 |
| 性能待查（调试构建）：`submit_turn` 约 300ms 对基线约 47ms；空闲 CPU 约 3% 对 0%；扩展监视在 3 万个文件的树下空闲 5.4% 对 1.9% | 审阅修复的测量脚本 | 审阅修复 | 性能报告时用发布构建重量 |

## 尺子本身的问题

| 问题 | 来源 | 状态 |
|---|---|---|
| cb8 上并行跑全量时每轮 5–9 个场景偶发不一致，单独重跑一致（常见：`gap-tool-glob`、`gap-tool-grep`、`gap-permissions-json-rules`、`robust-large-message`、`loop-parallel-tools`、`loop-multi-tool`）。至少一部分的根因是上面的 `Keep-Alive` 差异 | cb8 验收 | 在查 |
| `hook-*-close-session`、`hook-*-server-stop` 四个场景也用了 `term.mjs` 延迟退出的包装，没有去掉包装的测量 | 审阅 | 在修（随 hook 帧次序） |
| `turn-steer-terminal` 把插话触发点从 `model_request_started` 挪到 `model_selection_changed`，正好绕开 Rust 经 sidecar 一跳落盘的次序不确定 | 审阅 | 在修（量两侧） |
| 范围偏宽的放过：四个记忆场景放过整段 `state_json`；`hook-cancelled` 的 `accepted` 放过会话文件第 20–29 条和整份 `projections.json`；TCP 场景整值放过 `moduleInstanceId`（证明不了「新实例」）；子代理场景的用量正则只认字段名；`always-on-fire-git` 一次略过 256 处 | 审阅 | 在修（收窄） |
| `scripts/upstream-tests/run.mjs` 的缺省运行包含 parity 组，而它在 Rust 上按设计有 14 个不过，所以退出码不能反映「计入的全过」；也还没在 Linux 上跑过 | 主会话 | 待派 |
| 耗时（`durationMs`）的位数漏进会话文件大小：`fileSize`、`resultBytes`、`content-length` 随之差几个字节。`gap-tool-grep` / `gap-tool-glob` 已各自声明 `volatile`，`gap-permissions-json-rules` 基线自比稳定所以不能声明、在慢机器上稳定差 11 字节。要在比较器层面定一条通用规则 | Linux 差异 | 待定规则（主会话） |
| 基线停机偶发退出码 1（`gap-tool-grep` 自比 3 次里 2 次，疑似停机时触发未捕获异常的处理，`cli/pilotdeck.ts:266-274`）：频率和条件没量清，是否登记为基线 bug 未定 | Linux 差异 | 待量 |
| Rust 调试构建的一轮比基线慢 4–8 倍（压缩一步约 0.5–0.8 秒），时间余量紧的场景在慢机器上会翻红 | Linux 差异 | 性能报告时看发布构建 |
| `surface.json` 标为「不计」的项（第一组 28 项，第二组又有若干：TUI 本地命令、QQ 前缀、做不到的错误码等），理由是子代理写的，抽查五项成立；整张表要人过目（交付标准第 2 条） | 审阅 | 待整理 |
| 约 35 个没有 `volatile` / `accepted` 的新场景，没有逐条核对说明与步骤是否对得上 | 审阅 | 待派 |

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
