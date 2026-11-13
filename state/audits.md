# 审阅与盲审记录（`dev/takeover`）

交付标准第 3 条要求每次盲审记下：谁审的、覆盖了哪些区域、找出什么、各是几级。这里每行一次盲审或代码审阅，只追加；`scripts/bbx/status.mjs` 读这张表算各功能域「最近一次盲审 / 代码审阅」。

填写约定（`status.mjs` 按它解析）：

- 「类型」取 `盲审`（审查者不是该区域的实现者，只看基线源码和场景、不看 Rust 实现）、`缺口审查`（只看基线写场景找差异，但写场景的人同时修 Rust，不满足盲审对审查者的要求）、`代码审阅`（读 Rust 实现对照基线）三者之一。
- 「覆盖」里用反引号写功能域或子能力的标识（取自 `scripts/bbx/features.json`），`*` 表示全部功能域。
- 「发现」写条数与级别（分级见 `specs/delivery/README.md`）；核实不了的写「记录不全」，不补猜。

2026-11-13 补记的前几行依据 `state/takeover.md`、`state/issues.md` 和 git 历史（合并提交与它新增的场景名）。当时都没有按现在的四级分级记录，级别凡是没写明的都是「记录不全」。

| 日期 | 类型 | 执行者 | 覆盖（功能域 / 子能力） | 发现（条数与级别） | 记录在哪 |
|---|---|---|---|---|---|
| 2026-11-11 | 代码审阅 | Opus 子代理（安全类风险审查；只拿代码和基线对应位置，每条发现须附复现） | `perm.modes` `perm.paths` `tools.execute-code` `gw.http` `agents.oneshot` | 5 处：plan / ask 模式运行时拦截缺失、工作区内符号链接指向外部的路径检查（Rust 比基线严）、`execute_code` 超时被脱离进程组的孙进程拖住、`/auth/local-token` 不看 Host / Origin、`execute_code` 只读判定放行（保持基线，记为已知风险）。级别：记录不全（按现在的分级前四处属第 1 级） | 提交 `c36003f8d`、`b749ab6ab`，合并 `a5fbd0c10`（复现场景 `secreview-*` 在修复提交 `528050708`、`9a020458d` 等里删除）；`state/takeover.md`「做过的决定」2026-11-11 |
| 2026-11-11 | 代码审阅 | Opus 子代理（健壮性风险审查：超时、断连、重启恢复，做法同安全审查） | `gw.connections` `gw.ws-protocol` `session.recovery` `loop.submit` `perm.interaction` `tools.interaction` `ext.mcp` | 记录不全（留下 17 个 `robust-*` 场景；发现条数与级别没有记下） | 合并提交 `58584be0e` |
| 2026-11-11 | 盲审 | Opus 子代理（只看基线源码和场景、不看 Rust 实现的缺口审查；审查者是否碰过这些区域的实现：记录不全） | `tools.files` `tools.search` `tools.misc` `tools.web` `tools.runtime` `perm.sandbox` `perm.rules-file` `perm.interaction` `context.prompt` `config.project` `config.env-paths` `model.transport` `loop.continuation` `loop.recovery` `session.uploads` `session.transcript` `ext.plugins` | 16 处差异，其中安全相关的 3 处（`agent.sandboxMode`、`permissions.json`、`agent.interactionProfile` 在 Rust 里不生效，按现在的分级属第 1 级）、整个缺失的 `web_search` 工具 1 处（第 2 级）；其余 12 处级别记录不全。另留一份未测清单 | `state/takeover.md`「重要的认识（2026-11-12）」；提交 `e4fb6b1b7`、`3d3089058`、`a982cd96b`、`5fa978d7a`、`5d4a9e29c`，合并 `1f24c62aa`（`gap-*` 场景） |
| 2026-11-12 | 代码审阅 | 主会话（对着基线逐段读） | `perm.decision` `perm.rules-file` `perm.sandbox` `hooks.tool-events` `tools.shell` | 认为一致，未发现问题：权限判定顺序与 `ask` 收尾、规则匹配、`permissions.json`、`PermissionRequest` hook 与询问的先后、Seatbelt profile 与写入围栏、bash 命令分类与分词 | `state/takeover.md`「2026-11-12 主会话对着基线逐段读过并认为一致的」 |
| 2026-11-12 | 缺口审查 | Opus 子代理（按上一次盲审的未测清单写场景并修 Rust） | `loop.recovery` `loop.tool-dispatch` `model.catalog` `model.router` `memory.capture-recall` `memory.maintenance` | 修了记忆维护调度、记忆请求头顺序 2 处；其余条数与级别记录不全（新增 12 个场景） | 合并提交 `7388b0131` |
| 2026-11-12 | 缺口审查 | Opus 子代理（CLI 与环境变量缺口审查，同时修 Rust） | `cli` `config.env-paths` `memory.config` | 修了 `run --model` 不带 `--project`、browser-use 启动前准备 2 处；其余记录不全（新增 9 个场景） | 合并提交 `c662e94f2` |
| 2026-11-13 | 缺口审查 | Opus 子代理（按 `coverage.mjs --surface` 的缺口写场景并修 Rust：循环行为、事件、落盘记录、文件） | `loop.recovery` `loop.steer-abort` `context.compaction` `model.transport` `memory.config` `hooks.tool-events` `session.transcript` `agents.oneshot` `tools.state` `auto.always-on-cycle` | Rust 修了 8 处，级别记录不全；未修的进了台账（`turn_aborted` 状态、压缩中中止，第 2 级） | 合并提交 `8c39e603b`（18 个场景） |
| 2026-11-13 | 代码审阅 | 独立审阅者（只读、没有运行，结论是线索） | `perm` `modules.binding` `modules.tools` `modules.sidecar-tcp` `hooks.execution` `model.google` `memory.config` `channels.startup` `loop.steer-abort` `agents.oneshot` `loop.submit` `auto.cron-config` `tools.misc` | 台账里来源为「审阅」、仍开着的：第 3 级 7 条、第 4 级 1 条、尺子 5 条（已修掉的从台账删了，总条数记录不全）。范围是 `0cbf97495..8c39e603b` 的产品改动；读过未发现问题的区域和没来得及读的文件列在 `state/issues.md` 末尾 | `state/issues.md`（来源列「审阅」；末尾两节） |
| 2026-11-13 | 缺口审查 | Opus 子代理（表面缺口第二组：工具参数、hooks、斜杠命令、会话配置、协议、ws 方法，同时修 Rust） | `tools.runtime` `perm.paths` `hooks.tool-events` `memory.capture-recall` `model.transport` `model.catalog` `agents.sdk-defs` `agents.oneshot` `channels.weixin-login` | 记录不全；未修的进了台账（PostToolUseFailure 中止，第 2 级） | 合并提交 `a57be1a4f`（8 个场景） |
| 2026-11-13 | 缺口审查 | Opus 子代理（只看基线源码与实际运行列行为清单、补表面项与场景，再跑 Rust；同一代理也负责修，不满足盲审要求） | `gw.ws-protocol` `gw.connections` `gw.http` `gw.server-entry` `gw.projects` `session.recovery` `modules.binding` `modules.tools` `modules.knowledge` `modules.sidecar-stdio` `modules.sidecar-tcp` `modules.host-http` `modules.sop` | Rust 差异 6 处：第 2 级 1（WS 与 HTTP 共用 64 条连接上限，满时新连接被重置、/health 也连不上；处理方式待主会话定）；第 3 级 2（慢读客户端被 1009 output limit exceeded 关闭、基线全部送达，只有量测没有场景；运行记录写坏时两侧都起不来但输出不同）；第 4 级 3（HTTP 传输字节：chunk 长度大小写、头名大小写、431/400 的 content-length: 0、HTTP/1.0 状态行；请求头写一半就半关闭时基线回 400、Rust 不回；单帧超过 100 MiB 关 1009）。疑似基线 bug 5 处（未鉴权的请求目标 `//[` 与不加掩码的帧都能让服务停机，坏百分号编码的项目键让服务停机，运行记录写坏后起不来，写入可经符号链接写出项目外——最后一条未运行验证），未修，留给主会话 | 本次提交的场景 `gw-*`、`recovery-*`，表面维度 `gw-behaviors` / `module-behaviors` / `recovery-behaviors`；子代理报告 |
