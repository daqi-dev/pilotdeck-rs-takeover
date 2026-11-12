---
id: D1-STARTUP-CWD-ERROR-001
---

## 类别

崩溃修复：启动目录被删除或无法读取时，将模块加载阶段的未捕获异常改为明确的启动失败。

## 基线依据

固定cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85、Node22.22.0、macOS ARM64在隔离HOME/PILOT_HOME下执行实际默认入口、help和gateway usage。删除cwd后全部exit1、stdout空，错误为`ENOENT: no such file or directory, uv_cwd`；目录无读取权限时为`EACCES: permission denied, uv_cwd`。built入口在stack-utils/index.js:6模块初始化时未捕获，附Node堆栈。独立C1证据为`_tmp/20261113-c1-cwd-errors/startup-oracle.json`及`permission-oracle.json`与相邻原始stderr文件。

## Rust 行为

`pilotdeck-cli/src/main.rs`把current_dir的真实操作系统错误交共享Node errno格式器，stderr一行、exit1；移除该分支原来的Todo/exit2。不包含不存在的Node模块加载堆栈。

## 范围

仅进程在读取启动工作目录时已经失败的分支。错误种类、syscall、stderr核心说明、stdout空和退出码必须保持；有效目录下所有命令输出不变。没有对普通CLI错误的通用豁免。

## 机器判据

`pilotdeck-cli/tests/startup_cwd_exit.rs`通过独立helper进入本测试独有目录并删除该目录，再启动真实CLI，要求exit1、stdout空及完整单行ENOENT说明；该结果尚须随固定候选运行。冻结基线实测独立覆盖无权限路径，不因本登记自动判绿。

## 批准记录

用户2026-11-13执行计划明确授权修复崩溃。此处基线确为模块导入阶段未捕获异常，采用Rust正常启动错误路径；仅省略Node内部崩溃栈，不扩大为任意CLI输出差异。
