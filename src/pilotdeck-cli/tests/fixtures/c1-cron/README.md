# C1 cron 正式 CLI 入口取证

`capture.mjs` 启动真正的固定 TS / Rust CLI 子进程，不导入产品函数，不改写基线。
HTTP `/health` 与 WebSocket `/ws` mock 只监听容器内 `127.0.0.1:18789`，使用同一份
`cases.json` 驱动两侧。以下 14 个场景及断言是待正式执行确认的候选，不预先声明基线通过。它们覆盖 list/create/delete/stop 成功、四类在线用法错误、
四命令 RPC 失败，以及 delete/stop 的基线参数优先级。

启动驱动必须同时满足：

- 环境变量 `PILOTDECK_C1_CRON_DOCKER=private-no-published-ports`；
- 容器内存在 `/.dockerenv`。

守卫在创建产物、绑定端口和启动产品进程之前执行。E0 必须用 `--network none`，
不发布任何端口，不使用 host 网络。驱动不会停止占用端口的其他进程；绑定冲突直接失败。
本机只执行纯脚本自测，自测 mock 绑定随机端口、子进程为自编模拟器，不运行 PilotDeck。

## E0 执行命令

以下采用现有 cb8 任务布局和 `bbx-docker.sh` 相同的只读系统库挂载方式，不修改 bbx
运行器。由 E0 在构建过同一提交的 `checkout` 中运行；本目录必须已包含在集成候选。
镜像使用 E0 已准备的镜像，`--pull=never` 不下载镜像。输出目录必须全新。

```sh
C1_CRON_ROOT=/home/clawbox-08/ws/pilotdeck-rs-cc2c
C1_CRON_SHA=$(git -C "$C1_CRON_ROOT/checkout" rev-parse HEAD)
C1_CRON_OUT="$C1_CRON_ROOT/logs/c1-cron-$C1_CRON_SHA-$(date +%s)"
C1_CRON_LIBS="/lib/$(uname -m)-linux-gnu"
mkdir -p "$C1_CRON_ROOT/logs"
docker run --rm --pull=never --name "pdrs-c1-cron-$$" \
  --network none --read-only --user "$(id -u):$(id -g)" \
  --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp:rw,nosuid,nodev,size=256m \
  --mount "type=bind,src=$C1_CRON_ROOT,dst=$C1_CRON_ROOT" \
  --mount "type=bind,src=$C1_CRON_LIBS,dst=$C1_CRON_LIBS,readonly" \
  --mount "type=bind,src=$(command -v git),dst=/usr/local/bin/git,readonly" \
  --env HOME=/tmp/c1-cron-driver-home --env PILOT_HOME=/tmp/c1-cron-driver-home/.pilotdeck \
  --env PILOTDECK_C1_CRON_DOCKER=private-no-published-ports \
  --env "PATH=$C1_CRON_ROOT/tools/node/bin:/usr/local/bin:/usr/bin:/bin" \
  --workdir "$C1_CRON_ROOT/checkout" \
  "${PDRS_TEST_IMAGE:-debian:bookworm-slim}" \
  node src/pilotdeck-cli/tests/fixtures/c1-cron/capture.mjs \
    --side both \
    --ts-root "$C1_CRON_ROOT/baseline/PilotDeck" \
    --rust-root "$C1_CRON_ROOT/checkout" \
    --rust-bin "$C1_CRON_ROOT/checkout/src/target/debug/pilotdeck" \
    --rust-sha "$C1_CRON_SHA" \
    --out "$C1_CRON_OUT"
```

先做 TS 两轮自比时，将 `--side both` 改为 `--side ts`，每轮使用新输出目录；
`--rust-root/--rust-bin/--rust-sha` 可省略。两轮的原始 stdout/stderr、请求 method/params、
自然退出或挂起分类必须一致；仅请求 UUID、观测时间、隔离目录与 PID 是运行身份。
不能因为两轮都有失败而标为基线通过。

## 产物与判定

- `identity.json`：源码完整 SHA、TS 源码与正式构建入口哈希、Rust 二进制哈希、Node
  可执行文件哈希、平台、驱动与场景哈希、网络 namespace、驱动 PID。
  驱动强制 Node v22.22.0；TS SHA 强制固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`；Rust SHA 与
  `--rust-sha` 一致；两侧 tracked 源码必须干净。二进制与提交的构建对应仍由 E0
  的构建记录证明，不能仅凭源码 SHA 推断。
- 每场景两侧 `command.json`：完整 argv、隔离环境、共同 cwd。HOME/PILOT_HOME 分侧
  隔离，token 是已知 mock 值。`NODE_NO_WARNINGS=1` 同时应用两侧，避免 Node 运行时
  实验性警告混入业务 stderr；保存的是这个明确环境下的原始输出，没有后处理删除行。
- `stdout.bin` / `stderr.bin`：子进程原始输出字节；`result.json` 附 UTF-8 文本和哈希。
- `network.jsonl`：HTTP 请求、WS 原始 JSON 帧、规范 UUIDv4、close、连接终止；`process.jsonl`：
  自有 child PID/进程组、输出到达、观察边界、清理信号及退出。请求/hello 字节顺序也验证。
- `summary.json`：固定场景分母、实际行数、每行判定、Rust 是否通过、TS 挂起数量。

首次输出等待上限 15 秒。出现完整预期终端输出后继续观察 1.5 秒；如果进程仍存活、
自有 WS 仍打开且没有收到 close，TS 记录为 `observed-hang`，然后仅对本次创建的进程组
发送 SIGTERM；500 毫秒后仍未退出才发 SIGKILL。自然退出和清理产生的退出严格分开。
输出缺失、错误 RPC、错误 stderr、信号退出、端口冲突与启动失败都不能记为成功。

`observed-hang` 的 `evidenceValid=true` 只表示挂起证据满足观察条件，
`productPassed=false`、`naturalExitCode=null`，绝不冒充 TS 正常退出。
Rust 要求预期退出码、自然退出、未用清理信号、收到自有 WS close。
驱动退出 0 表示证据完整且所选 Rust 检查通过，并不表示存在挂起的 TS 产品通过。
挂起差异仍需按仓库规定登记，正式执行前不得写成既成事实。

## 采集器自测

```sh
node --test src/pilotdeck-cli/tests/fixtures/c1-cron/capture.test.mjs
```

自测覆盖隔离守卫、输出/退出/RPC/hello/close 缺失与错误负控、场景缺失/重复负控、TS 挂起不能混为 Rust
通过、真实自有 child 的自然退出与超时清理、随机端口 HTTP/WS 交互、无输出启动超时。
2026-11-13 本机 Node 22.22.0 的 7 个自测全部通过。增加 UUIDv4 校验后的首次运行
曾抓到 mock 解析器把 127 字节的 16 位长度帧误判为 64 位长度头，已修复分支并覆盖该
边界；首次失败保留在 `_tmp/20261113-c1-cron/driver-selftest.log`，修复后记录为
`driver-selftest-fixed.log`。该结果只验证驱动，不代表 TS 或
Rust 正式 CLI 场景已经运行；固定 18789 的实际执行由 E0 单独记录。
