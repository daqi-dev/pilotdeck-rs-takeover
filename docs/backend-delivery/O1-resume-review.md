# O1 恢复后的独立审查

2026-11-13；审查目录为 C1 worktree。只读审查 Rust/Cargo，未编译、未提交、未编辑公共接线；本记录不表示交付。

## 本轮实际证据

重跑固定 TS `edges.mjs` 两轮成功，第二轮精确 compare 第一轮。原始目录 `_tmp/20261113-o1-telemetry/edges-resume-review-{a,b}` 包含 driver/source identity、环境、HTTP、context、oracle。由既有 source-identity 检查固定 cc7c4e6 对应源码及实际 dist 模块；未修改 resources。真实 loopback 验证非法 UTF8 token replacement/hash、sanitize 的 __proto__ setter 与 spread own-property 区别、零 in-flight receipt。Rust 相应修改已静态复核，17 个专项仍未编译，不能称通过。

额外实际 TS 因果探针 `nonzero-drain-review.json`：第一次 observe held → whenDrained receipt → release 第一条并等释放微任务 → 第二次 observe held → 旧 receipt 首次等待仍立即完成。TS 源码 `TelemetryObserverRegistry.ts:316-320,345-348` 在首次归零时 resolve 已创建的 Promise。

## 新发现阻断

`telemetry/registry.rs` 的 when_drained 在非零时订阅 count watch，receipt 仅在 poll 时检查当前 count。若在 receipt 首次 poll 前旧观察归零，随后新观察进入，则 watch 当前非零，旧 receipt 会错误等待新观察。新增零状态测试不能覆盖此问题。建议在 RegistrationState 保存当前 drain generation 的一次性完成 receipt；同步取得 receipt，Release 首次归零时永久完成并清除当前代，后续 acquire 不改变旧 receipt。因果回归必须用旧任务 release acknowledgement 保证归零后再准入新任务，随后 poll 旧 receipt Ready、新 receipt Pending；不要以 sleep 断言调度顺序。冻结解除后仅修改独占 registry/tests。

其他新修正静态复核：context 已先解析 deployment mode 再读 token，read bytes + UTF8 replacement；sanitize 只在赋值路径调用 set_prototype；registry 已排空 registration 通过 started receipt 保持反向 disposer 同步前缀顺序；bundle 注入 base 不 shutdown，自建 base 即使 observers dispose Err 也 shutdown，persistent worker 持有责任。以上需要集中编译和行为验证，非运行结论。

## 公共接线方案

1. `lib.rs` 导出 `pub mod telemetry`。模块只依赖现有 protocol JsValue、共享 ModelHttpTransport/fetch、maintenance trait；不需要新 Cargo 依赖。构造必须在现有 Tokio runtime 内（sender 创建 timer/worker）。
2. server 在 YAML 加载后创建唯一 Arc<TelemetryCollector>：RuntimeContextOptions::from_process(env,pilot_home,cwd) → resolve_runtime_context → TelemetryCollectorOptions，enabled 使用 YAML telemetry.enabled 覆盖 env；transport 使用现有共享 Arc。替代 CLI telemetry::probe_runtime_context，避免重复 git 探查。runtime context 发生错误仍按真实错误返回，不造成功值。
3. host/assembly 接收可选 Arc<dyn TelemetryClient>，用 GatewayTelemetryBundle 注入它，保留 collector 到 server shutdown。bundle.client() 交给 GatewayAgentEventTelemetryObserver 与所有业务调用；bundle.observers() 给 live observer 管理。无注入的独立 Gateway 则让 bundle 自建，只有该 bundle 负责关闭 base。
4. turn admission 解析 resolve_turn_context(channelKey, request.telemetry)，构造 GatewayTelemetryContext(session/run/channel/permission/owner/execution/phase)。在真实 loop_start、通过 timeout/current-generation 过滤后的 Agent 事件、Gateway loop_error 三处调用 observer；不可在已有事件转发链重复观察。维护任务用同一个 collector 实现 MaintenanceTelemetry；现有 assembly maintenance telemetry:None 是待接入口，具体 opaque wrapper 沿用当前 memory assembly 的类型。
5. reload 对同一 base 调 set_enabled；保持已实测 collector/sender config alias timer 行为。shutdown 时先 channel state flush，再输出 collector.snapshot()（替换 server 常量），再 dispose Gateway bundle，最后由 server await collector.shutdown。不得输出最终发送后的 snapshot 冒充基线时点。

以上公共文件由根唯一写入；此子任务不编辑 lib/assembly/CLI/Cargo。模块可独立挂载，native_process 接口新增字段时所有 ServerOptions 构造点由根统一更新。

## 窄差异准备

建议新建 `specs/gateway-parity/diffs/O1-TELEMETRY-SHUTDOWN-INFLIGHT-001.md`，按现有 README 六字段。类别为丢数据 bug 修复；固定依据 sender.ts:80,111-119,167-182 及 module-second/shutdown-inflight-baseline/raw.json。明确只修 shutdown：同步停 timer，等待同一 owned flush 成功/失败/超时，再 persist 未发送队列；取消外部 waiter 不取消持有责任。普通 flushing 时 flush() 仍即时返回；重试逆序、重试上限、enabled alias timer 不变。验收包括 held headers 成功（shutdown 等待，成功后无残留）、held headers timeout/503（失败队列持久化且重启恢复）、drop shutdown waiter、重复 shutdown 共用receipt，及普通 flush 即返负控。当前两个 mock Rust 测试不代替真实 socket/正式入口；账本批准持久指针由根填写，不能据历史作者文档自动声称批准。

## 验证队列

交给 c1_validation：先冻结候选全文件 SHA，再 telemetry::tests 17 项及新增 drain 回归；首次错误保留。根接线后真实 Rust server 用 formal.mjs 对同平台 TS 自比目录；验证四事件、snapshot 时点、真实 collect、queue 删除、退出码、reload enabled、正式禁用遥测负控（必须在 telemetry 断言失败）。sender 另补真实 socket headers-only、timeout释放和在途shutdown差异。现有正式模型文本负控只证明 WS 断言，不能替代遥测负控。最后按集成批次四门，不由本子任务占用构建槽。

占位扫描 telemetry 产品文件无 unimplemented!/todo!/NOT_IMPLEMENTED/TODO；默认未知 Agent 事件空分支符合 TS ignore，路径 CurDir 分支跳过点路径，trait 默认 dispose 对应 TS 可选 disposer。未发现 TS 业务转发。

## 冻结解除后的修复

根已解除 C1 Rust 冻结并授权 O1 独占目录修复。registry.rs 新增当前非零代 drain completion，when_drained 同步领取代际 receipt，Release 首次归零永久完成旧代。新增 nonzero_drain_receipt_stays_completed_after_a_later_observation，旧 receipt 首次 poll 前先完成旧观察再准入新观察，断言旧 Ready / 新 Pending。仅 rustfmt 完成，现为18项专项待 c1_validation 编译。shutdown 窄差异已新增 O1-TELEMETRY-SHUTDOWN-INFLIGHT-001，引用用户计划授权，保留真实 socket 与正式接线待验证状态。

## root 公共接线第一批（待编译）

开发树切换 root `/Users/da/ws/pilotdeck-rs-cc2c`；保留 C1 模块源不覆盖。已复制稳定 telemetry/fixture/交付文档/差异，导出 lib::telemetry。Options 增加显式注入 NativeTelemetry 与预解析 context；ServerOptions 传真实 context/YAML enabled；LocalOptions 传可选预解析 context。CLI 两入口用真实 context 替代两份模拟 git probe，不重复解析。server collector 在 Tokio runtime 内构造；server 注入 base 生命周期归 server，assembly 自建 bundle 的 base 生命周期归 bundle。

server snapshot 从固定常量改为真实 collector.snapshot，在 switch ProductionFaces 成功 channels.flush 后、stop_owner 前执行；serve_until_signal 返回后仍在 runtime 内 await collector.shutdown。assembly 新API消费、reload、maintenance、SubmitHost trait与turn hooks由协调者和本子任务按独占范围接续，当前尚未全部接通，不声称产品可用。

首次 C1 精准编译 exit101 暴露 O1 Arc import、poll! feature、JsValue Drop move、Undefined 临时引用，已在 C1 与 root 同步修复。测试用 FutureExt::now_or_never 对 Pin<&mut> 单次 poll，不增加 Cargo feature；base 直接构造 JsObject；Undefined 改 static。保留首次失败路径 `_tmp/20261113-c1-resume-validation/second-first-compile-errors.txt`。本子任务未运行 cargo。

## 正式 CLI/mock 证据要求

在公共接线及集中编译通过后，先固定源码 manifest、完整 SHA、二进制 SHA 与同平台 TS身份；产品命令全部用新 HOME/PILOT_HOME、假token、随机loopback地址（固定18789场景只Docker私网）。每次输出目录新建，首次失败不覆写。

- 正常 server：同一正式 formal.mjs，TS 双跑自比后 Rust --rust-bin/--rust-root/--rust-sha 对比；公开 WS submit_turn → mock 模型 → 四事件真实 HTTP，snapshot queued4/sent0/depth4，随后 collect，JSONL 删除、SIGINT code0。事件字段/顺序精确，UUID/时间/路径只按既有已验证公式归一化。
- 禁用与负控：ANALYTICS_ENABLED=true 而 YAML false 时无collect；正常金样场景 --mutate disabled-telemetry 必须在遥测断言失败，不能以模型输出负控代替。reload false/true 分别验证同一 collector 配置与实测 timer alias，必要时显式触发批次发送，禁止伪造setEnabled成功。
- snapshot 因果：真实 turn事件先入队，hold发送headers，确认日志在channel flush完成后且collector.shutdown仍pending；释放/503/timeout后确认最终queue与metrics。必须证明snapshot早于最终flush完成，不能仅比较打印文本。
- 生命周期：server注入base只由server最终shutdown；直接native、本地CLI自建base由bundle.dispose关闭；live observer disposed/error不跳过自建collector shutdown。普通CLI remote成功但已构建fallback时也需清理fallback collector。取消外部等待不取消owned发送/清理责任。
- 失败与恢复：真实socket held headers成功、503、timeout与重启恢复覆盖窄差异；检查失败后磁盘恰含尚未发送记录，成功后无重复恢复。保留metrics/request/jsonl/socket-open/退出码及源码身份。检查无额外网络目标，所有mock请求记录后按PID精确清理。

只有同一集成候选专项、正式入口及适用四门通过后才更新交付状态。

## root 最终公共接线候选（未编译）

根已授权并释放 assembly.rs/submit.rs/submit_host.rs；C1 ownership 版本作为基底，未覆盖其owned::prepare/流取消/drain逻辑。hooks已挂：loop_start 在真实 prepare 前；Agent mapper 在 timeout/current-generation 判据之后、原observe_event之前；pump Err在原fence/terminal逻辑前记loop_error。Agent attribution与permission使用loop开始时的固定context，避免后续live permission变化改写历史归属。GatewaySubmitHost返回bundle.client，并按现有requested/live/default模式解析实际permission。

SubmitError新增可选原始Error.name，普通错误缺省Error；RuntimeError.js_error.name、config Error.name、SdkSessionError.name均在已有来源转换处保留，未根据业务code猜类型。唯一现有SubmitError字面量测试补None，协议code/message保持原输出职责。

assembly创建单GatewayTelemetryBundle，server注入base由server持有，直接native/local由bundle自建；outer资源构造失败会await dispose。host cleanup在automation/channel/session/project清理后调用bundle.dispose，将失败纳入既有结果；observe错误不跳过owned collector shutdown。配置订阅显式unsubscribe。项目工厂opaque MemoryTelemetry包装同一bundle client；生产RefreshBundle现在使用真实ProjectMemoryMaintenanceController（按精确published runtime调度），不再走旧NoMemoryMaintenance默认构造；旧测试/独立构造API保持可用。

H1仅assembly接线：NativeInteractionResponder使用同一GatewayInteractionCoordinator；settings创建后、startWatching前调用NativeChannels.attach_config_store；startup channels全部入队后activate_config_reload。未改channels实现。该接线不代表所有config reload链的次序/错误恢复已完成正式验证，automation现有update_subsystems no-op仍待其它任务处置。

实际双runtime边界要求额外持有所有者：sender与bundle现在捕获创建时Tokio Handle；Gateway host线程触发enqueue/dispose时，发送和base清理worker仍属于collector/bundle原runtime，避免host先退出时取消外部持有collector的发送责任。root新增两项跨runtime因果测试：sender外部runtime退出仍发送并等待held headers；bundle调用者runtime退出/observer disposal失败仍关闭owned base。root专项现在20项，C1模块候选仍18项；二者不能共享通过声明。新增测试未运行。

本批文件级rustfmt(skip_children)、全树git diff --check、gateway frozen检查通过（30条）。未运行Cargo、未提交。root候选公共文件和20项测试仍须集中编译及独立审查，再执行正式CLI/mock矩阵。

## root 首次check及独立审查修正

root 首check exit101：services.rs 的 turn_model_selection 返回RequestError（无name），此前新增读取其name不合法；已移除该访问。另两条config/sdk与RuntimeError.js_error的真实来源继续保留；RequestError边界此前已压平的Error.name没有凭业务code猜造，正式模型选择失败路径的来源恢复仍需扩充该真实边界后验证。

独审指出上一版bundle只将base清理worker迁到owner，registry的Pending observe/dispose仍在调用方runtime派生，会中断held观察；已将registry及各registration共享创建runtime的OnceLock<Handle>，三个worker路径均使用owner。无runtime构造的独立registry在首次异步操作绑定一次，正常bundle在创建runtime即绑定。同步observe首poll/dispatch屏障/同步stop admission保持。原先“caller退出导致observer dispose Err、仍关base”的弱测试已替换为强因果：foreign runtime发track+dispose后退出，held观察仍inflight，dispose仍Pending且base未发送；release后observer.dispose完成再base shutdown成功。

telemetry配置订阅现有显式Options.telemetry_reload，仅server=true；native/local=false。监听器只对changedPaths等于telemetry或以telemetry.开头调用set_enabled。新增真实ConfigStore回归：无关模型配置变化不能重设当前client enabled；telemetry开/关变化生效；unsubscribe后不再生效。H1 adapter reload同时保留local CLI不启动channels边界：in_process_cli不attach/activate。根未展开统一reloadChain的剩余工作。

skipPermissions项经独审更正：正式Services恒返回Some(default)，固定TS createLocalGateway同样传默认mode，fallback在本轮生产入口不可达，未作为P1扩修。

root专项现21项，C1仍18项。上述修正后rustfmt/diff-check/frozen30通过，第二次编译尚未运行；首次失败不擦除。

## Maintenance 崩溃隔离与 SDK 公共接线候选

根在正式 TS 两轮自然退出证据后授权 O1-MEMORY-MAINTENANCE-RESOLVE-001。Services::after_turn 已仅对后台 resolve/schedule 错误保留成功 turn、不传播 submit Err，保留原 TypeError 的 runtime 错误遥测并输出项目诊断；下一次 after_turn 仍调度。服务调用错误/取消/queued-repeat/精确 generation 租约路径不扩大。新增 Services→实际 RefreshBundle 的项目回退与原session/run身份、首错后二次成功、准确单次 ErrorName/handler input 测试，仍待集中编译执行。

新 `maintenance-formal.mjs` 两轮真实 TS server/WS/mock 成功，`_tmp/20261113-o1-maintenance-formal-ts-{first,second}` 的 contract.json 和 driver.mjs 逐字节相同。真实结果为四条feature加一条runtime error、queued5/sent0/depth5 snapshot、首turn_completed、关闭WS后自然exit1/signal=null、queue删除。旧S1第二turn探针会在shutdown窗口额外引发session error，因此本driver的TS分支不再发第二turn；Rust正判仍必须证明第二turn成功。原S1失败/重复取证保留。

准确wire error properties只有module=runtime、ownerModule=runtime、loopStage=loop_end、errorCategory=runtime_error、code=TypeError。handler传入的metadata.source=unhandledRejection被既有collector丢弃于该首事件，runtime又不派生feature；不能凭handler input发明wire source或sessionId。unit spy验证handler input，正式driver验证真实五字段。

受控Rust故障生成器只生成独立positive/negative源码overlay，不修改/构建shipping源码；在真实resolver acquire前对特定隔离项目注入一次TypeError，negative额外仅恢复旧同步Err传播。driver要求保留源码overlay哈希与实际二进制SHA绑定。初次overlay已生成于`_tmp/20261113-o1-maintenance-mutants-first`，之后assembly已继续接SDK，正式构建前必须对最终候选重新生成新目录，不能沿用旧overlay哈希。Rust正式正判/旧路径负控尚未运行。

SDK公共接线已按作者精确API：ConfigChange走既有RefreshOptions.sdk=same SdkEventBus而非server专用第二订阅，保持空changeClasses与native/local边界；FileChanged紧随with_hooks配置with_sdk_events。新增真实settings reload→RefreshBundle→samebus→HTTP ConfigChange回归，仍待编译。session创建一次sdk_snapshot原子取得config/plugins；一个plugin contribution lease的hooks/MCP/commands/skills沿同一静态快照传递，逆序清理保证MCP/hooks先于plugin lease、plugin lease先于project lease。同快照InputProcessor在record/model前消费directory-plugin命令。disk manifest无programmatic prompt贡献，旧JS函数加载已排除；原生X1 prompt仍走extension_lease，不为Vec<String>臆造命名或插入额外system prompt。

旧native_settings/native_settings_for_process contained-harness便利构造及NoMemoryMaintenance已获根确认改为明确test-only；它们原本用于设置replay不运行maintenance，如今生产全由带真实leased controller/SDK bus的with_maintenance组合替代，不使用allow dead_code掩盖产品空实现。observer未用import及registry let-chain clippy项已精准修。

本批仍为未编译候选，不把TS自比或新账本存在当Rust通过。下一步根冻结最终源码后集中check/tests；维护故障两种源码overlay与正常shipping入口分别验证。
