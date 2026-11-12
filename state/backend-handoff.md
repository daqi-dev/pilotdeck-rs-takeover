# 后端交付交接

更新：2026-11-13（Asia/Shanghai）。本文件是本轮唯一进度入口；范围见 `docs/plan/backend-completion.md`。当前尚未完成交付，继续执行，不以阶段进展代替最终成果。

## 身份和约束

- 本机 `/Users/da/ws/pilotdeck-rs-cc2c`，`codex/backend-completion`。原始 Rust `e8702d36c82cd5fa030d5e8ba3e724b0bfa68c95`；TS固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，参考路径 `/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，只读。
- 最新固定产品候选 **第三批 `8fa6e677e4312fb13a71f46d084cebc5fdd169a4`**。其后 harness/组件/依赖已合至 `4db0e19c`；第四批产品仍为未提交开发树，启动先看 `git status`，不得 reset。用户 `plan.md` 和 `.codex/` 保留，不纳入产品提交。
- cb8 Linux aarch64：`/home/clawbox-08/ws/pilotdeck-rs-cc2c`，`checkout` 保持8fa供验证；独立组件源码clone允许后续提交，必须分别记录身份。
- 全部产品命令（含help/version）隔离 HOME/PILOT_HOME，Cargo使用 `/Users/da/.cargo`、Rustup `/Users/da/.rustup`。不得接触真实 `~/.pilotdeck`、受保护的 `~/ws/PilotDeck-rust-program-20261009` 或参考材料。测试外发全部mock；没有真实服务、推送、发布授权。
- 根会话实际额度是主+3子智能体；项目配置12未热生效。唯一本地重型槽jobs4，cb8唯一重型槽jobs6，各最多2个正式harness/BBX。不要杀别的任务进程。
- 独立C1聊天 `01a1d10e-8941-7392-bd13-9bb63886a7c9`（host local），用户明确授权双方直接协调。worktree `/Users/da/.codex/worktrees/c1-cli-completion/pilotdeck-rs-cc2c`，branch `codex/c1-cli-completion`，实际已创建12子智能体，当前8活跃/4空闲或已完成；加本会话3活跃，共11活跃子智能体。两协调者另计，不新建聊天/增槽。同步merge `8086ab52` 不得cherry-pick。

## 分工和当前状态

| 包 | 状态 | 负责人及下一步 |
|---|---|---|
| E0/V0 | 进行中 | env_validation；8fa固定Linux验证、组件源码构建、harness负控与身份 |
| A0/X1 | 进行中 | root；总装配、公开扩展/注册与最终独立消费者回归 |
| R1/R2 | 进行中 | native_runtime；SOP取消drain补丁、TCP重复请求审计及正式对拍 |
| B1 | 进行中 | staffdeck；知识、SOP失败/取消/多会话隔离，随后外部skills |
| C1/H1 | 进行中 | 独立C1根；非TUI CLI、全部渠道；root保留Weixin fetch与总装配 |
| T1 | 进行中 | root；native组件生命周期、PDF预算/字段顺序、libvips图像、诊断与history |
| S1/M1 | 进行中 | C1复用作者；原子restore/锁，Google complete及后续上下文审计 |
| O1 | 进行中 | root；遥测仍为真实缺口，已请求C1空闲作者做新模块，root保留接线；cron/always-on需最终回归 |
| D1 | 进行中 | root/E0；默认native已接，完整/最小发行、无Node与资源/性能仍待验 |

主线assembly/main/Cargo/lib由root唯一写入；C1公共文件例外已明确授权（channel mod/loader/protocol、native_process/channels、turn submit/handle/completion、permission_prompt/elicitation/request_user精确取消RAII、session_archive/storage transcript与factory structured writer lease、model Google complete/http）。不要并发改这些文件。C1提供assembly patch后root应用。

## 已有证据

- 第一候选50cfd7c9 Linux fmt/clippy通过，nextest415通过2失败；第二14351 Linux clippy首败42项、nextest421通过1失败；首次日志保留，第三修复未倒填旧结果。
- 第三8fa本机gateway/CLI全targets clippy/fmt通过；SOP new_session hooks+receipt、correction、recovery_pending、recovery_durable均TS双自比+Rust通过，`_tmp/20261113-B1/rust-third-*`。节点输入包装、nested prompt来源首两次失败已定位修复，原始目录保留；live stream211通过12显式TS依赖ignored。
- 第三8fa cb8全targets clippy/fmt通过（57.24秒），CLI/gateway/transport构建通过（47.96秒）；四组上述SOP正式suite均通过，`logs/8fa6e677-sop-formal/`。focused nextest正在由E0跑，未完成全workspace及doc。
- 第四开发树 `_build/backend-validation/20261113-fourth-precheck/`：check-3、clippy-3均全targets通过，之前JsValue/B1测试/MSRV/sidecar clippy首败保留并修复。CLI+transport build通过；CLI SHA256 `e613e90110bef5cc354e0d210badc89e85432e083c19b982fbe3445d52c0afc0`。
- 第四定向17单测全过：knowledge5、staffdeck6、真实SessionTools collision1、native_component3、image1、pdf1；`selected-results.json`记录实际test binary/hash/count。
- TCP旧binary在macOS accepted socket继承O_NONBLOCK导致正式peerEOF和5单测失败；attach显式set_nonblocking(false)后TCP8单测全过，sidecar SHA `9d7482957cdf397afc9ad5c08f119dd42e52cfe74cc2cc39ba86d17d4569a9eb`。`_tmp/20261113-R2/formal-compare-2`与`orphan-compare-1`两组TS双自比+Rust通过/artifactsUnchanged；非resume EOF差异账本 `C-TCP-NONRESUMABLE-EOF-001.md`。active duplicate精确行为仍待R2补测。
- 第四知识正式5组全部通过（inherit/new_session/stale/untrusted/invalid），`_tmp/20261113-B1/rust-fourth-knowledge-*`。SOP cancel_model首败：HTTP已关闭、后续会话可用，但合法aborted结果被丢为agent_invalid_state；B1/R1补丁已稳定，等待重建复验。后续cancel_prepare/owner_error/isolation尚未Rust验证，不可宣称通过。
- cb8固定源码构建Node22.22.0、pnpm10.32.1、nextest0.9.148、rg15，MuPDF1.27组件6/6含JPEG字节一致。libvips8.17.3与Sharp0.34.5均源码构建/装载成功，静态codec链接修复有日志。
- 图像37组真实TS read_file oracle：helper首轮34字节一致、3失败（EXIF顺序访问两预算、损坏JPEG额外VIPS警告）。a903ff30补StaySequential/autorot物化与Sharp式日志处理，待E0独立组件重建复验。不能写成37全过。脚本4cb310cf支持PDRS_IMAGE_SOURCE独立且真实路径限制。
- 14351的17正式BBX原始5SAME/12DIFF保留：PDF键序3（第四已改待正式）、诊断8（严格窄策略cbe99b49与T1账本R1，负控32全拒绝）、身份1。V0扩展56项、比较器61项通过；loaded Sharp/MuPDF/tokenizer/共享库完整身份已补。
- C1已合：YAML JS数字键排序a4da54f1，SDK cancel fixture6104de64，严格capture comparators a5e78c34，HTTPS完整身份bf56b970。Linux Docker HTTPS fixture使用network none无hostport，E0实际双TS采集中。

## 当前运行与下一步

1. **本地重型槽已交C1**：冻结manifest后只做一次jobs4 `cargo check -p pilotdeck-gateway --all-targets`；主线不得同时cargo build/check/test。完成回报后root取回槽，集中编译SOP取消补丁。两正式轻量槽已空；B1等新CLI，R2可跑现有TS oracle。
2. B1/R1稳定补丁：Factory wait_owned信号取消后drain已准入工作、不启动下节点；NodeFactory保留真实aborted终态；provider用户取消仍forward已持久partial，parentclose/waiterdrop只drain。root需审查及重建后让B1复跑cancel_model，再cancel_prepare/owner_error/isolation；不放宽失败标准。
3. E0持cb8构建槽跑8fa focused nextest（报告会话21841，需向E0确认）；然后独立a903 helper重编+37场景。HTTPS Docker两轮TS可轻量并行，不在Rust test重链接期间捕获Rust。
4. 第四未提交包括knowledge/SOP、native_component/image/PDF/read_file、TCP；完成定向修复与正式验证后冻结第四候选交E0。同一场景中不得覆盖二进制。
5. C1 Signal/Mattermost/HomeAssistant、共享网络/精确取消、S1原子restore、M1 Google complete冻结待check。WS压缩20格oracle确认8位窗口；root依赖提交936271b3(flate2) +4db0e19c(zlib-rs safe API)，已要求作者同步。WhatsApp独立新源码待集成，其他渠道尚未完成。
6. O1已有placeholder证据：CLI server固定零snapshot，assembly telemetry None；读TS collector/sender/context和observer后实现真实队列，所有测试绑定loopback。已请求C1复用空闲作者做新gateway telemetry模块；若未确认不要双方重复写。root保留CLI/native server/assembly接线。
7. 更新本文件和包报告；最终仍需同一集成SHA全量fmt/clippy/nextest/doc、适用BBX、native/外部模块和StaffDeck组合重启路径、最小/完整发行包、无Node、性能和跨平台报告。GitHubCI尚未实际运行；真实外部服务保持明确待验。

## 全局收口调整（用户询问进度后已执行）

- 明确承认批次过大、首编偏晚、集成及文档滞后；不将不同新根因当作效率合理的证明。停止扩大本批审计，只完成已有矩阵；WhatsApp/O1已分配独立任务继续。P0条件具备，P1主干有证据但完整验收未关，P2进行中，P3未完成，不报告无可靠分母的完成百分比。
- Linux8fa focused nextest已完成552/552通过、7572未选，不能称全量。a903 helper独立37/37已通过。TCP最新4组正式全部通过（formal-compare-3/orphan-compare-2/active-repeat-2/completed-repeat-2），binary92d98abcf84c92f5b93bb13434ca5dd14c5cb8696aafbc8265cae49810d04902。
- 本地clippy-5通过46.52秒/build-3通过54.36秒；clippy-4测试模块位置首败保留且修复。新CLI 7d3ddddb60e35aa5eeb23c9b989c0ecbac0cd2dfa9c706c29f4e1e93b0b8254f：B1 cancel_model-2首轮取消已完全修复，整体仍失败于恢复轮父wire最终回复超前于已排队的节点事件。B1/R1正在统一审查事件顺序屏障，暂不扩新变体。
- 唯一本地重型槽又交C1一次定点修正后的all-targets check（首失败10lib/39test含重复，未运行测试）；主线暂不cargo。C1返回槽后root跑最新SOP/TCP相关单测。E0无重型任务，正在独立审T1组件管理并准备既有3条正式image TS自比。
- C1 SDK cancel存在两轮同TS挂起，未标通过，已要求复用空闲作者定位实际读流/abort归属，不靠延长timeout。全量四门、适用BBX、性能与完整发行仍必须执行。
