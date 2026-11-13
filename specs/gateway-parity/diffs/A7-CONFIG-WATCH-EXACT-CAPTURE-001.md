---
id: A7-CONFIG-WATCH-EXACT-CAPTURE-001
---

## 类别

基线 bug 修复的固定证据比较适配，并单独精确接受同批已有版本计数差异。监听缺陷与基线自身 watcher 测试的通知/快照更新意图矛盾；P2兼容性差异。本条只接受下述三份固定原件的三个确切叶，不修改产品，不建立未来watch或version的一般豁免。

## 基线依据

固定 TS `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：`resources/PilotDeck/src/pilot/config/PilotConfigStore.ts:107-129,152-158` 对已存在的配置文件本体一次注册 `fs.watch`；`src/pilot/config/updateLocalSettings.ts:83-85,318-329` 以临时文件rename覆盖，`src/cli/createLocalGateway.ts:1384-1402` 正式更新后reload但不重新注册watch。`tests/pilot/config/loadPilotConfig.spec.ts:70-98` 要求外部改写后通知并更新快照；`src/gateway/protocol/frames.ts:153-160` 将watch重载列为异步配置通知例子。该测试直接写已有自定义文件，没有覆盖先rename再写的精确序列。

固定harness `644a8d80a45916bbe7a0b0ee93ff6e331a9dbf98` 的 `settings-update` 正式入口：step26成功设置4096后，step27在相同dev/inode/单链接文件上一次原地写4096→2048。TS1/TS2在完整30秒窗口均没有目标通知；Rust约309ms收到精确 `config_changed`，其同通知index关联的resolve证明sourceHash及version推进。三侧下一次唯一watch-rewrite实际provider请求均 `max_tokens=2048`。

“原子替换后watch仍挂在旧inode”是源码支持的强因果推断；raw没有注册时inode、内核事件或30秒后观察，不能冒称逐事件直接证明、TS永不通知或窗口内内部快照绝不变化。三侧顶层notifications均4，不能声称Rust总数更多。

step25的number版本TS9/9、Rust10在step26/27之前；step9版本均3。三侧step25完整snapshot仅version及loadedAt不同，公开config/contentHash/env与default来源一致；step26设置4096后捕获版本各恰加1。`PilotConfigStore.ts:44-50,60-83` 与固定Rust `config/store.rs:722-747` 都对每次成功reload消耗版本，包括无内容变化的reload；`GatewayRuntimeRefreshBundle.ts:71-81,109-111,134-136` 说明RPC/轮前reload与无变化不广播。缺少step9→25逐次reload原因轨迹，不能指认哪次回调，也不能用之后外写解释此前9/10。根据实际beforeFile移除step26唯一4096新增行得到的重建字节，其SHA等于step25来源SHA；此为重建，非直接采集的step25全文。

原件已封装在 `scripts/bbx/fixtures/config-watch-exact/{ts-1,ts-2,rust}.json.br`；压缩前字节SHA分别为 `470a8eda78ffb832e517152e340cc95cc735e3359c877107c7b3a0fa96daa228`、`c855e606c8b817a008ea3dea8f82460cd5f63d0e87f5bd88c21f4742d245a0ff`、`6efc3c541a8cbd661b1244c73567f6cce24b7fcdb0a94b42e7053d602d734ba0`。原公共644严格DIFF2报告与绕场景accepted的归一化DIFF3并存于同目录；来源是已有真实捕获，不冒称新采或新候选通过。

## Rust 行为

产品 `d67705eece5a035b9f689809bf5b4088755ebcb8` 的 `src/pilotdeck-gateway/src/config/store.rs:25-43,722-747` 持续采样配置路径，成功重载推进版本。在这份真实原件中，step25版本10；step27写后约309ms有目标通知、version11→12、sourceHash从 `a41e135d307385c5fbe58ff10778207aa26a26dde90b852d7fbd24b342590e18` 推进至 `e8d7815fb5641a4b2a57490f824d727f5b2667491fb48c0072a8a8199a1cf261`，下一轮实际使用2048。

主provider已行使业务参数三侧相同：max_tokens序列 `[4096,8192,8192]`，flags轮reasoning_effort=high，清除后不存在；随后watch-rewrite唯一请求2048。公开snapshot存在redacted字段，不把公开config一致夸大为直接读取了所有内部字段。

## 范围

只适用上述产品/TS/harness和三原件完整字节。公共接受入口保持最窄原TS1→Rust配对，TS2作为强制sealed见证；self、TS2替代TS1、反向配对、未来采集、其他候选均不获许可。

三个路径：`$.steps[25].frames[0].result.version` 必须为number，TS1/TS2=9、Rust=10；`$.steps[27].externalConfigRewrite.observationExpired` 必须TS1/TS2=true、Rust不存在；同一对象 `observed` 必须TS1/TS2=false、Rust=true。其它类型、值、对象或step均拒绝。旧settings场景regex不改，本条不继承或冒称旧A6.3已获批准。

全部真实写入字节、sourceHash、前后snapshot/version、完整通知内容/归属/顺序、provider请求/响应/usage、durable链和原public ids逐侧有序内容锁定；不因公共normalizer忽略timing等而放宽这些固定证据。仅返回三条精确许可，不改写raw或视图。

## 机器判据

`scripts/bbx/config-watch-exact-policy.mjs` 自己读取并固定SHA验证两输入原始字节及第三份sealed见证；压缩原件与30文件644闭包在有界解压前后均核代码内固定SHA。closure保存固定driver/scenario源码与完整采集来源，当前复用helper和normalization再核原哈希。runtime不执行Git、不读取开发机 `_tmp`，不接受可替换manifest、调用者Map或声称身份的raw自授权。先重验完整语义和未变的有序raw内容，重新计算过滤旧场景规则前恰为三个原值差异，再输出精确路径。公共evidence验证对这个固定run另做完整内容/顺序核验。

`node --test scripts/bbx/config-watch-exact-policy.test.mjs`：37底层内容负控、39完整输入身份负控、1 helper+manifest替换负控，另加20个version/type/缺失/其它step/source/config直接语义负控；2个直接正控。附加压缩闭包/自洽替换/第三见证/调用者raw内容与键序/方向与self/公共evidence控制，以及真实公共CLI旧raw self、pair、改值拒绝、无Git环境运行。所有分母分开报告，不将身份SHA拒绝冒充底层语义拒绝。

固定三raw的公共validateArtifact、Rust `verifyConfigWatchInput`、TS self先执行；公共比较器逐叶打印 `COMPATIBILITY-VALUE fixedConfigWatchExactApproved`。既有场景regex保留，只有固定策略已消费的同一version叶不再重复计STALE；不改变其它规则。原DIFF2与原DIFF3保留，与新公共重判并列。

## 批准记录

2026-11-14主协调依据用户“项目所有决策你都可以自行决策”的授权，正式决定固定三原件的三叶差异，持久指针 `state/decisions-20261114.md` 的“settings-update：固定三原件的配置监听及版本计数差异”一节。这是授权范围内的主协调决定，不冒称用户逐条批准，也不借旧A6.3或regex批准状态。

私有原模块SHA `e11547d9f2e5f96f7043b14bc7a83a6df2dc9b0e40729d58e45b2094016978cd` 已经非作者Terra实际复验77负控/2正控；额外step25补证SHA `a9ec7c15cffc4489ed679bce0fd67f95d2a9cbbc74d17e8b7e04c385925a2515` 的20直接语义负控/1正控另行复验。公共迁移仍须非作者独审及主协调必要冻结登记；没有据此称整模块或新候选交付。
