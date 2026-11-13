---
id: C-DIAGNOSTICS-FIXED-PARSER-001
---

## 类别

经用户明确批准的固定 parser 兼容差异。仅下列三项诊断存在、位置与恢复数量差异；不是整个场景、全部诊断或任意原生 parser 行为的豁免。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85` 的 `src/tool/builtin/filesystem/syntaxDiagnostics.ts:147-166,238-266` 使用 TypeScript、parse5、csv-parse。真实 `tool-write-file-diagnostics` 两次 TS 原件自比 SAME，原 product `2aa207a55fc15d906c3fb5db647a1c06bddbf563` 与 harness `eb575d930fed5a0b6959eafe07b925df6cfc170c` 原 pair DIFF416 保留。

基线对固定 `bad.html` 无诊断；对未闭合引号 `bad.csv` 报开引号 L2:1；对固定 `bad.ts` 报三条恢复诊断。实际原件与逐层关联见 `_tmp/20261113-e0-resume/2aa207a5-affected-21-first/media-python-diagnostics-slot-2/run/`，原件身份写入公共 fixture manifest。本条不将派生测试输入冒充新的正式 capture。

## Rust 行为

`src/pilotdeck-gateway/src/runtime/tools/builtin/syntax_diagnostics/native.rs` 使用原生 fragment parser、CSV parser 与 Oxc；固定输入的 Rust 输出分别为一条 L1:1 HTML 诊断、CSV EOF L3:1、一条 L1:19 unexpected-token 诊断。真实诊断不能删除，文件写入及其他工具行为不变。

## 范围

仅 `scripts/bbx/diagnostics-policy-contract.json` 的 scene SHA、十五个实际写出文件及三个确切 tool ID／文本组合：

- `d/bad.html`：`<div><p>unclosed</div></span>\n`，TS 无诊断与 Rust 一条恢复诊断。
- `d/bad.csv`：`a,b\n"unterminated,1\n`，一条未闭合引号诊断的 L2:1 与 L3:1。
- `d/bad.ts`：`const a: number = ;\nfunction (\n`，TS 三条与 Oxc 首条恢复诊断。

许可包括这些准确工具结果在 provider、detail、durable、history、registry 中的已核复制，以及按 UTF-8、o200k、有界校准及真实序列化复算的有限派生值。许可仍要求固定合同中每个具体差异路径的左右值，不允许仅按路径或诊断前缀放行；未知差异、新输入、新 parser 输出和其他场景均不获许可。

## 机器判据

`scripts/bbx/diagnostics-policy.mjs::diagnosticsAcceptedPaths(rawLeft,rawRight,baselineRoot)` 只接实际 raw 对及基线路径。它使用公共原规则 `normalization.mjs` 内部生成 fresh views／完整差异，并使用 `loadPinnedTokenizer` 核实 tokenizer/ranks/base64 源哈希。不接调用方 views、differences、计数函数、passed 或 Map。

必须检查十五个实写文件对象、真实成功结果与 full detail、provider body 的准确三结果替换、JSON 字节长度、前序校准链、累计 o200k 差额、durable 实际 UTF-8 大小和 raw skeleton；416 个有限双值之外拒绝。`diagnostics-policy.test.mjs` 使用已扫描的压缩 synthetic raw 派生输入，含原正控及真实 raw 字段破坏控制。两份 fixture 保留行为及源身份，只移除既有 IGNORED 顶层 serverLog/startMs；转换、原件 SHA、解压 SHA 和明文扫描记录在 `fixtures/diagnostics-fixed-parser/manifest.json`。

本条自身不是正式重判通过证据；独审、冻结、真实 raw 正式重判及负控结果由主协调按各自实际身份记录。冻结 aggregate 与原首件不改。

## 批准记录

用户已明确批准上述 C 三固定诊断存在／位置／数量范围，持久记录见 `state/decisions-20261113.md:83`。批准候选绑定原私有 manifest `c58356b54a5860844e2bd45a0847dfae5b69e039b3509916c03724995e7cd1e4` 与可信 raw 入口 `627e56b5196ca493a79800507122c59902df5a73154cd512b79942d1020ec7fe`。公共模块仅迁移已批准的固定合同和关联校验，不追加其他许可。本批准不是产品发布、远端推送或全场景验收授权。
