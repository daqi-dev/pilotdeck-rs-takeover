# O1 遥测独立审查（C1 审查线）

本记录覆盖固定 TS 遥测到 Rust 的队列、collector、context、live observer 及接线边界。作者为 online；审查者只读产品代码与基线，独占本文。根协调者独占 `lib.rs`、assembly、native_process、server 与 CLI 接线。本次未运行 cargo、容器或产品进程，未访问真实用户状态或真实遥测服务。

- 工作树：`/Users/da/.codex/worktrees/c1-cli-completion/pilotdeck-rs-cc2c`。
- 固定 TS：`/Users/da/ws/pilotdeck-rs/resources/PilotDeck`，`cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`，只读。
- 当前状态：**首版实现与 TS 证据已独立复核，发现下列 context/registry 待修项；尚未最终冻结、挂载或运行 Rust，不能交付。** 下述源码依据与实际取证分层列出。
- H1 Mattermost/Home Assistant 与 M1 Google 的既有候选结论保持独立，不能代替本包交付。

## 已核对的固定 TS 边界

| 环节 | 源码依据与可观察约束 |
| --- | --- |
| collector → sender | `src/telemetry/collector.ts:38-57` 先解析配置和 runtime context，再创建真实 sender；即使禁用也建立 context 并恢复队列。`sender.ts:62-88` 的 enqueue 达 batchSize 后同步进入 async flush，在第一次 await 前已经设置 flushing 并摘走 batch。Rust 仅 spawn 整个 flush 会改变同一调用栈中后续 enqueue 的分批和 queue cap 结果。 |
| 失败重排 | `sender.ts:91-104` 对失败 batch 按原顺序逐个 unshift，保留项在队首逆序，超过 maxRetries 的项增加 dropped。每次失败停止当前 flush；sent 按成功 batch 的事件数计数，sendFailures 按失败请求计数，retries 按重新入队的事件数计数。 |
| 外发 | `sender.ts:125-140` 只移除 baseUrl 最后一个 `/`，POST JSON 数组到 `/collect`；仅等待 fetch response headers 并检查 response.ok，不读取 response body。timeout 包围 fetch，不应照搬模型 complete 的整 body deadline。默认地址存在于 `collector.ts:33`，所有取证必须显式使用随机 loopback endpoint，实际请求也须拒绝非本地目的地。 |
| JSONL 恢复 | `sender.ts:143-165` 逐行 trim、忽略坏 JSON，仅以 `parsed?.event?.eventId` 的 JS truthiness 接受整 event；attempts 为 `Math.max(0, Number(value) || 0)`，不可强制转成无符号整数。恢复不裁剪 maxQueueSize、不增加 queued，禁用时也恢复；文件保留到 clean shutdown 覆写或删除。 |
| 持久化 | `sender.ts:167-182` 空队列 rm(force)，非空 mkdir 后直接写每个 envelope 加换行，文件错误吞掉。写入不能为匹配测试而静默丢掉仍由发送 worker 持有的 batch；原实现是否确有此丢数据缺陷需实测和差异登记。 |
| collector 映射 | `collector.ts:62-115` 的 metadata 在保留字段之后展开，可覆盖 module/outcome 等字段。trackError 先发 error_occurred；只有 module 与 ownerModule 均合法才再发 failed feature，且第二条只保留 code/tool/provider/model/normalized providerBaseUrl 等指定 metadata。 |
| 属性清洗 | `collector.ts:208-246` 按 key 的 `/path\|cwd\|root\|dir\|file/i` 删除字段，包含普通子串也删除；字符串 trim 后若像绝对路径则删除，数组过滤元素后为空也删除，空嵌套对象也删除，null 保留。不能改成更窄的路径字段白名单。 |
| context | `context.ts:41-65` 从 token 文件 trim 内容计算 installationId，否则使用 platform/arch/HOME；instanceId 包含 pilotHome、进程 executable、entrypoint 与 deploymentMode。ID 是 SHA-256 前 24 个 hex。`:162-172` 的 cacheKey 不含 HOME、cwd、docker 标记或 token 文件内容，相同 key 后的这些变化仍命中旧 context。 |
| Gateway context | `GatewayTurnTelemetryContextResolver.ts:14-33` 先看同时 truthy 的显式 ownerModule/executionKind，再看 always-on channel 前缀，最后按 nullish 默认 session/user_session。phase 同样保留 truthy 与 nullish 两种不同分支。 |
| Gateway 事件 | `GatewayAgentEventTelemetryObserver.ts:25-204` 忽略 model_request_started，使用 model_event/request_started；tool_result 成功不发主 agent 事件，而 subagent_tool_result 成功会发。错误交给 collector 后可能派生第二条，subagent 强制 executionKind=subagent。工具错误分类先检查 invalid/argument/param/schema，再检查 parse/json/decode/format。 |
| live observers | `TelemetryObserverRegistry.ts:207-248` 先调用 base client，再向 registry 发一次原始调用的浅快照；base 内部的 trackError 双事件不递归成为两次 wrapper observation，禁用外发也不等于禁用 live observers。注册按插入顺序，replace 先发布新代再 drain 旧代；observe 不阻塞调用者，失败隔离；dispose 停 admission、等待 in-flight 并汇总失败。 |

## 当前需实测的风险与方案要求

1. **collector setEnabled 与共享 config。** `collector.ts:117-119` 先修改 config.enabled，再调用 sender.setEnabled；sender 初始持有同一个 config，`:44` 的 equality early return 可能令禁用 → 启用不启动 interval。作者已纳入真实 collector → loopback 场景；尚未以源码推导标记运行事实，不凭直觉改变行为。
2. **shutdown 遇到正在发送的 batch。** `sender.ts:80` 在 flushing 时立即返回，shutdown `:116-117` 随即持久化剩余 queue，可能没有等待或保存 in-flight batch。作者将用 held response 实测；若确认丢数据，修复须按已授权缺陷例外登记，保留普通 flush 的兼容行为与原始失败证据。
3. **同步 admission 与异步资源归属。** Rust 应在触发 flush 的调用中同步 claim 和摘 batch，再交 owned worker；取消一个 flush/shutdown 等待 future 不能销毁唯一 in-flight batch、timer 或持久化责任。Observer replace/dispose 同样需要持续拥有正在排空的旧代。
4. **实际 HTTP 与负控。** 作者计划以真实 collector/sender 和随机 loopback HTTP 保存原始请求、JSONL、metrics，覆盖 sanitize、双事件、metadata 覆盖、逆序重试、恢复、queue cap、timeout、toggle 与 in-flight shutdown；随后再跑正式 server。取证必须保留全部依赖源码身份、隔离 HOME/PILOT_HOME、两轮自比和真实 mock 响应负控。

以上已发给作者。首版已落 `telemetry/{mod,context,collector,sender,observer,registry,tests}.rs`，采用 Arc collector、JsValue 字段、现有 transport/shared fetch、纯 Gateway event/context mapper、独立 registry/wrapper；首轮只读审查完成，当前 12 个专项尚未运行。

## 已独立复核的实际 TS 证据

`_tmp/20261113-o1-telemetry/module-second/oracle.json`、`module-fourth/oracle.json` 与入库 `tests/fixtures/o1-telemetry/module-oracle.json` 已由审查者逐字节比较相等，SHA-256 为 `0192ed886b1831064722fa0100536ff915a978028c29f34d3b201f8e3fac9926`，共 **9 格、12 次实际 HTTP 请求**。驱动使用原 collector/sender/registry/Gateway observer 与随机 loopback HTTP，没有替换它们的返回值；fetch 守卫拒绝非本地目标。这是模块层证据。

- 失败请求实际顺序为 AB → BA → AB → C，最终 queued=0、sent=1、sendFailures=3、retries=4、dropped=3、queueDepth=0。真实 mock 将 503 改为 200 的负控产生 AB → C，原断言失败；原始 `module-negative/retry-order/failure.json` 已读取，未篡改 oracle。
- 初始 false 后通过 collector 启用，110 ms 内没有自动请求，仍 queued=1；显式 flush 才发送。初始 true 后 false→true 保留原 timer 并自动发送。两种 config 别名行为已实际观察，Rust 保留。
- shutdown-inflight：返回时 pending response 仍开，JSONL 只有 queued，未包含 inflight；释放响应后又依次发送 inflight 和 queued，sent=2，但文件仍保留 queued。该实际状态同时带来丢失与重复恢复风险。根已批准只修 shutdown：同步停 timer，由持久 worker 等待同一 flush 的结果后 persist；普通 in-flight flush 仍即时返回。修复的 Rust 运行尚待验证。
- registry 原始日志为 old/error → new/feature_used → disposed/broken → disposed/new → disposed/old；两次 observer failure 被隔离。Rust 首版测试尚未完整对比这条顺序，见下面待修项。

`formal-first/contract.json`、`formal-second/contract.json` 与入库 `formal-oracle.json` 已逐字节比较相等，SHA-256 为 `c5a29ccff713404dd5ae3fc470c80cddda4b15da2bebe86aaaf68d103c2e6af2`。这是实际固定 `pilotdeck server` → public WS submit_turn → 真实模型 HTTP → collector HTTP 的正式 TS 场景：四事件依次 loop_start/model_request/model_response/loop_end，SIGINT 正常停止，snapshot queued=4/sent=0/depth=4，随后发送，退出 code=0、无 signal，JSONL 不存在。原始 HTTP、WS、stdout/stderr、PID 与信号保留。

现有正式负控 `formal-negative` 修改真实模型输出，在 public WS 文本断言失败，清理使用 SIGTERM；它尚未走到遥测合同，不能算遥测字段或缺失事件的正式负控。已要求追加遥测禁用等实际配置负控以触发遥测断言。模块第三轮的内存 undefined 与落盘缺字段比较错误已保留，第四轮改为 JSON 层比较。作者后续扩充 oracle 时需保存最终驱动快照和 command/args/cwd/platform/白名单 env，并用同一最终驱动双跑；当前 source-identity 明确只核对 12 个模块，不声称完整 dist 及依赖重建。

## 首版实现的具体待修项

| 编号 | 位置与问题 | 验证要求 |
| --- | --- | --- |
| O1-C1-01 | `context.rs:185-186` 用 read_to_string 读取 server-token，非 UTF-8 文件会报错；TS context.ts:47 用 utf8 替换解码并成功 hash。Rust 还先读取 token，TS 先 resolveDeploymentMode；token 读取失败时 git probe 的可观察副作用顺序不同。 | 作者补隔离文件字节与错误顺序实测，按结果修复并回归；不得读取真实 token。 |
| O1-C1-02 | `registry.rs:266-268` 调用 when_drained 时只订阅 count，将零值判断推迟到 future 首次 poll。TS :317 在调用时 inFlight=0 已返回 resolved Promise；后续新 observe 不应让先前 drain receipt 再等待。 | 同步固定零值结果；因果测试为取零状态 receipt → 增加 held observation → 旧 receipt 仍立即完成，而新 receipt 持续 Pending。 |
| O1-C1-03 | `registry.rs:214-223,335-364` 对每个 registration 分别 spawn，零 in-flight 的 disposer 调用可被多线程重排。当前 TS oracle 已有 broken → new → old 的明确顺序。 | 对当前真实 TS registry trace 增加多线程 Rust 回归，保持同批已排空项的反向启动顺序；有 held observation 的旧代仍独立 drain。 |
| O1-C1-04 | 作者自检 `collector.rs:277` 对 `__proto__` 的 own insert 与 TS 对普通对象的逐项赋值不等价。 | 限于当前字段语义补真实 collector oracle，区分 object spread 与赋值，不能全局粗暴丢弃该 key。 |

上述项均已发作者。首版 sender 的同步 claim/take_batch、失败逐项 push_front、disabled restore、整次发送 receipt 与 shutdown 持久 worker 路径暂未见额外局部静态阻断。现有 Rust 发送测试用 ModelHttpTransport mock，不能代替真实 socket 取消释放或正式 CLI 验收。Registry 的 Pending 首次同步 poll 和 dispatch gate 已审阅，可保持 callback prefix 先于下一 observer；不因此免除 disposer 顺序验证。

## 接线验收边界

固定 TS `src/cli/pilotdeck.ts:41-44` 先用已加载 YAML 的 enabled 覆盖创建 collector，`:139-140` 热重载同步调用 setEnabled。`GatewayTelemetryBundle.ts:33-40,49-65` 只 shutdown 自己创建的 base，注入 collector 的生命周期仍属于调用者。`PilotDeckServerShutdownBundle.ts:32-39` 在 flush channel state 后、disposeGateway 和最终 telemetry.shutdown 前记录 snapshot；不能用最终发送完成后的指标替代这个时点。

Gateway `InProcessGateway.ts:1459-1472` 发 loop_start；`:1587-1599` 在 timeout/current-generation 检查后观察 Agent 事件；`:1714-1726` 将 Gateway 层异常作为 loop_error 上报。正式验证需证明这些生产调用真实进入同一个 collector，而不是只在测试里直接调用 mapper。

当前 Rust `pilotdeck-cli/src/telemetry.rs` 仅模拟 context git probes；`server.rs:12-18` 仍是零指标常量，`:25-26` 手动 probe。接入新 collector 时应明确由真实 context 替代这些旧路径，避免重复 git probe，并按既有 snapshot 时点报告实际队列。主协调者拥有这些文件，本审查不编辑。

## 未闭合项

- 上述 context/registry/特殊字段修复与扩充后的最终 oracle、正式遥测负控复核。
- 冻结 Rust collector/sender/context/registry/mapper 的逐项审查与文件哈希。
- 新模块实际挂载，CLI/server/Gateway 共享实例及所有权、reload、shutdown、恢复接线。
- 集中编译、专项测试、真实 Rust 正式入口回放及故障注入；当前均未验证。
- 关闭前逐条核查 todo/unimplemented/NOT_IMPLEMENTED、空分支与 TS fallback；不能以模块未导出或本地 mock 成功标记 O1 完成。
