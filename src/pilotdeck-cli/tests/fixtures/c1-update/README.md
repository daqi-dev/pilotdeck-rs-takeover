# C1 update 基线特征探针

探针直接导入固定基线的 `src/runtime/updateCommand.ts`，注入本地 Node 或 shell 子进程，不执行真实 Web updater、不联网、不写基线目录。它核对 Git SHA 和 Node 22.22.0，记录平台、退出码、合并输出及 UTF-8 分块行为。

从 Rust checkout 根目录运行，输出保存到新的临时证据目录：

```sh
env HOME=/dev/null PILOT_HOME=/dev/null node --experimental-strip-types \
  src/pilotdeck-cli/tests/fixtures/c1-update/baseline-probe.mjs \
  /absolute/path/to/PilotDeck
```

`split` 特征记录基线的数据损坏：将“中”的前三字节分成两次写入，基线把有效 UTF-8 解码成两个替换字符。批准差异 `D1-UPDATE-UTF8-CHUNKS-001` 在 Rust 侧保留同一管道的不完整字符尾部；真正无效字节及 EOF 残片仍替换。Rust 侧回归见 `c1_update_exit.rs` 和 `update.rs` 单测。

可选 Git Web updater 是已有安装提供的外部 Web 管理组件。本组只验证 Rust CLI 的安装根发现、参数和环境传递、双流输出、退出码及资源缺失行为，不将外部脚本列为已迁移的 native 核心，也不新增 Rust 发行更新协议。

## 取消所有权

`D1-UPDATE-CANCEL-OWNERSHIP-001` 针对仅向 CLI PID 发送 SIGINT/SIGTERM 时原实现留下 updater 及其 HTTP 连接的缺陷。Rust 将该 updater 放入本次命令创建的独占进程组，转发用户信号，宽限 3 秒后强制清理该组；组长保留为未回收子进程，最终组信号发出后才回收，随后 CLI 保留原始 SIGINT/SIGTERM 终止状态。

`c1_update_exit.rs` 的四项取消测试分别覆盖 CLI PID／前台进程组与 SIGINT／SIGTERM。测试内的 ignored helper 是被这些测试实际启动的 mock runtime，另创建同组 worker 并保持本地 HTTP 连接；二者故意忽略温和终止，以验证强制清理。成功断言先于测试 cleanup，失败则按 helpers 原子记录的进程出生身份核验后清理，不以清理结果冒充产品行为。

清理范围不包括 Web 服务已经接管的更新事务（基线 `webUpdateCommand.js:74–75`），也不包括 `processScope.cjs` 明确以 `detached: true` 启动、由 Web 管理组件拥有的 build guardians（`processGuardian.cjs:1–2`）。CLI 没有为排除范围的 Web 服务发明取消协议，也不会向这些独立组广播信号。
