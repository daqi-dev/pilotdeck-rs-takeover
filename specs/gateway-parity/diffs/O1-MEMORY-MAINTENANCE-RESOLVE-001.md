---
id: O1-MEMORY-MAINTENANCE-RESOLVE-001
---

## 类别

bug 修复：成功 turn 完成后的后台 memory maintenance resolver/schedule 拒绝，会触发全服务异常退出；属于已授权可修复的崩溃。仅隔离该后台调度失败，不改变模型、工具或正常 submit 的错误语义。

## 基线依据

固定 OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85。`src/cli/GatewayRuntimeRefreshBundle.ts:113-134` 的 async afterTurnCompleted 调用 maintenance.schedule；`src/cli/createLocalGateway.ts:1503` 返回该 Promise；`src/gateway/client/InProcessGateway.ts:1850-1854` 不等待返回 Promise。`src/cli/pilotdeck.ts:271-273` 全局 unhandledRejection 记录 runtime 错误后 shutdownAndExit(1)。

独立审查者使用实际 server、公有 WebSocket submit_turn、真实本地模型及 analytics HTTP，对真实 maintenance resolver 注入 TypeError。`_tmp/20261113-o1-after-turn/formal-fault-2/` 与 `formal-fault-3/` 两轮：先发送最终 turn_completed，随后出现第五条 runtime error telemetry，shutdown snapshot queued=5；客户端关闭 WS 后自然 exit=1、signal=null，后续 turn 未交付。源码与实际 dist 转译身份已核对。`formal-fault-1/` 持有 WS 导致 15 秒未退并用 SIGTERM 清理，保留为失败证据，不算自然退出通过。

## Rust 行为

预期：`native_process/services.rs` 的正式 after_turn 仍以原 sessionKey、可选 projectKey 与 runId 调用同一 RefreshBundle。仅对 maintenance resolver/schedule 的返回错误保留已完成 turn，记录原始 Error.name 的 runtime 错误遥测（module=runtime、metadata.source=unhandledRejection）与明确诊断，不将该后台错误转换为 submit Err，也不退出整个服务；随后 turn 仍可执行。控制器内部 maintenance service 失败的既有隔离路径不变。

本条建立时该产品窄修尚待下一解冻批实施，旧同步传播路径尚不符合上述期望，不能宣称已验证。

## 范围

仅 after-turn memory maintenance 调度/解析失败以及导致的进程终止或 submit 错误。正常 turn 的事件、持久化、model/provider 请求、正常错误、取消/owned drain、maintenance 成功及同代合并行为不变。租约保活继续覆盖精确 generation 的所有在途/queued-repeat/child 工作，不因容错提前释放资源。没有新增产品开关、命令行参数或外部协议字段。

## 机器判据

`src/pilotdeck-gateway/tests/fixtures/o1-telemetry/maintenance-formal.mjs` 通过正式 CLI、公有 WS 和真实 loopback HTTP 验证：首个 turn_completed 保留；同一 resolver-before-acquire 边界真实 TypeError 被观察；runtime 错误恰一条且保留 TypeError/source；第二 turn 成功；SIGINT 正常退出 0；真实九事件上传、snapshot queued=9/sent=0/depth=9、发送后 JSONL 不存在。

TS 使用只读模块 preload 在同一 resolver 边界注入一次异常；Rust 使用有完整源码/二进制哈希的独立 fixture-source mutant，不在产品新增测试环境变量。负控保持相同 resolver 注入，只恢复旧同步 `?` 传播，必须使首个 turn/后续 turn/准确 runtime telemetry 中的正向断言失败。正常 shipping candidate 另行执行无注入 formal.mjs；注入候选不能冒充发行产物。上述 Rust 正向与负控尚待构建执行，不用忽略输出替代断言。

## 批准记录

用户提供的 `docs/plan/backend-completion.md:29` 明确允许修复崩溃、挂起、丢数据和安全缺陷。本轮根协调者于 2026-11-13 在正式 TS 两轮自然 exit1 证据后明确授权本条窄修：保留成功 turn，maintenance resolver/schedule 错误不成为 submit Err 或全服务退出，必须发准确 runtime 遥测及诊断并证明随后 turn 可用。授权只覆盖这里的异常边界；实现和机器判据仍未验证，不能以账本存在冒称完成。
