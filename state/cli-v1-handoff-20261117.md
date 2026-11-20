> **归档说明**（2026-11-20）：本文件由 `_tmp/20261117-agentcpm-pd-v07/handoff.md`（84 行，2026-11-17 WP0–WP4 交接清单）原样归档而来，内容未改动。性质：历史交接记录。其中勘误与待裁决事项（`pendingDecisions` 等）的后续状态以 `state/cli-v1-audit-20261117.md` 与 `state/cli-v1-decisions.json` 为准。

# agentcpm-pd S1+S2 CLI & Gateway v1 交接清单（2026-11-17）

> **2026-11-17 交接核查后的勘误与增补**：本文件两处陈述已被只读核查更正（Kernel 版本、WP4 完成口径），并新增两项待人裁决。全部证据与去向见 `state/cli-v1-audit-20261117.md`（正本，两仓同步）；待裁决项已登记 `state/cli-v1-roadmap.json` `pendingDecisions`，收口任务已登记 `state/cli-v1-design.json`（`cli.task.wp4-commit-gate`、`cli.task.governance-registration`）。接手后先读审计记录再动手。

## 1. 工作树与仓库总览

1. **主设计/治理工作树**：`/Users/da/ws/pilotdeck-rs-cc2c`（分支 `codex/backend-completion` @ `cf143ff81`）
   - **Stage A 已全部完成并同步**：
     - 设计文档：[007_agentcpm-pd-cli-architecture.md](file:///Users/da/ws/pilotdeck-rs-cc2c/docs/plan/007_agentcpm-pd-cli-architecture.md)（`v0.7`）
     - 台账与 Schema：`state/cli-v1-{design,operations,command-tree,public-contract,capabilities,tasks,decisions,roadmap}.json` 与 `state/cli-v1-sources/plan-v07.json`
     - 校验与同步脚本：[check_s0.py](file:///Users/da/ws/pilotdeck-rs-cc2c/scripts/cli-v1-status/check_s0.py)、[check_s0_schemas.cjs](file:///Users/da/ws/pilotdeck-rs-cc2c/scripts/cli-v1-status/check_s0_schemas.cjs)、[manage.py](file:///Users/da/ws/pilotdeck-rs-cc2c/scripts/cli-v1-status/manage.py)
     - 飞书多维表格 `IrwwbSz6tawEBXs4FTicINdJnrd`：已校验 `manage.py check` 与 `manage.py plan`（`0` diff）。
   - **注意**：该工作树保留着其他后端条线的未提交修改，**严禁**对其执行 `git reset`、`git checkout --` 或 `git add -A`。

2. **Stage B 实现工作树**：`/Users/da/ws/pilotdeck-rs-agentcpm-pd`（分支 `codex/agentcpm-pd-cli-20261116`，基于 `b42bfdd97`）
   - 依赖软链：`deps/agent-kernel` -> `/Users/da/.codex/worktrees/kernel-iface-pilotdeck/agent-kernel`。**勘误（2026-11-17 核查）**：实际 HEAD 为 `8ecdd2f9`（`fb54c74a` 的后代，含 5 个 scheduler/host 修复），并非本条原先所写的 `@ fb54c74a`；与设计 §11.3/cb8 钉值不一致，待人裁决（`pendingDecisions/20261117-kernel-pin`）。
   - 未动红线文件：`src/pilotdeck-cli/`、`src/pilotdeck-gateway/src/methods/mod.rs`、`src/pilotdeck-gateway/src/native_process/assembly.rs`。

## 2. 已提交里程碑与当前工作区状态

| 阶段 | 提交 / 状态 | 覆盖模块 | 实测验证结果 |
| :--- | :--- | :--- | :--- |
| `WP0-init` + `WP1a` | `6bdda17f0`（已提交） | `src/agentcpm-pd-protocol`（`id`, `errors`, `contract`, `wire`, `envelope`, `framing`）与 `agentcpm-pd-client`/`cli` 脚手架 | `cargo test --offline -p agentcpm-pd-protocol` 9/9 通过；3 个 `check_frozen.py` 通过；`unsafe_boundary` 45/46（仅 Linux `/proc` 项需 `cb8` 或 `PDRS_UNSAFE_BOUNDARY_ALLOW_NON_LINUX=1`） |
| `WP1b` | `9d7dbcc7e`（已提交） | `src/agentcpm-pd-protocol/src/{registry.rs, schema_validator.rs, lib.rs}`（285-op 注册表、28 `currentRound`、11 `control`、`schema.list`/`schema.get`、Draft 2020-12 校验器） | `cargo clippy --offline -p agentcpm-pd-protocol --all-targets -- -D warnings` 0 warnings；`cargo test --offline -p agentcpm-pd-protocol` 13/13 通过 |
| `WP2` | `9dda2a75d`（已提交） | `src/agentcpm-pd-client/src/{error, config, idempotency, autostart, ws_client, lib}.rs`（D7/D8 目标解析、`0700/0600`、幂等账本、`flock` 单胜者自启动、`/v1/ws` 状态机） | `cargo clippy --offline -p agentcpm-pd-client --all-targets -- -D warnings` 0 warnings；`cargo test --offline -p agentcpm-pd-client` 4/4 通过 |
| `WP4` | **工作区未提交**（已完成编码、`fmt` 与 `clippy --lib` 检验） | `src/pilotdeck-gateway/src/{lib.rs, native_ws.rs, http_proxy.rs, public_v1/{mod,store,runtime,ws,bootstrap}.rs}` | `standalone_v1_gateway_enforces_bearer_auth_and_serves_ws_initialize_and_gap` **PASS**；`all_28_ops_and_s3_capability_unavailable_and_crash_recovery_validate_against_schemas` 此前定位到的两处 bug 已在工作区修复（见下），因额度切换交接尚未重跑测试，因此暂未 commit。**2026-11-17 核查更正**：WP4c 的 `submit_owned` 未接入（run 托管走 public_v1 内置离线 RunDriver，待人裁决 `20261117-wp4c-submit-owned`）；gap 帧零断言；提交前验证须按 §7.1 升格（见审计记录） |

### `WP4` 最新修复点说明（接手后首跑验证）
1. `src/pilotdeck-gateway/src/public_v1/runtime.rs`：`PublicV1Runtime::dispatch` 已将 `!spec.is_current_round => -32004 CAPABILITY_UNAVAILABLE` 提前到 `IdempotentSubmit`（`-32086`）和 `validate_input` 之前（在 `bind_instance_id` 之后）。
2. `src/pilotdeck-gateway/src/public_v1/store.rs`：`V1Store::resume_queue` 修复了在仍持有 `self.conn.lock()` (`mut conn`) 未 drop 时再次调用 `self.conn.lock()` 导致的自死锁，改为复用 `&conn` 调用 `build_queue_json` 后先 `drop(conn)` 再 `notify_waiters()`。

## 3. 关键技术约定与避坑记录

1. **离线锁依赖与 Rust 2024 限制**：不添加任何新外部 crate，Cargo 命令统一带 `--offline`；Rust 2024 edition 中 `gen` 为关键字；工作区启用 `unsafe_code = "forbid"`，文件锁统一用 `std::fs::File::lock()`，进程存活与 `SIGTERM` 用 `rustix::process`。
2. **`/v1/ws` HTTP 升级细节**：
   - `http_proxy.rs:1219` 原仅对 `/ws` 发放 `ReadLease`，已扩为 `matches!(request.uri().path(), "/ws" | "/v1/ws")`。
   - `native_ws.rs` 与 `public_v1/bootstrap.rs` 通过 `acceptor::websocket_response` 返回 `101` 后，必须使用 `tokio_tungstenite::WebSocketStream::from_raw_socket(socket, Role::Server, None).await`，**不可**再调用 `accept_async`。
3. **契约细节**：
   - 条件 `instanceId` 注入：仅 `op.declares_instance_id == true` 时注入/校验；`schema.list`/`schema.get`/`terminals.create` 含 `additionalProperties: false` 且未声明 `instanceId`。
   - `run.submit` 入参 `input` 类型为 `[Part]`（如 `[{"kind": "text", "text": "..."}]`）。
   - `Identity` Schema 差异：`instance.initialize` 必填 `nodeId`；`service.start`/`service.status` 不含 `nodeId`（`additionalProperties: false`）；`system.capabilities` 允许可选 `nodeId`。
   - Offline `RunDriver`：通过正常文本或 `[pd-driver:sleep=...]` / `[pd-driver:fail]` / `[pd-driver:approval]` / `[pd-driver:question]` / `[pd-driver:events=N]` 支持无外网密钥测试全状态机、交互暂停与慢消费者/控流压测。
4. **远端验证机**：
   - `cb8` 在线（`rustc 1.99.1`，Kernel 已就绪于 `/home/clawbox-08/ws/iface-20261116/agent-kernel` @ `fb54c74a`，环境变量 `export PATH="$HOME/.cargo/bin:$PATH" CARGO_BUILD_JOBS=6 LANG=en_US.UTF-8`）。
   - `mbp2`（`192.168.10.201`）此前 SSH 超时不可达，用本机 macOS arm64 承担第二平台验证。
5. **待清理分支占位**：`/Users/da/ws/pilotdeck-rs-agentcpm-pd` 中先前 429 失败子智能体留下的空分支/linked worktrees 仍挂在 `6bdda17f0`（`codex/agentcpm-pd-wp{1b,2,4}`），不影响主开发分支 `codex/agentcpm-pd-cli-20261116`。

## 4. 接手后建议执行步骤

1. **验证并提交 `WP4`**（2026-11-17 核查后升格，007 §7.1 闸门为强制项）：
   - 先等两项待裁决有结论（Kernel 钉版、WP4c `submit_owned` 口径），至少 Kernel 钉版须裁定或明示单机先行；
   - 验证范围**不得只跑** `--lib public_v1`（2 个测试）：WP4 重排了 `serve_socket` 闭包并动了 `http_proxy.rs` ReadLease 匹配，须跑 `cargo test --offline -p pilotdeck-gateway`（lib+tests 全量，含 `unsafe_boundary`、`live_e2e`、`native_*`）与 `cargo clippy --offline -p pilotdeck-gateway --all-targets -- -D warnings`，并按 §7.1 跑 native-ws 对拍与变异测试；
   - 补 `standalone_..._and_gap` 测试的 gap 帧断言（或改名），并为 `resume_queue` 死锁修复与 `-32004` 前置门控补回归测试；
   ```bash
   cd /Users/da/ws/pilotdeck-rs-agentcpm-pd/src
   cargo test --offline -p pilotdeck-gateway
   cargo clippy --offline -p pilotdeck-gateway --all-targets -- -D warnings
   # 全绿后再执行提交：
   git -C /Users/da/ws/pilotdeck-rs-agentcpm-pd add src/pilotdeck-gateway
   git -C /Users/da/ws/pilotdeck-rs-agentcpm-pd commit -m "feat(gateway): add public /v1/ws transport, SQLite WAL store, and 28-op S1+S2 runtime"
   ```
   （注：`git add src/pilotdeck-gateway` 恰好完整覆盖 `WP4` 全部改动 `http_proxy.rs`/`lib.rs`/`native_ws.rs`/`public_v1/`。）

2. **实现并提交 `WP3`（`src/agentcpm-pd-cli`）**：
   - 编写 `src/agentcpm-pd-cli/src/{commands.rs, connect.rs, lib.rs, main.rs}`：
     - 表驱动解析 `OperationRegistry::global()` 命令树与 `recipes`（拒 `tui` 与未知顶层词返回退出码 `2`；全局参数 `--target/--output/--input-json/--request-key/--timeout/--non-interactive/--no-start/--secret-output`；`run submit --text/--text-file` 互斥与单 stdin 规则；`run events --follow` NDJSON；`run wait` 退出码策略 `0/1/3/130`；`#[cfg(feature = "embedded-gateway")]` 接入 `public_v1::bootstrap`）。
     - `api connect` 3-task `pd-connect/1` stdio 引擎（`1 MiB` 超长帧恢复、`initialize` 附 `cli` 进程信息、本机只读 `schema.*`、拒绝 `service.*`/`instance.*`、Control `16` > Normal `64` > Event `biased; select!`、断线 `conn.state` 重连 / `InstanceChanged` 退出 `1`、EOF 5 秒排空不取消 run 退出 `0`、`SIGINT` 退出 `130`）。
   - 双特性门禁均通过后提交：
     - `cargo clippy --offline -p agentcpm-pd-cli --no-default-features --all-targets -- -D warnings && cargo test --offline -p agentcpm-pd-cli --no-default-features`
     - `cargo clippy --offline -p agentcpm-pd-cli --all-targets -- -D warnings && cargo test --offline -p agentcpm-pd-cli`

3. **实现并提交 `WP5` E2E 测试（`src/agentcpm-pd-cli/tests/e2e_v1_roundtrip.rs`）**：
   - 使用 `env!("CARGO_BIN_EXE_agentcpm-pd")` 与隔离临时 `AGENTCPM_PD_HOME` 实跑：28 操作全闭环、`T1–T11`、`A1–A8`、`terminals.output` `-32004`、负控，以及 **`P3`（`kill -9` + 重启 + `initialize` + `requests.lookup` `< 2000ms` 实测并打印毫秒数）**。

4. **双平台 `WP0-gate` 全量门禁**：
   - 在本机 macOS 及 `cb8`（Linux aarch64，通过 git bundle 部署至 `/home/clawbox-08/ws/pilotdeck-rs-cc2c/agentcpm-pd-20261117` 并链接 `deps/agent-kernel`）上运行 `src/` 四条全量命令：
     - `cargo fmt --all --check`
     - `cargo clippy --all-targets -- -D warnings`
     - `cargo nextest run --workspace --no-fail-fast --profile ci --config-file ../.config/nextest.toml`
     - `cargo test --workspace --doc`
   - 加跑 Linux 原生 `unsafe_boundary`、3 个 `check_frozen.py`、`todo!/unimplemented!/NOT_IMPLEMENTED` 扫描与 `gitleaks`。

5. **实证回写与飞书同步**：
   - 将真实 commit SHA、双平台验证结论、`P3` 实测耗时写入 `/Users/da/ws/pilotdeck-rs-cc2c/state/cli-v1-design.json`（同步拷入 worktree），跑 `check_s0.py`、`check_s0_schemas.cjs`、`manage.py check`、`manage.py sync` 与 `manage.py plan` 确认 `0` diff。
