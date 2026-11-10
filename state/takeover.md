# 接续记录（`dev/takeover`）

新会话先读这份，再读 `CLAUDE.md` 和 `.42cog/intent.md`，然后看 `git log`。不要往 `state/board.md` 追加；`state/` 下其余文件是上一轮流水线的记录，只当线索。

最后更新：2026-11-10

## 现在由 Rust 承担并有证据的入口

**没有。**（标准：bbx 对拍 SAME 或差异已登记；Rust 独立应答；四条命令全量通过。）

当前对拍结果（`node scripts/bbx/check.mjs`）：

| 场景 | 结果 |
|---|---|
| `turn-text` | DIFF 216 |
| `survey-empty-params` | DIFF 181（100 个 method 里 69 个应答一致） |
| `cli-survey-offline` | DIFF 16（25 条命令里 20 条一致） |

## 验收手段

`scripts/bbx/`（说明见其 README）。同一场景先后喂给基线 `node dist/src/cli/pilotdeck.js` 和 Rust 二进制，比较 WS 帧、HTTP、CLI 输出、发给假 provider 的请求、落盘文件。

- 改完代码：`(cd src && cargo build -p pilotdeck-gateway -p pilotdeck-transport -p pilotdeck-cli) && node scripts/bbx/check.mjs`
- 新场景先跑 `--self`（基线自比）确认稳定。
- 负控记录：2026-11-10 把 `native_process/methods/tools.rs` 里 `background_task_stop` 的 `sessionKey is required.` 改一个字母，`survey-empty-params` 从 181 变 182 并指出该处，还原后回 181。注入点必须先确认在被调用路径上。

## 正在做

**M1 最小闭环**：Rust `pilotdeck server` 成为真入口（参数、环境变量、启动输出与基线相同），`turn-text` 场景 SAME。

主要差距（来自 `_build/bbx/turn-text.diff.txt`）：工具声明 5 个对 28 个、系统提示词、provider 请求头、`context_budget` 事件、会话落盘缺记录类型、`runs/registry.json` 路径、会话归属目录、`read_session_messages` 未接线、`describe_server` 缺 `projectKey` 和 13 项能力。

## 之后

M2 会话域 → M3 工具回路（同时重跑 `scripts/parity` 的 53 场景并做负控）→ M4 其余 method 族 → M5 gateway HTTP → M6 CLI → M7 退役（删代理模式、删未接线代码、原版 `ui/server` 接 Rust gateway 实测）。完整计划和盘点见 `_tmp/20261110-inventory/REPORT.md`（不入库；丢了就按本文件重做盘点的相应部分）。

## 人已定

2026-11-10 人答复「都按你的建议，之后你来决定就行」：

1. 阶段 B（`ui/server`）推迟，不取消。先做完 A、C、D 并让 TS gateway 退役，再回来定 B。在那之前不在 B 上做实现。
2. Node 运行时的 `ExperimentalWarning` 两行 stderr 算已接受差异（`scripts/bbx/diff.mjs` 的 `ACCEPTED`）。
3. 此后这类取舍（已接受差异、阶段顺序、范围内的做法）由我决定，在里程碑汇报里说明决定了什么和依据。我对这句话的理解不包括：推送到原仓库、改接手仓库可见性、对外发布、删除远端资源、改 `intent.md` 的收敛方向，这些仍然先问。

## 做过的决定

| 日期 | 决定 | 依据 |
|---|---|---|
| 2026-11-10 | 新建 `scripts/bbx` 作为验收手段，不再用 `scripts/gateway-parity` 和 `scripts/ui-server-parity` 验收 | 前者比的是透明代理模式，后者的被测件是一次性代理，都证明不了 Rust native 实现的任何事 |
| 2026-11-10 | 保留 Rust native gateway，按对拍差异收敛，不整体重写 | 它能独立跑通一轮对话，69 个 method 的错误路径与基线逐字节一致；缺的是接线和端到端验证 |
| 2026-11-10 | `scripts/parity`、`scripts/cli-parity` 暂留，用到时先做负控 | 静态看是真对拍，但我没跑过 |

## 停用的旧闸门

| 停了什么 | 它原本想防什么 | 现在由什么替代 |
|---|---|---|
| `.github/workflows/ci.yml`、`pr-gate.yml`（文件留在原处，没删没改；它们只在 PR、push 到 `main`、merge queue 时触发，接手仓库只推 `dev/takeover`，所以不跑） | 四条构建测试命令；另有约 30 步守卫：负控装置自检、CI 配置守卫、冻结账本只追加、评审结果块格式、入口总账记账一致性 | `.github/workflows/takeover.yml` 只跑四条命令。对基线的一致性由本机的 `scripts/bbx` 验收。冻结账本我不改，所以只追加检查暂时没有对象；要动账本时手工跑对应的 `check_frozen.py` |

旧 workflow 没删的原因：仓库里有 Python 守卫和至少一个 Rust 测试会读 `ci.yml` 的内容。等 M7 精简测试时一起处理。

## 已知的坑

- 这台 macOS 上全量 nextest 有 3 个失败：`unsafe_boundary::compiler_enforces_gateway_boundary`（守卫只支持 Linux）、`channel::sms::tests::dual_stack_occupier_is_reported_by_the_fixed_host_bind_probe`、`mcp_https::replays_ts_mcp_https_over_local_tls`（后两个与 IPv6 有关，稳定复现，根因未查）。
- 全量 nextest 约 12 分钟，`src/target` 约 15 GB。改一行 gateway 后增量构建约 35 秒。
- 基线 CLI 的 `cron` 和默认对话路径会探测写死的 `127.0.0.1:18789`。bbx 在该端口有监听者时拒绝运行，不要绕过。
- bbx 独占固定目录 `/private/tmp/pdrs-bbx` 和端口 38411 / 38412，不能并行跑两个。
- `pilotdeck-gateway` 不带 `--native` 是透明代理（转发给 TS）。验证 Rust 行为时必须走 native。
- native gateway 每个会话拉起 `pilotdeck-agent-loop-sidecar` 子进程；基线默认是进程内。
- 原仓库 `daqi-dev/pilotdeck-rs` 只读。它的其他分支历史里有 18 条 gitleaks 命中（测试夹具里的 key 形状字符串），不在 `dev/takeover` 的历史上；只推 `dev/takeover`。
