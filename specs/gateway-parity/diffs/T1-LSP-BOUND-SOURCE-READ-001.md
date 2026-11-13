---
id: T1-LSP-BOUND-SOURCE-READ-001
---

## 类别

安全缺陷修复。按主协调已授权的精确范围追加登记，不产生整场景判分豁免。修复对象是 LSP source 在边界检查后按可变路径重新打开造成的 workspace 外读取；不是一般符号链接禁用策略。

## 基线依据

固定基线 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`resources/PilotDeck/src/lsp/provider/NodeStdioLspProvider.ts:117-139`：realpath 检查（129）后，以原可变路径 stat（133）、readFile（137）；didOpen 在207。若检查后路径被替换，检查结果与实际打开对象可能不同。

实际受控取证原件为 `_tmp/20261113-lsp-toctou-characterization/first/result.json`（SHA256 `35a883e59205595e561ee6987eacb08d0c26fc1d6d27a4401a1d65f353233c84`）、`receipt.json`（`515d3fcc1289f3b579eb239ca711e3eb1c706bf6832614801e6e7fdabf75e9dd`）及 `wire.jsonl`（`f9029d35610a520f6ad7b3309246aeaff21d1a6e6d253f1382d29af68c985977`）。探针调用真实 NodeStdioLspProvider 与 NodeFsPort，仅在真实 realpath 返回后设置受控替换屏障；这是插桩安全特征取证，不是未经修改的正式对拍成绩。真实 stat/readFile 随后读取自有 workspace 外假 canary，didOpen.text 等于 `OWNED_FAKE_CANARY_NO_REAL_PRIVATE_DATA\n`，hover 正常返回；canaryReachedServer=true。子进程 didClose/shutdown/exit，childAlive=false；探针 exit0、forced=false。未读取真实隐私文件，不重采、不改原件。

## Rust 行为

已实现默认自有 NativeLspExecutionPort（`src/pilotdeck-gateway/src/runtime/lsp/types.rs`）：构造时绑定原 workspace 的规范根及目录描述符；解析后的相对 source 通过 `project_files::open_file_at` 从该描述符逐段打开，各段 NOFOLLOW，最终文件使用同一个已打开描述符 stat 和有界读取，拒绝检查后替换的链接和非普通文件。目录重命名或原路径被外来目录替换不得改变根描述符的归属。

合法 workspace 内符号链接先经过 realpath，随后打开解析所得的根内相对规范路径，仍应成功。只在真实 source 打开后确认的普通文件上读取，正式 stdio 使用配置的 maxDocumentBytes；最多读取 max+1 字节以检测增长后超限，不以早先 stat 大小截断并假报成功。此实现已在36候选完成双平台编译及下述精确测试；它不是整个候选的全绿声明。

错误沿用已有 LSP 错误通道：native IO/open 拒绝、非普通文件、超限为 LSP_MALFORMED_RESPONSE；取消为 ABORT_ERR / `LSP query aborted`。已解析到根外使用现有 `LSP source resolves outside the workspace: PATH`；非普通文件和超限分别保留 `LSP source is not a file: PATH`、`LSP source exceeds N bytes`。OS IO 文案不声明跨平台固定值，不新增 wire 字段，不把所有 realpath ELOOP 都当作替换攻击。

## 范围

仅默认自有 Unix native LSP source 读取与生产 stdio→native port 的安全边界（当前 macOS/Linux）。不修改合法 source 的 URI、languageId、文本、查询和正常 LSP 生命周期。普通根内及嵌套根内 symlink 保持允许；不扩大为任意 symlink 拒绝。

opaque/custom/guest LspExecutionPort 默认 bounded 方法仅有返回长度检查，不据此声明 OS 目录约束或分配上界；无 execution 的旧 stdio 路径也不自动获得本条保证。非 Unix 的 fail-closed 可用性需单列准确验证，不能冒称已有等价实现。本条不承诺 hardlink、mount namespace、同一文件并发写入的不可变快照或所有 LSP 扩展进程访问隔离。

## 机器判据

以下精确验收在36候选 macOS/Linux 实际通过。保留 TS 首件及假 canary 阳性，不将原件改成修复期望。

1. `runtime::lsp::types::tests::native_read_preserves_legitimate_inside_symlink_and_exact_limit`：合法根内 symlink 的5字节读取成功，4字节上限明确失败。
2. `real_barrier_rejects_source_symlink_swap_that_unprotected_read_leaks`：实际屏障后替换 source；未保护路径读取确见自有假 canary，受保护读取拒绝。
3. `original_root_descriptor_survives_rename_and_foreign_real_directory_replacement` 与 `workspace_realpath_is_the_original_pinned_root_after_logical_alias_replacement`：原根被 rename/旧路径被外来目录占据后，读取与 workspace identity 仍锚定原 owned 根。
4. `raced_fifo_is_nonblocking_and_rejected_as_nonregular`：真实 FIFO 在1秒内拒绝；`cancellation_before_or_at_open_is_explicit_and_owned`：打开前及屏障后取消都明确拒绝。
5. `actual_stdio_didopen_preserves_inside_symlink_and_never_leaks_raced_canary`：真实 stdio server 的 didOpen，合法 inside-link 发送 OWNED，protected-race 不发送 canary，故意退回不安全 read 的控制分支真实发送 FAKE_OUTSIDE_CANARY；实际子进程 PID 经安全 process probe 确认消失，不用文件存在性代替 reaped。
6. 以上七个 exact 测试在 macOS 的逐例 exit0 与 Linux nextest PASS 行均见 `_tmp/20261113-lsp-bound-source-read-draft/final-draft/evidence-index.json`。精确命令以原 command JSON/每条 command 数组为准，重验须使用隔离 HOME/PILOT_HOME/TMPDIR、已封测试二进制，不需或允许重采安全 TS probe。

限制：当前真实屏障覆盖 source 叶子链接替换及原根真实目录替换；没有单独证明所有中间目录竞态排列。max+1 逻辑存在且超限实际受测，但没有单独的“打开后并发增长”实际屏障收据；不据此宣称不可变 source 快照。真正 native model→LSP→next-provider 生产集成使用测试 provider，不能冒称这七项 stdio 安全测试本身经过完整 model journey。未经测试的零上限、非 Unix 平台和其他外部自定义执行端口不被列为 PASS。


证据分层：36 候选 `caf2960026007f27b7fa2c14141e00e118252182` 的 macOS 编译/clippy 通过，562 个 nonignored 测试实际 557 PASS、5 FAIL；Linux 同源受影响批次实际 558 PASS、5 FAIL（总563）。五个失败均为另列的 LSP 装配生命周期问题，不能把这两个批次写成全绿。37 候选 `ae08d7bfa6c9ff7a721d438e338a06c4844070cf` 修复首次空 local automation publication 不应提前置换 project generation，双平台各实际8项7 PASS、1 FAIL；剩余 reload 测试错误地要求 dirty 旧 session 下一轮仍用旧 provider。固定 TS reload→markAllDirty→recreate 合同明确要求下一轮使用当前代际，38 候选 `d12879a016550c4cf77d9bdb5e80ab170c79e61b` 保留重建前旧 lease 存活、重建后旧代际恰好释放一次的强断言后，双平台各实际1/1 PASS，38未重跑其余七项。37/38 收据只证明相应 native 生命周期集成，不冒称已经完成新的 TS/Rust 全量 formal 对拍。

原始文件及测试源身份以`_tmp/20261113-lsp-bound-source-read-draft/final-draft/evidence-index.json` 为准：包含各原始日志/结果/命令/收据 SHA256、36/37/38 固定 Git 对象的相关源码 SHA256。读取日志重算哈希，没有重跑产品、测试或安全 probe。

## 批准记录

2026-11-14 主协调按本会话用户“项目所有决策你都可以自行决策”及 `.42cog/intent.md` 的基线 bug 默认修复授权，明确批准本条精确修复和公共追加。持久决定见 `state/decisions-20261114.md` 的“LSP：绑定来源读取与 prepared cwd 两项精确基线修复”。这是授权范围内的主协调决定，不冒称用户逐字批准了该具体实现。

非作者实际原件/源码复核见 `_tmp/20261114-lsp-diff-independent/review.md`，SHA256 `f1d7bf5a9992c0a3e7dc803a4945f2dbb3e5068d23cb165461b3b12c94556453`。本条只接受已列的窄行为差异，不修改冻结判分逻辑，不忽略整场景，不扩大未知平台或执行端口保证；真实验证范围及限制按前文。初始安全取证的汇总 turn 被工具终止历史仍保留，probe内部exit0不改述成整个turn自然完成；所有首件原值、失败退出码及候选分母保持。
