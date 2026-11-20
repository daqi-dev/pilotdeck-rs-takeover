# 后端交付统一交接

更新：2026-11-13（Asia/Shanghai），本次用户已明确授权按本记录恢复核心后端交付。原停工快照保留在 `e8618392e7e4eb24c227eedd28e14c84237ca8a4`。**本期尚未完成交付。StaffDeck 本期继续延期，不作为门禁。**

## 本次恢复状态

- 根从 `e8618392e7e4eb24c227eedd28e14c84237ca8a4` 恢复；初始只有用户的 `.codex/`、`plan.md` 未跟踪，未覆盖。C1 的 tar 与 manifest SHA-256 重新核实均与下文一致，146 文件未改变。
- 当前实际额度为主协调者加 12 名子智能体；首批启用 E0、C1 验证、S1 独审、H1 协议、O1 遥测、D1 发行、SDK 装置审查及 H1 剩余渠道盘点，随后增加 native 回归修复。公共接线由根协调者持有；本机唯一 Cargo 槽交 C1 验证（jobs4），cb8 唯一重型槽由 E0 调度（jobs6）。
- 旧 `ea6d8e71` 全量已结束：nextest **7983 run / 7977 passed / 6 failed / 742 skipped**，exit100；doc exit0。六个失败为 factory production adapter、两项 PDF 工具、execute_code nested glob、unsafe boundary 构建环境检查及 write_tools 文件权限；分别归因与修复中，不能称全量通过。原 PID/PGID 已结束。详细证据 `_tmp/20261113-e0-resume/old-failures.txt`，远端原日志保持。
- C1 已完成四轮窄验证，首次失败全部保留：S1 新10+既有restore5通过；Signal17、HA25、WS8、MM13、O1模块18通过；第四批 WhatsApp macOS24、shared/network18、owned25全部通过（67次执行/62个去重测试，0ignored）。最新 C1 binary `2f77ecdd817835d0d313044d65c11a9c8e66b9253901a7eccdbc68f599f68b4b`，汇总在 C1 `_tmp/20261113-c1-resume-validation/{results,third-results,fourth-results}.json`。这不代表根集成或正式入口通过。
- SDK 装置补齐全 provider 唯一性、信号 PID/时间证据核验，固定源53项装置测试通过；C1 `docs/backend-delivery/C1-sdk-cancel-review.md` 已完成独审。正式 TS 自比/Rust 重放仍未执行，尚未移入根。
- 根主分支 HEAD 仍为 `e8618392`。独立 core 验证候选 `977026b2922e7196afcafa747e7865a201a15a81` 通过 clippy、9项定向lib、15/16 integration；首败分别为fmt和综合工具的时区别名。下一候选 `824b9160546691aa365aac6b88144cdee99adeb8` 在其上增加同源 `tzdata.zi` Link解析、fmt修复及局部变量命名修正（避免源码扫描器把变量include误认宏）。824的fmt、gateway+transport clippy、transport system_clock44、lib9、integration16全部通过。四项实际负控为3 KILLED/1 SURVIVED/0 INVALID，四项均恢复通过；hooks原注入检查规范化config中已剔除的字段，恒不触发。新窄候选 `cf143ff81c2277745ccd6870e5e1151de811561e` 改到实际diagnostics并补正向因果前置，仅重跑hooks，尚未合主。
- `A8.3-CRON-TZ-ALIAS-001` 末尾明确未批准，不是已接受差异；本次没有据此放宽oracle。cb8同源tzdata.zi已有 `L America/Los_Angeles US/Pacific`，产品修复利用此记录，不修改系统时区树或手写别名表。
- 根工作区已受控三方应用 C1 S1/ownership/H1共享及首三渠道101文件，另应用已测WhatsApp27文件；均为工作候选，清单 `_tmp/20261113-c1-root-integration/`，未直接合C1分支。七个剩余渠道独立作者已产出新模块，QQ/WeCom的bundle及共享命令真实宿主仍待接，不能因有代码而称已迁移完成。
- O1在根补充正式server/local/assembly/turn/maintenance接线及跨runtime ownership（root21专项，C1只有18专项）；尚未根编译验收，统一automation/channel reload顺序仍未闭合。根 `TurnRequest` 补附件、plan工具策略、synthetic消息和project字段；MD5仅使用已有缓存的固定RustCrypto md-5 0.10.6源码，完整metadata现在277包，发行许可需最终候选复验。
- 根首次gateway+CLI all-targets check exit101有37诊断（含lib/libtest重复）；第二次仅余Email测试E0509；修复后第三次fmt及all-targets check均exit0。正在同一冻结源码编译gateway lib测试二进制（jobs4，根src/target），尚未跑根WIP测试。精确首次日志与三批结果在 `_tmp/20261113-c1-resume-validation/` 和 `_build/backend-validation/`，所有原始失败保留。
- 824 core v2父PID1136738已结束且clean；cb8唯一在途为cf143ff8 hooks-v3（父PID1285278，`logs/cf143ff8-hooks-v3`），E0持有单路构建/负控队列。cf143ff8的fmt/clippy/66静态锚已过，实际hooks正控/变异/恢复在途。既有target为满足负控守卫已原地迁移至 `checkout/_build/negative-target`（约315GB），后续显式指定该路径，不重复创建src/target。977负控两轮均为装置INVALID、未产生变异：首次target在src内，第二次include标识符被保守扫描器误报；v2保留判据且更名局部变量后重做。
- D1装置9/9与276包完整许可归集通过；根新增MD5后的277包完整归集也已通过，证据 `_tmp/20261113-d1-resume/license-probe-277/`。这只是当前锁定缓存材料验证，最终clean发行候选仍需同机重新归集。release构建、assemble/verify/smoke、内建资源、平台、性能和组合恢复仍未完成。新渠道第三方SDK未锁/未装的部分明确只有固定adapter mock合同，不冒充真实SDK协议或账号验收。
- 后续已分派：QQ/WeCom真实bundle持久化与启动/重载顺序；共享commands真实搜索/更新/状态宿主；SMS永久noop、Slack缺native提交器、BlueBubbles缺真实交互的旧渠道修复；O1统一reload chain/动态cron及always-on发布/正式after-turn maintenance；Email TLS与自行限制清理；32个技能/3个插件的独立发行资源。均在进行中或待解冻，不能因mock存在宣告交付。
- 本节是当前状态；下文“停工”“下次恢复”叙述属于先前快照，新的验证结果只追加实际证据，不将旧成绩移记到新源码。

## CLI v1 线（agentcpm-pd）

- 线性质：新 CLI 第一性方案（007），独立分支 `codex/agentcpm-pd-cli-20261116`（工作树 `/Users/da/ws/pilotdeck-rs-agentcpm-pd`），不受本文件冻结约束。
- 正本链：方案 cc2c `docs/plan/007`（v0.7.2）；任务台账 cc2c `state/cli-v1-design.json`（46 任务）+ `scripts/cli-v1-status/README.md` 操作手册。
- 当前进度一句话：S1–S5 全部收口（final46 双平台四条全绿、23/23 旅程 e2e），t023 端到端验收进行中（唯一登记缺口 `resources.watch` 与 J4 快照接线，本轮修复中）。
- 进度细节不在此维护，以台账与 cc2c `_tmp/20261119-s3-gate-disposition.md` 等登记为准。

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
