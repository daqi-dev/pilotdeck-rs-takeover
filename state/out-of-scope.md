# 不计入覆盖的表面项（交付标准第 2 条，待人过目）

> 本文件由 `node scripts/bbx/status.mjs` 生成，不要手改。理由来自 `scripts/bbx/surface.json` 各项的 `skip` 字段；要改理由或改成计入，改那里再重新生成。

- 生成时间：2026-11-14T09:25:27Z
- 仓库提交：`7b75a0851`

共 109 项，其中没有理由的 0 项。

## 没有理由的

（无）

## `loop` 对话主干与 agent 循环（10）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| loop-behaviors | `loop:agentError:agent_model_capability_error` | `loop.continuation` | src/agent/protocol/errors.ts:7 | 基线没有产生它的代码路径：AgentErrorCode 联合里的取值（src/agent/protocol/errors.ts:7），全仓库没有任何 agentError("agent_model_capability_error", …) 或同名字面量的写出点（grep 实查）。 |
| loop-behaviors | `loop:agentError:agent_tool_result_pairing_failed` | `loop.continuation` | src/agent/protocol/errors.ts:10 | 基线没有产生它的代码路径：AgentErrorCode 联合里的取值（src/agent/protocol/errors.ts:10），ensureToolResultPairing 只做修补不报这个码，全仓库没有它的写出点（grep 实查）。 |
| loop-behaviors | `loop:agentError:agent_transcript_error` | `loop.continuation` | src/agent/protocol/errors.ts:11 | 量不了：要让记录 accepted input 那一步写盘失败。基线写 turn_started 与写下一条记录之间没有任何外部步骤可插入，两次写同一个文件（src/gateway 的 TurnRunner.ts:148-205）；把文件设只读则基线在 turn_started 就以 gateway_submit_failed 失败，到不了这里；限制文件大小只能套在基线的 node 外、套不到 Rust 二进制上，两侧条件不对等。Rust 侧对应分支未经对拍验证，列给人过目。 |
| loop-behaviors | `loop:stopReason:unsupported_recovery` | `loop.continuation` | src/agent/protocol/result.ts:20 | 基线没有产生它的代码路径：AgentStopReason 联合里的取值（src/agent/protocol/result.ts:20），全仓库除这一处类型声明外没有任何地方写出 "unsupported_recovery"（grep 实查）。 |
| loop-behaviors | `loop:turn_continued:max_turns` | `loop.continuation` | src/agent/protocol/state.ts:7 | 基线没有产生它的代码路径：AgentLoopTransitionReason 联合成员（src/agent/protocol/state.ts:4-11），AgentLoop 发出的 turn_continued 只用 next_turn、model_error、auto_compact、user_steer 四种（src/agent/loop/AgentLoop.ts 全部 type:"turn_continued" 处 grep 实查），max_turns 在 max_turns / 中止路径上是 stopReason，不是转移原因。 |
| events | `event:assistant_stream_end` | `loop.events` | src/gateway/protocol/types.ts:588 | 基线没有产生它的代码路径：GatewayEvent 联合成员（src/gateway/protocol/types.ts:588），全仓库没有任何地方发出 type:"assistant_stream_end"（grep 只有这一处类型声明）。 |
| events | `agent_status:model_recovery_failed` | `loop.recovery` | src/gateway/client/GatewayAgentEventProjector.ts:254 | 基线没有产生它的代码路径：投影器有这个 case（src/gateway/client/GatewayAgentEventProjector.ts:251-262），但 AgentEvent 的 model_recovery_failed（src/agent/protocol/events.ts:102）在全仓库没有任何发出点（grep type: "model_recovery_failed" 只有类型声明与投影 case）。 |
| loop-behaviors | `loop:agentError:agent_aborted` | `loop.steer-abort` | src/agent/protocol/errors.ts:2 | 基线没有产生它的代码路径：只有投影器的 session_aborted 分支写出这个码（src/gateway/client/GatewayAgentEventProjector.ts:263-269），而 AgentEvent session_aborted（src/agent/protocol/events.ts:149）在全仓库没有发出点（grep 只有类型、投影、遥测与 InProcessGateway.ts:1643 的读取）；TurnRunner.ts:476 只是按这个码选 stopReason。 |
| loop-behaviors | `loop:turn_continued:aborted_streaming` | `loop.steer-abort` | src/agent/protocol/state.ts:8 | 基线没有产生它的代码路径：AgentLoopTransitionReason 联合成员（src/agent/protocol/state.ts:4-11），AgentLoop 发出的 turn_continued 只用 next_turn、model_error、auto_compact、user_steer 四种（src/agent/loop/AgentLoop.ts 全部 type:"turn_continued" 处 grep 实查），aborted_streaming 在 max_turns / 中止路径上是 stopReason，不是转移原因。 |
| loop-behaviors | `loop:turn_continued:aborted_tools` | `loop.steer-abort` | src/agent/protocol/state.ts:9 | 基线没有产生它的代码路径：AgentLoopTransitionReason 联合成员（src/agent/protocol/state.ts:4-11），AgentLoop 发出的 turn_continued 只用 next_turn、model_error、auto_compact、user_steer 四种（src/agent/loop/AgentLoop.ts 全部 type:"turn_continued" 处 grep 实查），aborted_tools 在 max_turns / 中止路径上是 stopReason，不是转移原因。 |

## `model` 模型协议与路由（1）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| env | `env:STAFFDECK_SOP_MANAGEMENT_API_KEY` | `model.catalog` | src/pilot/staffdeck/projectBinding.ts:245 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/pilot/staffdeck/projectBinding.ts:245），要连 StaffDeck 服务。 |

## `tools` 工具（6）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| env | `env:PILOTDECK_RPC_PORT` | `tools.execute-code` | src/tool/builtin/executeCode.ts:818 | 交付平台是 macOS 与 Linux（specs/delivery/README.md 第 1 条）；基线只在 win32 上给 execute_code 用 TCP 传输（src/tool/execution-world/ExecutionTransportPort.ts:33），这个变量只在 TCP 分支里设（src/tool/builtin/executeCode.ts:814-821），其余平台走 UDS、设的是 PILOTDECK_RPC_SOCKET。 |
| env | `env:PILOTDECK_RPC_TOKEN` | `tools.execute-code` | src/tool/builtin/executeCode.ts:819 | 交付平台是 macOS 与 Linux（specs/delivery/README.md 第 1 条）；基线只在 win32 上给 execute_code 用 TCP 传输（src/tool/execution-world/ExecutionTransportPort.ts:33），这个变量只在 TCP 分支里设（src/tool/builtin/executeCode.ts:814-821），其余平台走 UDS、设的是 PILOTDECK_RPC_SOCKET。 |
| files | `tmp:pilotdeck_execution_*/` | `tools.execute-code` | src/tool/execution-world/NodeExecutionWorkspacePort.ts:11 | CLI / server 里走不到：缺省前缀 pilotdeck_execution_ 只在调用方不给 prefix 时用（src/tool/execution-world/NodeExecutionWorkspacePort.ts:11）；唯一的调用方 execute_code 总是传 pilotdeck_execute_code_（src/tool/builtin/executeCode.ts:424-425），那个目录由 tool-execute-code* 场景覆盖（diff.mjs 第 7 条归一化它的随机后缀）。 |
| tool-params | `execute_code.error:invalid_rpc_token` | `tools.execute-code` | src/tool/builtin/executeCode.ts:595 | 交付平台是 macOS 与 Linux（specs/delivery/README.md 第 1 条）；只有 TCP 传输才校验令牌（src/tool/builtin/executeCode.ts:462 expectedToken 只在 transport.kind === "tcp" 时给，:595-596 才会回 invalid_rpc_token），而 TCP 传输只在 win32 上用（src/tool/execution-world/ExecutionTransportPort.ts:33）。 |
| env | `env:LD_LIBRARY_PATH` | `tools.shell` | src/sandbox/nsjail/NsjailProvider.ts:496 | 基线里到不了：只在 NsjailProvider 里读（src/sandbox/nsjail/NsjailProvider.ts:496），而基线 src 下除测试外没有任何地方构造 NsjailProvider（grep `new NsjailProvider` 在非测试代码里 0 处），server、CLI、sidecar 三个入口都走不到。2026-11-14 主会话核对。 |
| tool-params | `web_fetch.error:tool_aborted` | `tools.web` | src/tool/builtin/webFetch.ts:401 | 基线里到不了：web_fetch 在请求进行中被中止时，基线走的是 tool_execution_failed 而不是 tool_aborted（场景 abort-tool-web-fetch 用本机 HTTPS 服务实测）；清单里这一项是按源码里的错误码枚举出来的，实际没有输出会带它。 |

## `perm` 权限、沙箱、路径检查（2）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| records | `transcript:permission_failed` | `perm.decision` | src/session/transcript/TranscriptEntry.ts:47 | 量不了：要让权限判定本身抛错才会写这条记录。试过在权限询问期间中止、回非法的决定、断开连接，都走的是别的分支（permission_cancelled / deny），从对拍装置外部触发不出来。Rust 侧有没有对应分支未经对拍验证，列给人过目。 |
| tool-params | `*.error:permission_cancelled` | `perm.decision` | src/tool/execution/ToolRuntime.ts:346 | 基线里到不了：只在权限判定返回 cancel 时给出（src/tool/execution/ToolRuntime.ts:345-346；modules 的 src/agent/modules/capability/hostToolPort.ts:330 同源）。唯一产生 cancel 的是 src/interaction/ProfiledPermissionDecisionPort.ts:44-48，取决于交互策略；gateway 用的是缺省策略（src/cli/SessionInteractionBundle.ts:111、src/agent/session/AgentSessionScopeBundle.ts:55），它对 permission 一类的回落是 deny，只有 question 才回落 cancel（src/interaction/InteractionPolicy.ts:35）。 |

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

## `agents` 子代理与后台任务（1）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| tool-params | `agent.error:subagent_launch_failed` | `agents.oneshot` | src/tool/builtin/agent.ts:492 | 基线里到不了：只在 fork.launchBackground 抛错时给出（src/tool/builtin/agent.ts:479-494）。launchBackground（src/agent/sub/OneShotSubagentPort.ts:77-91）唯一会抛的是「定义不是 background」，而 agent.ts:391 已经按 isBackgroundDefinition 筛过；它调的 gateway 侧 launch（src/cli/ProjectSessionRuntimeBundle.ts:435-443）是 BackgroundSubagentRuntime.start（src/agent/sub/BackgroundSubagentRuntime.ts:45-75），同步建任务、run 放进 promise 链，不抛错。 |

## `ext` 插件、skills、MCP（12）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| env | `env:USERPROFILE` | `ext.mcp` | src/mcp/config/expandPlaceholders.ts:12 | 量不了：只在没有 HOME 时作为 ${userHome} 占位符的兜底（src/mcp/config/expandPlaceholders.ts:12：process.env.HOME ?? process.env.USERPROFILE ?? homedir()）；要让它生效就得去掉被测进程的 HOME，而去掉 HOME 后基线与 Rust 会碰到本机真实的 home 目录（~/.pilotdeck），这是对拍装置不许碰的。scripts/install-asr.mjs:23 的读取同属独立脚本，不经过 gateway / CLI / sidecar。（主会话 2026-11-14 裁决） |
| env | `env:APPDATA` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:50 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:50-51），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| env | `env:LOCALAPPDATA` | `ext.plugins` | src/runtime/commandShell.ts:78 | 交付平台是 macOS 与 Linux（specs/delivery/README.md 第 1 条）；只在 resolveWindowsGitBash 里拼 Git Bash 候选路径（src/runtime/commandShell.ts:78），非 win32 平台在 :46-50 已直接用 /bin/bash 返回，走不到这里。 |
| env | `env:PILOTDECK_DESKTOP_DOWNLOAD_MIRROR` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:80 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:80），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| env | `env:PILOTDECK_DESKTOP_PLAYWRIGHT_ARCHIVE_DIR` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:77 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:77、183），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| env | `env:PILOTDECK_DESKTOP_PLAYWRIGHT_BROWSER_SET` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:84 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:84），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| env | `env:PILOTDECK_DESKTOP_PLAYWRIGHT_CHROME_BASE_URL` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:109 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:109），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| env | `env:PILOTDECK_DESKTOP_PLAYWRIGHT_MIRROR` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:78 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:78），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| env | `env:PILOTDECK_SESSION_BROWSER_PROXY` | `ext.plugins` | src/sandbox/egress/SessionEgressProxy.ts:283 | 基线里到不了：只在会话出口代理里读（src/sandbox/egress/SessionEgressProxy.ts:283），出口代理只由 NsjailProvider 创建（NsjailProvider.ts:194），而基线 src 下除测试外没有任何地方构造 NsjailProvider。2026-11-14 主会话核对。 |
| env | `env:PLAYWRIGHT_BROWSERS_PATH` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:45 | 不在范围：只由基线自带的独立脚本 src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs 自己读（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:45、253、266），这个脚本由 browser-use-install skill 让模型在 bash 里用 node 直接运行（skills/browser-use-install/SKILL.md），不经过 gateway / CLI / sidecar 的代码；gateway 不读这个变量，被比较的输出里没有它的影响。（主会话 2026-11-14 裁决） |
| files | `home:playwright-browsers/` | `ext.plugins` | src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:51 | 做不到（需要真实外部服务）：browser-use 插件安装脚本下载浏览器的目录（src/extension/plugins/builtin/browser-use/scripts/install-browser.mjs:51），安装要联网下载。 |
| tools | `tool:<extension contribution>` | `ext.plugins` | src/tool/registry/registerExtensionToolContributions.ts:17 | 基线里到不了：plugin.toolContributions 只在 src/extension/plugins/protocol/plugin.ts:28 声明、src/extension/plugins/runtime/PluginRuntime.ts:829 读取，基线源码里没有任何地方给它赋值（从磁盘发现 / 加载的插件不带这个字段），只有嵌入方用代码构造插件对象时才会有。 |

## `memory` 记忆（1）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| files | `other:~/.edgeclaw/memory` | `memory.config` | src/context/memory/edgeclaw-memory-core/src/service.ts:170 | CLI / server 里走不到：记忆核心只在 rootDir 为空时才用 ~/.edgeclaw/memory（src/context/memory/edgeclaw-memory-core/src/service.ts:169-171、619），唯一的构造点 src/context/memory/createEdgeClawMemoryProviderFromConfig.ts:50-57 传 cfg.rootDir，而 src/pilot/config/parseMemoryConfig.ts:71 总给它缺省值，配置写空串时 230-237 直接报 CONFIG_MEMORY_VALUE_INVALID。 |

## `config` 设置、配置、热重载（3）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| env | `env:PILOTDECK_RUNTIME_PROXY` | `config.env-paths` | src/sandbox/nsjail/NsjailProvider.ts:571 | 基线里到不了：只在 NsjailProvider 的沙箱 shell 里设置（src/sandbox/nsjail/NsjailProvider.ts:571），基线 src 下除测试外没有任何地方构造 NsjailProvider，三个入口都走不到。2026-11-14 主会话核对。 |
| files | `home:runtime/<purpose>/` | `config.env-paths` | src/pilot/writablePaths.ts:75 | CLI / server 里走不到：回落目录只由 ensureWritableDirectory 在首选目录不可写时用（src/pilot/writablePaths.ts:20-34、75-86）。两个调用方：计划文件管理器只在传了 pilotHome 且没传 storage 时才走它（src/tool/builtin/planFile.ts:26-46），唯一的构造点 src/cli/SessionPlanTodoBundle.ts:38-41 两样都不满足（缺省会话的计划目录实测是 <项目根>/.pilotdeck/plans）；resolveBrowserUseOutputDir（src/cli/createLocalGateway.ts:555-567）没有调用方，browser-use 实际用 BrowserUseSessionMcpSpecPreparer 的 <项目根>/.pilotdeck/browser_screenshots，建不了目录时整轮失败、不回落。 |
| events | `event:config_changed` | `config.hot-reload` | src/gateway/protocol/types.ts:737 | GatewayEvent 联合里的成员（src/gateway/protocol/types.ts:737），基线没有任何地方以事件帧发出 type:"config_changed"；配置变化只走同名通知（notification:config_changed，src/cli/GatewayRuntimeRefreshBundle.ts:102）。 |

## `modules` 外部能力模块与 sidecar 部署（12）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| env | `env:DATABASE_PATH` | `modules.sop` | src/cli/createLocalGateway.ts:778 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/cli/createLocalGateway.ts:778），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_APPROVAL_USER_ID` | `modules.sop` | src/cli/createLocalGateway.ts:977 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/cli/createLocalGateway.ts:977），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_COPY_ACTOR_USER_ID` | `modules.sop` | src/composition/nativeApprovalSessionResolver.ts:75 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/composition/nativeApprovalSessionResolver.ts:75），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_COPY_USER_TOKEN` | `modules.sop` | src/composition/nativeApprovalSessionResolver.ts:77 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/composition/nativeApprovalSessionResolver.ts:77），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_FORMAL_API_ORIGIN` | `modules.sop` | src/cli/createLocalGateway.ts:977 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/cli/createLocalGateway.ts:977），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_PROJECT_BINDING_ENABLED` | `modules.sop` | src/cli/createLocalGateway.ts:965 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/cli/createLocalGateway.ts:965），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_SOP_MANAGEMENT_CREDENTIAL_ID` | `modules.sop` | src/composition/nativeApprovalSessionResolver.ts:76 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/composition/nativeApprovalSessionResolver.ts:76），要连 StaffDeck 服务。 |
| env | `env:STAFFDECK_SOP_RUNTIME_ORIGIN` | `modules.sop` | src/pilot/staffdeck/projectBinding.ts:162 | 不在范围：StaffDeck（人已定第 7 条）。只在 StaffDeck 的项目绑定 / 审批副本校验里读（src/pilot/staffdeck/projectBinding.ts:162），要连 StaffDeck 服务。 |
| files | `home:auth.db` | `modules.sop` | src/cli/createLocalGateway.ts:778 | 不在范围：ui/server（阶段 B）的认证数据库；gateway 只为 StaffDeck 副本校验读其中的安装所有者（src/cli/createLocalGateway.ts:778）。 |
| files | `home:sop/` | `modules.sop` | src/pilot/config/parseModulesConfig.ts:331 | 不在范围：SOP 模块（StaffDeck 工作流）的状态根，只在 modules.sop 配齐 endpoint / definitionsPath / defaultSopId 时出现（src/pilot/config/parseModulesConfig.ts:324-331），由 src/sop/staffdeck/ 下的 SopAgentLoop / StaffDeckSopControlPlane 使用，开启需要 StaffDeck 外部服务；StaffDeck 不做（2026-11-12 人定）。sop-disabled 等场景比的是模块未开启时的表现。 |
| files | `home:staffdeck/project-bindings.json` | `modules.sop` | src/pilot/staffdeck/projectBinding.ts:18 | 不在范围：StaffDeck 项目绑定存储（src/pilot/staffdeck/projectBinding.ts:18），StaffDeck 不做（2026-11-12 人定）。 |
| module-behaviors | `mod:sop.staffdeck-authority` | `modules.sop` | src/composition/nativeSopAuthority.ts | 不在范围：StaffDeck 不做（specs/delivery/README.md「不在范围」；2026-11-12 人定），只要求缺省与未开启时的表现，后者由 sop-disabled / cfg-modules-binding 覆盖。 |

## `channels` 渠道（只要求启动表现）（42）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| slash-commands | `channel-command:/ask` | `channels.commands` | src/adapters/channel/qq/QQChannel.ts:19 | 不在范围：各渠道的生产实现（人已定第 6、12 条）。QQ 渠道收到消息时认的触发前缀（src/adapters/channel/qq/QQChannel.ts:19），要真实 QQ 机器人连接才有消息进来。 |
| slash-commands | `channel-command:/chat` | `channels.commands` | src/adapters/channel/qq/QQChannel.ts:19 | 不在范围：各渠道的生产实现（人已定第 6、12 条）。QQ 渠道收到消息时认的触发前缀（src/adapters/channel/qq/QQChannel.ts:19），要真实 QQ 机器人连接才有消息进来。 |
| env | `env:BLUEBUBBLES_PASSWORD` | `channels.startup` | src/adapters/channel/bluebubbles/BlueBubblesChannel.ts:55 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/bluebubbles/BlueBubblesChannel.ts:55）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:DINGTALK_CLIENT_ID` | `channels.startup` | src/adapters/channel/dingtalk/DingTalkChannel.ts:48 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/dingtalk/DingTalkChannel.ts:48）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:DINGTALK_CLIENT_SECRET` | `channels.startup` | src/adapters/channel/dingtalk/DingTalkChannel.ts:49 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/dingtalk/DingTalkChannel.ts:49）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:DINGTALK_STREAM_DEBUG` | `channels.startup` | src/adapters/channel/dingtalk/DingTalkChannel.ts:69 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/dingtalk/DingTalkChannel.ts:69）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:DISCORD_BOT_TOKEN` | `channels.startup` | src/adapters/channel/discord/DiscordChannel.ts:41 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/discord/DiscordChannel.ts:41）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:EMAIL_ADDRESS` | `channels.startup` | src/adapters/channel/email/EmailChannel.ts:66 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/email/EmailChannel.ts:66）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:EMAIL_PASSWORD` | `channels.startup` | src/adapters/channel/email/EmailChannel.ts:67 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/email/EmailChannel.ts:67）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:HASS_URL` | `channels.startup` | src/adapters/channel/homeassistant/HomeAssistantChannel.ts:66 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/homeassistant/HomeAssistantChannel.ts:66）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:IMAP_HOST` | `channels.startup` | src/adapters/channel/email/EmailChannel.ts:68 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/email/EmailChannel.ts:68）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:IMAP_PORT` | `channels.startup` | src/adapters/channel/email/EmailChannel.ts:70 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/email/EmailChannel.ts:70）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:MATRIX_ACCESS_TOKEN` | `channels.startup` | src/adapters/channel/matrix/MatrixChannel.ts:50 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/matrix/MatrixChannel.ts:50）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:MATRIX_HOMESERVER` | `channels.startup` | src/adapters/channel/matrix/MatrixChannel.ts:51 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/matrix/MatrixChannel.ts:51）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:MATRIX_USER_ID` | `channels.startup` | src/adapters/channel/matrix/MatrixChannel.ts:52 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/matrix/MatrixChannel.ts:52）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:MATTERMOST_TEAM_ID` | `channels.startup` | src/adapters/channel/mattermost/MattermostChannel.ts:50 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/mattermost/MattermostChannel.ts:50）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:MATTERMOST_TOKEN` | `channels.startup` | src/adapters/channel/mattermost/MattermostChannel.ts:48 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/mattermost/MattermostChannel.ts:48）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:MATTERMOST_URL` | `channels.startup` | src/adapters/channel/mattermost/MattermostChannel.ts:49 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/mattermost/MattermostChannel.ts:49）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:QQ_ALLOW_GROUPS` | `channels.startup` | src/adapters/channel/qq/QQChannel.ts:44 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/qq/QQChannel.ts:44）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:SIGNAL_ACCOUNT` | `channels.startup` | src/adapters/channel/signal/SignalChannel.ts:86 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/signal/SignalChannel.ts:86）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:SIGNAL_HTTP_URL` | `channels.startup` | src/adapters/channel/signal/SignalChannel.ts:84 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/signal/SignalChannel.ts:84）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:SLACK_APP_TOKEN` | `channels.startup` | src/adapters/channel/slack/SlackChannel.ts:42 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/slack/SlackChannel.ts:42）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:SLACK_BOT_TOKEN` | `channels.startup` | src/adapters/channel/slack/SlackChannel.ts:41 | 渠道凭据（src/adapters/channel/slack/SlackChannel.ts:41）：各渠道的生产实现不在范围（specs/delivery/README.md 范围段），渠道只对齐启动表现。与其余渠道凭据环境变量同一理由，上一批漏标。 |
| env | `env:SMTP_HOST` | `channels.startup` | src/adapters/channel/email/EmailChannel.ts:69 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/email/EmailChannel.ts:69）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:SMTP_PORT` | `channels.startup` | src/adapters/channel/email/EmailChannel.ts:71 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/email/EmailChannel.ts:71）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:TELEGRAM_BOT_TOKEN` | `channels.startup` | src/adapters/channel/telegram/TelegramChannel.ts:42 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/telegram/TelegramChannel.ts:42）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:TWILIO_ACCOUNT_SID` | `channels.startup` | src/adapters/channel/sms/SmsChannel.ts:66 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/sms/SmsChannel.ts:66）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:TWILIO_AUTH_TOKEN` | `channels.startup` | src/adapters/channel/sms/SmsChannel.ts:68 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/sms/SmsChannel.ts:68）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:TWILIO_PHONE_NUMBER` | `channels.startup` | src/adapters/channel/sms/SmsChannel.ts:70 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/sms/SmsChannel.ts:70）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:TWILIO_WEBHOOK_PORT` | `channels.startup` | src/adapters/channel/sms/SmsChannel.ts:72 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/sms/SmsChannel.ts:72）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_AGENT_ID` | `channels.startup` | src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:101 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:101）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_ALLOWED_USERS` | `channels.startup` | src/adapters/channel/wecom/WeComChannel.ts:215 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom/WeComChannel.ts:215）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_CB_PORT` | `channels.startup` | src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:105 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:105）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_CB_SECRET` | `channels.startup` | src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:102 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom-callback/WeComCallbackChannel.ts:102）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_DM_POLICY` | `channels.startup` | src/adapters/channel/wecom/WeComChannel.ts:214 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom/WeComChannel.ts:214）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_GROUP_POLICY` | `channels.startup` | src/adapters/channel/wecom/WeComChannel.ts:216 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom/WeComChannel.ts:216）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WECOM_WEBSOCKET_URL` | `channels.startup` | src/adapters/channel/wecom/WeComChannel.ts:208 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/wecom/WeComChannel.ts:208）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WHATSAPP_BRIDGE_PATH` | `channels.startup` | src/adapters/channel/whatsapp/WhatsAppChannel.ts:51 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/whatsapp/WhatsAppChannel.ts:51）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| env | `env:WHATSAPP_BRIDGE_URL` | `channels.startup` | src/adapters/channel/whatsapp/WhatsAppChannel.ts:53 | 不在范围：各渠道的生产实现（人已定第 6、12 条，渠道只要求启动表现一致）。只在渠道适配器构造 / 连接外部服务时当凭据或地址读（src/adapters/channel/whatsapp/WhatsAppChannel.ts:53）；启用渠道与否由配置决定，这个变量不改变启动时的被比较输出。 |
| events | `agent_status:channel_submit_failed` | `channels.startup` | src/adapters/channel/feishu/FeishuChannel.ts:630 | 不在范围：只由飞书 / 微信等 IM 渠道的生产实现发出（src/adapters/channel/feishu/FeishuChannel.ts:630），触发需要渠道真实收到消息；渠道只对齐启动表现（2026-11-12 人定）。 |
| files | `home:im-attachments/` | `channels.startup` | src/adapters/channel/feishu/FeishuChannel.ts:167 | 不在范围：飞书 / 微信渠道收到附件时的落盘目录（src/adapters/channel/feishu/FeishuChannel.ts:167），要渠道真实收到消息。 |
| files | `home:matrix-bot-storage.json` | `channels.startup` | src/adapters/channel/matrix/MatrixChannel.ts:54 | 不在范围：Matrix 渠道生产实现的同步状态（src/adapters/channel/matrix/MatrixChannel.ts:54），要连上真实 Matrix 服务器才会写。 |

## `cli` CLI（11）

| 维度 | 项 | 子能力 | 基线位置 | 不计的理由 |
|---|---|---|---|---|
| env | `env:PAGER` | `cli.default-chat` | src/adapters/channel/tui/TuiChannel.ts:44 | 不在范围：pilotdeck tui（人已定第 13 条）。只在 TUI 渠道里选分页器（src/adapters/channel/tui/TuiChannel.ts:44），gateway / CLI 子命令 / sidecar 不读它。 |
| slash-commands | `tui-command:/clear` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:806 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:806），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/dashboard` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:689 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:689），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/exit` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:812 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:812），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/help` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:809 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:809），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/mode` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:765 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:765），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/new` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:678 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:678），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/permissions` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:706 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:706），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/sessions` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:687 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:687），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/switch` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:688 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:688），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
| slash-commands | `tui-command:/view` | `cli.default-chat` | src/adapters/channel/tui/app/TuiApp.tsx:787 | 不在范围：pilotdeck tui（人已定第 13 条，specs/delivery/README.md 范围段）。这是 Ink TUI 的本地命令（src/adapters/channel/tui/app/TuiApp.tsx:787），只在 TUI 进程里处理、不发给 gateway；TUI 要交互终端，bbx 起不了。 |
