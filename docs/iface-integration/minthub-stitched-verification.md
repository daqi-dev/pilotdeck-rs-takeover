# MintHub 20 包拉取后拼装测试方案与双平台实测结果（2026-11-16）

## 1. 先前的差距与本方案的目的

Wave 3 的 `--verify-install`（`proj-online`）在隔离项目目录中以 8 个顶级根包（零 `path:` 局部路径参数）从线上 MintHub 求解并校验了 60 个包（20 个 `pilotdeck-rs/*` + 40 个 `agentcpm/*` 与 `iface/*` 外部依赖），确认了 `mint.lock` 与包分发元数据完整性；但此前的 Cargo 编译与单测仍在工作区 monorepo `src/` 目录内直接运行，尚未把「从线上 MintHub 实际拉取到本地 `.mint/packages/` 的 20 个拆出包源码」在隔离目录中拆包重组并串联运行。

本方案交付了一套可重复执行的 **4 阶段 MintHub 下载包拼装与端到端实测体系**，确保全部 20 个 `pilotdeck-rs/*` crate 源码**100% 源自线上 MintHub 拉取后的 `.mint/packages/pilotdeck-rs/`**（零文件取自 `repo/src/pilotdeck-*` 实现目录），并在 **macOS aarch64** 与 **cb8（Linux aarch64）** 完成双平台实测。

- 编排入口：[verify-minthub-stitched-assembly.sh](../../scripts/verify-minthub-stitched-assembly.sh)
- 脚手架生成器：[pilotdeck_minthub_scaffold.py](../../scripts/lib/pilotdeck_minthub_scaffold.py)
- 跨包联合 E2E crate：[scripts/minthub-stitched-e2e/](../../scripts/minthub-stitched-e2e/Cargo.toml)（[stitched_packages_e2e.rs](../../scripts/minthub-stitched-e2e/tests/stitched_packages_e2e.rs)）

## 2. 四阶段拼装与测试架构

### Stage 1：纯远端拉取、每文件 SHA-256 校检与安装账本脚手架
1. 使用临时隔离的 `MINT_HOME`（不触碰 `~/.mint/credentials.toml` 或用户桌面环境）预热索引，随后以 **8 个顶级 `pilotdeck-rs/*` 根包**（由 20 个 `packages/minthub/*/mint.toml` 入度计算得出：`child-signals`、`regex-proto`、`yaml`、`model-iface`、`tool-iface`、`trusted-http-host`、`session-migration`、`iface`）执行纯远端 `mint add` 与 `mint verify`。
2. `pilotdeck_minthub_scaffold.py` 核对 60 个包的 `manifest.json` 逐文件 SHA-256 摘要，并生成内核 `agentcpm.kernel/install@1` 安装账本 `components/install.json`（声明 50 个兼容组件、按内核规则排除 10 个 `iface`/无制品/保留接口包；显式设 `umask 022` 并锁定目录 `0o755` / 账本与清单 `0o644`，避免 Linux `umask 0002` 下触发 `COMPONENT_LEDGER_WRITABLE`）及三个隔离构建区。

### Stage 2A：14 个纯包独立拼装工作区（零 `deps/agent-kernel` 依赖）
- 将 MintHub 下载的 14 个无内核适配器依赖包（`child-signals`、`regex-proto`、`tokenizer`、`yaml`、`protocol`、`context-core`、`http-transport`、`model-core`、`tool-core`、`transport`、`agent`、`context-runtime`、`model-runtime`、`turn-preparation`）拼为独立 Cargo workspace（`pure-build/`，不含任何 `deps/agent-kernel` 路径）。
- 验证所有纯语义核心、codec、bounded tool/compaction 与 tokenizer 在脱离 Kernel 源码树后仍可独立编译并通过完整单测（`1,044` 通过、`3` skipped、`0` 失败）。

### Stage 2B：线上 `agentcpm/iface@0.1.6` 兼容性探针（真实边界显影）
- 将下载的 `pilotdeck-rs/model-iface@0.1.0` 与 14 个纯包针对线上 MintHub 拉取的 `agentcpm/iface@0.1.6` 构建临时工作区（`hub016-probe/`）并执行 `cargo check -p pilotdeck-model-iface`。
- 预期结果：**非 0 退出（101）且精确产生 107 个编译错误**（缺少 `DriverTerminal`、`ModelLocalResourceRelease`、`env::http_pull`、`model_session` 等 `0.1.8` 新增公开项）。
- 该探针客观证明了当前版本轴边界：`https://mint-hub.ea.modelbest.co` 目前托管 `agentcpm/iface@0.1.6` 与 `agentcpm/host@0.1.6` 且尚未发布 `agentcpm/scheduler-protocol`；因此 6 个内核适配包的完整联编必须拼接固定提交的 `deps/agent-kernel@fb54c74a8cd6a3c6683d513bc4071cddc295c10e`（提供 `iface@0.1.8`、`host@0.1.7` 与本地 `scheduler-protocol`），直至上游完成 `0.1.8`/`0.1.7` 发版。

### Stage 3：全 20 个下载包拼装与全工作区 + Harness 实测
- 在 `full-build/` 中从 `.mint/packages/pilotdeck-rs/` 装入全部 20 个包并符号链接 `deps/agent-kernel@fb54c74a8`：
  - **Stage 3a**：按 `--release` 编译 `pilotdeck-native-loop`（`pilotdeck-rs/iface@0.1.1` 生产二进制）及 `managed_native`、`resident_native` 夹具二进制。注：实测发现在 Linux ARM64 上若将子进程二进制以 `debug` 模式构建，单文件会嵌入约 560–588 MiB DWARF5 调试信息，在未优化的纯 Rust `sha2` 下单次摘要计算耗时 55–80 秒，引发 `atomic_install` 30 秒 UDS 超时与 `managed_model --gate-child` 阻塞；切换 `--release`（并设 Stage 3b `nextest` `--build-jobs $JOBS --test-threads 4` 避免多进程 UDS 宿主竞争）后与 Wave 3 一致快速稳定通过。
  - **Stage 3b**：运行全 20 包工作区 `cargo nextest run`（1,309 passed, 3 skipped, 0 failed）。
  - **Stage 3c**：串联拼装态二进制运行 `pilotdeck-iface` 的 `resident_model`（3 checks）与 `managed_model`（4 checks + `gate-child` 7 组 checkpoint）端到端 harness。

### Stage 4：跨包端到端联合测试 `pilotdeck-minthub-stitched-e2e`
在 `clippy -D warnings` 零警告通过后，以 `--test-threads 1` 串行执行 [stitched_packages_e2e.rs](../../scripts/minthub-stitched-e2e/tests/stitched_packages_e2e.rs) 的 3 个跨包联合集成测试：
1. `test_01_minthub_install_site_ledger_mount_and_tamper_fail_closed`：读取脚手架生成的 `components/install.json`，验证 20 个 `pilotdeck-rs/*` 包均在账本中；篡改已下载包 `mint.toml` 时 `agentcpm-host` fail-closed 拒绝启动，干净账本 + 拼装编译的 `pilotdeck-native-loop` 正常拉起 UDS 常驻宿主、建会话并验证未授权 `shutdown` 返回 `-32013`。
2. `test_02_stitched_session_migration_context_cas_compaction_and_tokenizer`：串联 `pilotdeck-session-migration@0.1.0`、`pilotdeck-context-iface@0.1.0`、`pilotdeck-context-core@0.1.0`、`pilotdeck-context-runtime@0.1.0`、`pilotdeck-tokenizer@0.1.0`、`pilotdeck-protocol@0.1.0`：固定会话归属 -> 静止 JSONL 审计（4 条重构消息） -> `AuthoritativeContextLedger` 首次/幂等二次导入 -> `PilotDeckTokenCounter` 与 `pilotdeck-tokenizer` 等价断言 -> `PilotDeckContextContract::assemble` 只读投影（Micro + Snip）+ `trace_input` + `PilotDeckCompactionPersistence::commit` -> `handle_kernel_turn_failure` 确认绝不静默回退旧 runtime。
3. `test_03_stitched_host_native_loop_model_and_workspace_builtin_tools_e2e`：串联 `pilotdeck-tool-{core,iface}@0.1.2`、`pilotdeck-model-{core,runtime,iface}@0.1.0`、`pilotdeck-trusted-http-host@0.1.0`、`pilotdeck-http-transport@0.1.0`、`pilotdeck-iface@0.1.1` 与 `agentcpm-host`：先实测工作区 6 个内置工具（`write_file`/`edit_file`/`grep`/`read_file`、`../outside.txt` 越界拒绝 `path_not_allowed`、批切分），再将 6 工具通过 `ExternalToolBinding` 注册到常驻 `agentcpm-host`、挂载 `StitchedModelSetup`（`SharedPilotDeckModelFactory` + `TrustedHttpRuntime` + `ScopeExecutor` + 本地 mock SSE HTTP），通过 UDS 驱动编译态 `pilotdeck-native-loop` 与 `ProductController` 完成完整回合、`consumer_demand` 消费事件流、校验回环模型请求 1 次与资源 lease 释放及干净拆解。

## 3. 工作区底座与测试夹具透明清单

脚手架严格禁止复制任何 `repo/src/pilotdeck-*` 库或二进制源码（20 个被测包全部来自 `.mint/packages/pilotdeck-rs/`）；仅从仓库复制构建底座与 6 个跨 crate `include_str!` 测试只读锚点：
- 构建底座文件：`rust-toolchain.toml`、`LICENSE`、`src/Cargo.lock`
- 离线 vendor / 数据目录：`src/vendor/tiktoken-rs`、`src/vendor/icu_collator`、`src/data/icu_collator_data`
- 测试期只读锚点（仅供原测试 `include_str!`）：5 份 `src/pilotdeck-gateway/tests/fixtures/{output-schema/regress-oracle,sidecar-payload/oracle,compaction-orchestrator/oracle,edgeclaw-capture-turn/oracle,compaction-engine/oracle}.json` 与 `src/pilotdeck-cli/src/main.rs`（`protocol/tests/object_density.rs` 静态 pretty-print 锚点）
- 拼装适配底座：`deps/agent-kernel` @ `fb54c74a8cd6a3c6683d513bc4071cddc295c10e`（Stage 2A 完全不使用）

## 4. 双平台实测汇总（最终同一版本脚本与 E2E 用例）

| 阶段 / 指标 | macOS aarch64 (`darwin`) | cb8 (`Linux aarch64`, Cortex-X925) |
|---|---|---|
| Stage 1 `mint add` 锁定 / 校验 | `60` 包锁定，`mint verify` 60/60 | `60` 包锁定，`mint verify` 60/60 |
| Stage 1 `components/install.json` | declared=`50`, excluded=`10`, SHA256 `ef920219f5fefe31062f98d1ab7c81fd21caefe8b2ac7a85c51c5da6ccec156a` | declared=`50`, excluded=`10`, SHA256 `ef920219f5fefe31062f98d1ab7c81fd21caefe8b2ac7a85c51c5da6ccec156a` |
| Stage 2A Pure-14（零 Kernel） | `1044` run: `1044 passed, 0 failed, 3 skipped`（`30.270s`） | `1044` run: `1044 passed, 0 failed, 3 skipped`（`36.019s`） |
| Stage 2B `agentcpm/iface@0.1.6` 探针 | 预期失败（exit `101`, `107` compiler errors） | 预期失败（exit `101`, `107` compiler errors） |
| Stage 3a `--release` `pilotdeck-native-loop` SHA256 | `1682de2caecbf96c55ec32a5670d1b5c78c424a4fd13b4166fa02b2921f32290`（`1m 49s`） | `0ed42114e09232157d060c397acd3594efcbdb5fe2df902acf8766a33aa69d15`（`51.44s`） |
| Stage 3b Full-20 工作区 `nextest` | `1309` run: `1309 passed, 0 slow, 0 failed, 3 skipped`（`57.077s`） | `1309` run: `1309 passed (1 slow), 0 failed, 3 skipped`（`76.809s`） |
| Stage 3c `resident_model` + `managed_model` + `gate-child` | 3 + 4 + 7 checkpoint groups 全通过 | 3 + 4 + 7 checkpoint groups 全通过 |
| Stage 4a `clippy -D warnings` | 通过（0 警告，`25.91s`） | 通过（0 警告，`15.69s`） |
| Stage 4b `pilotdeck-minthub-stitched-e2e` | `3` run: `3 passed, 0 failed`（`13.083s`: `3.221s` / `0.508s` / `9.352s`） | `3` run: `3 passed, 0 failed`（`15.273s`: `2.663s` / `0.778s` / `11.831s`） |

## 5. 可复现执行命令

- **macOS aarch64**：
  ```bash
  bash scripts/verify-minthub-stitched-assembly.sh \
    --mint /Users/da/.local/bin/mint \
    --jobs 6 \
    --out-report-dir /Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c/_tmp/20261116-minthub-stitched-verify/macos
  ```
- **cb8（Linux aarch64）**：
  ```bash
  LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8 bash scripts/verify-minthub-stitched-assembly.sh \
    --mint /home/clawbox-08/ws/iface-integration-20261115/artifacts/kernel-ee4c6471-mint-chain/mint \
    --jobs 16 \
    --full-test-threads 4 \
    --out-report-dir /tmp/cb8-stitched-report
  ```
- 机器可读汇总输出位于 `_tmp/20261116-minthub-stitched-verify/{macos,cb8}/stitched-summary.json`；首败排查与修复细节归档于 `_tmp/20261116-minthub-stitched-verify/first-failure-debug-elf-sha256-and-handshake.md`。
