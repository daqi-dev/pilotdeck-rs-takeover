# SDK 本地目录插件正式入口候选

状态：待固定 TS 校准。未在本机运行 BBX，没有绕过18789守卫；本场景当前不作为通过成绩。正式执行交根协调/E0，在新固定源码+harness快照和隔离cb8环境进行。

本场景走真实 WS `submit_turn.sdkSessionConfig.plugins:[{type:"local",path:绝对Gateway路径}]`，不调用private product loader，也不把user/project自动发现当SDK能力。fixture刻意放在`PILOT_HOME/sdk-directory-plugins/local-a|local-b`，不位于`PILOT_HOME/plugins`或项目`.pilotdeck/plugins`。目录、manifest和Markdown均独立编写，无JS函数、账号、网络服务或复制参考资源。

第一批只保护三个完整能力及scope：

1. A配置同时选local A目录、chosen skill及blue style。完整命令`/local-a:probe SDK-DIR-PHASE-A`应按固定InputProcessor展开；**同一真实模型请求**必须有该完整正文/参数、selected skill和style。真实`read_skill`返回SDK-only目录recipe，再实际读取hidden skill并确认不返回hidden内容。
2. 另一session不带SDK配置，其系统提示不得出现A/B贡献。
3. 原session改为B目录、B selected skill及red style：实际新命令/skill/style全部切B，历史仍保留A终态。
4. 原session提交`{}`清空SDK目录与style/skills限制，系统提示不再泄漏A/B，历史仍在。
5. 不存在的SDK目录必须从真实WS返回SDK_PLUGIN_NOT_FOUND，并且不发送模型请求。

精确固定7个stream provider请求；标题请求单独匹配，不冒充主turn。四次实际WS history和tree中的accepted_input证明落盘、读取及模型文本一致。fixed300ms等待沿现有SDK场景，只用于时序；每个正式submit/读历史都有有界deadline和真实内容断言，不能把睡眠当完成。

MCP连接/子进程/释放可另立生命周期专项。本最小场景**没有MCP配置或假MCP通过**；不能据本场景关闭插件MCP gate/生命周期工作。hooks/lease另有组件测试，也不能混算这里三能力的正式成绩。

## 固定基线依据

- InProcessGateway.ts的validateSdkSessionConfig接受非空distinct绝对local plugin路径。
- ProjectRuntimeRegistry.ts993-1023先refresh，再resolve local插件、combined view解析skills，全部成功后原子发布；1808-1847保留realpath/重复path/非目录/加载错误/manifest name冲突的精确错误。
- PluginCommandLoader.ts45-58使用目录basename形成完整命令名，故`local-a:probe`，不是manifest name的`sdk-directory-a:probe`；短名不被猜作alias。
- ProjectSessionFactory.ts154→createAgentSession.ts155→TurnRunner.ts192-201实际InputProcessor先展开再accepted_input；无附件GatewayAttachmentTurnComposer.ts69是Text。
- 同一PluginSessionRuntimeBundle contribution lease的技能读取和outputStyle实际prompt必须在当前正式入口验证，纯loader或此前模块InputProcessor oracle不能代替。

## E0执行与身份

保持scenario、verifier和公共harness不变，在既有隔离容器/私网中使用仓库实际固定Node/TS基线执行。不要裸本机执行、不要设置BBX_ALLOW_STALE。

```sh
node scripts/bbx/check.mjs --self sdk-directory-plugin-contributions --out NEW_TS_SELF_DIR
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs --self-test ACTUAL_TS_A_JSON
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs ACTUAL_TS_B_JSON
# TS校准和实际负控完成后，匹配源码的新Rust binary：
node scripts/bbx/check.mjs sdk-directory-plugin-contributions --out NEW_PARITY_DIR
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs ACTUAL_RUST_JSON
```

verifier严格核对capture内scenario SHA与实际输入JSON、全部必需assertions非skip/执行通过、7个真实stream请求、模型system/user/tool内容、真实history和durable transcript；输出artifact/scenario/verifier精确hash。既有DIFF/INVALID和首次失败均留原位。TS事实若与候选断言冲突，应保存原始请求/落盘并查入口，不能放宽断言假装校准。

## 实际负控，必须另外执行

`--self-test`仅检验verifier是否拒绝采集结果损坏，**不是实际产品故障注入**。至少做下面一个实际变体；建议三个都做，产物分别新目录。

```sh
# 只写新变体输入，不启动产品；目标文件必须尚不存在。
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs --write-negative-scenario wrong-command NEW_COMMAND_SCENARIO.json
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs --write-negative-scenario wrong-skill NEW_SKILL_SCENARIO.json
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs --write-negative-scenario wrong-style NEW_STYLE_SCENARIO.json
# 以下在E0隔离环境中，走真实TS WS和磁盘目录：
node scripts/bbx/run.mjs ts NEW_COMMAND_SCENARIO.json NEW_ACTUAL_NEGATIVE_CAPTURE.json
node scripts/bbx/fixtures/sdk-directory-plugin-contributions/verify.mjs NEW_ACTUAL_NEGATIVE_CAPTURE.json NEW_COMMAND_SCENARIO.json
```

最后命令必须非零退出：变体修改实际目录command/skill/style正文，仍由真实SDK加载并运行；verifier将capture身份绑定到该变体，但语义期望仍固定为原A正文/recipe/style，不因输入变体而自适应。此设计证明正式loader→processor/read_skill/prompt路径确实影响原断言，不能用改capture JSON替代。保留变体SHA、真实原始capture、stderr及退出码。当前仅生成器/语法已静态检查，真实TS/Rust/负控都尚未执行。
