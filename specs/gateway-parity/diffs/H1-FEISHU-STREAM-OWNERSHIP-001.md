---
id: H1-FEISHU-STREAM-OWNERSHIP-001
---

## 类别

两项独立的 bug / 资源安全修复：停止接口未实际关闭 Lark WebSocket，导致 socket 与后台任务继续存活；未经上限校验的分片数量可直接驱动数组分配。此条只接受下面明确列出的停止所有权和分配保护差异，不接受普通 stream 输入、输出、ACK、路由、重连或错误格式的任意变化。

## 基线依据

固定 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：

1. `src/adapters/channel/feishu/FeishuChannel.ts:226-233` 的 stop 只在 `typeof wsClient.stop === "function"` 时调用它，随后清空引用。实际依赖 `@larksuiteoapi/node-sdk@1.65.0` 的 `lib/index.js:89444-89471` 暴露 `close(params)`，没有 `stop()`；它持有 WebSocket、心跳和重连定时器。适配器的检查因此跳过实际关闭。`:257-269` 注册消息 handler 并启动该 WSClient；`:280-307` 的入站调用继续进入消息处理链。
2. 同一实际 SDK `lib/index.js:88875-88905` 的 `DataCache.mergeData` 直接以 `new Array(sum)` 创建分片表，然后按 `seq` 写入；`:89366-89377` 把收到的字符串 `sum` / `seq` 经 `Number` 转换后传入，没有分配上限。异常大的分片数可能消耗巨量地址空间 / 内存或抛分配异常。

轻量、本地、隔离 HOME/PILOT_HOME 的真实固定 SDK oracle 位于 `_tmp/20261113-feishu-stream/oracle.cjs`，第三轮输出 `oracle-third.json` 实际捕获 endpoint 请求、即时二进制 ping、错误事件的无 handler ACK、正确事件 ACK，且只触发正确消息一次。第一轮实际调用 `wsclient.stop()` 抛 `TypeError: wsclient.stop is not a function`；首败保留。第二轮假 appId 被实际 SDK start gate 拒绝，亦保留。该 oracle 不是正式 gateway / BBX 入口通过证明，也尚未单独实测巨量分配；分配风险依据为上述实际运行依赖的代码。

## Rust 行为

1. `src/pilotdeck-gateway/src/channel/feishu/stream.rs` 的 `StreamOwner` 持有 stop 信号、连接任务和共享清理 receipt。停止调用同步发出取消信号；独立清理 worker 等待连接任务结束，使 pending endpoint HTTP、WS dial、已建立 socket、心跳与重连等待均不再由调用者 future 的存活决定。生产 `.with_network(...)` 路径还通过 `lifecycle.rs` 统一关闭入站 / cron 出站任务 admission，取消并 join 已注册任务，然后对已接收但未自然结束的 turn stream 执行 `cancel_and_drain`。失败的 stream 保留供下一次 stop 重试；任务 panic 或清理失败不得报告成功。普通自然结束的 stream 不伪造取消操作。
2. `stream.rs` 的 data/event 分片处理在创建 `Vec<Option<Vec<u8>>>` **之前**校验 `sum`，只接受 `1..=4096` 的分片数；本条新增接受的分配保护差异是 `sum > 4096` 的拒绝。超过上限时返回内部连接处理错误 `invalid stream event fragment index`，不创建该分片表，不把该片送入消息 handler，不为该片发送成功 ACK；当前连接 reader 返回并释放 socket，随后按当前服务端连接配置决定重连。不是默默截断为 4096，也不是丢片后对该消息回复成功。

## 范围

停止修复限于该飞书 adapter 实例拥有的 stream 连接和已注册生产任务 / turn stream。它不授权取消其他渠道或其他 session 的 turn，不授权忽略失败，不承诺 OS 进程退出、任意外部进程或所有其他飞书功能缺口已经闭合。

分配保护限于单个 stream data/event 的分片数量：4096 是包含的最大值，4097 开始拒绝。不因本条放宽合法分片的顺序、拼接字节、消息去重、事件类型、普通 ACK、合法消息正文、鉴权或回复行为。`sum == 0`、非法序号、格式错误与其他协议处理仍需各自对照基线，本条不把所有这类输出差异自动接受。此上限也不是对总消息字节、同时缓存消息数或进程总内存已有完整保证的声明。

## 机器判据

新增 `channel::feishu::stream_tests`：

- `frame_schema_matches_fixed_sdk_binary_and_rejects_corruption`：固定 SDK 实际二进制 ping；故意改变 service 与截断 / 溢出编码必须不能通过精确断言。
- `real_endpoint_fragmented_stream_dispatch_dedupe_ack_ping_and_owned_stop`：真实 loopback endpoint + WS；逆序分片、错误事件与重复 message_id；实际 Gateway 入站与实际 Open API 出站；收到 ACK/ping 后停止，丢弃首 stop waiter 后第二 waiter 仍观察同一 socket owner 完成。
- `owned_stop_cancels_pending_endpoint_exchange_before_ws_exists`：本地 peer 已实际读完 endpoint 请求后悬挂响应，stop 必须使真实 TCP exchange 结束。
- `owned_stop_retains_failed_turn_stream_and_retry_receipt_after_waiter_drop`：明确 witness 证明 stream.next 已被 poll；第一次 cancel/drain 失败后 stop 不得成功；原 stream 必须被保留，重试 waiter 丢弃后仍能清理并由另一个 waiter 观察成功。
- `peer_close_repulls_endpoint_and_reconnect_delivers_real_event`：本地旧 peer 关闭后，必须重新 POST endpoint，第二 WS 实际消息进入 Gateway。

**登记时这 5 项 Rust 测试尚未编译 / 执行**。只完成 rustfmt 与 diff 静态检查，不能因条目存在标绿。统一 Cargo 槽运行受影响 `channel::feishu` 全部测试及 clippy，并保留首次失败。

**4096 / 4097 上限的专门拒绝测试尚缺**，源码当前冻结，已向根协调者报告待编译后补：在真实 WS 发送 sum=4096 的未完成片后发送独立合法消息，要求连接仍能成功 ACK / 入站；再在独立连接发送 sum=4097 的片，要求连接被释放、该片无成功 ACK / 无 Gateway 入站。移除上限或把上限改为 4097 的故障必须被后一断言抓住；把上限改为 4095 必须被前一断言抓住。不得用尝试巨量分配导致测试 OOM 来充当负控。

正式 BBX 飞书 mock 场景尚在设计，必须先固定 TS 自比与实际故障负控；上述私有测试不替代正式入口验收。

## 批准记录

用户 2026-11-13 的 `AGENTS.md` 与 `docs/plan/backend-completion.md` 授权本期修复明确的崩溃、挂起、丢数据与资源安全缺陷并登记窄差异。根协调者在当前会话明确授权本条文件，并原话确认：“批准sum<=4096作为本期已授权资源安全修复，须……登记两独立范围：TS不存在stop导致socket/task泄漏→owneddrain；恶意/异常分片sum无上限→分配保护……不能泛视所有stream不同输出均accepted。”

本条登记该已授权的两项具体修复；批准不等于测试通过，也不替代缺失的 cap 拒绝测试或正式入口证据。

## 后续实际验证（候选 46；保留上方登记时状态）

上方“尚未编译 / 执行”和“4096 / 4097 专门测试尚缺”描述的是本条初写时状态，并非现在的测试结论。候选 `c88650f6d89bd839d900e6256aa65bdca82e0b3d` 的 Mac 与 Linux 实际全量逐项记录中，`channel::feishu::stream_tests` 七项均 PASS，包括上方五项、`fragment_count_4096_preserves_connection_4097_rejects_without_ack_or_dispatch` 与 `submitted_request_matches_nine_actual_fixed_ts_config_mapper_cases`。Mac 记录 `_tmp/20261114-mac-full-c88650f6/actual-test-states.json`，Linux 记录 `_tmp/20261113-e0-resume/c88650f6-final-workspace-r2/test-status-index.json`；完整身份与哈希见 `_tmp/20261114-candidate51-readiness/feishu-stream-diff-audit.json`。该结果不改写初写时事实，不冒称后续其他候选已重新运行。

停止连接同一基线缺陷另有正式 B 组固定原件：配置禁用、reload 完成后，TS 原 socket 返回匹配 nonce 的 Pong，Rust 原 socket 已真实 close；基线语义及派生值登记见 `H1-FEISHU-STOP-DERIVED-001`。已批准的固定 raw 离线严格重判结果 `_tmp/20261113-abc-formal-644a8d80-final/result.json` SHA256 `a3b4aa0efe32f3edcdfdea5b6b2a47ea92c95746291487b137f565ed681ae358`，self 与 pair 均 SAME；这是按已批准精确派生策略重判，不是原始 strict 无差异，也不证明 4096/4097 场景被该 BBX stop 测试覆盖。两侧实际 capture 的 Rust 产品身份为 `2aa207a55fc15d906c3fb5db647a1c06bddbf563`，和上述 46 组件测试身份分列。
