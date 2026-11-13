---
id: D1-SDK-SIGNAL-ABORT-ORDER-001
---

## 类别

已授权的崩溃 bug 修复。固定 SDK CLI 在真实 SIGINT/SIGTERM 对当前 run 发出 abort_turn 后，观察流先结束可使 run.close 拒绝尚未完成的 abort Promise；signal handler 的 void run.abort 没有消费拒绝，从而产生未处理 Gateway client closed，或受时序影响输出错误。修复只协调同一请求的生命周期，不改变 Gateway/模型事件或显式 cancel API。

## 基线依据

固定 `cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85`：`src/cli/sdkCli.ts:69-85` signal handler 与观察 finally；`packages/sdk/src/client.ts:1354-1363` abort 等待 RPC 后才设 aborted；`:3131-3140` iterator done 触发 run.close；`:1752-1759` query.close 关闭 transport；`packages/sdk/src/transport.ts:195-204,307-316` close 拒绝 pending RPC。

正式 Docker TS 双自比 `_tmp/20261113-e0-resume/5046398b-sdk-ts-self/` 原结果 0/1 SAME、DIFF2 保持。两轮均真实信号，provider open=true/flushedChunks=1；第一轮 result aborted_streaming 后输出 aborted JSON，第二轮相同 result 后打印 C1 partial 并出现未处理 Gateway client closed 栈。原两轮没有 RPC 原帧，不能断言其准确网络排序。

E0 对未改固定 CLI/SDK 的真实 WebSocket mock 门控证明因果，原帧见 `_tmp/20261113-e0-resume/5046398b-sdk-order-diagnostic{,-r2,-r3}/`。相同 terminal→stream-final→abort-response 帧序下，同批交付可产生干净 aborted JSON，下一 event-loop/等 client close 释放则重现未处理 close 栈。r3 先等待真实 stdout result，再同 TCP batch 交付 final/abort，得到 result→aborted JSON、stderr 仅原 SQLite warning。诊断不冒充真实 Gateway/provider/持久化验收。

## Rust 行为

`src/pilotdeck-cli/src/sdk/turn.rs` 仅对单个真实 CLI OS signal、已对当前 query/run 发出的单个 abort RPC：观察 stream final 先到时，保留同一 query transport，继续在原 abort deadline 内等待原 RPC；不重发 abort、不重复 submit。成功对应 response 才以 `output.rs` 的 Aborted 状态输出 `{"status":"aborted","reason":"CLI interrupted"}`；--json 使用原 sessionId/result envelope；自然退出 1、释放 control/query。已经投影出的 provider terminal event 原字节及顺序不变。

RPC 错误、无对应响应、malformed、连接中断、原 deadline 超时不得转换成成功 aborted；保留原错误和退出 1。该新源码尚待根集中编译及正式 binary 门控和持久恢复验收，不能以登记本身声称通过。

## 范围

仅真实 SIGINT 或 SIGTERM 由当前 CLI 已安装的 once listener 接收、已针对当前 session/run 发出一个 abort_turn 的竞态。成功必须匹配该 query transport 上该 abort request ID，不接受其他 run/请求的 response。一个信号发送一个 abort，不新增重试或第二 submit。

不适用于无 signal 的普通 query 错误、显式 cancel/abort API、其他 session/run、多个独立 signal/多个 abort、RPC error、断线或超时。`D1-SDK-QUERY-ERROR-DISPOSE-001` 明确排除在途 abort，本条不借其豁免扩大范围。不会全局忽略 stderr、Gateway client closed、退出码、连接 close 或 provider 事件。

原正式 TS 自比 DIFF 保留并标 baseline unstable；不得改写成 SAME。真实 Gateway 持久化与 resume 必须独立正判，不由协议 mock 代替。

## 机器判据

`src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order/{driver,verify}.mjs` 驱动正式 Rust CLI、随机本地 WS、隔离 HOME/PILOT_HOME：同 TCP batch、next event-loop、held abort 三正向门；无关 run/RPC ID、malformed response、RPC error、断线、既有 deadline 五控制路径。严格核对 binary/driver SHA、实际 signal、单 submit/abort、当前 session/run、未变 terminal event、精确 aborted JSON/exit1、两 owned socket 释放；错误控制路径不得出现 aborted 成功。

封存 4525 CLI `e408a2bf557d106e0043134ecf2fe7bca0b3602535451a2ff28b67eb8ce6d06b` 首次三门均 Gateway client closed、缺 canonical JSON，证据 `_tmp/20261113-sdk-cancel-order-diagnostic/rust-4525-first/` 保留。修复后实际执行结果与固定 binary 身份由 `docs/backend-delivery/D1-sdk-signal-abort-order.md` 追加；当前待验证。

正式 `c1-sdk-cancel-resume` 的artifact层须真实provider partial、原始持久registry重放、SIGINT/PID/时间与同session/project/request/run证据、canonical CLI输出/exit1，随后同durable session resume成功、历史中保留被取消partial且无重复provider请求。该artifact没有abortRPC原帧，不声称此层直接观察请求/响应或socket close；这部分由同binary SHA的独立8wire严格PASS原始证据及收据证明，两个证据层缺一不可。私有 `scripts/bbx/sdk-signal-policy.mjs` 仅在两层成功时逐值映射具名serve stdout/stderr，其前stream事件须完全一致，TS self仍保留DIFF；公共比较接线须根审核后单独落实，不能先让比较器宽容。

## 批准记录

2026-11-13 用户本轮 `docs/plan/backend-completion.md` 明确允许修复崩溃、挂起、丢数据、安全问题；当前会话根协调确认「SDK race诊断已充分支持已授权崩溃bug范围」，并明确授权本条最小 signal/current-run/同 abort Promise 的修复、独立差异账本与严格正负控制。批准不代替编译、行为测试或正式恢复验收；未通过项仍记待验证。

### 2026-11-13 公共接线与装置验证追加

本条原部分不回写。新增 `scripts/bbx/sdk-signal-policy.mjs` 只对具名第1 serve逐值映射stdout/stderr；公共diff/evidence接线通过两绝对环境路径加载原wire目录/receipt，核固定实跑driver/verifier哈希、重跑严格8raw验证、绑定实际formal binary SHA，inputIdentity保存receipt及34个raw文件hash并在使用时复核。缺失、篡改、手写PASS无raw、错误binary/source均INVALID，不能当普通DIFF绕过。固定具名scene加accepted声明及Rust-only canonical footer/empty-stderr已执行断言，原signal/timeout/等待/turn不改；TSself始终不接受，原DIFF保留。

第一轮公共单测中synthetic数据缺model正断言、随后旧check临时copy依赖表缺新policy造成的失败日志保留；分别修测试数据和相关测试依赖清单后，policy40/public12/原evidence-check39合计91/91通过。真实macOS同binary八wire原始证据重验与synthetic formal克隆的全链验证得到：正向SAME且打印仅两条ACCEPTED；缺partial/缺receipt INVALID；无关tree仍DIFF；原TS1/TS2 self仍DIFF。这不是实际Rust正式恢复验收，Linux实际Gateway/provider/持久resume仍待E0。

后续收口追加：具名scene实际声明与两个Rust-only断言已落地，裸diff校验当前scene声明、两侧已执行断言；双方删除策略也INVALID，TSself无新声明仍原DIFF。增加真实受信driver/verifier字节篡改、receipt改字节、缺失executed assertions等反例后，四组公共/私有/原回归合计97/97通过。最终全链装置探针使用真实mac八wire原件并执行断言的synthetic formal克隆，正向exit0、缺partial/receipt INVALID2、无关tree DIFF1、原TSself DIFF1；不作为实际Linux正式恢复结果。
