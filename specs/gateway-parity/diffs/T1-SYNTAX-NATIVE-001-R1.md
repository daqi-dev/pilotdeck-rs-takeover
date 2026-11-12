---
id: T1-SYNTAX-NATIVE-001-R1
original: T1-SYNTAX-NATIVE-001
---

## 类别

有意改进。沿用原条目用户已授权的原生写后诊断有限文案差异；本条补正式入口机器判据和具体定位边界，不豁免诊断能力缺失。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，Linux Node 22.22.0，正式 gateway submit_turn → write_file/edit_file → provider 二次请求 → transcript/投影/文件落盘。harness ad6884c02e8a318b89763fe8d3744df81fe9feea 的 `t1-diagnostics-{write,edit}-{js,ts,html,csv}` 八场景先完成 TS 自比。原始对拍证据在 cb8 任务目录 `logs/14351f4e-compare-harness-ad688/slot-{1,2}/`；本机完整副本 `_tmp/20261113-e0-v0/diagnostic-raw/`，原始差异在 `compare14351/`。

JS `const value = ;` 与 TS `const value: number = ;` 分别报告 L1:15、L1:23 error；HTML 重复属性为 L1:13 error；CSV `a,b\n"unterminated,1\n` 为 L2:1 error。对应好文件均无诊断，坏/好写入均成功且字节正确。

## Rust 行为

受测产品 14351f4e1d53b2262015cfea484d811342204ba3 同样成功写入并报告 error，JS/TS 定位相同，说明为原生 Oxc 诊断。HTML parser 给行号而无列号，按原条目列记 1（此夹具 L1:1）；CSV 未闭合引号使用 EOF 位置（此夹具 L3:1）。对应好文件无诊断。

## 范围

仅上述八个固定夹具、`call_bad` 的完整成功结果诊断块。保留 Created/Updated 前缀、edit 的 replacement 数量、error severity、行列结构、非空说明、完整详情和落盘字节。行列仅允许上述明确位置：JS L1:15、TS L1:23 两侧相同；HTML TS L1:13/Rust L1:1；CSV TS L2:1/Rust L3:1。CSV 起点/EOF 定位仅适用于这份未闭合引号夹具，不泛化任意位置。

允许同一已验证诊断串在事件、provider 请求和 transcript/投影中的复制，以及由它严格重算得出的 UTF-8 resultBytes、JSON 请求 Content-Length、o200k token 差和既有有界 calibration/ratio 投影。其余 provider 字段、headers、权限、事件、结构化补丁、文件内容、请求/结果数量和键序仍逐项比较。没有路径通配或整个 output/provider 忽略。

## 机器判据

`scripts/bbx/syntax-policy.mjs::syntaxContract` 每侧检查真实二次 provider 请求、坏/好结果、严格诊断格式/位置、完整工具详情及正确落盘；以场景 `syntax-contract` 正向断言执行。`syntaxAcceptedPaths` 独立读取固定基线 js-tiktoken 的 o200k 编码，重算实际诊断 token 差、校准值和所有接受的派生数值，按具体值检查后才登记单个差异路径。报告逐项保留 raw a/b 和接受原因。

`node --test scripts/bbx/syntax-policy.test.mjs scripts/bbx/evidence.test.mjs` 41/41 通过。另对八份真实原始产物作离线比较器重放，8/8 仅命中上述已登记差异；每份注入 severity 改变、错盘、任意 provider model 改变、任意 token 改变共 32 个真实 comparator 子进程，32/32 拒绝，0 假绿。证据 `_tmp/20261113-e0-v0/diagnostic-policy-replay/`。这些离线重放不改写原始 run manifest，也不冒充新版 harness 已完成正式产品执行；新版固定 harness 的正式自比/对拍另记 E0-V0 报告。

## 批准记录

沿用用户 2026-11-13 明确执行 plan.md / docs/plan/backend-completion.md 中“JS/TS、HTML、CSV/TSV 写后诊断允许有限的文案差异，但必须保留诊断能力”的授权。主协调者 2026-11-13 明确要求按该授权增加严格正向判据、逐项派生核验和负控，并明确同意上述 CSV 固定夹具起点/EOF 边界；未扩展到其他输出或协议差异。
