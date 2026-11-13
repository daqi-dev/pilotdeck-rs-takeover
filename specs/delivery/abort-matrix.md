# 中止点矩阵（gateway / 内置 agent 循环）

> 2026-11-13 首版。用途：「用户中止 / 关会话 / 停机打断正在进行的事」这一块的盲审依据。
> 基线 `cc7c4e6`（`resources/PilotDeck/src`），默认部署（进程内循环）；显式外部 sidecar 部署另见 `agent-loop-sidecar-*-abort`。
> 「一致」一列是本文件写成时在 macOS 上的对拍结论（`node scripts/bbx/check.mjs <场景>`，槽位 1）；以当次命令输出为准，不引用本文件。
> 场景都由 `scripts/bbx/scenarios/abort.gen.mjs` 生成（`abort-*`、`abort-timeout-*`），其余是已有场景。

## 1 列：中止的来源

| 列 | 基线做什么 | 依据 |
|---|---|---|
| A `abort_turn` | `AbortController.abort(reason)`，原因缺省 `aborted`、带 runId 时 `aborted:<runId>`；runId 不是当前轮则什么都不做。RPC 等这一轮完全收尾才回 `{ok:true}` | `gateway/client/InProcessGateway.ts:2000-2020`，`agent/session/AgentSession.ts:279-282` |
| B `close_session` | `router.close` → `session.dispose()` → `abort("session_closed")`，再 `interactionCoordinator.closeSession`。原因里没有 abort/cancel/timeout，所以**不发** `turn_aborted` 状态 | `InProcessGateway.ts:2049-2054`，`AgentSession.ts:274-277`，`agent/loop/AgentLoop.ts:3910-3913` |
| C 连接断开（关闭帧、RST） | 对本连接在途的每个会话 `abortTurn({sessionKey})`（原因 `aborted`，同 A），除非 `disconnectInteraction` 说有挂起的询问要保留这一轮（`preserveTurn`）。帧随连接丢失，只剩落盘与运行记录。只发 FIN 另见台账 | `gateway/server/GatewayWsConnection.ts:48-62` |
| D 进程收到 SIGTERM | `automation.stop` → `server.close()`（**等所有连接结束**，客户端不断开就一直等）→ `disposeGateway`（逐会话 dispose，等同 B）→ 退出。客户端断开时先走 C | `cli/pilotdeck.ts:257-280`，`cli/PilotDeckServerShutdownBundle.ts` |
| E 同会话再 `submit_turn` / 插话 | 再提交：`session_busy` 错误，正在跑的一轮不受影响。插话：进信箱，循环顶（`AgentLoop.ts:616-619`）或无工具调用的结尾（1866-1882）才应用；不是中止 | `InProcessGateway.ts:1050-1068`，`agent/session/SteerMailbox.ts` |
| F `submit_turn.timeoutMs` | 定时器到点：记 `turn_timeout` 状态、撤回挂起询问、`session.abort("timeout:<runId>")`、入队 `turn_timeout` 错误后**立刻关队列**；泵循环 `if (timedOut) break` 不再转发；`finally` 里 `void router.close()`、不等泵。客户端最终看到的是 `error: result_unknown`（关队列后入队失败），被中止的循环在后台继续收尾：它派发的 hook 照跑、事件仍追加进 `runs/registry.json`、会话记录在 router 关掉写入器之前照写 | `InProcessGateway.ts:1384-1439, 1587, 1824-1836` |

## 2 行：中止到达时在做什么，在途的事怎样对信号作出反应

共同规则（基线）：中止只是触发一个 `AbortSignal`。进程内循环只在固定检查点读它：循环顶（`AgentLoop.ts:597-614`，带 `turn_aborted` 状态）、建好请求后（680-698，带状态）、流抛错时（859-886，**不带**状态，安全的半截文本落盘）、流正常结束后（916-946，带状态）、工具批次前后（1932-1946、1971-1984，**不带**状态，工具结果不发 `tool_result`、不落盘）。两个检查点之间的代码照常跑完。在途的操作各自看信号：

- hook（`extension/hooks/execution/CommandHookExecutor.ts:37-44`，`tool/builtin/bash/commandRunner.ts:96-101`）：在跑的被 SIGTERM 进程组、3 秒后 SIGKILL，**等子进程 close** 再以 `cancelled` 结束；中止之后才派发的 hook，`abort` 监听装在已中止的信号上永远不触发，所以**照常跑完**，结果仍记为 `cancelled`。
- bash 等子进程：同上，被杀；已中止后才启动的工具在 `ToolRuntime.ts:85-87` 直接 `tool_aborted`（不跑 hook）。
- `PreModelRequest` / `InstructionsLoaded`：不等待（`AgentLoop.ts:699-702, 2281-2283`），被杀后的完成帧只在本轮收尾前 close 才上流（`gateway/client/GatewayTurnEventCoordinator.ts:38-50`）。

| # | 行 | 在途操作的反应（基线） |
|---|---|---|
| R1 | 轮次已受理、尚未开始（预检、配置刷新、取会话） | 网关自己的检查点直接 `turn_completed aborted_streaming`，没有 `turn_started`（`InProcessGateway.ts:1093-1190, 1247-1251, 1344, 1355, 1539`） |
| R2 | `UserPromptSubmit` hook 执行中 | hook 被杀、`cancelled`；`user_prompt_submitted`，循环顶检查 → `turn_aborted`（A）+ `aborted_streaming`（`agent/turn/TurnRunner.ts:229-241`） |
| R3 | `InstructionsLoaded` / `PreModelRequest` 执行中 | 不等待；hook 被杀；本轮按当时所处的位置收尾；完成帧见上 |
| R4 | 上下文准备中 | `createModelRequest` 之后的检查点（680-698） |
| R5 | 自动压缩中 | 摘要请求带信号被取消（`awaitCompactionWithEvents`），随后循环检查点 |
| R6 | 模型请求已发出、尚无响应头 | 流抛错分支（859-886）：没有半截消息，**不带** `turn_aborted` |
| R7 | 流式输出中（文本 / 推理 / 工具调用已到但结束原因未到） | 同 R6；安全的半截**文本**落盘并发 `assistant_message`，推理与工具调用丢弃 |
| R8 | `PreToolUse` hook 执行中 | hook 被杀，`cancelled` 不阻断；工具照常走权限并**在已中止的信号上执行完**（基线 bug，已登记在 `hook-cancelled`） |
| R9 | 权限询问未答 | 询问被撤回、工具不执行（`permission_denied` 帧），批次后检查点收尾 |
| R10 | 工具执行中 | 批次整体等完（`executeToolsWithEventPump`），各工具按信号收尾；失败的走 `PostToolUseFailure`（`isInterrupt` 只在错误码为 `tool_aborted` 时为真，`ToolRuntime.ts:418-423`）；然后批次后检查点 |
| R11 | `PostToolUse` / `PostToolUseFailure` 执行中 | hook 被杀，结果保留但被批次后检查点丢弃 |
| R12 | 并行 / 分组批次进行中（部分已完成） | 已完成的结果、正在跑的收尾都等；之后才开始的调用 `tool_aborted`；整批不发 `tool_result` |
| R13 | `Stop` / `StopFailure` 执行中 | hook 被杀；**之后没有检查点**：Stop 以 `completed` 结束，StopFailure 以原本的 `model_error` 结束（1677-1696, 1884-1929） |
| R14 | `SubagentStart` / `SubagentStop` 执行中 | hook 被杀；子代理按中止收尾（`subagent_completed aborted:true`），父轮批次后检查点（`agent/sub/OneShotSubagentPort.ts:139, 206`） |
| R15 | 轮次收尾（落盘、标题生成、记忆捕获） | 标题生成与本轮的信号只连到 `finalizeSessionMetadata`（`TurnRunner.ts:569, 640-648`），之后不受中止影响 |
| R16 | 续写、流中断恢复、空回复重试的间隙 | 下一次循环顶检查点（带 `turn_aborted`） |

## 3 矩阵

格内：基线表现要点 / 场景 / Rust 是否一致（✓ 一致，✗ 不一致，— 没有场景）。列 C、D 与 A 的区别只在「帧随连接丢失」与「停机先等连接断开」，按第 1 节的依据从 A 推出；表里只列有场景的格子。

### R1–R7：轮次开始到模型输出

| 行 | A abort_turn | B close_session | C 断开 | D SIGTERM | E 再提交 / 插话 | F 超时 |
|---|---|---|---|---|---|---|
| R1 受理未开始 | 无 turn_started、`aborted_streaming` / `turn-abort-before-request-status` / ✓ | 同 A，无状态 / — | 同 A / `robust-drop-close-frame-midturn`、`robust-drop-reset-midturn` ✓ | — | — | — |
| R2 UserPromptSubmit | hook cancelled、`turn_aborted`、aborted / `abort-hook-user-prompt` / ✓ | 同 A 但无 `turn_aborted` / 同 / ✓ | 同 A | — | — | 见第 4 节 / `abort-timeout-hook-user-prompt` / ✗ |
| R3 InstructionsLoaded / PreModelRequest | hook 被杀，完成帧在 truncated block 之后、终态之前 / `hook-*-cancelled` ✓；晚退出 50/300/900/2000 ms 都不上流 / `abort-hook-advisory-late-*` / ✓ | 同 A / `hook-*-close-session` / ✓ | — | 停机等断开，有挂起询问时保留这一轮，dispose 后被杀 / `hook-*-server-stop` / ✓ | — | — |
| R4 上下文准备 | 无法稳定命中（毫秒级，没有可观察的触发帧） / — | — | — | — | — | — |
| R5 自动压缩 | 摘要请求取消 / `compact-auto-aborted` ✓（手动：`compact-manual-aborted` ✓） | `compact-auto-close` ✓、`compact-manual-close` ✓ | — | — | — | — |
| R6 已发出、无响应头 | 无半截消息、无状态 / `abort-model-stream`（s-sh-run） / 帧与会话 ✓；4 轮里 1 轮 Rust 读历史时缺上一轮保留的流（未量 20 次） | 同 / s-sh-close / ✓ | 同 A / `robust-drop-close-frame-midturn` ✓ | — | — | / `abort-timeout-model-stream` / ✗ |
| R7 流式输出中 | 文本：半截落盘（`abort-mid-loop` b、c ✓）；推理：丢弃（s-sr-run ✓）；文本后工具调用已到：工具不执行、半截文本不落盘（s-sc-run ✓） | s-sr-close、s-sc-close、s-st-close / ✓ | — | — | — | s-st-timeout / ✗ |

### R8–R12：工具

| 行 | A abort_turn | B close_session | C 断开 | D SIGTERM | E 再提交 / 插话 | F 超时 |
|---|---|---|---|---|---|---|
| R8 PreToolUse | hook 被杀，工具仍执行（基线 bug） / `hook-cancelled`（accepted 只放过 ran.txt） / ✓ | 同 A / `abort-hook-pre-tool` / ✗（同一基线 bug，未登记，见第 5 节） | — | — | — | `abort-timeout-hook-pre-tool` / ✗ |
| R9 权限询问 | 撤回、工具不执行 / `params-abort-reason-snapshot` ✓ | 同 / `abort-permission-pending` / ✓ | 有挂起询问时保留这一轮 / `robust-drop-*-permission`、`robust-disconnect-permission-new-conn` ✓ | 同 C 后 dispose / `hook-*-server-stop` ✓ | 插话在工具之后应用；再提交 `session_busy` / `abort-permission-pending` ✓ | `abort-timeout-permission-pending` / ✗ |
| R10 bash | 子进程被杀（无 late 文件），PostToolUseFailure `isInterrupt:false`，无 `turn_aborted` / `abort-tool-bash`、`abort-mid-loop`、`turn-abort` / ✓ | 同 / `abort-tool-bash` / ✓ | 同 A / `abort-tool-bash-disconnect` / ✓ | 先断开再停机 / `server-stop-pending-turn` ✓ | 插话在工具后应用、再提交 `session_busy` / `abort-tool-bash`、`turn-steer` ✓ | 后台继续：子进程仍被杀 / `abort-timeout-tool-bash`、`robust-timeout-submit-turn` |
| R10 execute_code | 探测 python 时被中止 → 「non-Error value」失败 / `abort-tool-execute-code` / ✓ | 同 / ✓ | — | — | — | `abort-timeout-tool-execute-code` |
| R10 MCP | `notifications/cancelled` 带 `aborted:<runId>`；「MCP call timed out … -32001: aborted:<runId>」 / `abort-tool-mcp` / ✓ | 请求发出前已中止 →「MCP call failed …: unknown」 / 同 / ✓ | — | — | — | `abort-timeout-tool-mcp` |
| R10 web | 无可用的慢速本机端点（web_fetch 把 http 升到 https） / — | — | — | — | — | — |
| R10 外部能力模块 | HTTP 请求被打断 / `modules-tools-abort`、`modules-tools-batch-abort` ✓ | — | — | — | — | — |
| R10 `agent` 一次性子代理 | 子代理 aborted、PostToolUseFailure `isInterrupt:true`、SubagentStop 照跑 / `abort-tool-agent`、`hook-post-tool-use-interrupt`、`permission-subagent-abort` / ✓ | 同 / `abort-tool-agent` / ✓ | — | — | — | 子代理按自己的期限超时、hook 照跑 / `abort-timeout-tool-agent` / ✗ |
| R10 后台子代理 | 不在父轮的在途操作里（`subagent` 立即返回） / — | — | — | — | — | — |
| R10 `task_*` | `task_wait` 以「aborted before task … finished」失败、`isInterrupt:true` / `tool-aborted-running`、`hook-post-tool-use-interrupt` / ✓ | — | — | — | — | — |
| R11 PostToolUse / PostToolUseFailure | hook 被杀 / `abort-hook-post-tool`、`abort-hook-post-tool-failure` / ✓ | 同 / ✓ | — | — | — | `abort-timeout-hook-post-tool*` / ✗ |
| R12 批次部分完成 | read_file 已完成、bash 被杀、write_file `tool_aborted` / `abort-tool-batch` / ✓ | 同 / ✓ | — | — | — | `abort-timeout-tool-batch` |

### R13–R16：收尾

| 行 | A abort_turn | B close_session | C | D | E | F 超时 |
|---|---|---|---|---|---|---|
| R13 Stop | hook 被杀，`completed` / `abort-hook-stop` / ✓ | 同 / ✓ | — | — | — | `abort-timeout-hook-stop` / ✗ |
| R13 StopFailure | hook 被杀，`model_error` + `model_request_failed` / `abort-hook-stop-failure` / ✓ | 同 / ✓ | — | — | — | 超时落在 provider 重试里 / `abort-timeout-hook-stop-failure` / ✗ |
| R14 SubagentStart | hook 被杀，子代理不发模型请求，`subagent_started`/`subagent_completed` 照发 / `abort-hook-subagent-start` / 帧 ✓；会话记录次序偶发不同（4 轮里 1 轮，未量 20 次） | 同 / 同 | — | — | — | `abort-timeout-hook-subagent-start` / ✗ |
| R14 SubagentStop | hook 被杀，完成帧在 `subagent_started` 之前（事件泵次序） / `abort-hook-subagent-stop` / ✗（只差会话文件大小 1 字节：子代理耗时位数，台账「尺子」一行） | 同 / ✗（同上） | — | — | — | `abort-timeout-hook-subagent-stop` / ✗ |
| R15 标题生成 | 中止在回复完整之后到达，基线这时已经解绑，标题照常写入 / `abort-turn-finishing` / ✗（Rust 这一窗口更长，标题被取消、下一轮重生成） | 同 / ✗ | — | — | — | 不适用 |
| R16 续写 / 恢复间隙 | 循环顶检查点 / 无稳定触发帧 / — | — | — | — | — | — |

## 4 结构性原因与修法

1. **中止被当成 hook 绑定吊销**（已修，`1a2d2073b`）。Rust 的本轮 hook 端口一见中止就关掉：在途派发立刻回 stale、不等被杀的 hook 退出，之后的派发直接失败。表现：PostToolUseFailure 不跑（`hook-post-tool-use-interrupt`）、UserPromptSubmit 中止后缺 `turn_aborted`、SubagentStart/Stop 缺子代理事件、Stop 中止报错。修法：中止只经 kill 信号到达 hook，绑定留到本轮关闭端口时才吊销。
2. **循环在中止后任何端口调用都失败**（Stop / StopFailure 两段已修，`73a2eb9fc`）。基线只在检查点读信号，两个检查点之间照常执行；Rust 循环的每次事件投递与宿主调用都先查取消。已修 Stop 与 StopFailure 之后的收尾；其余「检查点之间」的段落（如 R4 上下文准备、R16 恢复间隙）没有稳定的触发帧，没有场景，未逐段审。
3. **工具中止时的原因文本**（已修，`ee7d2a041`）：基线 `throwIfAborted()` 抛出原因字符串本身，execute_code 因此是「non-Error value」而不是 `tool_aborted`，MCP 的取消通知与错误带本轮原因。
4. **advisory hook 的收尾窗口**（已修，`616c540da`、`c34ffeedf`）：Rust 自定的 1 秒等待改为从中止时刻起 25 毫秒，并在本轮释放后丢弃迟到的完成帧。各 20 次：`abort-hook-advisory-late-50` 基线 20/20、Rust 20/20 不上流；`hook-model-request-cancelled`（立即退出）两侧 20/20 上流；`abort-hook-subagent-stop` 帧次序两侧 20/20 相同。
5. **超时之后被中止的循环不再收尾**（未修）。基线超时只是停止转发并异步关会话，被中止的循环在后台跑完：它派发的 hook 照跑、事件仍写进 `runs/registry.json`、会话记录在写入器关闭前照写、子代理按自己的期限结束。Rust 超时时立刻关掉转录写入器与 hook 投影，这些副作用都没有。所有 `abort-timeout-*` 场景因此不一致。修法方向：超时后不立即 `close_transcript`，让被中止的执行按 A 列的路径收尾、只停止对客户端的投递；基线这一段本身依赖 `router.close` 与循环收尾谁先完成，修之前要先各自量 20 次。

## 5 已知不一致与待定

- `abort-hook-pre-tool`（B 列）：与 `hook-cancelled` 同一基线 bug（被中止杀掉的 PreToolUse 不阻断，工具在已中止的信号上照常执行）。没有新登记 accepted；是否给这个场景也登记由主会话定。
- `abort-hook-subagent-stop`：会话文件大小差 1 字节，来自子代理 `durationMs` 的位数（台账「耗时位数漏进会话文件大小」），比较器层面的通用规则未定。
- `abort-turn-finishing`：时序窗口差异（Rust 一轮更慢，中止落在解绑标题生成之前）。
- 所有 `abort-timeout-*`：第 4 节第 5 条。
- 没有场景的格子：R4、R16（没有可观察的触发帧）；web 工具（采集器没有慢速本机 HTTPS 端点）；后台子代理（不在父轮的在途操作里）；工具调用参数传到一半（假 provider 一块发出完整参数，需要采集器支持分块的工具参数）。
