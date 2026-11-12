# C1 默认在线 CLI 正式入口证据

此驱动通过真实 `pilotdeck` 子进程运行 8 格：argv 单轮、stdin 多轮/空行/CRLF/独立 CR、两轮间 Ping/Pong、hello 拒绝后的本地 fallback、本轮断线、stdin EOF、远端流中 SIGINT、本地 fallback 模型流中 SIGINT。正式入口参数不增加测试选项，也不导入或替换产品模块。`fixture-client.mjs` 仅供采集器自测，不属于产品验收。

`mock.mjs` 只复用 `../c1-cron/capture.mjs` 导出的 WebSocket 编码函数 `frame`；该文件由 cron 作者维护。其余在线 HTTP、WS、模型 SSE 与子进程采集均在本目录。

## E0 运行

仅 E0 在 Docker 私有网络运行，无宿主端口映射。驱动在创建文件、启动进程或绑定 18789 前同时检查 `/.dockerenv` 与精确声明：

```sh
export PILOTDECK_C1_ONLINE_DOCKER=private-no-published-ports
node src/pilotdeck-cli/tests/fixtures/c1-online/capture.mjs \
  --side ts \
  --ts-root /baseline/PilotDeck \
  --out /evidence/c1-online-ts-first

node src/pilotdeck-cli/tests/fixtures/c1-online/capture.mjs \
  --side ts \
  --ts-root /baseline/PilotDeck \
  --out /evidence/c1-online-ts-second

node src/pilotdeck-cli/tests/fixtures/c1-online/capture.mjs \
  --side both \
  --ts-root /baseline/PilotDeck \
  --rust-root /candidate/pilotdeck-rs-cc2c \
  --rust-bin /candidate/pilotdeck-rs-cc2c/src/target/debug/pilotdeck \
  --rust-sha FULL_40_CHARACTER_CANDIDATE_SHA \
  --out /evidence/c1-online-pair
```

路径由 E0 替换为容器内实际路径；输出目录必须全新。TS SHA 必须为 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`；两侧 tracked checkout 必须干净。固定 Node 应为 v22.22.0。Rust 可执行文件与受测 SHA 的构建关系仍须由 E0 的构建身份账本证明，驱动额外记录二进制 SHA-256。

默认启动期限 20 秒，观测到全部预期输出后的退出观察窗 1500ms；可通过 `--startup-ms` / `--observe-ms` 明确传值，数值进入过程记录。baseline 的活跃本地 gateway / WS 会让单轮命令输出后仍不退出，驱动将其记录为 `lifecycle: observed-hang, productPassed: false`，不会把观察截止当作成功。Rust 必须自然退出；SIGINT 场景必须因实际用户模拟信号退出，不能靠清理超时通过。

全部 HOME、PILOT_HOME、配置、技能/插件空目录、TMPDIR 与 cwd 在每格私有目录中；PATH 是唯一继承的执行环境项。`NODE_NO_WARNINGS=1` 对两侧一致，作为命令环境显式保存。Git telemetry 探测由 `DOCKER_CONTAINER=1` 与固定 `COMMIT_HASH` 抑制。两侧预置相同 `.legacy-bundled-skills-migrated-v1` 标记（`completed\n`，基线 migrateLegacyBundledSkills.ts:66-67/126），隔离本专项与已有迁移告警；标记进入初始落盘快照。mock token/API key 仅是本地夹具常量。HTTP `/health`、WS `/ws`、模型 `/v1/chat/completions` 全在容器 127.0.0.1:18789；不会调用真实服务。

## 证据与判定

- `identity.json`：驱动 PID、Node/平台/hash、网络 namespace、隔离声明、源码 SHA、关键 TS 源码/构建模块 hash、Rust 可执行文件 hash、场景/驱动/共享帧编码器 hash。
- 每格 `command.json`：完整命令、参数、环境和 cwd。
- `stdout.bin` / `stderr.bin`：原始字节；`result.json` 同时保存可读文本和 hash。
- `network.jsonl`：真实健康检查、hello、全部 RPC、mock 发送事件、Ping/Pong、close、模型 HTTP 请求。
- `process.jsonl`：运行中观察点、PID 与 Linux start ticks、stdout/stderr 到达、信号目标身份、退出和清理。
- `files-before.json` / `files-after.json`：每个实际落盘文件的相对路径、长度、hash 和原始 base64；不删改原始文件来归一化。
- `summary.json`：分母与逐格证据有效性、Rust 结果、TS observed-hang 数量；驱动 exit 0 只表示所选证据收集有效及所选 Rust 格满足断言，不表示 TS 挂起被当作成功。

输出字节、hello 字节与 submit_turn 字段顺序由驱动精确断言；只将由本次真实 cwd 派生的 sessionKey 与该 cwd 对照。复核 TS 两轮及 TS/Rust 的原始持久化材料时，只可对已核实的 fixture 根路径、生成 UUID/时钟做明确映射，不能整块忽略持久化文件或模型请求。当前驱动保留全部模型请求与落盘证据，但不冒充已经完成这些复杂文件的语义对拍；E0 首次运行的实际差异必须单列归因。

SIGINT 是发送给明确登记的 CLI PID；驱动持续记录其已观察子孙 PID/start ticks，取消后检查并报告残留，只清理身份仍匹配的本任务进程。**前台进程组/PTY 信号、真实模型服务、发行包资源布局仍需各自验收，本驱动不覆盖。** 不会通过名称批量杀进程。没有后台服务或其他任务进程被此驱动启停。

## 本机安全自测

```sh
node --test src/pilotdeck-cli/tests/fixtures/c1-online/capture.test.mjs
```

自测只用随机端口和显式假产品子进程；包含 8 格正向收集、基线挂起记录/Rust 挂起拒绝、错误退出、错误渲染 final payload、RPC 缺失/重复/顺序变更、跨轮复用 UUID、缺失 Pong，以及信号发送失败/额外 cleanup 被伪装成用户取消的负控。首次自测的 6 项失败是 macOS `/var` → `/private/var` 临时目录 realpath 差异；已把夹具根路径实化，原始失败保存在 `_tmp/20261113-c1-online-driver/first-test.log`，第二轮 13/13 通过在 `second-test.log`；随后补 WebSocket payload=127 编码边界回归，14/14 通过在 `third-test.log`。独立审查后的判据补强自测 15/15 通过在 `review-test.log`。这不是正式产品运行成绩。
