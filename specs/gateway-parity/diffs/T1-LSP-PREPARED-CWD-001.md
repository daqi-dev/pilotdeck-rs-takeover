---
id: T1-LSP-PREPARED-CWD-001
---

## 类别

与基线自身类型/文档矛盾的缺陷修复。已按授权精确范围追加登记，不与 T1-LSP-BOUND-SOURCE-READ-001 的读取竞态混为一个根因，不产生全场景判分豁免。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。`resources/PilotDeck/src/tool/execution-world/SandboxPort.ts:38-46` 将 SandboxedCommand 定义为含 executable、args、cwd、env 的 exact command，并要求准备器返回它实际执行约束的命令或拒绝。`resources/PilotDeck/src/lsp/provider/NodeStdioLspProvider.ts:57-66` 获取 prepareSubprocess 返回值后只将 executable/args/env 转交 LspProcess，未传返回 cwd；174-175 spawn 使用原 source.workspaceRoot。返回 cwd 与 sourceRoot 不同时，实际执行的工作目录与 exact command 合同不符。

固定 TS 首次真实 wire 的派生索引 `_tmp/20261113-lsp-stdio-independent-review/workspace-name-baseline-witness.json`，SHA256 `eddd7bdbb7a6d0f8ac7f61e7db616067624fc3841045469b44824e299fd50fbe`。本次独立复核其 driver/server/wire/owner 四个原件 SHA 均匹配：真实 callback 返回 sibling `changed-process-cwd`，实际 childcwd 为 `source-project`；initialize.rootUri=file:///workspace，initialize 与 workspace/workspaceFolders 回复中的 name 均为 source-project。完整 didOpen/hover/didClose/shutdown/exit，owner exit1、forced=false、remaining=[]；这是错误期望被真实基线击穿的首件 FAIL，保留不改绿。首次 child 完整 argv/env 未 journal，不能声称已实测它们，只能引用固定源码消费 executable/args/env 的事实。没有重采。

## Rust 行为

仅执行 prepareSubprocess 返回完整命令的 cwd：`src/pilotdeck-gateway/src/runtime/lsp/stdio.rs` 的子进程构造使用 command.cwd，同时保留已准备 executable、args、env。未准备执行端口时仍沿原 sourceRoot 的默认命令构造。

初始化 rootUri、workspaceFolders 的 uri/name，以及后续 workspace/workspaceFolders 回复，仍来自原 sourceRoot 的 workspace 身份；进程执行 cwd 与 LSP workspace 身份为两个具体合同，不相互替换。36候选已完成双平台编译与真实 prepared childcwd 回归，见机器判据；不表示整个候选批次全绿。

## 范围

精确限定 prepareSubprocess 实际返回 cwd 与原 sourceRoot 不同的 LSP 子进程启动。正常同 cwd 命令不应变化。不得顺便改参数解析、环境合并、URI 映射、source 读取权限、位置结果映射、超时、错误码或启动清理。

本条不声明任意自定义 sandbox 的安全性，不为 workspace 外 source 读取提供许可；与目录描述符读取安全修复分别验证。不得忽略整个 LSP scene，也不得允许未绑定的任意 childcwd。

## 机器判据

36候选的 `runtime::lsp::stdio::tests::bound_stdio_executes_prepared_command_but_keeps_source_workspace_name` 在 macOS 实际 exit0、Linux nextest PASS（0.037s）。此测试使用真实 Python stdio server，要求原 command/args 不可执行；prepared callback 返回可用 executable、实际 args、受控 env、不同 cwd 与 guest URI。真实 child 回报 cwd 等于 prepared cwd、preparedEnv 精确相等、argv 等于准备后的参数；initialize.rootUri/workspaceFolders 和服务器主动请求 workspace/workspaceFolders 的回复仍为原 source workspace 身份。真实 didOpen 文本与 guest source URI、宿主 locations 反映射、didClose/shutdown/exit 均被断言，dispose 后实际 PID 不存在。

源文件 `src/pilotdeck-gateway/src/runtime/lsp/stdio_tests.rs:542-677`；测试原件/命令/源码 hash 均在 `_tmp/20261113-lsp-bound-source-read-draft/final-draft/evidence-index.json`。未额外证明不可用 prepared cwd 的各 OS 逐字错误，也未对任意第三方 sandbox 声称安全性。该精确实际 child 测试不是仅检查 Command builder 的镜像断言。原 TS 首件期望错误导致 wrapper exit1 仍完整保留；未经篡改的 wire 证明 baseline cwd bug，不把它重命名为 TS formal PASS。


证据分层：36 候选 `caf2960026007f27b7fa2c14141e00e118252182` 的 macOS 编译/clippy 通过，562 个 nonignored 测试实际 557 PASS、5 FAIL；Linux 同源受影响批次实际 558 PASS、5 FAIL（总563）。五个失败均为另列的 LSP 装配生命周期问题，不能把这两个批次写成全绿。37 候选 `ae08d7bfa6c9ff7a721d438e338a06c4844070cf` 修复首次空 local automation publication 不应提前置换 project generation，双平台各实际8项7 PASS、1 FAIL；剩余 reload 测试错误地要求 dirty 旧 session 下一轮仍用旧 provider。固定 TS reload→markAllDirty→recreate 合同明确要求下一轮使用当前代际，38 候选 `d12879a016550c4cf77d9bdb5e80ab170c79e61b` 保留重建前旧 lease 存活、重建后旧代际恰好释放一次的强断言后，双平台各实际1/1 PASS，38未重跑其余七项。37/38 收据只证明相应 native 生命周期集成，不冒称已经完成新的 TS/Rust 全量 formal 对拍。

原始文件及测试源身份以`_tmp/20261113-lsp-bound-source-read-draft/final-draft/evidence-index.json` 为准：包含各原始日志/结果/命令/收据 SHA256、36/37/38 固定 Git 对象的相关源码 SHA256。读取日志重算哈希，没有重跑产品、测试或安全 probe。

## 批准记录

2026-11-14 主协调按本会话用户“项目所有决策你都可以自行决策”及 `.42cog/intent.md` 的基线 bug 默认修复授权，明确批准本条精确修复和公共追加。持久决定见 `state/decisions-20261114.md` 的“LSP：绑定来源读取与 prepared cwd 两项精确基线修复”。这是授权范围内的主协调决定，不冒称用户逐字批准了该具体实现。

非作者实际原件/源码复核见 `_tmp/20261114-lsp-diff-independent/review.md`，SHA256 `f1d7bf5a9992c0a3e7dc803a4945f2dbb3e5068d23cb165461b3b12c94556453`。本条只接受已列的窄行为差异，不修改冻结判分逻辑，不忽略整场景，不扩大未知平台或执行端口保证；真实验证范围及限制按前文。初始安全取证的汇总 turn 被工具终止历史仍保留，probe内部exit0不改述成整个turn自然完成；所有首件原值、失败退出码及候选分母保持。
