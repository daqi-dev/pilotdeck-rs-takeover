# PilotDeck-RS / ifACE 集成与发包交接文档（2026-11-17）

> 面向接手 Agent 的**决策与上下文交接**（不含冗余环境连接指南）。按本交接执行时，如外层系统处于执行模式或双人约定，继续遵循 `AGENTS.md`（产品正文用中文，文件名用英文，严禁伪造测试结果）。

---

## 一、四步收口计划现状与验收标准

当前总体目标按 `docs/plan/008_mig_to_iface.md`（P0–P7）推进收口，分为 4 步：

| 步骤 | 当前状态 | 已完成部分 | 卡点 / 剩余工作 | 验收标准（Gate） |
| --- | --- | --- | --- | --- |
| **Step 1：代码收敛与 macOS 实测** | **进行中** | 1) Kernel `codex/iface-pilotdeck-host-20261115` 已提交推送 `1f203736`、`8ecdd2f9`、`45362ad3`（scheduler 软取消允许 `may_settle` + `CancelledFinished` + `CancelledNonSuccess`）；<br>2) 产品侧 `codex/iface-integration-20261115` 已将 25 个文件按 3 个逻辑提交落盘（`ddc5b8f0f`、`d982c8232`、`76f1eb042`），含 `resources.rs` 解包 `accepted_delivery_unconfirmed`、`model_session_codec.rs` 轮询/状态清理、`model_session_tests.rs` 3 模式交错单测；<br>3) `cargo fmt --all --check`、`pilotdeck-iface` `119/119`（含 `model_session` `23/23`）、`pilotdeck-session-migration` nextest、macOS `full_late_local_gateway_and_channel_auto_discovery_e2e`（`24.53s`）通过。 | `full_late_ws_5_context_roles_wait_approval_cancel_resume_e2e` 在 `cb8` 卡在 Turn 4 overflow；在 `d982c8232`/`76f1eb042`/`45362ad3` 之后 macOS 尚未重跑 `ws_5` E2E、`kernel_cli_default_e2e` 与 `3x --test-threads 6` 压测。 | macOS 上：<br>- `kernel_fulllate_e2e`（`2/2` `--test-threads 1`）PASS；<br>- `kernel_cli_default_e2e`（`2/2`）PASS；<br>- `kernel_resident_e2e` + `kernel_opt_in_e2e` 连续 3 轮 `--test-threads 6` 零失败；<br>- `cargo clippy --all-targets --features pilotdeck-gateway/kernel-resident,pilotdeck-cli/kernel-resident -- -D warnings` 与 `cargo fmt --all --check` PASS。 |
| **Step 2：`cb8` Linux aarch64 全量门禁** | **已开始** | `cb8:/home/clawbox-08/ws/iface-20261116/{agent-kernel,pilotdeck-rs}` 与热缓存 `/home/clawbox-08/ws/iface-20261116/_targets/pilotdeck` 已就绪；release `pilotdeck-iface --bins --examples` 构建 PASS；`cb8` `full_late_local_gateway_and_channel_auto_discovery_e2e` PASS（`54.13s`, `running_at_deadline=0`）。 | `full_late_ws_5_context_roles_wait_approval_cancel_resume_e2e` 在 Turn 4（`kernel_fulllate_e2e.rs:813`）FAIL；全量 CLI/压测/Kernel 全套/四条 `AGENTS.md` 命令/3 个 `check_frozen.py` 均待跑完。 | `cb8` 上：<br>- `kernel_fulllate_e2e` (`2/2`)、`kernel_cli_default_e2e` (`2/2`)、`3x -j 6` 压测 PASS；<br>- `agent-kernel` **零 `--skip`** 全通过：`fmt`、`clippy`、`agentcpm-core scheduler`、`scheduler_scoped_actions`、`scheduler_journal_cas`、`transport::scheduler`、`transport::model_session`、`agentcpm-protocol-types`；<br>- `AGENTS.md` 4 条全量工作区命令通过（仅允许唯一已知祖传失败 `unsafe_boundary::compiler_enforces_gateway_boundary`）；<br>- 3 个 `scripts/{parity,gateway-parity,ui-server-parity}/check_frozen.py` PASS。 |
| **Step 3：MintHub Wave 4 升版发布与双端 4/4 拼接** | **未开始** | 第 18 批 20 包已发布；已确认 `src/pilotdeck-iface/Cargo.toml` 新增依赖的 `pilotdeck-context-iface` 尚未写入 `packages/minthub/iface/{mint.toml,README.md}`。 | 待 5 个变更包升版、修 scaffold dev profile 转发、发包、补 `test_04` 并在双平台通过 4 阶段联测。 | 1) 升版 `context-iface` `0.1.0->0.1.1`、`tool-core` `0.1.2->0.1.3`、`tool-iface` `0.1.2->0.1.3`（钉死 `tool-core = "=0.1.3"`）、`session-migration` `0.1.0->0.1.1`（钉死 `context-iface = "=0.1.1"`）、`iface` `0.1.1->0.1.2`（补 `"pilotdeck-rs/context-iface" = "=0.1.1"`）；<br>2) `scripts/lib/pilotdeck_minthub_scaffold.py` 转发 `src/Cargo.toml` 的 `[profile.dev.package.*]` + 新增 `test_pilotdeck_minthub_scaffold.py` 且单测通过；<br>3) `stitched_packages_e2e.rs` 新增 `test_04_stitched_full_late_5_context_roles_and_workspace_tools_e2e`；<br>4) `scripts/publish-pilotdeck-mint-hub.sh` (`--plan`/`--publish`/`--verify-install`) 成功，且 macOS + `cb8` 双平台 `scripts/verify-minthub-stitched-assembly.sh` **4/4** PASS；<br>5) macOS 与 `cb8` 均 `test ! -e ~/.mint/credentials.toml`。 |
| **Step 4：第 19 批冻结、飞书同步与收口** | **未开始** | — | 待 Step 1–3 全部清绿后收敛状态、同步飞书、更新文档与 PR/MR 终态。 | 1) 修改 crate 内 grep `unimplemented!`/`todo!`/`NOT_IMPLEMENTED` 零命中；<br>2) 冻结 Batch 19（`_tmp/20261117-nineteenth-root/`、`state/iface-{integration,feature-metadata}.json`，外部 PR/MR 未被人工合入前保持 `iface.P0.upstream`/`iface.P1.contracts`/`iface.P7.retirement` 为非 `delivered`）；<br>3) 仅同步 3 张 `iFACE_*` 飞书表（`root_apply.py`，严禁 `--delete`），回读零差异；<br>4) 更新 `docs/iface-integration/{README.md,minthub-stitched-verification.md}`、`scripts/iface-integration-status/README.md` 及根仓 `/Users/da/ws/pilotdeck-rs-cc2c/state/backend-handoff.md` **仅第 `1..15` 行**（`tail -n +17` SHA-256 必须维持 `ab65b34f3dc806fde55259aa22038213cbcaec93180c97ca1359e9cc2ec275e7`）；<br>5) `gitleaks` 0 实密泄漏，更新 PR `#1` 与 CodeArts MR `!65` 描述。 |

---

## 二、`ws_5` E2E 调试链总结（含 `cb8` 真实失败行号勘误）

1. **Turn 5 取消归因丢失的历史根因**：
   - `abort_turn` 与在飞 `model.delivery_advance`（或 `ModelClose`）竞争时，Kernel `control.rs:277-322` 先将 `call.cancel_requested` 置 `true` 并取消资源，再向子进程 stdin 发送 `provider.interrupt`。
   - 对非 `may_settle` 请求（`model.delivery_advance`），Kernel `client.rs:65-73,574-578` 将 `STEP_AUTHORITY_REVOKED`/`CALLER_ENDED` 包装为 `Failure { code: "OUTCOME_UNKNOWN", message_key: "scheduler.resource.accepted_delivery_unconfirmed", params.cause }`；同时 `execution.rs` 原逻辑在 `cancel_requested == true` 时拒绝为 `State::Unknown` 步骤保留终态候选，导致 Kernel 退化报 `OUTCOME_UNKNOWN`、`turn_failed`。
2. **本轮已完成的跨仓修复**：
   - **Kernel（`codex/iface-pilotdeck-host-20261115` 已推送至 `45362ad3`）**：
     - `1f203736`：允许软取消步骤放行 `may_settle` 结算资源，并把 `Closing -> Unknown` 兜底延后到 `driver.wait_settled` 之后；
     - `8ecdd2f9`：在 `lifecycle_gate.rs` / `execution.rs` 增加 `StepCommitKind::CancelledFinished`；
     - `45362ad3`：增加 `StepCommitKind::CancelledNonSuccess`，使软取消原子 Loop 步骤的 `State::Unknown`/`Failed` 保留真实 `error`（`scheduler.outcome_unknown.loop_requested_reconciliation`）与 `settlement_data`。
   - **Product（`codex/iface-integration-20261115` 提交 `d982c8232`）**：
     - `src/pilotdeck-iface/src/resources.rs`：新增 `soft_cancelled_port_error`（50x10ms 轮询等待中断信号）与 `is_delivery_advance_cancel_rejection`（解包 `accepted_delivery_unconfirmed` 嵌套 `cause`），在软取消且无先验效应时短路返回 `PortError::Cancelled`；
     - `src/pilotdeck-iface/src/model_session_codec.rs`：`advance_model` 在 `SessionEnd::Closed` + `ResponseEnd::Interrupted` 下允许有界轮询（`interrupted_polls < 50`）；`resource_failed` 在 `ModelPull` 收到 `PortError::Cancelled`（`Phase::Advance | Phase::Read`）时清理 `self.pending = None`；
     - `src/pilotdeck-iface/src/model_session_tests.rs`：新增 `delivery_advance_cancelled_interleaving_converges_to_cancelled_not_unknown`（3 种交错模式）与 `model_pull_delays_on_closed_session_end_with_interrupted_response`，本地 `pilotdeck-iface --lib` `119/119`（`model_session` `23/23`）全绿。
3. **`17:42 cb8` 实测结果与重要勘误（避免接手者误判）**：
   - `task-8199.log`（第 `780-846` 行）实测结果：
     - `full_late_local_gateway_and_channel_auto_discovery_e2e`：**PASS**（`54.13s`，`running_at_deadline=0`）；
     - `full_late_ws_5_context_roles_wait_approval_cancel_resume_e2e`：**FAIL**（`472.09s`）。
   - **勘误**：日志第 `831` 行出现的 `kernel_fulllate_e2e.rs:903` panic **不是** Turn 6 resume 失败，而是主线程 `client_out.lock().unwrap()` / `join().unwrap()` 转发客户端线程的 panic。**客户端线程实际失败于 `kernel_fulllate_e2e.rs:813`（Turn 4 `"TRIGGER_OVERFLOW"`）**（`task-8199.log:826-827`）：
     - 前 3 轮（Turn 1 `TRIGGER_READ_FILE`、Turn 2 `TRIGGER_DENIED_BASH`、Turn 3 `TRIGGER_ALLOW_BASH`）均已跑过；
     - Turn 4 触发 `MockOpenAiServer` HTTP 400 `context_length_exceeded` 后，网关发出 `model_request_started` / `context_budget` / `model_selection_changed`，随即报 `"Host execution failed: Loop requested reconciliation"`（即 `scheduler.outcome_unknown.loop_requested_reconciliation`，`finishReason: "model_error"`）；
     - 换言之，Kernel commit `45362ad3` 已成功把遮蔽诊断的 `OUTCOME_UNKNOWN` 还原为真实 `Loop requested reconciliation`，但也正因 Turn 4 中止，**Turn 5（`870` 行 abort）与 Turn 6（`885` 行 resume）在真实 E2E 中尚未被执行到**；macOS 上在 `45362ad3` + `d982c8232` 之后也尚未重跑该 E2E。
4. **Turn 4 根因排查焦点与 472s 耗时解释**：
   - **Turn 4 嫌疑链 A（`src/pilotdeck-iface/src/model_session_codec.rs`）**：HTTP 400 `context_length_exceeded` 被上游 model endpoint 转为结构化 `error` 事件后，`events.source_error_seen = true`；紧接着 TurnDriver 调用 `HostCall::ModelClose`，若满足 `advance_after_source_error`（`973-994` 行），`ModelClose` 进入 `Phase::Advance` 发送 `model.delivery_advance`。若 `model.delivery_advance` 被后端拒绝或此时已闭合，`reconcile_failure`（`643` 行）仅匹配 `HostCall::ModelPull` 而拒绝为 `ModelClose` 调和；或 `Phase::Read`/`CloseRead` 下落到 `505` 行 / `585` 行直接返回 `Err(self.unknown())`，触发 `Loop requested reconciliation`。
   - **Turn 4 嫌疑链 B（`src/pilotdeck-iface/src/context_codec.rs` & `recovery_binding.rs`）**：若模型错误阶段顺利收敛、进入 `RecoverFromModelError` 子能力调用，`ContextCodec`（`488-693` 行）或 `recovery_binding.rs`（`120-150` 行）任意协议/状态断言失败也会调用 `self.retain_unknown` 返回 `AcceptedUnknown`。
   - **为什么 `cb8`（128 核 Linux）也跑了 54s / 472s**：`src/Cargo.toml` 已给 `sha2`/serde/kernel 配好 `opt-level = 2/3`，release `pilotdeck-native-loop` 仅 16MB；真正耗时原因是 `opt_in_kernel.rs:984-995` 将 `CONTROLLER_BINDING` 设为 `Kind::Service`，而 Kernel `native.rs:437-443` 对每次 `provider.invoke`（每条 `TurnEvent`，一轮十余次）都冷启动并 `shutdown` 一次 `pilotdeck-native-loop --pilotdeck-product-controller-v1` 子进程（Turn 1–3 加启动已耗 ~470s）。`ws_fulllate` E2E 主 `WAIT` 常量在 `kernel_fulllate_e2e.rs:25` 为 `180s`，注意单步若逼近 180s 也有超时风险。

---

## 三、建议接手者的下一步（进程内测试先行）

1. **5 分钟快速定性 Turn 4（直接利用 `cb8` 热缓存）**：
   - 将当前最新提交的 `pilotdeck-rs` 以瘦身 `rsync`（见第五节红线）同步到 `cb8:/home/clawbox-08/ws/iface-20261116/pilotdeck-rs`（`$CARGO_TARGET_DIR` 已预热）；
   - 打开 `RUST_LOG=warn,pilotdeck_iface=debug,agentcpm_core::scheduler=debug` 单跑 `PILOTDECK_FULLLATE_E2E_CHILD=ws-fulllate`，先抓出 Turn 4 返回 `AcceptedUnknown` / `Loop requested reconciliation` 的精确调用点（区分是 `ModelSessionCodec` 的 `ModelClose`/`advance_after_source_error`，还是 `ContextCodec` 的 `RecoverFromModelError`）以及确切的 `cause`/`method`。
2. **进程内单测红→修→绿（参考 `model_session_tests.rs` 做法）**：
   - 若确认是 `ModelClose` / `advance_after_source_error` 问题，在 `src/pilotdeck-iface/src/model_session_tests.rs` 新增"HTTP 400 `source_error_seen` 后 `HostCall::ModelClose` 竞态/终态读取"确定性单测；若是 `RecoverFromModelError` 链路，在 `src/pilotdeck-iface/src/context_port_tests.rs` 补复现用例；同时也可补测"abort 后同 `sessionKey` 下一轮 resume 遇 Kernel/codec 状态残留收敛"用例。
   - 用几秒钟级的 `cargo test -p pilotdeck-iface --lib` 红→修→绿后再上真实 E2E。
3. **E2E 与全量流水线收口**：
   - 先单跑 `ws_fulllate`（`--test-threads 1`）确认 Turn 4 恢复、Turn 5 abort（`870` 行）与 Turn 6 resume（`885` 行）**全部真正跑到并通过**；
   - 随后依次完成 Step 1（macOS 全套）、Step 2（`cb8` 全套，包括无 `--skip` Kernel 与四命令）、Step 3（Wave 4 5 包发布 + 双端 `4/4` 拼接）、Step 4（Batch 19 飞书 3 表 + `gitleaks` + 文档 + PR/MR 终态）。

---

## 四、完整待办清单

- [ ] **定性并修复 Turn 4 (`TRIGGER_OVERFLOW`)**：确认 `Loop requested reconciliation` 准确诱因，在 `model_session_tests.rs` / `context_port_tests.rs` 补进程内单测红→修→绿，随即每步小 commit。
- [ ] **本地 macOS `ws_5` E2E 实测**（`d982c8232`/`76f1eb042`/`45362ad3` 之后首次重跑 `kernel_fulllate_e2e` `2/2` `--test-threads 1`，确认 Turn 4、Turn 5 abort `870` 行、Turn 6 resume `885` 行与 SIGINT drain 全过）。
- [ ] **macOS 剩余项**：`kernel_cli_default_e2e` (`2/2`)；`kernel_resident_e2e` + `kernel_opt_in_e2e` 连续 3 轮 `--test-threads 6` 压测；`cargo clippy --all-targets --features pilotdeck-gateway/kernel-resident,pilotdeck-cli/kernel-resident -- -D warnings`。
- [ ] **`cb8` Linux 全量门禁**：`kernel_fulllate_e2e` (`2/2`)、`kernel_cli_default_e2e` (`2/2`)、`3x -j 6` 压测；`agent-kernel` 无 `--skip` 跑通全套（`fmt`、`clippy`、`scheduler`、`scheduler_scoped_actions`、`scheduler_journal_cas`、`transport::scheduler`、`transport::model_session`、`agentcpm-protocol-types`）；`AGENTS.md` 四条工作区全量命令（仅记录已知 `unsafe_boundary::compiler_enforces_gateway_boundary` 1 条失败）；3 个 `check_frozen.py`。
- [ ] **MintHub Wave 4 发布与双平台 4/4**：
  - 升版 `pilotdeck-context-iface` (`0.1.0->0.1.1`)、`pilotdeck-tool-core` (`0.1.2->0.1.3`)、`pilotdeck-tool-iface` (`0.1.2->0.1.3`)、`pilotdeck-session-migration` (`0.1.0->0.1.1`)、`pilotdeck-iface` (`0.1.1->0.1.2` + 补 `"pilotdeck-rs/context-iface" = "=0.1.1"` 到 `packages/minthub/iface/{mint.toml,README.md}`)；
  - `scripts/lib/pilotdeck_minthub_scaffold.py` 转发 `[profile.dev.package.*]` + `scripts/lib/test_pilotdeck_minthub_scaffold.py`；
  - `scripts/minthub-stitched-e2e/tests/stitched_packages_e2e.rs` 新增 `test_04_stitched_full_late_5_context_roles_and_workspace_tools_e2e`；
  - `scripts/publish-pilotdeck-mint-hub.sh` 发布并在 macOS + `cb8` 通过 `scripts/verify-minthub-stitched-assembly.sh` `4/4`，两端确认 `test ! -e ~/.mint/credentials.toml`。
- [ ] **飞书第 19 批同步 + 交接收尾**：
  - 占位符扫描归零；
  - 冻结 `_tmp/20261117-nineteenth-root/` 与 `state/iface-{integration,feature-metadata}.json`（外部 PR/MR 合入前保持 `iface.P0.upstream`/`iface.P1.contracts`/`iface.P7.retirement` 非 `delivered`）；
  - `scripts/iface-integration-status/root_apply.py` 仅同步 3 张 `iFACE_*` 表（禁 `--delete`），回读零差异；
  - 更新 `docs/iface-integration/{README.md,minthub-stitched-verification.md}`、`scripts/iface-integration-status/README.md` 及根仓 `/Users/da/ws/pilotdeck-rs-cc2c/state/backend-handoff.md` 仅 `1..15` 行（校验 `tail -n +17` SHA-256 = `ab65b34f3dc806fde55259aa22038213cbcaec93180c97ca1359e9cc2ec275e7`）；
  - `gitleaks` 扫描通过；推送并更新 GitHub PR `#1` 与 CodeArts MR `!65` / `!64` 状态。

---

## 五、资产与风险清单

### 1. 代码工作区与最新基线提交
- **Product 集成 worktree**：`/Users/da/.codex/worktrees/pilotdeck-iface-integration/pilotdeck-rs-cc2c`
  - 分支：`codex/iface-integration-20261115`
  - 代码落盘提交：`ddc5b8f0f`（dev profile 优化）、`d982c8232`（codec 取消归因/队列硬化）、`76f1eb042`（gateway/cli FullLate 与 E2E）、随后为本交接文档提交。
  - `deps/agent-kernel` 符号链接指向 `/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel`（**切勿 stage**）。
- **Kernel 工作tree**：`/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel`
  - 分支：`codex/iface-pilotdeck-host-20261115`
  - `HEAD`：`45362ad384720c69fb6c14b03b0b0d823a66c781`（已推送至 `origin`，含 `1f203736`、`8ecdd2f9`、`45362ad3`）。
  - 推 CodeArts 须带 `GIT_SSH_COMMAND='ssh -o ProxyJump=cb8'`。
- **远端 `cb8` 环境**：
  - 工作目录：`/home/clawbox-08/ws/iface-20261116/{agent-kernel,pilotdeck-rs}`
  - 预热编译缓存：`CARGO_TARGET_DIR=/home/clawbox-08/ws/iface-20261116/_targets/pilotdeck`
  - `mint` 二进制：`/home/clawbox-08/ws/iface-integration-20261115/artifacts/kernel-ee4c6471-mint-chain/mint`

### 2. PR / MR 与关键配置路径
- **GitHub PR `#1`**：`https://github.com/daqi-dev/pilotdeck-rs-takeover/pull/1`（`codex/iface-integration-20261115`）
- **CodeArts MR `!64`**：`https://devcloud.cn-north-4.huaweicloud.com/codehub/project/5a5fe37f080d4dcdab39d8b3ea837bb1/codehub/3102476/64/mergedetail?source=codex/complete-input-inspection-pr&target=master`
- **CodeArts MR `!65`**：`https://devcloud.cn-north-4.huaweicloud.com/codehub/project/5a5fe37f080d4dcdab39d8b3ea837bb1/codehub/3102476/65/mergedetail?source=codex/iface-pilotdeck-host-20261115&target=master`
  - MR 文案与一键更新脚本目录：`/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel/_tmp/20261116-kernel-host-mr/`（含 `mr-body.md` 与 `update_mr.py`）。
- **MintHub Token**：`/Users/da/ws/pilotdeck-rs-cc2c/.env.local` 中的 `minthub_token`（严禁入库或写入 `~/.mint/credentials.toml`）。
- **飞书 iFACE 绑定配置**：`/Users/da/ws/pilotdeck-rs-cc2c/.git/worktrees/pilotdeck-rs-cc2c3/project-status/iface-integration.json`；同步脚本 `scripts/iface-integration-status/root_apply.py`。
- **MintHub 相关脚本**：
  - `scripts/publish-pilotdeck-mint-hub.sh`
  - `scripts/verify-minthub-stitched-assembly.sh`
  - `scripts/lib/pilotdeck_minthub_scaffold.py`
  - `scripts/minthub-stitched-e2e/tests/stitched_packages_e2e.rs`

### 3. 关键红线与未验证风险
1. **绝对禁止同步 `_build/` 与 `_tmp/`**：本地 `pilotdeck-rs` 含 `19G _build`、`14G _tmp`、`50G src/target`（`agent-kernel` 含 `16G _build`、`4.5G _tmp`、`9.7G target`）。向 `cb8` `rsync` 时必须完整排除：`--exclude='target/' --exclude='src/target/' --exclude='_build/' --exclude='_tmp/' --exclude='.git/' --exclude='deps/' --exclude='node_modules/'`。
2. **禁止干扰 `/Users/da/ws/pilotdeck-rs-cc2c`**：另一并行 Agent 正在根仓 `codex/backend-completion` 工作；本线只在收敛后、确认无冲突的前提下编辑其 `state/backend-handoff.md` **第 `1..15` 行**，不得改动第 `17+` 行或其他工作树文件。
3. **`mb-login` 凭证已过期**：本机 `mb-login` 临时凭证已于 `2026-11-16T21:34:08Z` 失效，更新 CodeArts MR `!65` 网页描述前需要人完成交互式 `mb-login huaweicloud` 登录；但 SSH `ProxyJump=cb8` 推送代码不受影响，`45362ad3` 已在 MR `!65` 源分支生效。
4. **macOS SIP `/bin/sh` 限制**：macOS 本机跑 `agent-kernel` 全套时，仅有 3 个拷贝 `/bin/sh` 的测试因 Apple AMFI 限制必须 `--skip`（`preadmitted_native_worker_can_start_after_stop_but_remains_owned`、`real_native_failure_requires_empty_inventory_and_closes_only_its_original_transaction`、`cancelled_parent_cannot_mint_a_failure_cleanup_grant`）；Linux `cb8` 必须 **零 `--skip`** 全通过。`agent-kernel` 上游已有 3 个 `transport::server` macOS 用例不在本任务范围内，勿改 `transport/server`。
5. **未验证事项**：Turn 4 overflow 修复、Turn 5/6 E2E 在真实环境跑通、`kernel_cli_default_e2e`、`3x -j 6` 压测、`cb8` 无 `--skip` Kernel 与工作区四命令、MintHub Wave 4 发包与 `4/4` 拼接、飞书第 19 批同步，均**尚未完成/未验证**。
