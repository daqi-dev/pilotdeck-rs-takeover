# CORE-SDK-AGENT-03：可序列化子代理作用域

负责人：h1_dingtalk_wecom。工作从根封存的`4525bb2abdee1e7e1f398889d9ad56baa3337367`检查点恢复后开展，未提交交付；当前状态进行中/待首次编译，不能称已交付。skills由Email的SCOPE02独占；旧JS函数/observer callback加载不实现，本包只覆盖固定GatewaySdkAgentDefinition JSON数据字段。

## 固定基线与入口

固定TS cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，只读路径/Users/da/ws/pilotdeck-rs/resources/PilotDeck。

- gateway/protocol/types.ts:487-503：mcpServers对象或数组/引用、memory inherit/disabled、initialPrompt、named observer和observerMessage，全部是可序列化数据。
- cli/ProjectRuntimeRegistry.ts:956-1023：真实setSdkSessionConfig；:998调用resolveSdkSubagentScopes；:2118-2200解析skills/model/MCP，MCP references取SDK-session已配置且enabled的endpoint，克隆为fork-local record，inline重复报INVALID_MCP_SERVER。不是在SessionAgentConfigBundle内解析。
- cli/SessionAgentConfigBundle.ts:206-239把已解析字段传给native定义。
- agent/sub/SubAgentSession.ts:150构造初始消息，:192-207把prefix和directive一起记录为accepted input；:294-318仅启动definition MCP并按allowed/denied筛选，结束释放；buildForkedMessages.ts:18-25是trim prefix可选user加directive user。
- cli/ProjectSessionRuntimeBundle.ts:568-595为memory disabled或skills scope创建独立context；memory disabled不带memoryResolver，不是删光父项目记忆数据。
- agent/sub/OneShotSubagentPort.ts:99-108检查命名observer；:355-465启动真实后台只读fork，禁止tools/MCP/skills/memory/initialPrompt/background/递归，plan，64活动/12000 UTF16单元digest，observer_report。

## 修改范围与共享接点

独占实现为runtime/tools/builtin/agent.rs、native_process/subagents.rs/subagent_generation.rs、新agent_scope.rs/agent_observer.rs/agent_scope_tests.rs。字段承载、fork-local MCP runtime及durable cleanup、initialPrompt双canonical user消息、named observer真实后台fork均已写；尚未编译或正式跑通。

共享文件由唯一作者修改，未并行写：Email持sdk_config.rs/tools.rs，接shape/immutable reference resolver/mapping、child MCP工具setter、memory context-state隔离、observer空skills；Protocol持host_factory.rs，接subagentMemoryDisabled→SessionContextOptions.memory_disabled；O1持assembly，接NativeSdkSessionConfig.mcp_config同Arc。WIRED_AGENT_FIELDS的本包字段仍关闭，等执行代码类型检查后统一开，不能只接validator假成功。

MCP通过同generation的McpProviderResource.create_runtime复用当前网络/proxy工厂，运行实例只属于child。ChildSession cleanup先drain session/hook/MCP再release generation；独立stop worker/receipt在调用方取消时仍持有运行时及lease。startup取消等待实际MCP stop；child.dispose失败不吞为成功。正常native MCP start状态中的endpoint失败沿用现有runtime与工具桥行为，不能硬返回工具成功。

initialPrompt保JsString/UTF16；复用现有TurnInput accepted+canonical synthetic域形成prefix user与directive user，持久化及模型消息顺序一致，没有拼接成一条文本。

## 已执行证据与边界

_tmp/20261113-agent03/oracle.mjs实际导入固定TS createNativeOneShotSubagentPort和buildForkedMessages；两轮first.json/second.json完全一致。覆盖observed成功和失败两种情况、observer override/digest/observer_report、prefix两user和孤立UTF16。故意将捕获observer allowedTools改成[*]后正向断言失败exit1，negative.stderr/negative.exit保留。HOME/PILOT_HOME/TMPDIR隔离，无真实账号/模型/外部MCP。

这次provider和observer launcher是内存mock，证明真实port编排合同，不证明AgentLoop执行或MCP实际进程生命周期。稳定fixture位于gateway tests/fixtures/agent03-scope。SDK数组MCP reference尚未正式入口实测，首正式正向按S1建议使用object stdio，array/ref单独对照；不得把源码推断冒充实测或擅自登记新差异。

私有新增7项测试，全部尚未运行：reference immutable snapshot；notfound/disabled/duplicate错误；prefix canonical双消息与UTF16；observer只读override和memory/skills flag；observer digest精确TS输出；MCP stop waiter取消保lease到实际stop；owner drop/失败receipt。仅rustfmt和diff whitespace静态检查。未自行Cargo，未继承旧snapshot测试成绩。

## 待闭合

首次check/no-run与受影响clippy；SDK gate统一开启；S1独立正式SDK配置→parent agent工具→真实child model request；private MCP发现/调用/实际process close；memory disabled实际不检索且另session不受影响；observer真实只读后台model call与report/取消/资源释放；worker仅技能A、parent/另一session仍A/B及read_skill拒B（Email技能作用域）；不可用/失败/取消与重启资源验收。未闭合前不称AGENT03完成。

根为避免“等正式场景通过才允许场景进入真实路径”的循环，已授权Email在实现接线和7项tests源码就绪后开放本包6字段（含既有skills），标待验证。不能据gate开放推断通过。正式SDK parent/child/observer/MCP场景已由根派S1独立装置作者；首正向使用object stdio，数组/ref单独对照。根现已冻结全src，交C1第六统一fmt/check，作者不再改Rust或src fixture。用户旧JS函数原样兼容排除仍不变。

第六统一check实际exit101，本包首错是observer::launch无await async边界造成递归future Send证明环（agent_observer.rs:221，subagents.rs:1397），证据_tmp/20261113-c1-resume-validation/root-sixth-errors.txt。根明确释放这两文件后已最小改为同步注册fn launch并去两处调用await；后台仍以真实ForkFuture<Send>执行并drain observer，不删执行来消错。rustfmt语法通过，等待第七类型检查，不标编译通过。其它源码继续冻结。
