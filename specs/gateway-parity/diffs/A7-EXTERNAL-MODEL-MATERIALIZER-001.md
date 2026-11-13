---
id: A7-EXTERNAL-MODEL-MATERIALIZER-001
---

## 类别

bug 修复：修复外部 HTTP `modelProvider` 的合法 prepared invocation 被宿主 native router materializer 当作 `RouterDecision` 解引用时的基线崩溃。本条只明确该错误桥接的替代行为，不作为一般外部模型、context 或整个场景的豁免。固定候选42两平台已实际通过下列14项model控制及context投影控制；42整批仍117/129 FAIL，不能据此标整链交付。

## 基线依据

固定 TS `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：

- `resources/PilotDeck/src/composition/runtimePorts.ts:99-104` 以 HTTP client 创建 host model consumer；`src/agent/modules/llm/hostModelInvokerPort.ts:88-112,246-273` 的外部 prepare 返回请求、路由、可选 token limits 和传输身份，没有 native `RouterDecision` opaque。
- `resources/PilotDeck/src/agent/loop/nativeAgentTurnCapabilitiesAdapter.ts:90-101` 的原生产 capabilities bridge 存在 native router 时把 `prepared.opaque` 交给 router；`src/router/policy/RouterRequestMaterializer.ts:18` 读取 `decision.requestPatch`。
- `resources/PilotDeck/src/agent/loop/AgentLoop.ts:2508-2530` 明确定义没有 routing materializer 时 prepared execution fallback：复制 prepared request；仅更新 messages/cacheBreakpoints/cachePlan 和 prepared provider/model。:778 的准备后预算路径及 :2497 的预算候选请求路径会调用该物化方法。

实际本地 HTTP witness 为 `_tmp/20261114-external-materializer-baseline/esm-second/result.json`，SHA256 `df62418bf2ad0543195050b9ae75b6753c800acc4b59987be42e6fbab6de1616`；探针 `_tmp/20261114-external-materializer-baseline/witness.mts` SHA256 `d148cc43e633e238881aa6a93631d9685f4ed1fb938f719ad582aad93afcb387`；执行回收收据 `esm-second/receipt.json` SHA256 `35f72e7c44b4ddcbd03ac605c71773bc466e9f81c29a0507e12894074eeb4a04`，exit0、reaped=true、timedOut=false。

witness 实际走本地 mock HTTP prepare → 原 runtime ports/host model consumer → 原 capabilities factory → 原 RouterRuntime 默认 materializer；没有手工构造 undefined 直接调用 materializer。外部 prepared 没有 own opaque，原桥实际转入 undefined；原 materializer 抛 `TypeError: Cannot read properties of undefined (reading 'requestPatch')`，POST=1、native dispatch=0。此证据覆盖真实生产 factory 的最窄桥调用，**不是完整 Gateway/AgentLoop 预算 turn 的实测**，不证明每条 external happy turn 都触发故障。首个 `.ts` 探针的 CJS 装置失败保留在 `first/` 及 `first-receipt.json`，未以成功重跑覆盖。

## Rust 行为

本草案只读标注共享root当前实现；受测42产物保持不变。下列materializer行为不随后续stream-owned pending原parity修复改变，由根补最终集成源码SHA：

- `src/pilotdeck-gateway/src/native_process/external_modules/model.rs:28` 的外部 model port bundle 安装独立 `PreparedExecutionMaterializer`；:42-77 的 materialize 从 `prepared.request` 复制全部可枚举字段，只用 candidate 的 `messages`、`cacheBreakpoints`、`cachePlan` 覆盖，再用 `prepared.provider/model` 覆盖路由。candidate 缺少 cache 字段时以 own `Undefined` 覆盖旧字段，JSON 输出中不保留旧 cache。
- materializer 不读取 `prepared.opaque`，不传给 native router，不访问 HTTP client，不因 endpoint 广告 `materialize_prepared_request` 就新增远端调用。provider 专有参数、prompt、tools、输出上限及其他未知字段均来自 prepared execution。
- `src/pilotdeck-gateway/src/runtime/turn/host_factory.rs:704-732` 在选中外部执行时保留所选独立 materializer，仍安装原 native config metadata facade，并按原次序统一 durable/sticky 包装。模型 adapter 低级返回 metadata=None 由该装配填充；不解释为生产删除原 metadata。

作者冻结文件 SHA256：model.rs `808c11cef726b28deb8d77a4d13b1ba3f06d82b16fd072a3a01b6d3175885a2d`；model_tests.rs `ad7c2b56623d8b81831d674a027b7df931592f7acf702a0b76ea1ff7dc5066b0`。

## 范围

仅当生产选择 `modules.modelProvider` 的外部 HTTP 实现，且宿主需要对该外部 prepared invocation 重新物化执行/预算请求时适用。触发点包括准备后的预算评估，以及压缩/恢复的候选消息窗口评估；须有实际进入物化路径，不能把整个 turn 自动视为本差异覆盖。

允许的变化严格是：用原 AgentLoop 已有 prepared-execution fallback 代替错误地把外部 opaque 当 native RouterDecision 的桥接。prepared request 的完整控制参数保留；仅 messages/cacheBreakpoints/cachePlan 与 prepared route 发生上述投影。

本条不扩大以下行为：宿主 native config metadata、sticky 路由、durable 记录、权限、native router 自身 materializer、模型请求/usage/reasoning 事件、stdio/TCP 协议字段和错误投影。低级远端 `materialize_prepared_request` 可选方法继续保留其显式调用能力，不自动暴露到生产 wrapper；新本地 materializer不发额外 POST，也不向 request/context 注入本地 prepared ID。external context 在 native/stdio/TCP 的请求字段投影与 budgetPreparationId 边界修复按原 parity 合同验证，**不属于此批准差异**。

## 机器判据

精确测试文件为 `src/pilotdeck-gateway/src/native_process/external_modules/model_tests.rs`，新增两项：

- `native_process::external_modules::model::tests::production_materializer_preserves_prepared_controls_and_replaces_only_context_and_route`（:495）：prepared prompt/tools/maxOutputTokens/generation/reasoning/providerOptions/未知字段保留；candidate 只覆盖消息窗口与 cache，route 取 prepared；伪造 RouterDecision 形状的 opaque 不影响结果；即便广告远端 materialize，网络调用计数仍为0。
- `native_process::external_modules::model::tests::production_materializer_clears_absent_cache_fields_and_never_decodes_external_opaque`（:546）：candidate 缺少 cache 字段时旧值被 own Undefined 清除，stringify 后不含旧 cache；opaque 为 None、普通字符串和伪 decision 三类均不触发 native 解引用；网络调用计数为0。

两测试是本地 mock transport 控制，42整个model测试集14项在macOS和Linux均实际14/14 PASS；`host::modules::context::projection_tests::native_external_budget_projection_retains_request_after_real_preparation_validation` 也在两平台实际通过。Mac原件 `_tmp/20261114-mac-4d42d660/test-results.json` 中两精确测试为test-0036/test-0037、context投影为test-0073，均exit0；完整候选/二进制/命令/日志身份见同目录 `selection-frozen.json`、`final-receipt.json`（SHA `de579e76556de9e827f826a9586d26a0c678cb507d32d709ddaaeac93a689a02`）。Linux原件 `_tmp/20261113-e0-resume/4d42d660-selection-v2/tests-gateway-lib.log` 包含相同14项及context控制PASS；counts-audit/postflight与sealed binary身份并列保留。非作者S1逐名核过两平台实际日志，不把作者未自行运行误写成无人执行。

42两平台完整受影响批次均117 PASS、12 FAIL（129实际），其中public外部模块E2E仅1/8 PASS。5个内部60s worker deadline及工具POST前置未到达的共同model-stream进展问题另按P1修复；fixture缺必填messageId和公开失败的oracle问题另修。上述问题全部保留首败，不在本条范围内，不因14项model控制通过而当完整模型/预算/三种部署链通过。43完整实测仍须独立绑定新候选，不能借本条自动判绿。

验证须从固定源码生成并封存测试二进制，先fresh list核两精确名字非ignored再执行。重验保留实际命令、平台、源码/二进制SHA及第一次输出，不自动重试到绿。真实witness不是fallback成功金样；fallback期望来自固定AgentLoop原有逻辑和明确字段控制。

TS witness 原件可只读复算：在仓库根执行 `shasum -a 256 _tmp/20261114-external-materializer-baseline/esm-second/result.json _tmp/20261114-external-materializer-baseline/witness.mts _tmp/20261114-external-materializer-baseline/esm-second/receipt.json`，与“基线依据”三项比对。真实 witness 不是新fallback的成功金样；fallback期望取固定 AgentLoop 源码和上面明确控制字段。

## 批准记录

2026-11-14，主协调按用户“项目所有决策你都可以自行决策”及意向书基线崩溃默认修规则，批准并正式登记本条精确修复。持久决定见 `state/decisions-20261114.md` 的“外部 HTTP 模型：修复宿主物化时的基线崩溃”，以及其后的42实际验证登记。此处是用户授权范围内的主协调决定，不冒称用户逐字批准该实现。

Astra已核原生产桥、原fallback及对应实现，S1另核三份TS witness身份、42两平台14项model与context投影实际结果及精确范围。只追加本条，不修改历史差异、FROZEN或评分实现，不豁免整场景、未来无证据差异或新stream ownership P1。未闭合的正式入口、完整预算turn、stdio/TCP组合及最终发行验证继续由原验收要求承担。
