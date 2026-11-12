# Core-scope audit

2026-11-13，当前共享工作区只读收口审计。固定基线 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，路径 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。未运行产品命令、Cargo或邮件/模型网络请求；本文件以当前生产构造链和固定TS源码为证据，未将静态发现当成实际运行结果。StaffDeck、TUI、ui/server不在本期审计范围。

扫描从 `pilotdeck-cli/src/main.rs::run` 的正式dispatch、`native_process/assembly.rs` 的正式构造出发，沿 production request / submitter / tools 到实际委托；不把测试fake、未配置时guard、允许的旧JS函数加载拒绝、无基线正式调用的可选callback当成stub。以下仅列确认存在的范围内产品缺口，非覆盖率或全后端完成声明。

## 确认缺口

| ID / 严重级 | 正式触发 | 当前产品结果及直接证据 | 固定基线能力 | 后续责任与所需验证 |
|---|---|---|---|---|
| CORE-SDK-HOOK-01 / P1 | `submit_turn.sdkSessionConfig.hooks.events.ConfigChange` 或 `FileChanged` | `NativeSdkSessionConfig::validate` 在 `native_process/sdk_config.rs:1292-1317` 因事件名本身直接返回 `CAPABILITY_UNAVAILABLE`；`native_process/hooks.rs:65-70` 构建hook settings时也显式排除两事件 | `src/cli/SessionInteractionBundle.ts:130-134,170-216,286-299` 为二者分别建runtime；ConfigChange注册gateway回调，FileChanged通过didSave发HTTP hook | 根已决定交给本审计作者专项实现。必须对固定TS捕获HTTP hook输入/selector，验证配置变更和正式write/edit触发、匹配、吞错、session释放；不能只删除拒绝 |
| CORE-SDK-SCOPE-02 / P1 | 同一SDK配置包含`skills:[]`与任意`agents`（包括合法空skills数组） | `sdk_config.rs:1304-1311` 的`scoped_agents`直接拒绝整个配置，注释明确child context没有compose该scope | `src/cli/ProjectRuntimeRegistry.ts:2118-2166` 将父skills数组继承给缺省/`all`子agent，解析显式子skills并拒绝越出父范围 | 根已交给本审计作者专项实现。需验证父子prompt/read_skill/tool registry同一scope、相邻session不泄漏、父范围越界错误；不能仅放行配置 |
| CORE-SDK-AGENT-03 / P1 | SDK合法AgentDefinition使用`mcpServers`、`skills`、`memory`、`initialPrompt`、`observer`或`observerMessage` | `sdk_config.rs:46-81` 的`WIRED_AGENT_FIELDS`不含这些字段，`agents_wired`检查所有非undefined own keys，最终在`:1311-1317`固定拒绝；这些是可序列化后端字段，不是JS函数加载 | `src/gateway/protocol/types.ts:487-503`明确fork-local MCP、skill visibility、memory disabled、initialPrompt与observer agent引用；`ProjectRuntimeRegistry.ts:2118-2185`实际解析子skills/MCP引用。`sdk_config.rs:560-633`自身也先验证合法observer字符串及其引用，随后却因未接线拒绝 | 本审计只登记；skills与上一项可共同实现，其余需根另排。需fork-local MCP启动/释放、memory范围、初始prompt与observer实际执行证据 |
| CORE-SDK-OPTIONS-04 / P1 | SDK合法顶层配置使用`outputFormat`、`plugins`、`agentProgressSummaries`、`forwardSubagentText`、`sandbox`或`taskBudget` | `sdk_config.rs:22-44`的`WIRED_FIELDS`均不含这些项；`:1284-1290`对任一非undefined键直接判未接，`:1315-1317`返回CAPABILITY_UNAVAILABLE；前面的shape validator已分别识别这些字段（`:279-305,320-345,393-443`），不是输入非法guard | `src/cli/SessionAgentConfigBundle.ts:132,140`接progress/structured-output停止；`ProjectRuntimeRegistry.ts:993,1016-1020,1058-1070,1382-1439,1808-1847`接session local plugins与预算；`ProjectSessionRuntimeBundle.ts:248,343,364`接plugin贡献与sandbox；`gateway/client/InProcessGateway.ts:1674`把forwardSubagentText送projector | 本审计登记给根拆包。需输出schema约束及停止、local plugin贡献的session隔离、progress/subagent typed事件、tool-policy sandbox、预算持久化和扣账实际生效，不能仅改allowlist |
| CORE-STEER-ATTACH-05 / P1 | 已接受的活跃turn调用`steer_turn`并传非空`attachments` | 正式`native_process/methods/steer.rs:145-149`无条件`STEER_ATTACHMENTS_UNSUPPORTED`；该分支在active run admission后直接退出，不进入准备/转交 | `src/gateway/client/InProcessGateway.ts:1859-1883`实际调用`prepareAttachmentTurn`并把content与allowedReadFiles转交router | 已立即报根。需复用现有submit附件准备能力，保持no_active_turn/turn_mismatch优先级，并验证模型实际收到附件与文件读取授权。`uploadedAttachments`在基线steer中被忽略，不能借修复改其行为 |

## 为什么这些拒绝是生产缺口

它们不是只有测试可构造的类型：`native_process/assembly.rs:1923-1930`向正式`SubmitAdmissionOptions`无条件安装`NativeSdkSessionConfig`，其中持有真实session overrides和ProjectRuntimeRegistry。`runtime/turn/admission.rs:770-808`将正式submit的`sdkSessionConfig`交该port校验、设置；`runtime/turn/submit.rs:548-557`在预检阶段执行校验。因此上述所有通过shape检查的合法配置，都会在正式执行前被native专有allowlist拦截，而不是仅在某个可选能力未配置时失败。`pilotdeck-cli/src/sdk/turn.rs:254-278`的正式SDK submit请求也有`sdk_session_config`字段。

CORE-STEER-ATTACH-05已有历史差异卡 `specs/gateway-parity/diffs/A7.2-STEER-ATTACH-001.md` 与R1；原卡明确写「类别：未实现」「未批准，非已接受差异」。所以不能用存在差异卡这一事实将当前拒绝视为已批准完整交付。此次审计未修改冻结标准或差异卡。

## 最小实现接续方案

当前根要求等第5轮check源码/binary封存后再动Rust；本轮仅写文档。专项作者建议先做HOOK-01与SCOPE-02：

1. 在`native_process/hooks/`新增独立SDK事件lifecycle，复用现有HookRuntime、HTTP执行器、sdk_hook_url、session cleanup。FileChanged接`runtime/tools/scheduler.rs:747-820`既有committed-save事件；将absolute/relative/changeKind转换为固定TS的relative filePath、absolutePath、project root、created/updated，在专用runtime中按relative路径匹配；保持plugin原FileChanged结果与SDK吞错语义。
2. ConfigChange复用`native_process/assembly/reload.rs`现有ConfigRefresh事件，注册session scoped listener并在session释放注销，发送真实changedPaths/changeClasses，不建立新OS watcher。现有`ToolContextPorts.file_update_notifier`只是字段，不能看到字段就假设有正式消费链；优先在当前ToolHooks实际入口完成SDK专用分发。
3. scoped agents复用`SessionTools::child`和现有`world.skills_filter`、read_skill筛选、prompt skills组装；先解析父子skill范围，再向child携带同一已解析scope。显式子skills越界拒绝应保留对应基线错误，不通过放宽scope绕过。
4. 预计独占`native_process/sdk_config.rs`、`hooks.rs`、`hooks/session.rs`、新`hooks/sdk_events.rs`与定向测试；`tools.rs`仅构造/child skills接线，`assembly.rs`与`assembly/reload.rs`为公共热点，由根授权精确块或统一接线。若需要扩展AgentDefinition scope，另授权`runtime/tools/builtin/agent.rs`与`native_process/subagents.rs`。
5. 固定TS probe通过SessionInteractionBundle实际入口捕获两类HTTP hook输入/释放及skill scope解析；本地mock、隔离HOME/PILOT_HOME，无真实hook账号或provider。Rust正式配置→turn→write/config-refresh路径是最终验收对象。

上述发现均已即时发主协调者；尚未实现或验收的项保持未完成。
