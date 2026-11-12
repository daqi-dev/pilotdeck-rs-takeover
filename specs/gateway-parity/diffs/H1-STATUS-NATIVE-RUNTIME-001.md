---
id: H1-STATUS-NATIVE-RUNTIME-001
---

## 类别

架构已授权差异，仅适用于 Rust native host 的渠道 `/status`。用户本轮要求默认核心不依赖 Node 并由 Rust 承担；固定 TS 的 `ChannelCommandRegistry.ts:160-192` 使用当前 Node 进程的 `process.version`，native 进程没有可报告的 Node 运行时。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，隔离 HOME/PILOT_HOME、fake git 的受控模块探针 `_tmp/20261113-h1-commands-oracle/status-ok{,-2}.json` 两轮一致：`Node: v22.22.0`，其余字段为实际 git 输出、进程 uptime 和平台；fake git 失败两轮也一致。此为模块级证据，非正式渠道入口。

## Rust 行为

Rust native 渠道 `/status` 保留 `Node:` 字段名，值精确为 `not used (native Rust)`，不得伪称安装的 Node 版本，也不得为了状态命令隐式运行 Node。分支、提交、进程运行时间、Node 平台名映射及错误行为继续按基线，不能因本条放宽。

## 范围

仅 native host 渠道 `/status` 的 Node 运行时字段；其他渠道命令、CLI、Node 进程和 updater 行为不在本条范围。

## 机器判据

`native_status_and_optional_updater_use_real_owned_processes` 在隔离子进程的 PATH 安装 fake git 和有副作用标记的 fake node，正向断言 status 的精确字段、平台与 Git 输出，且 status 前 fake node 标记不存在；后续 updater 正向调用 fake node 并确认标记出现，证明探针有效。此前先由测试驱动 fake git 实现相同状态合同，完整正式渠道正向与负控仍待同一集成版本运行。本条在登记时尚未实际运行 Rust 测试，不凭登记宣称通过。

## 批准记录

用户本轮明确授权默认核心不依赖 Node；2026-11-13 主协调在当前会话明确授权此窄条目。账本本体不改。
