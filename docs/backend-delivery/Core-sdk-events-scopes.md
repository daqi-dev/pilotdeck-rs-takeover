# Core SDK events and scopes

任务：CORE-SDK-HOOK-01 / CORE-SDK-SCOPE-02；根授权本作者唯一修改native `sdk_config.rs`、`session_overrides.rs`、`tools.rs`、`hooks.rs`及其私有SDK事件文件。公共assembly/services由O1唯一接线，插件/AgentDefinition/sandbox/预算各作者提供实现，本作者统一控制SDK字段gate。起点为根封存的`4525bb2`源码与binary；本批未创建交付提交，未运行Cargo。

固定基线`OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读根`/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。

## 产品改动及接线

- SDK ConfigChange/FileChanged分别有独立HookRuntime/LifecycleRuntime。ConfigChange与FileChanged采用固定TS各自的async-hook注册规则，复用当前shell、HTTP执行器与hook事件投影，SDK hook结果不改变保存工具结果。
- ConfigChange通过每个gateway唯一SessionOverrides持有的SdkEventBus分发；该bus实现已有`runtime::refresh::SdkConfigChange`，由O1接入现有RefreshBundle，保持空changeClasses、restart-required、run-policy等既有派发条件，覆盖server/native/local。没有在ServerReloadDriver重复订阅。
- FileChanged接当前ToolRuntime在committed save后的实际ToolLifecycle事件，转换为固定TS的relative filePath、absolutePath、root、created/updated，并按relative路径匹配；不建立OS watcher，不把已有但未消费的context字段当作接线。
- 每个session拥有独立SDK runtime。ConfigChange detached任务由session保留；close同步禁止新事件、注销listener并取消既有生命周期，返回的清理future等待任务和runtime释放。子agent共享父SDK file notifier且不拥有其关闭权，避免子agent结束关闭父session的callback。
- SDK父子skills在配置发布前解析：完整名称优先，否则唯一后缀；子agent缺省/`all`继承父显式数组；显式子范围越出父范围拒绝。解析结果映射到subagentDefinitions，child的world.skills_filter用于真实prompt skill projection与read_skill；observer强制空skills，不能被其definition重新扩大。
- 根要求同步协调的三个output control字段已接入已有执行链；本作者只添加精确flag映射和gate，独立正式运行测试由telegram作者持有，不额外添加provider schema行为。
- SDK plugins候选通过protocol作者的现有resolver准备，用combined plugin view解析skills；所有后续校验成功后，在SessionOverrides同一Inner锁原子发布config、plugins、project root。失败不修改既有配置/插件；clear在同锁删除。O1读取同锁sdk_snapshot组装并持有contribution lease。本作者未新增第二个plugin store。
- AgentDefinition额外字段由Ding作者执行真实fork生命周期，本作者先准备准确mapping/MCP引用解析及child setter，根确认该首批具备真实执行接线后已开放对应gate供正式测试，未运行部分仍待验证。sandbox已按inventory作者固定TS2280格oracle与实际helper接上最终完整registry filter，包含MCP/custom/agent/continuation注册后的剥除与alias移除，child继承同policy；catalog碰撞预检保持完整未过滤视图。taskBudget已按telegram作者的唯一store API在全部解析校验后、atomic commit前调用configure_sdk，取消不发布，同Arc的snapshot/run/terminal settle接线由对应作者/O1完成；这些都未运行Rust验证。

## 固定TS取证

`SessionInteractionBundle.ts:130-216,286-313`为独立SDK生命周期与payload依据。外部driver直接加载固定dist的SessionInteractionBundle，调用实际attach/config callback/didSave，networkFetch为本地无外发mock；捕获结果见`tests/fixtures/h1-sdk-events/expected.json`。三个HTTP请求分别是ConfigChange、FileChanged created、FileChanged updated；no.md不匹配，dispose注销一次。两个修正后结果完全相同：`_tmp/20261113-sdk-events/fixed-matcher/{first,second}.json`。

首次场景错误地把`*.txt`当glob，固定TS仅产生ConfigChange，原证据`_tmp/20261113-sdk-events/{first,second}.json`保留。读取`extension/hooks/config/matchHook.ts`确认matcher是exact/`*`/`/regex/`后，将场景修为exact `a.txt`并另开目录；这是场景归因，不是产品首次失败，不覆盖旧结果。

`ProjectRuntimeRegistry.ts:2118-2166`为scope依据。外部driver只读提取固定dist内实际私有`resolveSdkSessionSkills`与`resolveSdkSubagentScopes`函数，在VM中使用固定DialogGatewayError执行；没有修改参考目录、没有复制参考实现进Rust。六组实际输出覆盖inherit/all/empty、后缀解析去重、越界、不存在和歧义。`_tmp/20261113-sdk-scopes/{first,second}.json`双次cmp一致；可重放driver及固定结果位于`tests/fixtures/h1-sdk-scopes/`。这证明解析函数行为，不冒充完整Gateway fork验收。

强化版driver随后增加必需场景断言：events必须3次且注销1次，scopes必须6组且三类错误码准确。2026-11-13获根短槽2后使用Node v22.22.0各再跑两次，输出与对应checked-in expected也逐字节相同；新证据`_tmp/20261113-sdk-strict-drivers/{events,scopes}-first0.json`及second独立保存，manifest记录两driver与四输出SHA-256，旧bad-matcher材料不覆盖。

以上两个oracle均隔离HOME/PILOT_HOME，未访问账号、真实hook、模型或用户目录；轻量槽完成即归还。只输出Node自身SQLite experimental warning，没有产品错误。

## 验证状态

| 层次 | 状态 | 边界 |
|---|---|---|
| 接口 | 待验证 | bus、tools fluent API、atomic snapshot/commit、各作者API已对齐 |
| 实现 | 进行中 | HOOK/SCOPE产品路径已写，HOOK/SCOPE、controls、plugins、AGENT03、sandbox、taskBudget均已有实际委托/执行接线并开放gate；尚未编译验证 |
| 接线 | 待验证 | 已读到O1正式with_sdk_events、samebus RefreshOptions.sdk、同锁plugin snapshot接线；未运行 |
| 验证 | 进行中 | 固定TS双次自比通过；独立rustfmt通过；Rust新增测试未编译/未运行 |
| 交付 | 待验证 | 未在同一集成提交跑全量、未发行、未作真实账号验收 |

本作者新增Rust用例：真实HTTP三事件逐字段对比固定TS、销毁注销且禁止晚到事件、pending ConfigChange无HTTP响应时session取消drain；正式LocalGateway→本地HTTP模型→write_file→SDK FileChanged HTTP；六组固定TS scope解析及definition传递。O1另补正式ConfigStore/settings→RefreshBundle→samebus→HTTP callback；Ding作者负责正式parent/child fork范围场景，避免仅凭resolver标记scope交付。

建议统一构建槽定向filter：`native_process::hooks::sdk_events::tests::`、`native_process::sdk_config::scope_tests::`，再运行各作者独立SDK controls/plugins/agent/sandbox/budget测试。未执行的结果不能标通过。当前没有新增依赖、没有Cargo、没有自行提交/推送或部署。
