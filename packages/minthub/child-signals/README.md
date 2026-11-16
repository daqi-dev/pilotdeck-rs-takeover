# pilotdeck-rs/child-signals (`pilotdeck-child-signals`)

> 本 MintHub bundle 打包自 `pilotdeck-rs/src/pilotdeck-child-signals`，作为 `pilotdeck-rs` Cargo workspace 拆分包分发。若源 `Cargo.toml` 使用 `version.workspace = true`、`license.workspace = true`、`license-file.workspace = true` 或工作区路径依赖，其继承真值来自 `pilotdeck-rs/src/Cargo.toml`。

## 职责与定位

Unix 子进程 spawn 前置安全钩子（#386 / #552）：在 fork-to-exec 窗口重置继承信号并按需执行 setsid()。

封装 `#386`/`#552` 人工批准的最小 `unsafe_code = "deny"` 边界，仅暴露 compile-time 构建的 29 信号重置表与 `pre_exec` 安装钩子，供 bash runner、后台任务、ripgrep 与 hook 派生子进程使用。

## 接口与能力声明

- **包名与版本**：`pilotdeck-rs/child-signals@0.1.0`（`kind = "bundle"`，`layout = "any"`）
- **Cargo Crate**：`pilotdeck-child-signals`
- **需要契约接口（`[requires].interfaces`）**：无
- **声明能力（`[capabilities]`）**：无声明运行时能力

## 包级依赖

- 无内部 `pilotdeck-rs/*` 或 Kernel 包依赖。

## 源码边界与补充说明

仅限 Unix；绝不在 `src/pilotdeck-child-signals/` 目录内增删文件，本 MintHub 清单与说明只在打包阶段由 `packages/minthub/child-signals/` 叠加。

注意：`src/pilotdeck-child-signals/` 受网关 `unsafe_boundary.rs` 严格锁定（仅允许 `Cargo.toml` 与 `src/lib.rs`），因此本 MintHub 清单、README 与 skill 维护在 `packages/minthub/child-signals/` 并在打包暂存区叠加。
