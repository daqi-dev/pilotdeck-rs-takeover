# 问题台账（`dev/takeover`）

分级见 `specs/delivery/README.md`。只记还开着的；修掉并在 cb8 上验证后删行（历史在 git 里）。「来源」里的「审阅」指 2026-11-13 的独立代码审阅（只读、没有运行，结论是线索，修之前先复现）。

最后更新：2026-11-13

## 1 安全与数据

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| 半开连接（对端只发 FIN）永久占用连接名额；Rust 独有的 64 条上限攒满后拒绝一切新连接（含 HTTP）。基线不处理 FIN 但没有上限 | `native_ws.rs` 的 `peer_eof`、`MAX_CONNECTIONS`；`caps_tests.rs` 里 FIN 后名额去向已没有测试 | 审阅 | 在修 |

## 2 主路径

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| 扩展目录监视每 50ms 在异步线程上阻塞遍历并持锁；`registry.resolve` 要等这把锁。插件目录里有大树时占满一个核、拖慢所有请求 | `runtime/extension_watch.rs`、`runtime/project.rs` 的 `on_project_activated` | 审阅 | 在修 |
| 被监视的路径本身是符号链接时不往下走，热更新可能静默失效（基线未实测） | `extension_watch.rs` 用 `symlink_metadata` | 审阅 | 在修 |
| 模型请求的连接池不理会服务端的 `Keep-Alive` 提示：空闲连接被服务端关掉时多一次重试，基线（undici）不会 | `runtime/model/http.rs`；基线 `undici/lib/dispatcher/client-h1.js:644-658` | cb8 上的 Linux 差异子代理 | 在修（未提交） |
| 请求发出前中止不发 `turn_aborted` 状态；压缩进行中中止时摘要请求不取消 | 场景 `turn-abort-before-request-status`、`compact-auto-aborted` | 表面缺口 | 在修 |
| `submit_turn` 带 `taskBudget` 直接回 `CAPABILITY_UNAVAILABLE`（存储层有、没接线） | 场景 `loop-budget` 的说明 | 表面缺口 | 在修 |
| Linux 上稳定不一致：`always-on-fire`、`always-on-fire-execfail`、`hook-sdk-file-changed`、`http-api-web`、`robust-restart-crash-leftovers`、`skills-session-snapshot-at-create` | cb8 全量对拍 | cb8 验收 | 在查 |

## 3 边角

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| 轮次结束后再 `abort_turn`，上一轮还在跑的 advisory hook 基线会杀、Rust 不杀 | `runtime/turn/live.rs:806-817`；基线 `AgentSession.ts:279-282` | 审阅 | 待派 |
| 外部工具调用进行中用户中止，Rust 不打断那次 HTTP 请求 | `native_process/module_tools.rs:193-197`；基线 `hostToolPort.ts:192,244`、`HttpModuleClient.ts:107-113` | 审阅 | 待派 |
| 子代理工具运行时的 `warning` 事件没有转发到父会话流 | `native_process/subagents.rs:796`；基线 `ToolRuntime.ts:591` | 审阅 | 待派 |
| 非字符串 `workspaceCwd`：只在场景覆盖的两处模拟了基线的 TypeError，工具实际仍在项目根执行。场景没有工具调用所以看不到 | `host/modules/lifecycle.rs:130-132`、`session/factory.rs:1309-1344`、`native_process/assembly.rs:835-846` | 审阅 | 待派 |
| 「循环是否外部模块」有三套判定，`module-stdio-v2` 绑定指向 Rust 自己的 sidecar 且没设环境变量时 gateway 与 sidecar 判断相反 | `native_process/tools.rs:1065-1072`、`assembly.rs:1300-1309`、`pilotdeck-transport/src/factory_payload.rs:1018-1027` | 审阅 | 待派 |
| 进程本地时区的建模只对场景里的 11 个 `TZ` 值、只在 macOS 上有依据：`GMT` 的单点特判；POSIX 规则串不建模；`/etc/localtime` 是普通文件或多级链接；绝对路径 `TZ`；cron 缓存键用 Unicode 小写 | `pilotdeck-transport/src/system_clock.rs:220-310`、`runtime/cron/schedule.rs:161` | 审阅 | 待派（在 cb8 上量 Node） |
| sidecar 的 TCP 监听：accept 持续出错时空转；连接无上限、无读超时 | `bin/pilotdeck-agent-loop-sidecar.rs:153-166` | 审阅 | 在修 |
| 客户端只发 FIN 后，这一轮在第几帧写失败而中止取决于时序（基线约十来帧，Rust 约三四帧） | 无场景 | 断开后续跑 | 记为已知 |
| 显式 stdio 部署下 sidecar 在模块调用期间死掉；`module-stdio-v2` 绑定无场景；Rust sidecar 的 TCP 启动错误文案与基线不同；连接超时无场景 | — | sidecar TCP | 待派 |
| 记忆维护的实际抽取请求没有场景 | `runtime/memory/maintenance.rs` | 循环缺口 / 审阅 | 待派 |
| 表面清单里没做完的：`agent_execution_rejected`、`agent_turn_discarded`、`permission_failed`、`agent_transcript_error`（后者判断做不到） | `coverage.mjs --surface` | 表面缺口 | 待派 |

## 4 时序与外观

| 问题 | 位置 / 依据 | 来源 | 状态 |
|---|---|---|---|
| 被取消的 hook 的完成帧与截断的 assistant_block 的先后：基线 40 次固定，Rust 40 次里 14 次相反 | `hooks.gen.mjs` 的统计注释 | 断开后续跑 | 在修 |
| `complete()` 重试时基线向 stderr 打一行，Rust 不打 | 基线 `streamModel.ts:108-111` | 审阅 | 待派 |

## 尺子本身的问题

| 问题 | 来源 | 状态 |
|---|---|---|
| cb8 上并行跑全量时每轮 5–9 个场景偶发不一致，单独重跑一致（常见：`gap-tool-glob`、`gap-tool-grep`、`gap-permissions-json-rules`、`robust-large-message`、`loop-parallel-tools`、`loop-multi-tool`）。至少一部分的根因是上面的 `Keep-Alive` 差异 | cb8 验收 | 在查 |
| `hook-*-close-session`、`hook-*-server-stop` 四个场景也用了 `term.mjs` 延迟退出的包装，没有去掉包装的测量 | 审阅 | 在修（随 hook 帧次序） |
| `turn-steer-terminal` 把插话触发点从 `model_request_started` 挪到 `model_selection_changed`，正好绕开 Rust 经 sidecar 一跳落盘的次序不确定 | 审阅 | 待派（量两侧） |
| 范围偏宽的放过：四个记忆场景放过整段 `state_json`；`hook-cancelled` 的 `accepted` 放过会话文件第 20–29 条和整份 `projections.json`；TCP 场景整值放过 `moduleInstanceId`（证明不了「新实例」）；子代理场景的用量正则只认字段名；`always-on-fire-git` 一次略过 256 处 | 审阅 | 待派（收窄） |
| `surface.json` 有 28 项标为「不计」，理由是子代理写的，抽查五项成立；整张表要人过目（交付标准第 2 条） | 审阅 | 待整理 |
| 约 35 个没有 `volatile` / `accepted` 的新场景，没有逐条核对说明与步骤是否对得上 | 审阅 | 待派 |

## 审阅读过、未发现问题的

外部工具模块不走宿主权限判定（与基线一样宽，含子代理会话）；`modules.agentLoop` 绑定；sidecar TCP 的监听地址与鉴权；`browser_use.rs` 的路径；权限询问与事件转发不跨会话；`read_file` 路径检查；hook 进程环境变量；`execute_code` 只读判定；Google 请求；记忆请求头；渠道放行条件。

审阅没来得及读的：`channel/tests.rs`、`runtime/memory/maintenance.rs`、`pilotdeck-agent/src/turn.rs` 与 `default_factory.rs` 的基线对照、`runtime/turn/timeline.rs`、`host/modules/context.rs`、`pilotdeck-cli/src/chat_search.rs`、`runtime/cron/migration.rs`、`skills/external.rs`、`tests/` 下的集成测试改动。

## 等人定的

- 冻结表里登记的旧对拍脚本（`scripts/gateway-parity`、`ui-server-parity`、`exp008`、`a5-19`）是否退役。
- BlueBubbles 渠道的生产实现（配了凭据会真的轮询，与基线一致）是否保留。
- StaffDeck 的审批接口与项目绑定（设了环境变量才生效）是否收缩。
- `cli-update-usage`：Rust 的安装要不要带网页更新脚本（发布构建阶段定）。
- 公开 agent 评测的两侧对比（人说之后再决定）。
