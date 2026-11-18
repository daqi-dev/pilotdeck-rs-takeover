---
name: child-signals
description: Unix 子进程 spawn 前置安全钩子（#386 / #552）：在 fork-to-exec 窗口重置继承信号并按需执行 setsid()。
---

# child-signals

## 能力

封装 `#386`/`#552` 人工批准的最小 `unsafe_code = "deny"` 边界，仅暴露 compile-time 构建的 29 信号重置表与 `pre_exec` 安装钩子，供 bash runner、后台任务、ripgrep 与 hook 派生子进程使用。

## 先读什么

- `Cargo.toml`、`src/lib.rs`（`unsafe_boundary` 守卫对本目录做逐文件/逐字节校验）。

## 边界

- 仅限 Unix；绝不在 `src/pilotdeck-child-signals/` 目录内增删文件，本 MintHub 清单与说明只在打包阶段由 `packages/minthub/child-signals/` 叠加。
