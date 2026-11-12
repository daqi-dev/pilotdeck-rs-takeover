# C1 SDK 正式 CLI 基线取证

`capture.mjs` 是本轮独立编写的本地 WebSocket mock 驱动。它执行固定基线的正式 `dist/src/cli/pilotdeck.js`，不复制或修改 TS 参考源码，也不经过导出的函数替身。

固定身份为 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`、Node `v22.22.0`。脚本在启动产品进程前核对完整 HEAD、Node 版本，以及 SDK 源码、正式 CLI 和 SDK 构建产物的 SHA-256。七项哈希来自 2026-11-13 已核实的固定基线；不一致直接失败，不更新哈希绕过验证。构建产物或依赖缺失时由 E0 按固定基线从源码准备，不下载预编译工具。

## 复跑

在本工作区根目录执行。输出父目录必须已经存在。每个输出目录必须是全新目录；已经存在会失败，避免覆盖首次失败。`PDRS_BASELINE` 可指向 E0 的固定 Linux 基线目录，缺省为 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。

```sh
mkdir -p _tmp/20261113-c1-sdk-recapture
PDRS_BASELINE=/Users/da/ws/pilotdeck-rs/resources/PilotDeck node src/pilotdeck-cli/tests/fixtures/c1-sdk/capture.mjs _tmp/20261113-c1-sdk-recapture/errors-1 errors
PDRS_BASELINE=/Users/da/ws/pilotdeck-rs/resources/PilotDeck node src/pilotdeck-cli/tests/fixtures/c1-sdk/capture.mjs _tmp/20261113-c1-sdk-recapture/errors-2 errors
node src/pilotdeck-cli/tests/fixtures/c1-sdk/capture.mjs --compare _tmp/20261113-c1-sdk-recapture/errors-1/summary.json _tmp/20261113-c1-sdk-recapture/errors-2/summary.json
PDRS_BASELINE=/Users/da/ws/pilotdeck-rs/resources/PilotDeck node src/pilotdeck-cli/tests/fixtures/c1-sdk/capture.mjs _tmp/20261113-c1-sdk-recapture/hello-1 hello
```

`errors` 每轮 10 格，约 30 秒：run/resume 各覆盖缺少 project 的 model、query resume 拒绝、model 歧义、submit 拒绝和正常成功。前八格必须打印精确错误后仍保留一条 query 连接，在错误出现后的 3500ms 内没有自然退出；最后两格必须自然 exit 0、输出 `c1 success` 并释放两条连接。

`hello` 为 run/resume 两格，约 28 秒：第二条连接不回复 hello。基线 10 秒协议超时后必须输出 `Gateway hello timed out.`，再观察 3500ms 仍保持一条 query 连接。前置里程碑有 15 秒界限，启动或环境故障不能被当作产品挂起。

可以把 `hello` 再跑到新的输出目录，并用相同 `--compare` 命令自比两份 summary。自比仅在单独比较视图中去掉已登记的 Node SQLite 两行启动警告，并绑定合法且唯一的请求 UUID、合法 run UUID；stdout、业务 stderr、完整请求其他字段和顺序、退出、连接存活及关闭记录严格一致。原件不改写。

## 隔离与证据

每格使用随机 localhost 端口，不绑定固定 18789，也不连接真实 Gateway/provider。产品进程的环境由驱动明确构造，不继承真实凭据、代理或 HOME；HOME、PILOT_HOME、XDG 和临时目录全部位于该格的独占目录。只使用固定假 token。

驱动记录每个产品 child 的具体 PID，仅向自身 `Child` 发送清理 SIGKILL；不按名称杀进程。各清理步骤独立执行：即使发送信号或等待 child 管道失败，也继续终止自身 WebSocket 连接并关闭服务，最后汇总清理错误。逐条等待 socket close 事件后才检查活动连接数。原版错误场景的 SIGKILL 标为 `cleanupExit`，从不当作自然退出或成功。任何断言或环境错误保留当前格原始记录，并写 `failure.json` 后非零退出。

每轮输出包含：

- `identity.json`：完整基线 SHA、Node/平台/架构、源码和构建产物哈希、驱动哈希。
- 每格 `invocation.json`：正式入口、参数、隔离环境和 cwd；`stdout.bin`、`stderr.bin` 为未经替换的原始字节。
- 每格 `result.json`：RPC 请求原值、PID、自然退出与清理退出、观察边界前关闭记录、清理后活动连接数和逐格验证结果。
- 成功完成整轮才写 `summary.json`；失败写 `failure.json`，保留首次失败，不覆盖重跑。

纯驱动故障注入不启动产品进程：

```sh
node src/pilotdeck-cli/tests/fixtures/c1-sdk/capture.mjs --self-test-cleanup
```

它验证 kill 返回 false、child close 超时和 kill 时 child 已自然退出的竞争均不会跳过 socket/server 清理。

这里记录的是基线的挂起缺陷。Rust 按 `D1-SDK-QUERY-ERROR-DISPOSE-001` 修复后应自然 exit 1 并关闭 query，因此不要把 Rust 的修复结果塞入本 TS 原件。Rust 正式入口正向验收位于 `tests/c1_sdk_lifecycle.rs`；旧 signal 六格的精确期望和负控位于 `tests/common/signal_replay.rs`。构建和执行统一交由主协调者/E0 排队。
