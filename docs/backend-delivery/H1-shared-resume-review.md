# H1 共享 ownership、网络和装配恢复审查

2026-11-13；只读 C1 工作树 `bcad7cb1435103e31c9ec71877c7064f3cffc619` 加未提交 WIP。范围限共享 ownership/interaction/network/reload 和 `_tmp/20261113-h1-assembly/assembly.patch`，不评 Signal、Home Assistant、WS 私有协议或 S1。没有运行 Cargo、产品、容器，也没有改产品源码。以下为源码静态结论，不能替代当前固定集成候选的编译、正式入口与负控。

**结论：共享候选可按依赖拆成待验证窄批，但装配 patch 和共享 TurnGateway 原样生产接线暂不放行。** 两个具体阻断及一个组合验证缺口如下。

1. **启动期 proxy 事件遗漏。** patch 在 `settings.store.start_watching()` 之后、host 创建与 `start_channels()` 之后才 `attach_config_store`；`NativeChannels` 的 `ChannelNetwork` 在更早基于旧 options 创建。两者之间如发生 proxy 配置 reload，新订阅者拿不到已发布事件，网络持续使用旧 proxy，直到下次 proxy 修改。固定 TS `src/cli/pilotdeck.ts:138-151` 在 server 启动之前订阅，proxy 更新不依赖 serverRef；adapter reload 不同，其 handler 在 `:201-202` 检查 serverRef。需要对启动期真实 ConfigStore reload 构造屏障，验证最终 dispatcher，修补时不要把 adapter 与 proxy 的基线时序简单视为相同。
2. **owned 路径影响范围超出差异账本。** `channel/turn_submitter.rs` 将共享 `SubmitterTurnGateway` 每次提交改为 `submit_owned`；`native_process/channels.rs:341` 把同一 gateway 作为 ChannelStartDeps 提供既有渠道，包括 Feishu。即使没有调用 cancel，`runtime/turn/submit.rs` 的 owned timeout 也跳过 session-wide reject、close_transcript、close session，等待 worker 生命周期尾部。`H1-IM-STOP-DRAIN-001` 仅登记 Signal/Mattermost/HomeAssistant 的 stop/reload，不覆盖既有渠道正常 timeout 路径。须按渠道显式选择 owned，或先取得既有渠道行为证据并按授权/差异规则处理；不能把当前共享接线当作仅影响三个新渠道。
3. **跨子系统重载顺序未证明。** 固定 TS `pilotdeck.ts:136-172` 把 adapter 与 cron/alwaysOn 的异步变更排入同一个 `reloadChain`，C1 `channels_reload.rs` 只串行渠道队列。相同或紧邻配置事件同时改变 adapters 与自动化时，cron 可以在渠道替换前后触发不同结果；现有7项 bundle 测试只覆盖渠道队列，不证明跨子系统等价。这是待实际因果验证的风险，并非已实测产品错误。与 O1 联合在可控 blocked adapter reload / cron tick 下验证，并按基线结果决定是否共用队列。

## 可分离清单与依赖

- **registration RAII 小批**：`native_process/{elicitation.rs,elicitation_owned_tests.rs,permission_prompt.rs,permission_prompt_owned_tests.rs}`，`runtime/tools/builtin/{request_user.rs,request_user_owned_tests.rs}`。前两者 Drop 精确 Registration；后者 Drop 精确 AbortSignal，完成后 disarm。各自测试须随文件引入；与渠道 mount 可物理分离，但其取消收益需要 owned turn 端到端验证。静读未见新增阻断。
- **owned 核心小批**：`runtime/session/{completion.rs,handle.rs,handle_owned_tests.rs}`、`runtime/turn/{submit.rs,submit_owned.rs,submit_owned_control_tests.rs,submit_owned_tests.rs,submit_tests.rs}`，以及只引入方法的 `channel/protocol.rs`。`submit_tests.rs` 包含新测试模块声明。核心采用 completion 精确 token 与持有 `SessionSubmission` 的 activity guard，旧 stream 不按 session/run 字符串查询后继；保留 End/result receipt 支持被中断后重试。accept 一旦开始会等结果；取消后持久终结。普通 `submit()` 仍传 None，核心本身可独立纳入验证，但先不要把所有渠道改接 owned。
- **互动 helper 小批**：`channel/im_interaction.rs`、`channel/im_interaction/{helper.rs,native.rs,tests.rs}`、`channel/bluebubbles/interaction.rs`。helper 实例各持有独立聊天状态，Native responder 持有注入 coordinator，JsString 边界保留 UTF-16；旧 scalar facade 与新 helper 须一起处理以避免类型断裂。生产 Feishu/wecomCallback 获得 responder 属行为变化，需单独正式渠道回答测试，不与纯 helper 重构混为已验证。BlueBubbles 的既有 no-interaction、SMS/Slack 缺 gateway 仍是范围内待办，不能由新 helper 的单测抹平。
- **网络/重载与三渠道接线批**：`channel/network.rs` 依赖 `channel/ws_transport.rs` 及其子模块；`native_process/{channels.rs,channels_reload.rs,channels_h1_tests.rs,channels_network_tests.rs}` 同时引用三个新适配器、shared interaction 与 network；`channel/{mod.rs,loader.rs,tests.rs}` 以及三个 `tests/fixtures/channel-{loader-startup,unmigrated-startup,unmigrated}/negative_controls.py` 的相关更改必须与实际 mount/拒绝表成组审。不能只移除 loader 拒绝而不挂对应真实模块。`channel/mod.rs` 当前还声明 WhatsApp；它是未验证独立 WIP，窄批应按实际已验证范围选行，不随其他渠道整文件带入。WS deflate 和三个私有适配器由各自审核，不在本结论内。
- **最终接线**：assembly patch 涉及 host settings 同一 ConfigStore、interaction coordinator、channel shutdown ownership，应在解决第1条、核对主工作树相应代码和并发改动后由主协调重做；`git apply --check` 在 C1 当前源码可通过不等于对根树或运行行为放行。`in_process_cli` 在 patch 的 attach 之前提前返回，是否需要此入口热重载应按它实际职责核实。

网络独立实现通过锁快照确保已开始 HTTP/WS 保留原 transport，新请求使用更新 dispatcher；proxy-only 事件不重建渠道。`ReloadDriver` 使用 Weak 取消 Arc 环、退订后等待已排队 reload，并以 watch receipt 支持 close 被取消后再次等待；已看到针对失败后继续、snapshot 顺序、close retry 的测试。监听器被发布时复制，退订时其持有 sender 可保持 worker 活到 in-flight callback 完成；关闭标志防新工作。此为源码分析，不证明所有并发排列。

旧 `_build/20261113-045135-h1-shared-first-correct-filter`（25 passed）、`20261113-045210-h1-network-first`（6 passed）、`20261113-045311-h1-bundle-first`（7 passed）由此前集中运行，绑定当时的二进制/源码快照，非本次重跑，也不是此次新装配 patch 或最终集成 SHA 的验收。需要固定当前集成源码后执行受影响测试，并以真实模型 mock 验证入站→pending→渠道答复→同一 coordinator→继续；对 stop、同名后继、写失败、close retry、配置和跨子系统时序做故障负控，最终正式 Rust/TS 入口对拍。

审查身份参考 SHA256：assembly patch `f10cd64e6875d0208eea80758423fcf1c9c5480eba0439b7d87137f99ce700fb`；channels `9a94124811b7da7c6c203da1b1b23259179e24a3432429d34c4ea67bc352dec6`；reload `ac0c0fe740f9ad27b264827b7e98290c3952f3ff3fc4d5abf06db586583d81a3`；submit `b15df5e4179b41b3262de9a303174204a210891ae8d773035602167f68b98303`；submit_owned `df8cfe5fc3693919185b17c36f36e0b87e0bc013274f832fc383a2e0f3bc89fc`；network `3077875fce451e02a6a5d54a3760d8d219ad89da78030ed4cb21c46e8387d83d`；turn_submitter `d1852fe22a615dfe664b7ca9a4c6cf195c9eec182e6f33c5a52c391a59c912c8`。未提交。

## 主协调授权后的最小修复与冻结

主协调授权修复上述第1、2项；本节替代上文相关“未修”状态，不替代编译和运行。`SubmitterTurnGateway::new` 保持 legacy，新增 `new_owned`；`NativeChannels` 构建相互共享同一 submitter slot 的两个 gateway，装载出来的三个新渠道实例在启动包装层注入 owned gateway，其他渠道继续 legacy。选择在 adapter 实例，不从可变 `TurnRequest.channel_key` 授权；三新模块的实际 channel key 均为固定常量。包装保留 channel key 和 cron 调用。新增 wrapper 测试核对其注入的确是专属 gateway，且保留任意实例 key。后续 WhatsApp 或其他新渠道要明确加入拥有取消能力的实例集合，不能默认自动升级。

`attach_config_store` 提前订阅并在订阅后、callback 同锁门保护下读当前快照：若 proxy 与构造时配置不同则更新，保持未变化时原 startup/env 解析模式。proxy 事件立即更新，adapter 事件在 `activate_config_reload()` 前不重建。新增真实 TCP proxy 切换测试以及 adapter 事件前后 gate 测试。根装配须在拿到 `settings.store` 后、`start_watching` 前调用 `attach_config_store`，启动渠道后调用 `activate_config_reload`；不得再使用原 patch 的 late attach。配置与自动化共用队列仍由主协调处理，第3项尚未闭合。

五个修复文件的固定 SHA256：`turn_submitter.rs` `fb1c2da5baca1f13ae12165543482413d111d25f6acbac76e9c1c7b7bdeadae7`；`channels.rs` `1dbc583f62afe6eb93b682a68804a39824f51245126d4d4103fb770c30fcac4f`；`channels_reload.rs` `90e53b909bdc2595eb9b5a5da89432f14606653587e1a4a2b781c7dd1ec7841a`；`channels_h1_tests.rs` `52c528892718b08c74c7ebb6e5a1c357abbbb5f7a03dc69802e220280df31ded`；`channels_network_tests.rs` `f69f541ec8a9d8bd15a891ca4ee804ba1e67d6b5f1ecf33ade735a1a2e58c33a`。已对五文件执行 `rustfmt --edition 2024 --config skip_children=true`；未执行 Cargo 或新测试。此快照可送集中编译，不标记编译或动态验证通过。

## 第三批集中编译后的复审和修复

主协调第三批编译通过，shared/network/bundle 运行日志 `_build/20261113-052810-resume-third-shared/output.log`：17 passed / 1 failed，不能报整批通过。唯一失败为 `config_store_proxy_events_reach_the_existing_network`：新增早期请求已由第二代理成功送至 origin，但最终 origin 完整路径数组忘记新增 `/early-proxy`。日志显示实际有序九项为 `/early-proxy`、`/startup`、`/new-proxy`、`/explicit-bypass`、`/bypass-removed`、`/removed-proxy`、`/replacement-proxy`、`/closed-url-change`、`/closed-removal`。仅在原有期望数组首位补该项，保留第二代理计数与其他完整顺序断言。测试文件新 SHA256 `4b86fc15a9a38161a7cfe1e26d7af5ecd82e0d5c1138184905229fdeec2f5ae1`；修后待重新编译和重跑，不用推理替代实际通过。

继续检查发现 legacy gateway 原实现调用 `submit()` 后才返回 future，先前共享改动把 legacy 也延迟至 future 首 poll。现在仅 owned 分支延迟；legacy 恢复旧的同步提交边界。`channel/turn_submitter.rs` 新 SHA256 `9ba6c8480a32d86a386fb90ac61f4c8f202789bea43bf330d5d66256b2ccdc44`。现有 `h1_unpolled_channel_submit_never_starts_a_turn` 覆盖 owned 不被 poll 的情况；legacy 同步提交的独立因果回归尚待共享 fixture 补测，当前只完成源码核对。两个文件均已 rustfmt，未执行 Cargo。其余窄批依赖、自动化共同队列和正式入口验证限制仍如上。
