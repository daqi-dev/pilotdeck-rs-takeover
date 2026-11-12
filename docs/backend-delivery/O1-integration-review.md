# O1 公共接线独立审查

2026-11-13；根工作树首次 all-targets check 冻结期间只读审查，未改 Rust/Cargo、未执行构建或产品。证据为冻结候选源码，不表示编译或正式入口通过；`O1-resume-review.md` 中专项成绩不可外推本集成。

## 待修问题

1. **跨 runtime 的 observer 工作仍归调用方。** `telemetry/bundle.rs:dispose` 在 owner handle 的 spawn 之前调用 `observers.dispose()`；`telemetry/registry.rs:205-231,355-385` 随即在当前调用方 runtime 启动 registry 和 registration disposal worker。正常 Gateway host 在独立 runtime，host 超时/退出可取消这些 worker；owner runtime 中的 bundle worker 只等其 receipt，不能确保 observer drain 后再关闭自有 base。同样 `registry.rs:290-320` 的异步 observation 在调用方 runtime spawn，单独把 dispose 搬进 owner runtime 不足以保护之前已接纳的观察。建议 registry 持有创建时的 owner handle，pending observation 和 disposal 均归该 handle，同时保留 observe 同步首 poll、dispatch 栅栏和 dispose 同步停止 admission；测试在调用方 runtime 退出前已进入 held 异步 observer，随后确认 base 未关闭，释放后观察完成再关闭；另覆盖借用 base 不 shutdown、重复和丢弃 waiter。
2. **telemetry reload 订阅范围过宽。** `native_process/assembly.rs:1981-1990` 无条件响应每次配置事件，且安装在所有 assembly。固定 TS `src/cli/pilotdeck.ts:137-141` 仅 server 分支、仅 changedPaths 含 `telemetry` 或前缀 `telemetry.` 时调用 setEnabled；固定 `createLocalGateway.ts:689-699` 本身不安装此订阅。local CLI 用自建 collector 时启用来自环境；无关配置事件可用 YAML 值重设它。建议仅 server 装配订阅，按 changed_paths 过滤，测试 server 无关更改不触发、真正 telemetry 变更触发、local 同一变更不触发，并检查 enabled/timer 的实际行为。server 初始 snapshot 始终规范化 telemetry.enabled，不能未经实测假设无 YAML 必然由 env 控制。

## 已核对及边界

- server 建立唯一 collector，注入 assembly；memory factory、maintenance 和 turn host 都通过同一 bundle client，server 正常及启动失败路径调用自身 collector shutdown。bundle 借入 base 不负责 shutdown，自建 base 在 observer dispose 返回错误后仍尝试 shutdown；这些是静态路径，异步 owner 缺陷如上。
- 正常 switch 在 channel flush 后执行 snapshot，再 stop owner；server 的最终 collector shutdown 在 `serve_until_signal` 返回后。若 switch 前置失败导致 flush 未到达，不应将 snapshot 宣称已执行；需正式入口因果验证。
- loop_start 与 agent observer 正常接到实际 pump，timeout/current completion 筛选后才记录 agent 事件；pump Err 记录 loop_error。正式 native `Services::default_permission_mode` 恒 default，TS local boot 同样传默认 mode，先前提出的 skipPermissions 回退不构成该生产装配已确认缺陷；默认 mode 可缺省的其他公开 embedding 是否需覆盖待查。
- 仅看接口存在不能证明同一 collector 的实际输入和生命周期。须固定源码及二进制，用本地 mock 正式 CLI 验证四事件及真实字段、两类配置变更、snapshot 与最终 flush 顺序、失败/取消和外部 runtime 退出；queue 恢复与持久化按 O1 窄差异单独验证。

## 两项修复复审与渠道桥

随后只读复审根候选：registry 新共享 owner handle 用于 pending observation、registry dispose 和 registration dispose 三类 worker；bundle 创建时在 owner runtime 内建立 registry，保留 observer 同步首 poll 和 dispatch 屏障。新增 held observer 测试从另一 runtime track/dispose 后退出该 runtime，确认 observation 仍 in flight、dispose pending、base 未发送，释放后才 observer dispose 和 base shutdown。该测试直接约束原缺口；standalone registry 在 runtime 外构造会在首个异步动作绑定 runtime，不能无条件外推为创建时拥有 executor。源码和测试均待集中编译执行。

reload 改由 `Options.telemetry_reload` 仅 server 设置 true，按 changedPaths 的 telemetry 或 telemetry. 前缀过滤，ConfigStore 因果测试检查无关变化、开关及退订；scope 静态符合 TS。local 真正不订阅仍须正式路径验证。当前 assembly 还在 local CLI 排除 `attach_config_store` 和 `activate_config_reload`，与 TS 默认 CLI 未构建服务端 ChannelAdapterBundle 的边界一致；应以配置渠道后重载不启动 adapter 的实际测试保护。

`channel/turn_submitter.rs` 将 attachments、allowPlanModeTools、syntheticMessages、projectKey 的 Some 原值转发，None 省略；Some(false) 和 Some(empty array) 不被真值判断丢弃。`new` 继续同步调用 submit，`new_owned` 仅在 future 首 poll 创建并同次移交 stream；静态未发现字段丢失。现 WeCom 测试捕获 mock TurnRequest，不能证明四个字段到达真实 TurnSubmitter host。建议两种构造均以受控真实 host 检查所有字段和 None/false/空数组，并以 admission 屏障检查未 poll owned future 不启动、legacy 调用即启动、停止时仅精确 owned turn 被 cancel/drain；结合正式 WeCom 本地 mock 验证附件和项目路由。不能用独立字段捕获替代完整正式路径。
