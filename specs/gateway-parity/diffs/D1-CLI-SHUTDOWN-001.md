---
id: D1-CLI-SHUTDOWN-001
---

## 类别

基线挂起 bug 修复及正常关闭后 checkpoint 刷新的有限派生差额。仅接受本条固定场景与已完成任务证据，不恢复基线挂起。

## 基线依据


固定TS cc7c4e6、Node22在e287最终批次的三场景各两次自比均SAME，原pair分别DIFF9/6/24，原件保持。导航及9份rawSHA见本目录triage.json；实际来源产品e28748f5c69b22232acc0c3d900301f708f3e3fb、harness e8d4a686b34f22de26beb31bde05aa04a2cc391c。

仅下列7个0-based步骤：cli-entry-default [0,1,2]，cli-survey-offline-nontui [0,4]，cli-default-turns [0,1]。场景原字节SHA在policy中固定，argv/stdin/expectTimeout/timeout/合法非TUI步骤保持。

5个无有效项目的空/错误入口观测：固定EOF/空argv先完成实际gatewayprobe；空输入无模型调用，hello/未知command输出原UnknownprojectKey错误，registry确有相应完整failed terminal。不能将它们宣传成成功模型业务。两侧stdout一致，TS随后仍留存客户端资源直至runner既定deadline SIGKILL；Rust正确结束0。

有效项目两步骤观测：五个原始输入均完成，provider实际title/argv/text/read/read-after-tool/bash/bash-after-tool/e400各一次；真实read/bash回执进入模型，最后HTTP400产生原model_error终态，所有CLI stdout保持。TS完成后仍不退出，Rust在正常业务、EOF和错误路径显式close/shutdown。

源码：固定TS pilotdeck.ts347-348只await CliChannel.start无dispose；CliChannel.ts28-36在argv/interactive完成后返回，stop只改boolean。Rust cli_channel.rs54-77在outcome之后remote.close与gateway.shutdown。没有恢复挂起的要求。

## Rust 行为

`src/pilotdeck-cli/src/cli_channel.rs:54-77` 在 CLI outcome 后关闭 remote 与 gateway；`src/pilotdeck-gateway/src/runtime/session/factory.rs:741-760` 在关闭 writer 后 flush cache。真实原件七步业务输出/请求/落盘完整，Rust 正常 exit 0，固定 TS 完成任务或 EOF 后未退出而被 runner 按原 deadline SIGKILL。原 DIFF9/6/24 保留，不追写历史 PASS。

## 范围


上述7步且仅上述步骤：TS timedOut=true/exitCode=null/signal=SIGKILL；Rust无timedOut、exitCode=0/signal=null，共21叶。任何Rust超时、signal、非0、丢业务/输出/请求/落盘都不接受。

仅cli-default-turns的指定父chat projection允许其余18叶：TS日志完整130行、cache anchor真实指向129 turn_result；Rust日志同样完整130行、正常关闭后cache anchor指向130尾session_metadata。两侧各自13个view asOf与envelope一致；每个anchor身份必须逐字段匹配自身durable记录；metadata业务状态与真实尾metadata一致（现有ISO时间规则照旧）。两侧完整durable及所有projection语义须在现有归一化下相同，不能将所有sequence/metadata设volatile。

固定TS CheckpointBinding.ts23-26仅在turn_result调度，TurnRunner.ts641-647随后reappendTail；ProjectSessionStorage.ts349-361显式dispose才再flush。Rust runtime/session/factory.rs741-760正常close后flush。该18叶是同关闭修复的派生cache更新，不是丢失durable数据。缓存可重放不能成为随意丢cache或跳过anchor检查的理由。

## 机器判据

`scripts/bbx/cli-shutdown-policy.mjs` 的 `cliShutdownAcceptedPaths` 必须在可信公共比较器自行绑定的既有 load/normalizer 下核全部原始业务证据，并确认完整 ordinary diff 集合恰为 21 个退出叶及指定 18 个 checkpoint 叶。不能全局忽略 sequence、exit、metadata 或普通场景。TS self 不适用。

`cliShutdownEvidence` 是单侧真实合同：核固定场景/基线/完整终态、stdout、provider 回执、Rust 正常退出及同侧完整 durable/checkpoint 链。三个场景的 `cli-shutdown-completed-positive` 必须由 evidence 实际执行，不是固定 passed 标记；该单侧合同本身不授予 pair 差异许可。场景仅新增精确 accepted/assertion 声明，去掉这两行后原 scene SHA 必须一致；旧原件与当前精确声明场景身份分别校验。

测试 `scripts/bbx/cli-shutdown-policy.test.mjs` 使用三场景 TS1/Rust、TS2/Rust 六组原件及破坏负控。原始路径和 SHA 在 `_tmp/20261113-cli-final-triage/triage.json`；离线复算命令：`CLI_SHUTDOWN_ARTIFACT_ROOT="$PWD/_tmp/20261113-e0-resume/e287-final-triage-inputs" node --test scripts/bbx/cli-shutdown-policy.test.mjs`。原件不改，公共比较入口仍比较其余全部字段；缺业务、提前退出、错误终态/回执/链/anchor/view/witness 均拒绝。

证据限制：原 raw 没有独立 step finishedAt；挂起因果依完整终态、固定 EOF 供给及上述固定 TS 未 dispose 源码共同证明。仅出现 exit 0 或文件存在不足以通过。本条不宣称任意 CLI 场景、任意缓存刷新或新产品执行已验证。

## 批准记录

已批准按既有基线挂起默认修复方向执行本条窄登记及接线。依据用户提供的 AGENTS 行为约束与 `.42cog/intent.md:23` 对基线崩溃、挂起、丢数据的默认修复授权；2026-11-13 本会话 root 明确决定“按既有意向书基线挂起默认修复授权推进精确差异登记（不恢复挂起、不新增用户确认）”，后在 Ding 独立审查无阻断后授权正式 append-only 条目、三个固定场景和公共比较器接线。不是声称用户新增一次逐条专门许可。

实际原件两次 TS 自比、独立审查与追加负控见 `_tmp/20261113-cli-final-triage/{triage.json,ding-review.json,ding-probe-results.json}`；初始 DIFF 保留。新增单侧 API 和公共接线还须按同目录后续 receipt 记录实际离线验证结果，不能用本条批准文字替代执行证据。冻结 aggregate 与 FROZEN 未改。
