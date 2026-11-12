# H1 第一批独立审查（C1 审查线）

本记录覆盖 Signal 与共享 IM 接口，并汇总独立的 Mattermost、Home Assistant 审查。审查者不参与产品实现、不运行 cargo / 容器、不启动产品或真实服务；只在 C1 独立 worktree 写本文。`C1-review.md` 保持冻结。

- 审查起点：Rust `c8f8df2ec03edced01f716287ec45b7d75e0c0ad`，工作目录 `/Users/da/.codex/worktrees/c1-cli-completion/pilotdeck-rs-cc2c`。
- 固定 TS：`/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读。
- 范围依据：`docs/plan/backend-completion.md:126-153` 与主协调本轮授权。共享协议、装配、Cargo 依赖由主协调独占。源码行号均为审查时快照，候选改动后须复核。
- 当前结论：**审查进行中，H1 第一批未通过**。已有源码审计和候选驱动，不等于新适配器生产实现、真实 Gateway 回应、正式入口对拍、负控、重载或全量验证完成。

## 集中首轮实际结果（2026-11-13 04:53 更新）

本节为当前动态状态；后文“待运行”保留其审查时快照，已被本节覆盖的项目以本节为准。审查者只读核对 identity/output，不自行执行测试。`_build/20261113-044714-c1-s1-m1-h1-test-compile` 在 `ef3249ce3d815b8ece1feb37bac18690fdc5116f` 加当时工作区候选上 no-run 编译 exit0；不抹去此前两次 check exit101。随后复用测试二进制 SHA `3799c4facf2e25141c9f82a29ba0631950ef524230b351c16906a6ef0e2e92ea`，各 identity 一致引用 source manifest SHA `e930a441dcc9ce02d349fa6020253419f319c91806d59644d5fb19c7d92ede43`。已直接核对 `_tmp/20261113-c1-targeted-tests/source-before.json` 哈希和 binaries.json；manifest 明确排除未挂载的 ws_transport 子目录等范围，不把这些 helper 算进本轮编译。

| 已有运行目录（均在 `_build/`） | 实际结果 | 范围限制 |
| --- | --- | --- |
| `20261113-045135-h1-shared-first-correct-filter` | 25 passed，0 failed，exit0 | 真实 native pending/owned 生命周期专项已运行；仍非正式渠道端到端证明 |
| `20261113-045210-h1-network-first` | 6 passed，0 failed，exit0 | 共享网络及真实 ConfigStore 网络回归；不含未挂 deflate helper |
| `20261113-045311-h1-bundle-first` | 7 passed，0 failed，exit0 | mount/reload/ConfigStore/可重试关闭专项 |
| `20261113-045216-h1-mattermost-uncompressed-first` | 12 passed，0 failed，exit0 | 命令显式 skip 压缩 Binary/字典第13项；不将排除计通过 |
| `20261113-045141-h1-signal-first` | 3 passed，13 failed，exit101 | 12处 receive 前置等待及 redirect 的 fetch failed；作者定位假URL端口1触发 bad-port 预检，仍待基线证明、修测试和重新运行，不先免除失败 |
| `20261113-045223-h1-homeassistant-first` | 22 passed，1 failed，exit101 | 唯一失败是 real_websocket_auth_subscribe_rejection_and_cancelled_start 的实际关闭结果断言，仍在排查 |

HA 原始日志另有故意触发的 handler panic，所属 reaping_finished_panicked_message 测试最终为 ok；不将这些日志文字额外算成失败。首轮失败完整保留，后续修复和重跑应追加，不能替换此成绩。正式 Rust 入口、真实 permission/elicitation 渠道回应、全部差异、全量和发行闸门仍未闭合。

## 生产接线的阻断项

| 编号 | 已读证据 | 影响与闭合条件 | 状态 |
| --- | --- | --- | --- |
| H1-IM-01 | 起点 Rust `channel/protocol.rs:103-118` 的 `TurnGateway` 只有 `submit`；`native_process/channels.rs:122,378` 明确注入 `no_interaction()`；`channel/im_interaction.rs:74-129` 的 pending 恒 false、capture 空、answer None | 只完成正文收发不能解锁模型的 permission / elicitation 等待。必须让生产 helper 接到 assembly 当前同一个 `GatewayInteractionCoordinator`，正式模型 mock 发出交互后，经渠道答复继续模型执行；注入脚本 helper 的单测不能替代该证据 | JsString/native responder 候选已静读；assembly 同 coordinator 与正式验证待闭合 |
| H1-IM-02 | 起点 Rust `bluebubbles/interaction.rs:63-93` 将 helper 的 `JsString` 用 `from_utf16_lossy` 转成 `String`；`InteractionPort` 的 chat、session、reply 都为 scalar string | 直接推广旧接口会丢失孤立代理项，亦可能合并不同 chat / session key。Signal 的普通正文切块本身即可把 emoji 切成两个代理项（TS `SignalChannel.ts:331-343`）；互动提示、自由文本答案和键也需无损。历史 `A8.3-CH-FEISHU-009` 仅覆盖 Feishu 特定分段，不能扩张到 H1 新渠道 | JsString helper/native responder 候选已静读；集中测试与真实链路待验 |
| H1-LOAD-01 | 起点 Rust `native_process/channels.rs:432-443` 的 `reload` 只构造 Feishu/Weixin，managed keys 固定两者；TS `ChannelAdapterBundle.ts:79-108` 重载会重新加载 configured channels，并保留曾构造过的 channel key | 新增 startup mount 不会让 Signal / Mattermost / HA 的禁用、改凭据和替换生效。需按传入新 config 重建 loader 实例、保留 managed keys，再核对实际配置重载入口；不得仅调用 channel.start/stop 的单测自称热重载完成 | 主协调已有新候选，静态初审见下；生产 caller 与测试待闭合 |
| H1-LOAD-02 | 起点 Rust `channel/loader.rs:46-58` 拒绝 Signal/Mattermost/HA，`native_process/channels.rs:112-126` 未注册三者 | 离开 rejection 表必须同提交有生产 constructor、真实 IO 和对应测试；SDK 缺失、NoInteraction 或空 stop 不可计为本轮完成 | 三者 mount 与拒绝表移除候选已见；收发、取消及正式验证仍待闭合 |

共享作者提出保持 `ChannelStartDeps` / `TurnGateway` 不变，增加 `NativeInteractionResponder(Arc<GatewayInteractionCoordinator>)`，通过既有 responder 两方法回应，`binding=None`；每个适配器 constructor 创建独立 helper，仅共享无状态 responder。该方向静态可行：TS `InProcessGateway.ts:2627-2636,2776-2778` 与 `GatewayInteractionCoordinator.ts:108-129` 直接消费同一 pending bus。Rust `runtime/interaction/coordinator.rs:161-196` 已有对应入口，并在 elicitation delivered 时调用结果 hook。已静读新 `im_interaction/{helper,native}.rs`：映射与独立 helper 状态正确；生产 assembly 发布同 coordinator 尚待最终接线核验。

必须保留的映射：deny 带 `reason="User denied permission from IM channel."` 且不带 remember；allow 带 `remember=false/true`；缺失 requestId 是 miss 而不是字符串 `"undefined"`；elicitation helper 忽略 delivered bool，因此未命中仍返回无错误；permission 必须把 delivered=false 反馈状态机。helper 的 pending/token/generation 不得跨适配器实例共享（不同渠道可能使用同一 chat ID）。

后续共享方案已明确：新增 `JsInteractionPort` / `JsPermissionAnswer` / `js_helper_interaction`，对应旧接口的 string 输入输出改用 `JsString`，保留 token 类型；抽取唯一 helper 状态机，旧 adapter 通过显式兼容包装维持旧边界。已静读该候选，旧 scalar adapter 通过显式 lossy 包装维持既有边界；新三渠道直接用 JsString。尚未审到编译与实际链路结果。

取消独立审计发现：`TurnSubmission` drop 只释放 consumer barrier，`runtime/turn/submit.rs:343-358` 的后台 run 不会因此 abort；`:819-831` 等 publisher 完成后才 reject pending。停在 permission / elicitation 时，直接 abort adapter task 或丢弃 stream 可留下仍在等待的 run。`runtime/session/owner.rs:612-636` 的 `abort_turn` 能按 run 身份取消并等待 completion，但停止方必须同时 drain/drop consumer，避免 consumer fence 相互等待；不能以 session-only abort 误伤重载后的新 turn。自动生成 runId 到首个事件前还存在归属窗口，需要原生 admission observer 或 owned control handle。已反馈主协调与共享作者；产品能力与接受差异待定。

进一步复核：`owner.is_current_completion`（`:578`）后再 `start_gateway_abort`（`:640`）不是原子操作，即使中间没有 await，多线程仍可插入 complete / 同 runId 后继。`owner.abort`（`:741-779`）在解锁后调用选中的 live handle，不能仅凭选中过 handle 宣称同一 cached handle 的后继安全。将 cancel 放进本 submission 的 task 能消除自身 finally 交错，但还不足以证明外部生命周期安全：`owner.close`（`:853-881`）可独立移除旧 completion owner，等待 worker 后 `end_identity` 释放旧 admission；父 submission 暂停期间仍可接纳同 session 的新 run。旧控制此时按字符串 abort 或无条件 `reject_pending(session)` 都可能伤及后继。需真实 owned 身份与取消/释放的原子约束，测试必须含“外部 close → 同 session / 同 runId 后继 → 旧控制 cancel”的因果排列。

持久 accept 也不能被 cancel select 中途丢弃：必须等已进入的 accept 完成，再根据取消 flag 禁止后续 admission / 模型，并给已接受 run 持久终结；默认 submit 路径应保持行为。上述设计约束已反馈共享作者和主协调，未授权审查者修改 owner 或 submit 实现。

候选 owner-key pin 方案也有额外入口需覆盖：`native_process/continuations.rs:286-339` 的 queued followup 从 `current_handle` / `wait_idle` 直接进入 `LiveSessionHandle::submit_queued`（`runtime/session/handle.rs:428`），绕开 owner 的 begin_turn / begin_maintenance / compact 三个 admission 入口。因此仅在这三处拒绝 pinned key，仍不足以保护同 cached handle 的后继活动；须对 live 活动或本 run 的取消 signal 精确绑定，并保留 queued followup 的既有行为。已向主协调与作者报告，尚未有通过的取消实现结论。

后续已获授权的候选把取消下沉到 `SessionSubmission::abort_owned`：自身 `_activity` guard 在同步 abort 回调全程保持占用，EOF/Err 释放后取消为 no-op；`TurnCompletionHandle::cancel_owned` 只修改保存的 exact watch token。这比 session/runId 查询具备正确的归属基础，仍待并发、持久化与真实 pending 测试。此前另外两项 P1 已收到针对性产品修复和因果测试，静态缺口关闭、实际验证仍待：

- `TurnSubmission::next` 的 owned End 现在对 `result.as_mut()` await，settle 后才 take。`submit_owned_control_tests.rs` 真实构造 End/receipt，首 drain Pending→drop→retry 仍 Pending→释放 receipt；它保护实际回执边界，完整 post-agent 宿主路径仍须集成复验。
- owned 路径跳过 session-wide reject pending 后，permission / hooks 的精确 registration 关闭可见。elicitation 已新增 `PendingQuestion(Registration)` RAII：真实 round_trip pending future drop 会 dispose；同 request key 后继不会被旧 guard 清除。`elicitation_owned_tests.rs` 两项对应这一真实局部路径，正式模型 elicitation pending 取消证据仍不能省略。

最新 owned 候选还将失去 current completion 的旧 stream 改为 exact abort 后继续 drain，不再 break 丢流；活动 guard 回调重入/另线程 queued admission、外部 close 后同 session/runId 后继与 pending 保留、持久 accept gate/终结失败等因果测试已静读，尚未运行。该边界随后已修：owned timeout 不按 session reject/abort/close，不走 hard-close/backstop 截断 publisher；record_failure 错误/预算超时进入 sticky control.error，原 worker.finish 等待真实 terminal append 后自然 EOF，默认 submit/WS 分支保持原路径。TailStream 在真实 inner EOF 后通知并卡 gate，500ms 持续驱动取消必须不成功，放行后才完成；另测 timeout 写失败与重复 drain 始终 Err。候选 manifest SHA256 `0715abb0a75ff7eb9316c3710f031a0a464bc07445502b5bd13392e9b6120408` 已逐项验证20文件hash，21新测试尚未运行；该快照本可交集中编译，但随后的实际 native permission 调用链复核发现下述 P1，故当时局部无阻断结论暂撤回；后续窄修见下，始终不视作正式 pending 验收。

**已修、待动态验证的 P1：原生 permission 注册缺少 Drop 清理。** `native_process/permission_prompt.rs:200` 直接 register，`:245` await select，`:249` 正常续行才 dispose；`assembly.rs:1205` 安装该 prompt，`native_process/tools.rs:1758` 直接 await request。它没有登记到 `runtime/project/interaction_permission.rs` 的 Run.requests，因此此前审过的 RequestGuard/Run.close 不能保护这条原生路径。owned 跳过 session-wide reject 后，外层 ToolFuture drop 可留下 permission pending。主协调授权窄修后，已新增 `PendingPermission(Registration)` Drop，正常 dispose 与 Drop 共用 exact token；新两项真实 request→ask 测试分别验证实际 pending 首poll后 drop 清理、coordinator真实 decide 后同 key 新注册不被旧 future drop 删除。已独立静读，P1产品缺口静态关闭；当时共享21项加2项为23新测试，随后user dialog再加2项为25项，均尚未运行，未以另一套 TurnPermission 测试替代。

共享作者随后有限扫描发现、审查者独立确认另一个条件可达的原生取消缺口：启用 user_dialog_kinds 时，assembly 安装 GatewayUserDialogChannel，`runtime/tools/builtin/request_user.rs:259-280` 只在 wait 仍被轮询时把 context.cancelled 转给本地 AbortSignal；外层 ToolFuture 直接 drop 不会触发它。`user_dialog/channel.rs:298-318` 的 ChannelRequest 没有Drop，`:554-563` 虽正确绑定 exact owned state 的 abort callback，却不会自行触发。经主协调窄授权后，工具等待层已新增 AbortOnDrop，直接触发该调用的原signal；观察已settled结果即disarm，正常回答不额外cancel。新两项测试实际execute覆盖input/select/confirm/form，先tick证明poll运行、drop后bus清零且只一次cancel、后tick不再poll；两个独立channel共享bus并复用ID时，drop旧future不清后继，后继仍poll并可正常回答。审查者已独立静读关闭该产品缺口；共享候选 v3 manifest SHA256 `f47a72f504b233f02703d9a9e1a09445088dcb44526bd21073bf183d3ff36e3a` 已独立核对24文件hash全部匹配；合计25新测试尚未执行，仍不能宣称正式原生取消全链已闭合。

## Signal 的固定基线合同

以下是源码合同，除另行注明的实际入口记录外，不冒充基线实测。

| 路径 | 固定 TS 证据 | 必须验证的可观察结果 |
| --- | --- | --- |
| 配置与装载 | `loadEnabledChannels.ts:47-53`；`SignalChannel.ts:81-103` | options nullish 回退环境变量，restUrl 去尾斜线；缺 account 记录错误但 start 返回可 stop 的 handle；接收 URL 用 encodeURIComponent(account) |
| SSE / JSON 入站 | `SignalChannel.ts:123-180` | 接受独立 JSON 行和 data 行，CRLF，分片 UTF-8；忽略空行、坏 JSON、[DONE]、:ok；carry 跨重连保留，TextDecoder 每次连接新建；EOF 立即重连，HTTP 错误 / stream 异常等待 3 秒，stop 中断 backoff |
| envelope 与路由 | `SignalChannel.ts:31-61,180-185` | sync sent text 优先 data text；source 优先 sourceNumber；groupId 的选择与 nullish 语义；有 group 时 session 按 group，而回复 recipient 优先 source。不得自行修正成群发送 |
| 交互优先级 | `SignalChannel.ts:187-227` | elicitation → permission → busy → mapper。busy 时 `/new` 也被跳过；不能把权限回答误提交成普通 turn |
| 新会话 | `SignalSessionMapper.ts:13-28`；`SignalChannel.ts:229-240` | `/new` 和 `/new ` 前缀创建 UUID session；无后续文本只回复“已创建新会话。”；正常消息 trim；active guard 在 turn 后清除 |
| 模型与回应 | `SignalChannel.ts:244-278`；`signal-render.ts:3-36` | submit 仅传 sessionKey/channelKey/message；提问 / 权限即时提示，正文聚合到 final；异常覆盖已收正文为固定中文错误；turn 后 elicitation.clear 和 permission.clearAfterTurn |
| 权限投递状态 | `SignalChannel.ts:197-219,259-261` | 初始提示未投递保留 retry；confirmation 发送失败 releaseAnswer；状态文案不能推进；next prompt 以 requestId+answerToken 确认，避免迟到答复清掉新请求 |
| 出站 | `SignalChannel.ts:281-313,331-343` | `POST /v2/send` JSON 键顺序 message/number/recipients；按 2000 UTF-16 单元和换行/空格规则分块；非 2xx/网络异常不短路后续块，最终聚合 bool；HTTP 错误 body 截前 500 UTF-16 单元 |
| Cron | `SignalChannel.ts:119-120`；`ImCronDelivery.ts:4-33` | channel mismatch 返回 false；originSessionKey 优先 sessionKey；仅合法 chat session 后缀；send 返回 false 必须透传；无 recipient 时 false |
| Stop / 取消 | `SignalChannel.ts:105-115,281-282` | stop 把 running=false，中止并等待 receive；不会等待所有 active parseLine/turn/send。若修复悬挂/残留，必须先正式取证并登记接受差异，不能无声扩大取消范围 |
| 重载 | `ChannelAdapterBundle.ts:79-108` | configured channel 重新实例化，Signal mapper / recipient / helper 状态不持久化；旧 receive 应停止，新配置应发往新端点；正式重载入口待对拍 |

正式 CLI 在 `pilotdeck.ts` 安装全局代理后的 Undici header/body 预算为每请求/每 body 间隙 600 秒，不能把旧 `channel/http_client.rs` 的 300 秒整链超时直接当正式 Signal 语义，更不能给整个 SSE 生命周期套总计时器。该边界已向 Signal 作者提醒；本记录未运行长时测试。

## Signal 取证驱动审查

首候选 `src/pilotdeck-gateway/tests/fixtures/h1-signal/capture.mjs`（审查时 99 行）走真实 `dist/src/cli/pilotdeck.js server --port 0 --host 127.0.0.1`，随机 localhost bridge/provider，隔离 cwd/HOME/PILOT_HOME/TMPDIR，保留 stdout/stderr/network/files/exit。未 import 或 patch 产品模块，入口层级正确。

仍需闭合的判据：

1. `compare(contract, mutatedCopy)` 只证明比较器会拒绝不同 JSON；需真正改变 mock 输入、模型输出或产品路径，使正式路径的合同断言失败，并保留首败。
2. SIGINT 后 receive/socket 关闭必须在清理前观测；`finally` 的 socket.destroy 不能混算产品释放。记录 signal 对已登记 PID 的发送成功和自然退出；残留/失败清理单独归档。
3. 正文首轮覆盖不包含 permission/elicitation、未投递/错误/并发答复、busy `/new`、HTTP receive 错误 backoff、cron 或 reload，不能外推。
4. `git HEAD` 只核对源码仓库，不能单独证明 ignored dist 的构建来源。需保存固定源码构建来源/哈希，至少区分 source-only 身份和实际执行代码身份。
5. 自定义 Rust command 时不能无条件读取 `args[0]` 作为 entry 文件（通常是 `server`）；需分别记录可执行文件和实际脚本入口。

作者已收到上述意见，继续补齐其余场景。

后续静态复看确认驱动已加入真实 mock 模型输出变异、首条实际回复断言、清理前两条 receive-closed 断言及可选脚本 entryHash。已独立只读复核 `_tmp/20261113-h1-signal/formal-{first,second,third,negative}` 的 identity/contract/result/network：second 与 third 的合同精确相同（主 turn 输入为中文 hello、synced text、long-answer、hold-turn、after-reconnect；7 次 send、2 次 receive，code0，唯一 user-cancel SIGINT sent=true）；执行 Node 与入口脚本 SHA 匹配磁盘。两条 receive close 在成功路径 cleanup 前有断言。实际模型输出变异场景 `mutate=true` 在首回复断言失败，随后仅登记的失败清理 TERM；首次辅助 session-title 非流请求被驱动误挂起导致的 released-turn deadline 失败亦保留，未计入通过。这能证明该正文场景的正式 TS 自比及 mock 输出变异有效，不能外推为 Rust 成绩或权限/重载覆盖。

新增 `formal-final/session-contract.json` 已独立核对：5 条 durable run、首 DM general、group general、后三条相同 UUID 新 session；模型用户历史依次为 `[hello]`、`[synced]`、`[long]`、`[long,hold]`、`[long,hold,reconnect]`。当前驱动直接核对 registry 的 requestMaterial、UUIDv4、state/result 与模型 histories，正文合同与 formal-third 字节相同，补上 `/new` 只核确认文案的盲区。`source-identity.mjs` 现在检查固定 HEAD、九文件对 HEAD 无改动，并转译后对 dist（仅除 sourceMappingURL 和首尾空白）；formal/features 共用此核验。该范围不等于全 dist 构建来源已闭合。

## 另外两条独立审查线

| 适配器 | 审查者报告的已发现事实 | 本审查线的证据状态 |
| --- | --- | --- |
| Mattermost | `remaining_cli` 已读正式 TS server-a HTTP/WS：root URL 请求 `/users/me` 和 `/api/v4/websocket`；带 api prefix 时出现 `/api/v4/users/me` 与 `/api/v4/api/v4/websocket`，Origin 精确；作者原先错误假设 upgrade=0 导致首败，须保留失败并改为实际行为断言 | 后续候选已由独立审查者复核，hash与局部结论见下；Rust正式入口/互动/取消仍待，未独立重跑产品 |
| Home Assistant | `yaml_options` 两轮真实 WS 局部基线：订阅失败仍 start 成功；service 10001ms 超时、断线 5004/5005ms 重连；request ID 跨连接递增；cron 结果 false/true/false | 已独立读取 driver/manifest/5份输出并核对 driver SHA、自比与负控错误；该证据直接加载 adapter、gateway 为 `{}`，不是正式 Gateway 装配；Rust候选后续复审见下，未运行 |

HA 独立材料位于 `_tmp/20261113-h1-homeassistant-review/`。`driver.mjs` SHA256 为 `9188e077afafe081b3b65a3dc2bbdd8caf5c45dd8876476c9711b55adf00ba4b`，与 manifest 一致；ack 两轮 JSON 精确相同，timing 除实测时间外语义相同；`negative-delivery.stderr` 是将 failed-ack 期望故意改为 true 后实际调用产生的 AssertionError（exit1 由 manifest 记录）。该负控证明 ack 断言能发现错误期望，不能替代 Rust 产品变异负控。driver 在局部入口包装 setTimeout 以记录预算，不缩短实际时间；仅采集范围内的结论有效。

另已静态审读 `tests/fixtures/h1-channel-bundle/{capture.mjs,metadata.json}` 和 `native_process/channels_h1_tests.rs`：真实 TS Bundle / loader / 生命周期，channel start/stop 与 gateway 受控；明确为 module-composition 层。3 个 Rust 候选测试覆盖 loader 顺序、新 options、初始 deps.config 保留、历史 managed keys、未管理 key、start 失败恢复与 stop 失败重试，尚未编译。驱动 5 个原负控通过修改已采集 JSON 检查 verify，属于断言自测；作者随后明确标注，并增加真实 reconcile 前删除 signal 历史 key 的路径变异、15秒运行边界、首错/阶段保留和6文件转译对应核对。

已独立只读核验 `_tmp/20261113-h1-bundle/validated-{1,2}/summary.json` 精确相同，driverHash 匹配当前驱动，11 操作 / 9 reconcile / 5 断言自测均保留；`validated-mutant-1/failure.json` 在 verify 阶段显示 `disable-again-after-manual-start` 实际 changes=[]，普通合同断言拒绝，`runtime-negative-control.json` 记录 exit1。此为 module-composition 层真实历史 key 变异，不是正式 server 热重载或 Rust 成绩。

## Signal 产品候选复审（修订中）

已读 `channel/signal/{mod,wire,mapper,tests}.rs` 首候选。JS UTF-16 分块、group session / source recipient、busy 前 recipient 更新、pending 优先级与新 helper 使用方向正确；组件测试明确使用 mock Gateway / transport，未当正式装配证明。

- 每跳 headers 预算已改为 `HeaderTransport::send` 单次600秒，fetch 整链无总 timeout；body 每 frame 等待600秒。对应两跳各400秒虚拟时间测试待集中执行。`register_with_network` 现可注入公共 ChannelNetwork，最终 production caller 更新与代理路径验证仍待主协调，不能以 localhost 成绩外推。
- Signal `features-fifth/sixth/ready-empty-busy-batch.json` 已独立精确自比：真实 HTTP 同 write 两行、观察原 reader 一次 read 含两条，固定 TS 的 scripted empty async generator 只 submit1/busy1、无 send。候选现先 eager poll 完整个 body frame 的所有行，收集 Pending continuation 后统一 spawn，并在 submit 之后明确 yield 保留 active。静态修复方向符合此观测；worker2 回归仍待运行，且不宣称任意 HTTP/TCP 分片边界等价。
- 此前 `mod.rs:551-590` 的 take→await 导致 stop Pending/drop 丢唯一 owner 的 P1 已修为同步 stop prefix、持久清理 worker/watch receipt；最新因果测试覆盖未 poll waiter、两次 Pending/drop→release 和实际 stream/helper 终结，worker abort 发 sticky Err，静态方向已复审。相邻的完成 handler 收割漏项亦已修：is_finished 后读取 JoinHandle 结果，JoinError 在同 registry 锁内记 fatal/cleanup_errors，Pending 仍保留句柄。真实 parse_line→PanickingEvents→下一批收割测试静态覆盖 stop/retry 均 Err，尚未集中运行。新增 outbound 边界已独立定位：`channel/server.rs:72-90` 只转交 cron delivery future，`native_process/assembly/cron.rs:149-171` fire-and-forget spawn 不保存 drain；direct caller 暂停时 stop 广播不保证 HTTP 已被 drop。主协调授权 Signal/HA 自有 outbound worker/receipt 后，两者新增候选已交叉静读：Signal 同 OutboundState 锁内登记/check 与 stop 关门，caller仅receipt；真实TCP测试先确认完整POST，再暂停/丢 caller，stop join 后检查真实peer EOF/reset，随后caller才返回false。HA同runtime锁注册pending/deadline/enqueue/ackworker，stop结算pending并join outbound；真实WS测试覆盖暂停/丢caller/已ack三分支。收割完成任务消费JoinError并持久报错，新增异常worker测试防假成功。此项局部静态关闭，Signal16专项与HA专项待集中运行，不能当正式cron资源成绩。另已独立核对Signal `features-seventh/eighth` 六格 oracle字节自比：held-send格的实际HTTP POST无响应headers时stop已返回、promise未结算且response未destroy；释放响应后第二块仍发、delivery=true（2000/1块）。该真实adapter+HTTP基线合同为取消资源/迟到发送差异提供实际证据，未外推为正式server。共享 `cancel_and_drain` 真实性和正式模型取消仍待验。
- 正式 TS `reload-first/ownership.json` 已读：禁用 Signal 后 receive 关闭，provider 连接与 running run 保留，重新启用后仍保留旧 run；`reload-first/second/contract.json` 已独立精确自比。当前驱动在 cleanup 前断言 health200、PID 活、receive close、provider 是否关闭，区分 TS 留存与 Rust 应修复行为。这是基线资源留存证据，需与接受差异及 Rust 修复验证配套。

HA 的 `_tmp/20261113-h1-homeassistant-review/eager-empty-ws.mjs` 与两轮 stdout 已独立读：未改 adapter，通过真实 WS 单 socket 一次 write 发两个合法 frame，并只观察客户端 data；两轮均一次319字节read对应submit1/busy1，分write且等待微任务则159/160字节两read对应submit2/busy0。driver SHA 为 `442c2d7ff54bd1318a7e0ecacc844f71ab7ebedcb783b5374e7f8f981d252143`。这是实际传输批次观测加 scripted empty async generator，不是正式 Gateway；驱动主要记录观测，尚未把 busy 数量变成产品负控判据。

## 公共装配候选初审（主协调实现）

已读新 `native_process/channels_reload.rs`、`channels.rs` 与 `channel/mod.rs`。Deferred responder 晚发布、每次 configured load 创建新 mounts/helper、历史 managed 集合、新 constructor options 与初始 start deps snapshot 的分离方向一致。新 reload worker 使用 Weak 引用，idle 时无永久强 Arc 环；回调与 unsubscribe 交错由 listener 的 sender 存活和队列 EOF 确保 drain 等待在途发送。watch 结果支持外层 timeout 丢弃后再次等待，未消费唯一 JoinHandle。reload 返回错误后继续队列，close 在 drain 错误时仍尝试 server.close。静态未发现该部分新的阻断，追加的4项真实隔离 ConfigStore 测试已静读：blocked start 期间队列 snapshot 顺序、首次 close Pending/drop 后 retry drain、close 后 event 不重新订阅、一次 start 失败后后续恢复。连同原3项共7项，尚待集中运行结果，assembly caller 尚待主团队接入。

旧生产缺口仍须如实保留：当前 bluebubbles register 仍注入 no_interaction，SMS/Slack 默认 gateway 仍为空。未被生产消费的 NativeImGateway/CompatibilityStream 已按主协调决定移除；此次没有交付该兼容桥。新三渠道通过不能外推为全部既有 IM 回应链完成。

共享网络增量已静读：一个 `ChannelNetwork` Arc 注入 Signal/HA/MM mounts；`proxy` / `proxy.*` 事件即时换 dispatcher，proxy-only 不重建 adapters，与固定 `pilotdeck.ts:143-156` 对应。HTTP send 捕获当前 snapshot，每跳600秒；WS保留原请求/path/query/Origin，只把dial URI改http(s)，通过现有proxy/CA connector完成TLS一次。`_tmp/20261113-h1-ws-transport/second/third/summary.json` 14格已独立字节自比，属于实际adapter+installGlobalProxy局部IO而非正式server。Rust网络测试与真实ConfigStore网络回归未执行；permessage-deflate/defaultheaders差异仍明确未闭合，不以压缩关闭的mock成绩外推。

## 另外两条线的最新范围限定

Mattermost 独立审查者已只读核对正式 TS server-l/m 各8格、null-b/c 各2格、http-options-b/c 各6格、bom-a/b各1格及 me-null，当前 verify 与两轮合同通过；17个采集结果变异属于校验器负控。values-e/f 与 interactions-c/d 分别对入库 oracle 字节一致。stop持久owner、WS背压取消、首poll FIFO、假值 options、共享fetch redirect/单BOM和me=null错误分支已由该独立审查者复核，最终模块 SHA `4f46de8694256440e4e4a791364e7b7aec2b6efc83a4abec9a60efb746cadcea`、tests SHA `948990a33db641c73563185744b3cb067eefdd2c8e7d8ccd7dee34a069e4fa01`，12测试未运行，局部状态待验证。Mattermost 基线没有 deliverCronResult，不新增能力。

Home Assistant 独立审查者已读冻结候选，报告 env/no-text、Ready batch active、pre-stream stop、cancel Err 保留 stream、成功后 helper clear 与同步 stop prefix 修复，无新的局部静态阻断；但随后独立复核确认同形 P1：failed_streams/tasks/network 从 Shared take 到可取消 stop future；首 stop Pending/drop 可丢 owner 而重试假成功。此前无局部阻断结论撤回，作者已接受并改持久 worker/receipt，新持久 worker/watch 候选已静读，dispatch完成任务改显式消费JoinError记持久stop_errors；Pending重试测试第二次等待已改手动首poll。owned outbound已由 remaining_cli 与本审查线交叉复核，无新增局部静态阻断；三文件SHA为mod `e6b694e4abab2d4e5d872928c45a1a91cedac21306d2a4a5c21609a26f1c1cf8`、socket `994b77040d87a8c7b12555aff815be877e2318cbb3e42e38ac7d5ca78d47e5b6`、tests `2aa75a3ef3aebfd30ed70460efabcad3c5574372e6f3d6946e565a87d927b3aa`。动态验证仍待。正式 `ts-d/e` 与 `stop-a/b` 合同本审查线已只读精确自比，后者确认禁用后基线仍保留 provider；`negative-auth` 改隔离配置 token，经正式 CLI 到真实 WS 发出错误值并被普通断言拒绝，属于真实配置故障负控。较早正控目录未保存驱动源码快照，不能从其 driverHash 单独反推出完整历史驱动；negative-auth 保存了 driver。全部仍待正式 Rust/真实互动/cron/reload 与集中测试。

旧测试迁移已静读：三条旧拒绝断言各换为 production mounts+loader 的13格真值构造/key校验；23/3/7故障行保留，seam声明目标35→32明确转交，不混算 KILLED。空 loader/wecom mount 重锚仍保护同一故障，两个 assembly 锚点失效如实保留。新三渠道实际 mount删除/回插拒绝负控仍待执行，构造成功不替代收发证明。

## 首次编译后窄修复核（不等于动态验收）

已直接读取集中构建 identity 和原始 output：`_build/20261113-042603-h1-first-check` 在 `adcaa3b68ef618632786d970e0a7e28af41e1533` 上执行 gateway all-targets check，exit101；`_build/20261113-043343-h1-second-check` 在 `ef3249ce3d815b8ece1feb37bac18690fdc5116f` 上同命令仍 exit101（剩余 E0599）。两次失败均保留，以下只评静态修复，不报编译通过。

共享 IM v4 manifest SHA `4a14cc91861862774e5a4511bd9d67cf6dec38d4d7997bc5a375aaa216cc502c` 已独立核对，24文件与清单逐一相符；相对 v3 只有 `im_interaction.rs`、`im_interaction/native.rs`、`im_interaction/tests.rs` 三文件改变，其余21文件不变。移除无 consumer 的兼容桥与 reexport；NativeInteractionResponder 仍使用注入的同一 coordinator，consume 后 yield 的行为未改。测试显式 JsValue 变体和 static Undefined 替换未改变断言输入。该窄修未发现新的局部静态阻断；25项新测试及生产 native pending 生命周期仍待执行验证。

HA 窄修新快照：mod SHA `1bcbae3678ee44a8a9e6b40537984a2ae87e24b033ac84901e0fcfdf3064bf84`、socket `b9f943b66a724f2a1dbe515196f999afeed2788f4121d076c463116aa1538e08`、tests `6217c6a372070a95569a1baf24b0ef3c7c4239c68143f1d4aef33f09e027b0f4`、render `f967d0588bed95835c102d8a3aa1287a3119c49e20b7dbcb03987e4a1b338819`。HTTP upgrade 拒绝分支保留失败 CONNECTING：首次 ErrorEvent 后等待 stop，随后第二条 ErrorEvent；不 settle auth、不 disconnect、不排重连。首连继续等待20秒 auth deadline；已成功连接后的重连403则没有第三次握手。auth_invalid 先日志后 settle，watch wait 返回引用在进入 drain await 前释放。新增真实 TCP/WS 测试静态覆盖这些因果关系，未发现新的局部静态阻断，23项专项仍待集中执行。

已独立复核正式 TS `startup-d/e` 四格合同精确相等、各场景 identity driverHash 与同目录保存 driver 一致。403 两轮实际约20.66/20.62秒，stderr 顺序为 ErrorEvent→auth timeout→ErrorEvent；auth_invalid 先错误事件后 auth failed。`reconnect-rejected-a/b` 合同精确相等，第二次握手距断连5009/5012毫秒，驱动在产品仍运行且清理前实际等待5.5秒并断言没有第三次握手，stop 后第二条 ErrorEvent。各轮 exit0、SIGINT 已发送、failure=null。此为固定 TS 正式入口证据，不能替代 Rust 运行，也不将 driverHash 当全树 source/dist 一致证明。

Mattermost 后续 Binary/压缩上下文和编译窄修由 remaining_cli 独立复核：当前产品 SHA `4aebb20e5ce9e2da3966b160165a9098985afe090dd9c9496bfc8340ff66e53a`、tests `ed96ba48ab21095babab0cc0c0ac8e4ac3602af434af42a9a6620cac9b09b823`。完整原始压缩帧可解码；仅删除 Binary、保留既有 auth/result 后，两渠道均出现字典依赖错误。TS ts-b/c 与 fragment-a/b 的8条记录校验通过，各组7个结果变异被拒，仍是采集校验层而非 Rust 产品 mutant。共享 decoder 挂载与 Rust 执行尚未据此闭合。

共享 deflate 未挂载候选已初步静读：持久 inflater 保留跨消息字典，Binary 仍经过解压而 adapter 可忽略其消息，control frame 不进入 inflater，fragment 的末帧才追加 flush tail。读写 wrapper 没有独立任务，drop 随 connector IO 释放；duplex容量1/8192测试明确覆盖逐字节头和同读101尾部，不依赖TCP一次write必然一次read。已独立从 first/second 原始文件重做20格投影自比；18格输入帧与输出消息匹配入库fixture，window0/16两格为stderr RangeError、明确使用plain帧重建，并非声称崩溃前抓到了输入JSON。当前未运行 Rust。两项继续跟进：未结束HTTP头由wrapper自行缓存会延迟原parser限制，作者准备逐段转交；Node异步inflate与HA Ready批次 active 释放之间可能存在压缩双帧 busy 时序差异，已要求真实TS探针，尚未据此认定缺陷。现20格消息约1600字节，8192及更大解压输出循环边界覆盖已请求补充。此项没有给最终静态放行，也不能计正式入口验收。

解压调度候选已由真实 TS 证实：`_tmp/20261113-h1-homeassistant-compression/c,d` 的合同经本审查线独立精确自比，保存 driverHash 匹配，均exit0；使用原生 Node WebSocket、未替换 global WebSocket，仅观察自有随机端口 socket。plain 同read319字节为 submit1/busy1，compressed 同read115字节为 submit2/busy0；分read压缩亦2/0。negative-hold 在真实 scripted Gateway 挂起首turn，运行实际变1/1并由正常 admitted-count 断言 exit1，不是改结果JSON。该证据仅实际adapter+脚本Gateway，不是正式server。

共享 helper 最新 SHA `8134004e29daa05342eedb700b9c81d8f7794f613994e51ecfbc186dd6cfb901` 在每个完整 compressed data/continuation frame 解压前持久记录并暴露一次 Pending，plain/control 不增加边界；输入/字典均不在 Pending 前消费，丢弃临时 read future 后仍可继续同帧。新增确定性 manual-poll 测试 SHA `5e9c7fad91448880005294892975d45a976b5bfae4fc67a82c85e4c3f5360232` 已静读，duplex预装容量和header等待有界。原header缓存项已修为增量转交仅留3字节；8192/20480两格原始双跑和fixture经独立核对并只读解码确认，原20分母不变。此helper仍未挂/未运行Rust。单Pending会被HA now_or_never消费，不能独自保证continuation推进；HA方案要求批尾先drop ReadBatch guard，再只等待自有spawn首次repoll的receipt，不等待held流终结，真实JoinHandle仍由原registry持有。该方案静态可行，落地代码及plain1/1、compressed2/0、held1/1多worker真实WS组合验证仍待，不能把局部poll测试外推为集成通过。

HA receipt 落地增量已复审：mod SHA `36b0968936c002c38f703dfb649d86d5ca64641bc478426bf419206686a83732`、socket `fd4471406f5c258fcf39f99ee31bf3321b151d2a51d45ec64588003bce15230d`。批尾先释放 ReadBatch，再等待首次repoll通知；poll_fn在incoming首次repoll返回后发送，真实JoinHandle仍由tasks持有，panic只关闭通知但仍由原reaper/stop记录。局部产品未见新增静态阻断。第24项压缩测试对应fixture SHA `1eb19cd46767beefd9d989c7f81e7dc5d46448238e379be1a53b1a820f2c3540`，115字节帧、各帧信息和driver与c/d原始捕获一致，source result SHA精确指向c轮。测试初稿 SHA `e49bb00dacca11861e4f03faeb11fe0f0093a97c6ff7e50c80459efc83da6f19` 仍有P2因果不足：自动Pong可能在socket.next().now_or_never内flush，早于finish_read_batch等待最后handler完成；故Pong后立刻断言requests2/activefalse可能因调度假败。已要求Pong后补有界真实empty handler完成条件，held分支仍不可等待EOF。原23首轮22/1结果保持，此新项尚未执行。

## 后续可执行验收

由各作者提供冻结候选与驱动，主协调统一构建/运行；本审查者只读复核材料和源码。

- 两轮固定 TS 正式入口自比，准确标记 UUID / 时间 / 动态端口归一化，保存原始证据与固定分母。负控必须触达被保护的产品路径。
- 同一隔离模型 mock 对 Rust 正式入口验证：入站→真实 submit→permission/elicitation 提示→渠道回答→真实 coordinator→模型继续→出站；不能用 noop、只捕获调用的 scripted helper 或直接手工 consume pending 替代该链。
- 正常 busy、新会话、不同 chat 并发、回答失败/提示失败/迟到回答，明确下一条消息是否可推进；不同适配器相同 chat ID 不串状态。
- 首次启动、配置重载替换、禁用、stop、SIGINT、接收与发送错误后恢复，确认 cleanup 前 owned 连接/任务状态；差异先有固定基线证据再登记。
- Cron 从生产路由到 adapter 的实际交付与 false 回传；通道拒绝表、loader 注册、startup、reload、cron 装配全部有正式入口或有明确层级的测试证据。
- 新候选关闭前重新搜索范围内 TODO/空分支/NoInteraction/gateway=None/TS fallback；列出每项是否仍可由生产入口到达。运行结果不得以静态检查替代。
