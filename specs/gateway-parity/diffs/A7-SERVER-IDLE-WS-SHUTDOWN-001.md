---
id: A7-SERVER-IDLE-WS-SHUTDOWN-001
---

## 类别

基线挂起 bug 修复。保持网页的健康空闲认证 WS 连接时，server 收到 SIGTERM/SIGINT 后无界等客户端自行离开；本次修复产品停机闭环，不更改普通业务协议。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85：src/gateway/server/GatewayServer.ts:184 的 server.close；src/cli/pilotdeck.ts:257 的 shutdown.stop；src/cli/PilotDeckServerShutdownBundle.ts:30 等待 closeServer 后清理。实际隔离 oracle `_tmp/20261114-web-journey/shutdown/ts-idle-01/oracle.jsonl`：sessionCount0、无 submit，SIGTERM 后 listener 已关闭，但6002ms后进程及WS仍存活；关闭本探针自己的peer后19ms内退出0。无限等待来自源码没有期限，不冒称观测无限时间。Rust57真实网页现场同样20秒后被controller强杀，首败保留在run-01/lifecycle.jsonl。

## Rust 行为

native_process 产品server选用 native_ws::NativeWsServer::run_phased_closing_clients；S2停止listener后发送已有close_notice，沿用1001/server switching关闭帧与连接cleanup。有界transport/entries drain，正常idle无需用户关页即可退出0；超时仍明确失败并执行既有有界host cleanup，不抹去错误。旧run/run_phased及显式until_clients_leave兼容模式不变。

## 范围

仅产品server退出信号路径。连接观察到close_notice时停止dispatch；不声称SIGTERM瞬间原子拒绝所有已buffer请求。已接纳请求继续受既有drain约束；pending permission/elicitation可能preserve turn，最终由release_interactions/host shutdown收口。没有授权丢partial/终态、伪造成功或放宽其他协议比较。

## 机器判据

native_ws/tests.rs的4项product_shutdown_*分别验证idle close在S4前、已接纳请求保留、真实submit disconnect abort、超时后晚完成仍Err并清host；旧stop_accepting_drains_existing/idle_socket_drain_reports_deadline/bounded_drain及host_cleanup检查保持。新增测试自身不证明durable partial archive或pending permission接线；冻结源码后必须做产品入口、实际Web重启与partial/pending持久链验证。当前登记批准精确预期，测试未运行不算通过。

## 批准记录

2026-11-14主协调基于用户“项目内决策你自行处理”及继续真实Web测试、优先修核心主链授权作出项目内决定，见state/decisions-20261114.md的“Web主链集成缺陷”。不是用户逐字批准此实现。固定TS实际复现与Astra非作者窄审支持此方向；不改历史冻结文件和已有分数。实际验证结果由state/backend-handoff.md与本轮证据记录。
