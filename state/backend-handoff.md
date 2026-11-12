# 后端交付暂停交接

更新：2026-11-13 05:05:59，Asia/Shanghai。用户要求准备停下交接，**已停止新增实现、修复、构建和测试批次**；只保留此前已启动的 cb8 全 workspace 流水线写完日志。本文件是唯一总进度入口，不能将此前“继续执行”的历史记录当作恢复授权。**本期尚未完成交付。StaffDeck 本期延期，不再作为门禁。**

## 源码检查点

| 用途 | 身份 | 验证边界 |
|---|---|---|
| 根工作区 | `/Users/da/ws/pilotdeck-rs-cc2c`，`codex/backend-completion` | 用户 `plan.md`、`.codex/` 保留且不提交 |
| 最新产品/WIP 检查点 | `74336e9a28eced24509629963c5641838882e4ce` | 默认 native 入口、通用扩展碰撞回归、发行工具；Rust 最新改动尚未编译，许可补充代码未复验。之后仅交接文档提交 |
| 已验证第四候选 | `ea6d8e71fd71f57c929e81301965d41e573368a4` | T1 原生组件/image/PDF 与 R2 TCP；本文件下列 Linux 证据仅属于此 SHA |
| 已整合 Google complete | 根 `b0144315` ← C1 `bcad7cb1435103e31c9ec71877c7064f3cffc619` | C1 有定点实测，未完成根集成后的统一验收 |
| StaffDeck 第四批归档 | `codex/deferred-staffdeck-20261113@95ef92d86d1f4936a7f2c76249f6edf270423627` | 20 tracked + 4 new 文件逐 blob 核验；包含未编译的事件顺序/receipt WIP，当前主线不含此批 |
| 独立 C1 工作区 | `/Users/da/.codex/worktrees/c1-cli-completion/pilotdeck-rs-cc2c`，`codex/c1-cli-completion` | 最终 WIP 身份见后文和该树 `state/c1-cli-handoff.md`；禁止直接合并整个分支 |

原始 Rust 起点 `e8702d36c82cd5fa030d5e8ba3e724b0bfa68c95`。TS 固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读参考 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`。不要 cherry-pick C1 同步 merge `8086ab52`。本轮范围以 `docs/plan/backend-completion.md` 加用户 StaffDeck 延期决定为准，排除前端、ui/server、桌面壳和 TUI。

## 停工及仍在运行的任务

根 3 位子智能体均已完成停工交接：env_validation、native_runtime、staffdeck（后期转 D1）。本机不持有 Cargo 重型任务，D1 下载/查询均已退出，没有新增产品服务。独立 C1 的 12 位子作者也全部结束任务；C1 无 Cargo、测试、Node 取证、产品或容器进程。其全部 WIP 已归档并逐文件确认没有继续变化。此前确实是根 3 + C1 8 个活跃子智能体，两位协调者另计；这是历史峰值，不是停工后的运行数。

**cb8 仅保留已启动的全 workspace 流水线：**

- 目录 `/home/clawbox-08/ws/pilotdeck-rs-cc2c`；干净 checkout 固定 `ea6d8e71`，不切源码、不替换组件、不并发启动另一轮 Cargo。
- 原工具会话 `27973`；父 shell PID/PGID/SID `819567`，run.sh `819568`，cargo-nextest `819617`；没有所属容器和 bbx 容器残留。
- 命令 `cargo nextest run --workspace --no-fail-fast --profile ci --config-file ../.config/nextest.toml -j 6`；同一既有流水线随后运行 `cargo test --workspace --doc`，即使 nextest 失败也会执行 doc。
- 04:58:34 时仍在编译；交接复查已进入测试执行，最后复查日志进度到 6925/7983，**没有最终汇总**，不能推断全部通过。
- `logs/ea6d8e71-workspace-nextest/cargo.log`、`logs/ea6d8e71-workspace-doctest/cargo.log`；各自 `cargo.exit` 以及汇总 `logs/ea6d8e71-workspace-gates.exit` 当时均未生成。
- 下次先读完成标记和日志。需要停止时只处理此已确认 PGID，禁止按 cargo/node 名称批量杀进程。

安全续查：

```sh
ssh cb8 'tail -40 /home/clawbox-08/ws/pilotdeck-rs-cc2c/logs/ea6d8e71-workspace-nextest/cargo.log'
ssh cb8 'cat /home/clawbox-08/ws/pilotdeck-rs-cc2c/logs/ea6d8e71-workspace-gates.exit'
ssh cb8 'ps -eo pid,ppid,pgid,etime,comm'
```

最后一条仅查看进程；定位 PGID 819567，不操作其他任务。

## 已有验证证据

- **第四候选 ea6d8e71，Linux aarch64：** fmt、全 workspace/all-targets clippy、产品构建通过；15 项定向测试通过（native_component 3、image 1、PDF 1、TCP 9、端口选择 1）。T1 正式 14 场景全部 SAME（PDF 3、诊断 8、image 3），TS 双次自比同为 14/14；TCP 正式 default/orphan/active-repeat/completed-repeat 四组均通过 TS 自比与 Rust 比较。源码和产物身份检查通过。证据 `_tmp/20261113-e0-v0/fourth/` 及远端 `logs/ea6d8e71-*`。
- **原生组件单独身份：** MuPDF 1.27 源码构建，严格 6/6 包括 JPEG 字节；image helper `a903ff30` 与 libvips 8.17.3 / Sharp 0.34.5，真实 TS read_file 37/37 一致。首轮 34/37 的 EXIF/损坏 JPEG stderr 原始失败保留。helper 身份不能偷换成产品 ea6。Linux ARM64 动态依赖要求 GLIBC >= 2.38，macOS 组件和最终搬迁发行尚未验证。
- **图像正式装置：** 3 场景 TS 自比通过，6 份未修改副本被接受、10 份真实故障产物被拒绝；`_tmp/20261113-e0-v0/image-formal/{self-manifest,negative-manifest}.json`。
- **第三候选 8fa6e67：** Linux fmt/clippy/build、focused nextest 552/552 通过（7572 未选，不能称全量）；历史 SOP 4 组通过仅作历史证据，StaffDeck 现已延期。
- **本机旧第四开发二进制：** `_build/backend-validation/20261113-fourth-precheck/` 的 clippy-5/build-3 通过，TCP 4 组正式通过；CLI SHA `7d3ddddb60e35aa5eeb23c9b989c0ecbac0cd2dfa9c706c29f4e1e93b0b8254f`、sidecar SHA `92d98abcf84c92f5b93bb13434ca5dd14c5cb8696aafbc8265cae49810d04902`。它们早于当前根改动，不能用来验收当前 HEAD。
- 首次失败及修复原因保留；所有通过只对应记录的源码、平台、场景和产物，不倒填原始结果。

## 根检查点中待验证的内容

1. **默认 native 入口（D1）：** `gateway_mode(None)` 改为 native；显式 proxy/native、非法配置拒绝、参数优先级保留。省略 `--sidecar` 使用可选同目录路径，显式路径仍校验。新增实际 gateway 进程测试在无 sidecar、空 PATH、不传模式及 --native 下要求 native 公告、WS 101、SIGTERM 正常退出；旧 proxy 测试显式设置 proxy。差异账本 `specs/gateway-parity/diffs/D1-DEFAULT-NATIVE-001.md`。仅 rustfmt、语法/source guard、mode-routing 10 个变异锚点检查通过，**尚未 Cargo 编译或运行新增 Rust 测试**。
2. **通用扩展注册：** admission 使用未隐藏的 built-in 注册表，禁止以 excluded_tools 绕过保留名称/别名碰撞。真实 SessionTools 与公开 LocalGateway consumer 的四方向碰撞测试已写，尚未执行。此修复不依赖 StaffDeck，已从延期归档单独保留。
3. **发行工具（D1）：** `scripts/backend-release/` 实现 core/full 确定性装配、依赖许可、动态链接检查、完整性、无 Node 隔离 smoke。装置最后一轮 8/8 通过，但最后补 license supplement / AUTHORS 读取后未再检查；实际产品 release 构建、assemble/verify/smoke 均未执行。270 包首次许可预检 17 缺口保留；2 个 r-efi 许可定位到 AUTHORS，余 15 包所需 4 份固定来源许可已取回，但完整归集未复验，不能标已闭合。
4. **内建资源仍缺：** 本仓库尚无完整内建技能/插件资源；参考材料不得复制。full 包仅指原生组件组合，不等于资源或全部后端完成。macOS 原生组件、最终性能、组合重启与无 TS 环境验收未完成。
5. 旧 native-process 负控存在 4 个失效锚（configured-hooks、configured-modules、title-cancel-wait、title-timeout-wait），本次没有放宽断言，恢复时单列处置。

## C1 独立树：待集成与已知失败

C1 最终 HEAD 保持最后已验证逻辑提交 `bcad7cb1435103e31c9ec71877c7064f3cffc619`；未验证内容没有新增 Git 提交，仍保留在该工作树。独立归档 `_tmp/20261113-c1-stop-handoff/c1-unverified-wip.tar.gz` 含 146 文件（30 tracked dirty + 116 untracked），SHA-256 `f797f91cec2c56a8769585b585c50d77c959302bd7ecff082c264438b28f1486`；同目录 `manifest.json` SHA-256 `e4831933d4af7fc91ea2ffaf636d6c52e8de527986eac377c42af5eed02c8e77`。根协调已重新计算并核对 tar 和 manifest 的上述 SHA-256。另有完整 `git-status.txt`、逐文件 hash、`tracked.patch`、空 `index.patch`、`process-audit.json`。原始 `_tmp/`、`_build/` 证据留原位，不在源码 tar 内。不要清理该工作树或把其 dirty 状态误认为失控。

权威细节看 C1 的 `state/c1-cli-handoff.md`，协调聊天 `01a1d10e-8941-7392-bd13-9bb63886a7c9`（host local；用户已授权双方直接协调）。以下是停工前已收到的证据，不把后写源码算进旧结果：

- C1 H1/S1/M1 组合 all-targets check 第四轮通过（12.38s）；一次限定测试编译通过（2m10s）。该次不包含随后接入的 WS deflate、未挂接 WhatsApp、O1 和根装配 patch。
- M1 Google complete 新 2/2、旧 complete 9 通过/3 ignored、旧 Google stream 1/1、Google HTTP 6/6；shared cancellation ownership 25/25、network/WS proxy-CA 与真实 ConfigStore 6/6、渠道创建/重载 bundle 7/7、Mattermost 非压缩 12/12 均通过。
- S1 第一轮 10 项为 3 通过/7 失败，已定位测试 export 键序与真实 TS 相反，测试 oracle 修复后**未重编复验**。Signal 第一轮 16 项为 3 通过/13 失败，fixture port 1 被 Node fetch 拦截，已改假端口并加因果测试，**未复验**。HA 23 项为 22 通过/1 失败，取消启动时 WS 关闭断言未闭合，不能放宽为任何错误。
- H1 后续 WS permessage-deflate 已挂接为窄候选但未编译/测试；WhatsApp 20-test 独立候选仍未挂接。HA 当前 24 项中的新压缩用例另有已知 Pong 早于 handler 的同步时序风险，停工时未修。O1 最终 17 项测试及模块未挂接/未编译（其旧文档 12 项已过时），最后边界/Bundle 修正未完成独立复审；root 的 CLI/server/assembly/turn 接线尚未实现。其余全部渠道与 CLI 最终正式矩阵并未完成。
- C1 公共 ownership/原子 restore 改动仍需受控整合。根装配 patch 在 C1 `_tmp/20261113-h1-assembly/assembly.patch`，尚未应用。公共文件须单一作者，不能并行覆盖。
- SDK cancel 的两轮 TS 挂起已证明是 harness 等待被 SDK 过滤的 stdout partial，SIGINT 根本没发出；独立真实 SIGINT 因果诊断 2 轮 11/11 通过，证据 `_tmp/20261113-c1-cancel-cause/actual-sigint-verified.json`，SHA `4bb1efd19ec183b7bf1ec644d0f608483c4ac5c5bfc4fd36e676d6161cd6739f`。持久 run 事件精确触发的新 harness 已在 C1 归档，公共 runner 7 文件冻结，52/52 装置测试通过但独立审查未完成；provider 唯一性、posthoc 投影/信号绑定待审。新 harness 正式 TS 自比/Rust 重放均未执行，不能将旧挂起归因产品 abort。

## 下次恢复顺序

1. 阅读本文件、两边 `git status` 和 C1 最终 handoff，先收取 cb8 既有流水线的退出码/汇总，不重复启动测试。确认 StaffDeck 仍延期。
2. 本机唯一重型槽 jobs4、cb8 唯一重型槽 jobs6；正式/light suite 各最多 2。先验证根 `74336e9a` 默认入口与扩展注册这一窄批；不要让其他作者同时修改受测源码/二进制。
3. C1 按 handoff 验证其已冻结窄批，先关闭 S1/Signal/HA 原失败，再独立集成；不要将全部 WIP 一次混进主线。Google 已整合，禁止重复 cherry-pick。
4. 受控完成 H1 配置/交互总装配、O1 真正接线及剩余范围，不扩 StaffDeck。保持固定 TS oracle、实际故障负控及首次失败证据。
5. D1 先重新跑脚本语法与 8 项装置测试，再新目录重做 270 包许可归集；具备固定 release/metadata 后才 assemble → verify → smoke。完成内建资源、平台、性能及组合验收。
6. 最终须在同一集成 SHA 完成 fmt/clippy/全 workspace nextest/doc 和适用正式 BBX，逐项核对必需范围后才可称本期交付。当前没有可靠完成百分比分母。

## 不变边界

所有产品命令（含 help/version）隔离 HOME/PILOT_HOME；Cargo 使用 `/Users/da/.cargo`、Rustup `/Users/da/.rustup`。不读写真实 `~/.pilotdeck`，不触碰受保护 `~/ws/PilotDeck-rust-program-20261009`，不修改/复制参考材料，不下载预编译工具。产品外发仅 mock。未推送、未正式发布、未生产部署；没有对外发布授权。WIP checkpoint 只是可恢复快照，例外于普通“提交即已验证”的开发习惯，不是验收通过。
