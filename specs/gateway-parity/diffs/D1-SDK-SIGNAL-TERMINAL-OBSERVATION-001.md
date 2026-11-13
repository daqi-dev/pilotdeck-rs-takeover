---
id: D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001
---

## 类别

独立的已批准行为差异，不冒称已证明的崩溃、挂起、持久数据丢失或固定测试/类型矛盾，不借用D1-SDK-SIGNAL-ABORT-ORDER-001扩大旧条范围。

## 基线依据

固定cc7c4e6，当前2c9正式配对原件：`_tmp/20261113-e0-resume/2c9c4875-sdk-signal-formal/pair/`。TS真正收到SIGINT，partial已持久化、provider open/flushed1，stdout为六个公共事件seq0/1/2/3/4/6，随后直接canonical aborted footer，缺唯一公共result seq7；Rust六个相同事件后含完整result seq7、再canonical footer。TS只有原SQLite warning，Rust stderr为空，双方exit1。双方durable registry仍有当前run的seq8 turn_completed/usage{}/aborted_streaming，后续同durable session恢复成功。

此前3f正式TS含seq7；两轮harness、baselineDist/lock/entry、dependencies/tools/platform身份相同。原3f成功与本次首INVALID均保留。本次准确网络顺序未由formal capture直接观察，不声称其包含abort RPC帧。

固定源码packages/sdk/src/client.ts:1357-1363在abort ACK后设terminalSeen/done；:1195在done时结束，:1232跳过已terminalSeen的result，解释观察竞态。src/cli/sdkCli.ts:69使用void run.abort，:112-125消费迭代器后取result。没有查到真实OS signal后SDK事件必须保留result的基线测试/类型保证；transport.test.ts:4058-4059与README:27只约束run.result().status aborted。durable terminal存在，不称持久数据丢失。

## Rust 行为

Rust必须六事件加完整唯一公共result seq7：usage{}、finishReason aborted_streaming、当前run/session、status completed；它必须与本侧durable seq8精确对应。随后准确canonical aborted footer、empty stderr、exit1。独立同binary原始8wire仍由受信driver/verifier重验；formal层没有RPC帧，8wire是单独证据层。

## 范围

仅c1-sdk-cancel-resume的第1 serve：原SIGINT、12s deadline、settle100ms及persisted partial触发定义不变。只承认TS唯一缺result seq7，六个事件类型/sequence严格为turn.started0、user.accepted1、pilotdeck.model_request_started2、context.usage3、pilotdeck.model_selection_changed4、pilotdeck.assistant_block6，且每个当前run/session正确、除既有绑定identity归一化外所有值与Rust逐值相同。

TS必须canonical aborted footer、仅原精确SQLite warning、自然exit1；唯一durable seq8必须准确为同run turn_completed/usage{}/aborted_streaming。initial/recovery同session、三durable runs、partial/history、真实signal PID/时间、一次provider和恢复等现有强断言全部保留。

只对具名serve.stdout逐值接受该差异。不使terminal一般可选；不新增stderr宽容，不放过其他steps/tree/provider/exit。TS self不接受、仍保留原差异。非signal/显式cancel API/其他场景、错run、缺其他事件、缺durable/Rust缺terminal均不适用。

## 机器判据

sdk-signal-policy.mjs独立导出新policy ID，scene显式声明sdkSignalTerminalObservation，evidence/diff识别该声明并保留原abort-order策略。旧策略不认领本次stdout遗漏，新策略不认领旧crash形态；缺scene声明仍INVALID。

已审private提案及四个最小克隆负控位于`_tmp/20261113-sdk-terminal-proposal/`；公共回归与新规则重算产生新日志，不覆盖首capture/首INVALID。重算旧行为原件时仅新副本绑定新规则声明/本地重验wire身份，清楚标为新规则验证，不冒充新正式产品执行。新正式pair由E0另行协调。

本轮公共验证：原97项回归加新6项窄测试共103/103通过。当前真实first行为的新规则副本公共diff为SAME，只有本条认领stdout；四个实际副本破坏全部INVALID2，TS self保持DIFF1。首轮3项单测因测试fixture仍硬编码旧单声明而失败，日志保留；改成当前双具名声明后通过，没有修改产品行为或放宽验证。8个ready源文件SHA清单保存在`_tmp/20261113-sdk-terminal-proposal/public-ready-sha256.json`。最终新正式执行仍待E0，旧J2首INVALID不会回写。

## 批准记录

2026-11-13当前会话，用户在审阅固定TS取消终态观察差异后明确选择：「批准，保留 Rust 完整终态（推荐）」。根协调随后授权按已审private提案接线；对应决定由根记录于state/decisions-20261113.md。本条不改旧ledger、accepted-diffs或冻结规则。批准允许下面的精确差异，不代表原formal INVALID可以覆盖改分。
