# 第五批 Kernel 变更追踪与接入解读

本批确认了可信 HTTP 准备事实的公开携带方式，并修正了未开始 HTTP 时的真实资源清理。它为 PilotDeck 的受管模型接入补齐必要接口和资源规则；真实产品组合根是否安装了这些实现，仍需单独证明。Kernel 测试通过的范围、公开声明、产品接线和端到端交付保持分开。

本解读来自对冻结 Git 对象与已有运行收据的独立只读审查；该审查未新跑 Cargo 或产品测试。下列链接保留审查当时的草稿原件：[kernel-fifth-trace.draft.json](/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-iface-fifth-trace/kernel-fifth-trace.draft.json)，5 条逻辑变更见 [kernel-changes-additions.json](/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-iface-fifth-trace/kernel-changes-additions.json)，全部提交文件对象、行号和 SHA 见 [sources.json](/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-iface-fifth-trace/sources.json)。以上原件保留历史身份。正式变更来源已保存为 [iface-kernel-preparation-cleanup-5d6.json](../../state/iface-kernel-preparation-cleanup-5d6.json)，并随第五批三表同步完成回读。本文仍只解释本批截止点，不将后续验证反填为本批结果。

## 冻结范围与变更计数

基点为 `f7931007c72f087170589aab1c603a837c37ab01`，本批终点为 `5d6d4569628625ec8f251beb13f5a9f2ab0c9477`。逐提交核对四个提交，归为五条逻辑变化。第五批已将正式 Kernel 变化从 69 条追加为 74 条。

| 冻结提交 | 实际变化 | 逻辑 ID |
|---|---|---|
| `190ad94add55fc40a82874e239ee5294eb14e058` | 不透明 issuer/token 与请求准备字段；公开 API、env 身份和消费者 pin | `iface.change.http-preparation-opaque-issuer-token`、`iface.change.http-preparation-env-identity-registration` |
| `d551a172745d7c7d3f7da1b79ddc9c7724775dc4` | 11 个测试文件或测试模块补齐合法绑定、权限、feature 与制品前提 | `iface.change.valid-binding-feature-fixtures` |
| `1559b84b86faf67624c9f71d03658dec3e68b391` | 两项真实 ledger/submit/worker 的 duplex 关闭回归 | `iface.change.graceful-close-duplex-queue-regression-tests` |
| `5d6d4569628625ec8f251beb13f5a9f2ab0c9477` | 删除零 HTTP 的虚构 EmptyCleanup，要求底层真实回执，补回归 | `iface.change.prepared-zero-http-real-cleanup-receipts` |

`be4dcf56` 和 `856305e4` 是截止点之后的 fixture 修改。本批不纳入它们，也不以这些后续修改将旧测试失败改写为通过。`1559` 只补测试，不重复把上一批 `52a7` 的生产排空机制登记成本批新增实现。

## 接入时要分清的四件事

**运行所有权回答“现在是谁、有权运行哪一代作业”。** Kernel 已有的受管边界按原始 generation、job、session、principal、deadline、同源取消令牌、backend 身份和原准入 permit 核对。当前策略、owner、lease、取消、父调用或委派变化仍会影响后续执行。仅造出同名字符串、另一个取消令牌，或拿到准备 token，都不会获得这份运行权。冻结源码为 `5d6:kernel/host/src/transport/model_session/execution/http_scope.rs`，`check` 第 56 行及其各项检查。这是本批解释所依赖的既有机制，未登记为新增生产变化。

**可信准备回答“这份产品私有准备事实是谁认可的”。** `190` 在 `kernel/iface/src/env/http_pull.rs` 第 17、29、37、54 行提供 `HttpPreparationIssuer`、`issue`、`inspect` 和 `HttpRequestPreparation`。可信组合根创建 issuer，并把同一个 `Arc<issuer>` 交给可信 materializer 与 adapter。materializer 将私有状态封入 token；adapter 先比较 issuer 内部身份的 `Arc::ptr_eq`，再检查私有状态类型。自行新建的 issuer 即使签发了同类型状态，也不能被真正的 adapter 接受。

issuer 自身不实现 `Clone`，`Default`/`new` 各造独立身份；组合根共享的是外层 Arc。token 可克隆并共享同一状态，Debug 恒为隐藏内容的固定文本。最后一个 token 丢弃时状态可以被释放，但这只说明对象存活关系，不能作为运行授权有效或网络资源已释放的证明。API 没有判断 provider、origin、秘密或策略本身是否正确；这些准备事实须由产品可信组件产生并验证。

**单次 poll 资格回答“这一次继续执行是否仍被允许”。** 受管 Host 在 head/body poll 前重查运行条件；实际 DNS、拨号、重定向和连接池复用也要由可信出网路径核查。一次准备成功不能变成持续授权。冻结 `http_scope.rs` 第 339、394 行可见 head/body 的逐次检查；iface 文件第 3 行明确可信宿主每次派发及响应 poll 的责任。这里的“每次”是一项持续边界，不表示本批新造了一个可携带的 poll 授权票据。

**清理回执回答“哪些资源确实已经回收”。** `HttpExchangeControl` 是单次 attempt 的本地停止和回执；`HttpScopeCleanup` 汇合同一原作业 scope 的 attempt、池和辅助工作；`ManagedModelLifecycle` 还负责产品 connector 全局池、reactor 和 helper 的宿主退出收敛。它们覆盖的资源范围不同。业务完成、head/body 报错、发出 stop、调用 backend close 或取消 job 都不能代替对应的真实 Released 回执。Released 也不证明 provider 远端计算停止。

## 公开接口、安装身份与真实组装分别意味着什么

`HttpPullRequest.preparation: Option<HttpRequestPreparation>` 位于 `190:http_pull.rs` 第 76 行。Option 表示该字段的值可以是 None，**旧 Rust 结构体 literal 仍必须补这个字段**。原 env 0.1.9 的旧 pull literal 会出现 E0063，不能声称源码兼容。既有消费者须显式写 `preparation: None` 或可信的 Some；受管模型 adapter 应要求 Some，并使用真正共享的 issuer 校验。普通 driver 仅负责搬运，不应从请求内容自行推导 provider 或把 None 当作已经可信准备。

活动版本是 carrier `agentcpm/iface 0.1.8`、`iface/env 0.1.10`、`iface/types 0.1.11`、`iface/model 0.1.7`。本批只把 env 从 0.1.9 提到 0.1.10，更新了 env schema 与 8 个活动消费者的精确 pin；其余三轴保持。carrier 0.1.8 是本轮未发行候选，不能写成已发行。冻结 README 的概览表仍有旧 env/types 数值，本稿版本以活动 manifest 与实际登记收据为准。

carrier 提供 Rust crate 源码；身份包和精确依赖声明用于安装时的契约 peer 选择。API 指纹保留前 2,198 行原样，再增加 6 行，说明登记范围可追踪。它们均不证明真实产品已装配。指纹提取器不采集 inherent 方法，故 `new`、`Default`、`issue`、`inspect` 的实际可用性另看源码和外部 consumer 收据。

已有公开 Host 入口 `serve_resident_with_managed_model` 位于 `5d6:kernel/host/src/startup/embed.rs` 第 258 行，接受 `TrustedModelSetup`。Host 在 Shared 已构造且 worker/accept 尚未派发时安装 setup；transport/factory builder 在 HostState 锁外运行，目录与工厂随后原子发布。接口要求产品提供 transport 与 lifecycle，使用 Host 的可信 binding resolver 和 egress。这个安装能力是已有机制，可用于本批产品组装。

**本批未证明 PilotDeck 已使用此入口接好 issuer。** 截止 `5d6`，对 Kernel host、iface 和 `products/pilotdeck` 的冻结源码搜索中，Host 只有两处 `preparation: None` 的测试 literal；`HttpPreparationIssuer` 的 `issue`/`inspect` 使用集中于 iface 实现和其单元测试。公共字段和 manifest 不构成真实 materializer→token→adapter 的生产接线证据。后续产品组合根须实际持有同一 issuer、签发私有准备事实、安装强制校验的 adapter，并验证真实 CLI/Gateway/Loop 到受管 Host 的路径。

## 零 HTTP 为什么仍要清理

`5d6` 之前，cleanup 等待 `raw.start` 调用返回后，如果没有记录到 attempt，就构造 `EmptyCleanup` 并报告 Released。这把“未开始 HTTP”误当成“准备阶段没有资源”。实际 adapter 可以在 HTTP 之前拥有 lease、pool 或 helper。

本次在 `http_scope/cleanup.rs` 第 191–206 行修正：先保持 scope 关闭，拒绝新的 start；如果还有正在进行的 `raw.start` 调用，底层 close 暂缓；当这些调用落定，**无论是否曾成功 start，都调用底层 `close_scope`**。底层 callback 仍在锁外执行，晚到 attempt control 仍会跟踪并停止。这里不能表述为 close_scope 一返回就立即完成 raw cleanup。

聚合器只有在底层回执为 Released 且全部正在开始和已登记 attempt 都结算后，才报告 Released。底层尚未建立回执时为 Held；底层失败或不支持时为 Unknown，pending 不能减为零。多位 `released()` 等待者要随真实 owner 释放而被通知。已经关闭的 scope 即使权限恢复也不能重新派发。

新增准备 owner fixture 真实持有一个可观察 Drop 的 lease。它在零 start 下测试：外来取消源不能清理；原 claimed owner 撤权后仍能清理；重复 close 共享同一 handle 且底层只调一次；owner 尚存时 Held、pending=1；两个 waiter 挂起；释放 lease 后两者收到 Released、pending=0。另验证已 settled 的原 owner 仍可清理，以及 Unsupported 保 Unknown。它是 Kernel 测试 adapter 的资源证明，仍不能替代 PilotDeck 的真实 provider adapter 验收。

## fixture 与 duplex 回归的实际意义

`d551` 没有放宽生产准入。method-table、scheduler、TLS 模型测试补显式 `model=scripted` 和 fixture endpoint，避免先撞“缺绑定”而测不到原目标。账本用 0600、合法安装根用 0700，避免继承 umask 提前失败；权限拒绝规则不变。合同版本断言改用真实 `IFACE_VERSION`。Wasm 关闭 feature 时验证“不宣称支持且明确拒绝”，开启时保原 feature 用例。

运行期本地 Wasm 用例改成自行创建无效字节的未授权文件，证明 `WASM_NOT_ALLOWED` 在读取、解析或发布能力之前发生。原误导性的“安装真实 Wasm 并在下一边界使用”测试名同步改为“装载前拒绝”。这组 PASS 只证授权拒绝和零半装，不证明真实 Wasm guest 执行；真实 guest 源码构建仍受 WASI libc `-lc` 阻塞，不能用安装负控 PASS 抹掉。

`1559` 经真实输入账本和 `execution::submit` 接收 duplex boundary，放入真实 worker 队列；即使 generation 已报告终结，graceful close 仍要等 pending boundary 结算。放开队列后，过期 boundary 被丢弃，不调用 backend commit。投递失败则清掉 reservation，避免关闭永久等待。backend close 调用发生仍不等于 backend 确认资源释放：测试明确保留 `backend_resources_released=false`，不宣称 `closed_by_backend:true` 或伪造 Closed。

## 实际验证、分母与未闭合项

| 实际源码身份与范围 | 已有实际结果 | 解释边界 |
|---|---|---|
| `190` iface 全 lib | 225 PASS；preparation 6 是其中子集 | 6 项不可再加到 225 上；接口测试不是产品组装 |
| `190` Host native 全 lib | 1,730 PASS / 52 FAIL / 0 ignored | 原始全量失败保留 |
| `190` Host default 全 lib | 1,765 PASS / 52 FAIL / 0 ignored | 实际 feature 含 default、wasm-host；两组分母不相加作独立定义 |
| `190` 生产 lib、外部后端、相关 fmt/Clippy | 收据所列范围 PASS | 不等于上述全 host 测试 PASS |
| `5d6` native 受影响矩阵 | 369 个独立测试，365 PASS / 4 FAIL | 局部矩阵，未跑 default 或全 host 闭合 |
| `5d6` model_session | 267 PASS，HTTP scope 25、continuation 9 均在其中 | 较 `190` 262 净增 5：新增 6 个名字、替换掉 1 个旧名字 |
| API 注册 17 项 | 14 项退出 0，3 项预期负控退出 1 | **实际 HEAD 是 f793 dirty WIP，不是 190 重跑**；3 项是旧指纹、私有字段、旧 literal 负控 |

API 注册记录的三个业务源文件 SHA 与冻结 `190` 对象已逐一比对相等，可说明该段源码的静态适用性，不能改写实际运行身份。独立 rustc consumer 验证公共构造、同源/异源、类型、共享状态、Debug 和存活关系；它不运行 Host、不做真实 HTTP，也不是跨平台发行验收。`190` 的 canonical 6 项 preparation 测试另有实际 Linux 收据。

`5d6` 仍有 `tool-env` 5P/1F（空账本 `COMPONENT_GATE_NO_KERNEL_COMPONENTS`）和 remote 3 项失败（env-b 注册超时），native all-target Clippy 也因 unused `guest_sha256_component` 失败。9 项 package-install 负控 PASS 只代表实际选择的九项。原 process 2 项、TLS slow-consumer 失败未复跑或宣告修复；真实 Wasm guest 构建仍受 `-lc` 阻塞。

remote 失败的原 fixture 丢弃子进程输出，也未在超时前观测自然退出，原运行原因保持未观测。后续独立启动诊断发现 bwrap probe 的 `RTM_NEWADDR Operation not permitted`，自然退出 1 且连接 0；该诊断不是原 remote3 重跑，不可反填原始失败原因。使用的 exec-host 实际构建源码仍是 `190`，生产依赖闭包静态相等的证据可说明适用范围，不能重标为 `5d6` 构建。

核心收据为 [190 summary](/Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c/_tmp/20261115-iface-linux/kernel-190ad94a-summary.json)、[5d6 summary](/Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c/_tmp/20261115-iface-linux/kernel-5d6d4569-summary.json)、[5d6 detailed receipt](/Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c/_tmp/20261115-iface-linux/kernel-5d6d4569-native-affected/receipt.json) 和 [API 注册 summary](/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-http-preparation-api-registration/summary.json)。本稿的 [integrity-check.json](/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261115-iface-fifth-trace/integrity-check.json) 核对了 91 个现存 receipt/log 文件、冻结源对象及 API 前缀；它是一份只读完整性检查，不能算作新的运行验收。

第五批正式表已追加这 5 项和实际 run 引用，原 69 项保留。相关合同的五层状态保持原有判定，尤其 `productionWired`、`jointlyVerified` 保持 UNKNOWN；新增证据只说明已覆盖的局部接口、实现、可注入与测试范围，不能由这批 Kernel-only 结果将产品任务记为完成。接下来必须由产品真实组合根承担 issuer/materializer/adapter/lifecycle 的安装和联合入口验收。
