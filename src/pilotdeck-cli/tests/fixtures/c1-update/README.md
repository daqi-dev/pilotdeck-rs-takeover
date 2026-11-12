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
