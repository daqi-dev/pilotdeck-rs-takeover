# 不计入覆盖的表面项（交付标准第 2 条，待人过目）

> 本文件由 `node scripts/bbx/status.mjs` 生成，不要手改。理由来自 `scripts/bbx/surface.json` 各项的 `skip` 字段；要改理由或改成计入，改那里再重新生成。

- 生成时间：2026-11-14T04:50:44Z
- 仓库提交：`320552a46`

共 29 项，其中没有理由的 0 项。

## 没有理由的

（无）

## `loop` 对话主干与 agent 循环（9）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| loop-behaviors | `loop:agentError:agent_model_capability_error` | `loop.continuation` | src/agent/protocol/errors.ts:7 | 基线没有产生它的代码路径：AgentErrorCode 联合里的取值（src/agent/protocol/errors.ts:7），全仓库没有任何 agentError("agent_model_capability_error", …) 或同名字面量的写出点（grep 实查）。 |
| loop-behaviors | `loop:agentError:agent_tool_result_pairing_failed` | `loop.continuation` | src/agent/protocol/errors.ts:10 | 基线没有产生它的代码路径：AgentErrorCode 联合里的取值（src/agent/protocol/errors.ts:10），ensureToolResultPairing 只做修补不报这个码，全仓库没有它的写出点（grep 实查）。 |
| loop-behaviors | `loop:stopReason:unsupported_recovery` | `loop.continuation` | src/agent/protocol/result.ts:20 | 基线没有产生它的代码路径：AgentStopReason 联合里的取值（src/agent/protocol/result.ts:20），全仓库除这一处类型声明外没有任何地方写出 "unsupported_recovery"（grep 实查）。 |
| loop-behaviors | `loop:turn_continued:max_turns` | `loop.continuation` | src/agent/protocol/state.ts:7 | 基线没有产生它的代码路径：AgentLoopTransitionReason 联合成员（src/agent/protocol/state.ts:4-11），AgentLoop 发出的 turn_continued 只用 next_turn、model_error、auto_compact、user_steer 四种（src/agent/loop/AgentLoop.ts 全部 type:"turn_continued" 处 grep 实查），max_turns 在 max_turns / 中止路径上是 stopReason，不是转移原因。 |
| events | `event:assistant_stream_end` | `loop.events` | src/gateway/protocol/types.ts:588 | 基线没有产生它的代码路径：GatewayEvent 联合成员（src/gateway/protocol/types.ts:588），全仓库没有任何地方发出 type:"assistant_stream_end"（grep 只有这一处类型声明）。 |
| events | `agent_status:model_recovery_failed` | `loop.recovery` | src/gateway/client/GatewayAgentEventProjector.ts:254 | 基线没有产生它的代码路径：投影器有这个 case（src/gateway/client/GatewayAgentEventProjector.ts:251-262），但 AgentEvent 的 model_recovery_failed（src/agent/protocol/events.ts:102）在全仓库没有任何发出点（grep type: "model_recovery_failed" 只有类型声明与投影 case）。 |
| loop-behaviors | `loop:agentError:agent_aborted` | `loop.steer-abort` | src/agent/protocol/errors.ts:2 | 基线没有产生它的代码路径：只有投影器的 session_aborted 分支写出这个码（src/gateway/client/GatewayAgentEventProjector.ts:263-269），而 AgentEvent session_aborted（src/agent/protocol/events.ts:149）在全仓库没有发出点（grep 只有类型、投影、遥测与 InProcessGateway.ts:1643 的读取）；TurnRunner.ts:476 只是按这个码选 stopReason。 |
| loop-behaviors | `loop:turn_continued:aborted_streaming` | `loop.steer-abort` | src/agent/protocol/state.ts:8 | 基线没有产生它的代码路径：AgentLoopTransitionReason 联合成员（src/agent/protocol/state.ts:4-11），AgentLoop 发出的 turn_continued 只用 next_turn、model_error、auto_compact、user_steer 四种（src/agent/loop/AgentLoop.ts 全部 type:"turn_continued" 处 grep 实查），aborted_streaming 在 max_turns / 中止路径上是 stopReason，不是转移原因。 |
| loop-behaviors | `loop:turn_continued:aborted_tools` | `loop.steer-abort` | src/agent/protocol/state.ts:9 | 基线没有产生它的代码路径：AgentLoopTransitionReason 联合成员（src/agent/protocol/state.ts:4-11），AgentLoop 发出的 turn_continued 只用 next_turn、model_error、auto_compact、user_steer 四种（src/agent/loop/AgentLoop.ts 全部 type:"turn_continued" 处 grep 实查），aborted_tools 在 max_turns / 中止路径上是 stopReason，不是转移原因。 |

## `tools` 工具（1）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `tmp:pilotdeck_execution_*/` | `tools.execute-code` | src/tool/execution-world/NodeExecutionWorkspacePort.ts:11 | CLI / server 里走不到：缺省前缀 pilotdeck_execution_ 只在调用方不给 prefix 时用（src/tool/execution-world/NodeExecutionWorkspacePort.ts:11）；唯一的调用方 execute_code 总是传 pilotdeck_execute_code_（src/tool/builtin/executeCode.ts:424-425），那个目录由 tool-execute-code* 场景覆盖（diff.mjs 第 7 条归一化它的随机后缀）。 |

## `session` 会话存储、分叉、恢复、导出（6）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| events | `event:worktree_created` | `session.lifecycle` | src/gateway/protocol/types.ts:738 | GatewayEvent 联合里的成员（src/gateway/protocol/types.ts:738），基线没有任何地方以事件帧发出；worktree 创建只走同名通知（notification:worktree_created，src/cli/pilotdeck.ts:76-78 deferredBroadcast）。 |
| events | `event:worktree_removed` | `session.lifecycle` | src/gateway/protocol/types.ts:739 | GatewayEvent 联合里的成员（src/gateway/protocol/types.ts:739），基线没有任何地方以事件帧发出；worktree 删除只走同名通知（notification:worktree_removed，src/cli/pilotdeck.ts:79-81）。 |
| events | `notification:worktree_removed` | `session.lifecycle` | src/cli/pilotdeck.ts:80 | 基线没有产生它的代码路径：只在 GitWorktreeProvider.dispose 里广播（src/always-on/workspace/GitWorktreeProvider.ts:86-88 onWorktreeRemoved → src/cli/pilotdeck.ts:79-81），而 WorkspaceProvider 的 dispose 在全仓库没有调用方（grep "dispose(" 只有各 provider 的定义），工作区建好后一直保留。always-on-fire-git 证明 worktree_created 会发、工作区留在 always-on/worktrees 下。 |
| files | `session-root:.pilotdeck/control/` | `session.lifecycle` | src/cli/ProjectRuntimeResourcesBundle.ts:319 | CLI / server 里走不到：只在配置了 sessionExecutionProvider 与 sessionExecutionStorageRoot 时建（src/cli/ProjectRuntimeResourcesBundle.ts:276-331），这两个选项只能由嵌入方传给 createLocalGateway，pilotdeck CLI 的任何入口都不传（grep sessionExecutionProvider 只有 createLocalGateway / ProjectRuntime* / LocalGateway* 的转发）。 |
| files | `home:projects/<projectId>/chats/<tmp>` | `session.transcript` | src/session/fork/NodeProjectSessionForkPort.ts:187 | 只在一次调用内部存在：fork / restore / replace 先写 .<id>.<uuid>.fork.tmp 等临时文件再原子改名（src/session/fork/NodeProjectSessionForkPort.ts:187 起），调用返回时已不存在；session-fork* / session-restore / session-replace-* 场景比较结束后的整棵树，Rust 若留下临时文件会以多出的文件报差异，所以「不留下」已经在比。文件名含随机 UUID，中途也无法稳定观察。 |
| records | `transcript:session_metadata.staffDeckAdmission` | `session.transcript` | src/session/transcript/TranscriptEntry.ts:415 | 不在范围：只由 StaffDeck 的会话准入写出（src/cli/ProjectSessionFactory.ts:138 sessionAdmission），StaffDeck 不做（2026-11-12 人定）。 |

## `context` 上下文、压缩与缓存（2）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `tmp:pilotdeck-tool-results/` | `context.budget` | src/gateway/client/GatewayToolResultArtifactStore.ts:32 | CLI / server 里走不到：缺省根 tmpdir()/pilotdeck-tool-results 只在不给 rootDir 时用（src/gateway/client/GatewayToolResultArtifactStore.ts:32）。createLocalGateway 总是传 tmpdir()/pilotdeck-tool-output/<pid>（src/cli/createLocalGateway.ts:924-926，即 tmp:pilotdeck-tool-output 一项）；不传的只有测试用的 mapAgentEvent 门面（GatewayAgentEventProjector.ts:50-70）和没有调用方的 createGateway（src/gateway/Gateway.ts:45-70）。 |
| records | `transcript:control_boundary.microcompact_boundary` | `context.compaction` | src/session/transcript/TranscriptEntry.ts:404 | 基线没有产生它的代码路径：control_boundary 的 subtype 联合成员（src/session/transcript/TranscriptEntry.ts:404），全仓库没有写出 microcompact_boundary 的地方（grep 实查，微压缩不落边界记录），也没有读取方按它分支。 |

## `ext` 插件、skills、MCP（1）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `home:playwright-browsers/` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:51 | 做不到（需要真实外部服务）：browser-use 插件安装脚本下载浏览器的目录（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:51），安装要联网下载。 |

## `memory` 记忆（1）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `other:~/.edgeclaw/memory` | `memory.config` | src/context/memory/edgeclaw-memory-core/src/service.ts:170 | CLI / server 里走不到：记忆核心只在 rootDir 为空时才用 ~/.edgeclaw/memory（src/context/memory/edgeclaw-memory-core/src/service.ts:169-171、619），唯一的构造点 src/context/memory/createEdgeClawMemoryProviderFromConfig.ts:50-57 传 cfg.rootDir，而 src/pilot/config/parseMemoryConfig.ts:71 总给它缺省值，配置写空串时 230-237 直接报 CONFIG_MEMORY_VALUE_INVALID。 |

## `config` 设置、配置、热重载（2）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `home:runtime/<purpose>/` | `config.env-paths` | src/pilot/writablePaths.ts:75 | CLI / server 里走不到：回落目录只由 ensureWritableDirectory 在首选目录不可写时用（src/pilot/writablePaths.ts:20-34、75-86）。两个调用方：计划文件管理器只在传了 pilotHome 且没传 storage 时才走它（src/tool/builtin/planFile.ts:26-46），唯一的构造点 src/cli/SessionPlanTodoBundle.ts:38-41 两样都不满足（缺省会话的计划目录实测是 <项目根>/.pilotdeck/plans）；resolveBrowserUseOutputDir（src/cli/createLocalGateway.ts:555-567）没有调用方，browser-use 实际用 BrowserUseSessionMcpSpecPreparer 的 <项目根>/.pilotdeck/browser_screenshots，建不了目录时整轮失败、不回落。 |
| events | `event:config_changed` | `config.hot-reload` | src/gateway/protocol/types.ts:737 | GatewayEvent 联合里的成员（src/gateway/protocol/types.ts:737），基线没有任何地方以事件帧发出 type:"config_changed"；配置变化只走同名通知（notification:config_changed，src/cli/GatewayRuntimeRefreshBundle.ts:102）。 |

## `modules` 外部能力模块与 sidecar 部署（4）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `home:auth.db` | `modules.sop` | src/cli/createLocalGateway.ts:778 | 不在范围：ui/server（阶段 B）的认证数据库；gateway 只为 StaffDeck 副本校验读其中的安装所有者（src/cli/createLocalGateway.ts:778）。 |
| files | `home:sop/` | `modules.sop` | src/pilot/config/parseModulesConfig.ts:331 | 不在范围：SOP 模块（StaffDeck 工作流）的状态根，只在 modules.sop 配齐 endpoint / definitionsPath / defaultSopId 时出现（src/pilot/config/parseModulesConfig.ts:324-331），由 src/sop/staffdeck/ 下的 SopAgentLoop / StaffDeckSopControlPlane 使用，开启需要 StaffDeck 外部服务；StaffDeck 不做（2026-11-12 人定）。sop-disabled 等场景比的是模块未开启时的表现。 |
| files | `home:staffdeck/project-bindings.json` | `modules.sop` | src/pilot/staffdeck/projectBinding.ts:18 | 不在范围：StaffDeck 项目绑定存储（src/pilot/staffdeck/projectBinding.ts:18），StaffDeck 不做（2026-11-12 人定）。 |
| module-behaviors | `mod:sop.staffdeck-authority` | `modules.sop` | src/composition/nativeSopAuthority.ts | 不在范围：StaffDeck 不做（specs/delivery/README.md「不在范围」；2026-11-12 人定），只要求缺省与未开启时的表现，后者由 sop-disabled / cfg-modules-binding 覆盖。 |

## `channels` 渠道（只要求启动表现）（3）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| events | `agent_status:channel_submit_failed` | `channels.startup` | src/adapters/channel/feishu/FeishuChannel.ts:630 | 不在范围：只由飞书 / 微信等 IM 渠道的生产实现发出（src/adapters/channel/feishu/FeishuChannel.ts:630），触发需要渠道真实收到消息；渠道只对齐启动表现（2026-11-12 人定）。 |
| files | `home:im-attachments/` | `channels.startup` | src/adapters/channel/feishu/FeishuChannel.ts:167 | 不在范围：飞书 / 微信渠道收到附件时的落盘目录（src/adapters/channel/feishu/FeishuChannel.ts:167），要渠道真实收到消息。 |
| files | `home:matrix-bot-storage.json` | `channels.startup` | src/adapters/channel/matrix/MatrixChannel.ts:54 | 不在范围：Matrix 渠道生产实现的同步状态（src/adapters/channel/matrix/MatrixChannel.ts:54），要连上真实 Matrix 服务器才会写。 |
