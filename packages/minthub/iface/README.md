# pilotdeck-rs/iface (`pilotdeck-iface`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-iface`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

PilotDeck 接入公开 ifACE Scheduler SDK 的 AgentLoop 外壳：原生/恢复启动入口、资源多路编解码与 Kernel 客户端桥接。

通过 `agentcpm_iface::loop::LoopContract` 与 `agentcpm_scheduler_protocol` 暴露 PilotDeck 准入的 `AgentLoop`，提供 `AdmittedTurn`、`CallAssembly`、`native_loop_launch`、`production_entry`、`recovery_launch`、`resource_mux`、`scoped_native_entry`，以及 `pilotdeck-context-recovery-provider` 与 `pilotdeck-native-loop`（`--pilotdeck-scoped-native-loop-v1` / `--pilotdeck-product-controller-v1`）二进制入口。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/iface@0.1.3`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-iface`
- **需要契约接口（`[requires].interfaces`）**：`loop`, `model`, `tool`, `context`, `session`, `types`
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- `pilotdeck-rs/protocol` (`=0.1.0`)
- `pilotdeck-rs/agent` (`=0.1.0`)
- `pilotdeck-rs/turn-preparation` (`=0.1.0`)
- `pilotdeck-rs/transport` (`=0.1.0`)
- `pilotdeck-rs/context-iface` (`=0.1.1`)
- `pilotdeck-rs/context-runtime` (`=0.1.0`)
- `agentcpm/iface` (`>=0.1.6, <0.2.0`)
- `agentcpm/core` (`>=0.1.2, <0.2.0`)
- `agentcpm/host` (`>=0.1.6, <0.2.0`)
- `iface/loop` (`>=0.1.4, <0.2.0`)
- `iface/model` (`>=0.1.4, <0.2.0`)
- `iface/tool` (`>=0.1.3, <0.2.0`)
- `iface/context` (`>=0.1.5, <0.2.0`)
- `iface/session` (`>=0.1.13, <0.2.0`)
- `iface/types` (`>=0.1.8, <0.2.0`)

## 安装与验证

```bash
# 安装本包（含依赖闭包；registry 以实际部署为准）
mint --registry https://mint-hub.ea.modelbest.co add -y pilotdeck-rs/iface@0.1.3

# 在 pilotdeck-rs 仓库内运行本包单测（src/ 为 Cargo workspace 根）
cargo nextest run -p pilotdeck-iface --profile ci --config-file ../.config/nextest.toml

# 端到端装配验证（MintHub 拉包 + 编译 + 跨包 E2E；详见仓库 docs/iface-integration/minthub-stitched-verification.md）
bash scripts/verify-minthub-stitched-assembly.sh --mint "$(command -v mint)" --jobs 6 --out-report-dir /tmp/pd-stitched
```

## 源码边界与补充说明

`#![forbid(unsafe_code)]`；不实例化 Gateway 或第二套执行运行时，所有模型、工具与上下文宿主效应均经调用绑定的 Kernel 客户端完成。

注：`Cargo.toml` 另包含工作区本地路径依赖 `agentcpm-scheduler-protocol`（`deps/agent-kernel/kernel/scheduler-protocol`）以及可选 feature `host-composition`（`agentcpm-host` / `agentcpm-core`）；本 MintHub 清单声明非可选包依赖与完整的 6 项 `iface/*` 契约身份以保证闭包完整性。
