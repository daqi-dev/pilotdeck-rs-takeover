---
id: H1-FEISHU-STOP-DERIVED-001
---

## 类别

已批准停止所有权 bug 修复的精确派生差异，以及单独批准的固定 SDK 诊断输出差异。停止修复依据既有 `H1-FEISHU-STREAM-OWNERSHIP-001`；本条补充真实工具回执及其字节、token、持久副本的严格机器合同，不泛化允许所有 usage、日志或停止输出。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/adapters/channel/feishu/FeishuChannel.ts:226-233` 只在存在 `wsClient.stop()` 时调用它。实际 SDK `@larksuiteoapi/node-sdk@1.65.0` 的 `lib/index.js:89444-89471` 暴露 `close()`，没有该 `stop()`；原连接在配置禁用及 adapter reload 完成后仍回原 socket 的 exact-nonce Pong。正常 channel 回复及 reaction delete 均已先于禁用完成，然后独立 observer 使用真实 bash 工具读取 probe。

原 sealed TS/Rust 证据在 `_tmp/20261113-e0-resume/22-special-first/feishu-stop/run/`。TS artifact SHA256 `295a2ef31529167f98dde87526eb8788e03bc38dac4fddf7b5bb4f3b60dfd178`，raw SHA256 `6c1a21c325fa61b05b4f1cd48d297d3762cd0b73b9ed9f483661937ca14b3acd`。两侧 strict 及 TS self 已有效；原普通比较 `DIFF 64` 保留。

SDK 的三个 startup info 组分别来自 `lib/index.js:87814` 的 event-dispatch ready、`:89488` 的多行持久连接订阅说明、`:89217` 的 ws client ready；`:87857` 产生 `no not.registered handle` warn。这是 SDK 诊断，不是 Node runtime warning。固定 SDK 完整入口代码 SHA256 `50e8421dc44eaf648078706b41cd2aea3390667d17a3db3f4e6ce88f431b60b1`。

## Rust 行为

`src/pilotdeck-gateway/src/channel/feishu/mod.rs:1487-1506` 的 stop 等待 owned stream 清理。原件中禁用后实际观察到原 socket 的 EOF、peer 写半边 finish、真实 close，再成功返回 dead probe；不把超时或无 Pong 单独当作关闭证明。Rust artifact SHA256 `ac3e40d772bc7d3807f2e79c681790571bcfed3447128bfff59666355b665967`，raw SHA256 `fe76d3224a02f21d584987e82e81dc33f5f5bde67d7686fc5fc68454fa696739`。

真实 probe 回执由 TS 的 `alive:true / exact-nonce-pong` 变为 Rust 的 `alive:false / socket-close`，随后进入实际工具结果、下一次模型请求、持久记录和 projection。Rust 不伪装原 socket 仍活来保持派生数值相同。本条不修改产品行为。

## 范围

仅 `backend-feishu-stream-stop` 的 normal 变体、固定场景 SHA256 `55c8036ee20997dc37dc1f9ca0c898fc659212618ae0388cbd52613594c2bcb4`，且原严格业务与 socket 身份合同均成立时，许可常量模块列出的精确 64 叶：

- 62 个停止所有权因果叶：2 probe 字段、26 个完整工具回执及复制叶、1 HTTP content-length、31 个预算与复制叶、2 facts raw skeleton 计数。
- 单独的 2 个诊断字段：`steps[0].serve.stdout` 与 stderr。仅许可三个固定 startup info 组和一个固定 warn 组，各一次且阶段正确；删除这些精确文本后，其余输出在既有比较规则下必须逐字相同。

停止派生的原件数值为 stdout 112/109 bytes、完整结果 421/418 bytes、o200k 结果文本 110/107 tokens、messages 159/156、local total 10522/10519。同一实际 usage/calibration 的 12/10375 经既有 half-clamp 得 used/display/usage 5261/5260，ratio 为 used/57344。这里是本地估计校准派生，不能扩大为任意 provider usage 差异。

仅上述固定路径允许，完整 ordinary diff 必须恰好等于该集合。任意其他日志、ACK、请求正文、tool 结果、完成事件、序列、元数据或未知叶仍失败；TS self 不适用。原条目的分片分配上限等独立合同不受本条改变。

## 机器判据

`scripts/bbx/feishu-stop-policy.mjs` 的同步入口 `feishuStopAcceptedPaths(tsArtifactPath, rustArtifactPath, baselineRoot)` 自行读取同路径原 artifact、receipt、raw，重核内容 hash、固定基线/场景与 strict verifier SHA256 `38f9a48d631db21491fecba3ee83513d71b737c5eda7a59e714e00ee23063d87`。继续执行原 verifier 的 exact-nonce、disable→EOF→finish→close→probe、正常业务/权限/ACK/回复和实际 mock drain 合同。

模块直接绑定共享 normalizer/load，不接受 artifact 提供的 callback、passed marker 或外部改写 view。它独立复算每侧完整工具文本和 26 个副本、UTF-8 和 HTTP JSON 字节数、固定 tokenizer lite/rank 身份与 canonical message/toolcall/toolresult token、calibration 公式与 31 个预算副本；两份 JSONL 的 40/20 行序列、父链、完整 byte/hash、raw fingerprint、projection anchor 全字段和所有 view asOf；完整 facts 序列化骨架。随后核完整双侧 ordinary diff 精确集合才返回路径。

`scripts/bbx/feishu-stop-policy-constants.mjs` 保留精确 64 路径、三个 SDK info 组和一个 warn 组；不能靠放宽正则或动态增加路径让新差异通过。SQLite ExperimentalWarning 仅沿用公共比较器已有规则，不属于本条新许可。

测试 `scripts/bbx/feishu-stop-policy.test.mjs` 通过 `FEISHU_STOP_ARTIFACT_ROOT` 读取外部 sealed 原件，固定基线由 `BBX_BASELINE` 指定；原件不入产品。真实 pair 正控必须导出 64 路径，26 个重新计算 transport hash 的内容破坏负控必须拒绝，包括真实 TS 错 nonce、缺 EOF/finish、错误 close 顺序、回执/byte/provider 变化、token/calibration/ratio、持久副本/witness/anchor ISO、SDK 文本/阶段和额外日志。缺外部原件的 skip 不算正控通过。公共接线和最终 harness 重判结果由根协调者另行保存，不将条目批准当执行证明。

## 批准记录

已批准。持久批准记录为 `state/decisions-20261113.md:71`，用户原文：“批准 B 组精确范围（推荐）”。该决策文件本次批准版本 SHA256 `7fc1080a459bfa781f031f3b7a734996b54e72cbf7dc3c348085174c54cb1ffd`。用户于 2026-11-13 明确批准整批方案 B。根协调者随后原话授权：“用户已明确批准B：Feishu stop62严格派生+固定SDK3info1warn的2叶，以policy-ready c3a3/POLICY-CANDIDATE a873版本为准。”并授权迁移公共模块、新增本条及测试；公共 diff 唯一集成由 Protocol 负责。

批准依据的私有候选为 `_tmp/20261113-feishu-stop-final-triage/policy-ready.json`，SHA256 `c3a3d6c7f40cde23b639da16a58bc880708a145d57c3b8a40ce6b828ca38a80f`；`POLICY-CANDIDATE.md` SHA256 `a87386738b194632dc0de311e00868df804920d71faa26dabb07688a014a048b`。该版真实 sealed pair 正控、26 内容负控及 SDK 窄独审已完成。迁移只改变模块位置、可信共享函数绑定和外部测试输入方式，不能扩大已批 62＋2 合同。

原开发首败、原 DIFF64、历史 TS 连接泄漏和修复前停止失败证据保留。此批准不免除最终公共 harness 身份、原件重判与破坏测试，也不等于产品全量或最终发行已通过。
