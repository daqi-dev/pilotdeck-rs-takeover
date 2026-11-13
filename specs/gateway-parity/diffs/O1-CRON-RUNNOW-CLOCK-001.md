---
id: O1-CRON-RUNNOW-CLOCK-001
---

## 类别

既有基线 bug 修复的精确登记：run-now 不应因创建其即时一次性任务时的第二次时钟采样而拒绝自己。当前产品已有修复，本次不改产品、不新增通用 diff 过滤，不把原 profile 的 self DIFF27 / pair DIFF27 改成 SAME。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85，`resources/PilotDeck/src/cron/runtime/CronRuntime.ts:388-403` 的 runTaskNow 用 `new Date().toISOString()` 构造 once.runAt，然后调用 createTask；`:219-236` 的 createTask 另取 `this.now()`（默认 new Date），当 nextRunAt 早于该 now 时抛 `One-time Cron tasks must be scheduled in the future.`。跨毫秒二读自拒是源码支持的原因推断；原件没有直接记录两个时钟值，不能声称观察到具体相差多少毫秒。

原场景 `scripts/bbx/scenarios/cron-run-now.json` 预置 task-a、调用 cron_run_now、等待4000ms后 cron_list，不含取消步骤。三份不可变原件：
- `ts-1`：`_tmp/20261113-e0-resume/e287-baseline-unstable-existing/triplet-slot-1/run/raw/cron-run-now.ts-1.json`；SHA256 `3c058184dbf71815e0e587aa9668f2c9dcf4a6d164ca67f4d0cef4e4302b5bd4`。
- `ts-2`：`_tmp/20261113-e0-resume/e287-baseline-unstable-existing/triplet-slot-1/run/raw/cron-run-now.ts-2.json`；SHA256 `456828ba648355dd1d23d78c96ec76558e557d72374d6bb166a64e8de55cf164`。
- `rust`：`_tmp/20261113-e0-resume/e287-baseline-unstable-existing/triplet-slot-1/run/raw/cron-run-now.rust.json`；SHA256 `99c11e278b14b2b35c15bce39ab08701d690a23f22e6b40a8fe49dad415ecba4`。

TS1 真实失败：精确 future 错误、recentRuns0、notifications0、无 provider。TS2 与 Rust 均成功：started=true、新派生 task/run、history1、notifications8、title/main各1及完整落盘。旧 self 与 TS1/Rust pair 的27路径包含这些分支后果及 tasks.json 的六处键序/newline/skeleton 差异，全部保留，不单独免掉格式字段。

## Rust 行为

`src/pilotdeck-gateway/src/runtime/cron/manager.rs:1399-1473` 既有实现为一次 run-now 只读取一次注入时钟，以同一时刻生成 once.runAt 并传给 create_task_at。生产仍取真实墙钟；不将任意过去的一次性任务变为可接受，不改普通 create/update 校验、调度停止、取消、恢复或重放策略。

本次仅用现公共 comparator 离线对原 TS2 成功 raw 与原 Rust raw 复核，exit0/SAME，输出 `_tmp/20261114-final-cron-residual-review/successful-ts2-rust.txt`。这是在现公共 comparator 既有规范化及已接受差异下 SAME，不声明原始字节一致，也不宣称 diff 比较了单独 validator 负责的 evidence/failures；不能代替 TS1 失败分支比较，不把业务成功当作每个原比较通过。

## 范围

仅 runTaskNow 内自建即时 once 任务与 create_task_at 的同次时刻绑定。未找到公共账本中对应 #2047 条目；源注释的旧「AI代批，待人复核」不是本次批准依据。任何未来注册应引用现用户允许修复与基线类型/测试矛盾的规则及本次 root 明确裁定。

不忽略通用时钟字段；不允许缺失 run/provider/notifications/history/durable；不允许重复执行、改变原 seed task、错误 run/session 关联。自动4s timer、AO fire/abort 和持久恢复是各自独立证明，不冒充同三raw的等价。原 profile 继续记录非SAME，作为已知基线自拒分支证据。

## 机器判据

原成功分支封口使用已有命令，无新判据：`node scripts/bbx/diff.mjs <固定TS2原件> <固定Rust原件> --max 1000`，本次实际exit0/SAME及输出SHA见 `_tmp/20261114-final-cron-residual-review/review.json`。原 TS1 self/pair报告仍DIFF27，禁止对它们改标准取得SAME。

三份原件另经现公共 `scripts/bbx/evidence.mjs::validateArtifact` 完整离线验证，均 PASS（errors=[]）；这是 `check.mjs` 实际调用的公共 validator，核验步骤完整性、required assertions、failures 与 identity 等，不能由 failures=[] 或 diff SAME 代替。固定场景 SHA256 为 `59ddeac56de28dcb1d00213d8078c60738c1d0361dd12ff860e763d251d614b4`，与三份原件记录一致；原 raw SHA 逐一核对。首次离线结果及 validator/check 源 SHA、各历史 identity 见 `_tmp/20261114-final-cron-residual-review/validator-first.json`。该复验没有启动 `check.mjs` 的采集入口，不构成新产品采集或当前候选绑定。

确定性正向控制 `runtime::cron::manager_tests::run_now_shares_one_clock_read_with_the_task_it_creates` 使用每读取一次就递增的真实manager注入时钟，要求 run-now 成功，避免偶然同毫秒假阳性。实际当前 Linux45 exact PASS 见 `_tmp/20261113-e0-resume/e99ee172-final-workspace/test-status-index.json`；当前 Mac46 exact PASS 见 `_tmp/20261114-mac-full-c88650f6/nextest.stderr.log:3674`。两候选 manager 与该测试字节相同，实际source/hash及整份日志SHA见 `_tmp/20261114-final-cron-residual-review/review.json`；不借整套总体状态替代exact结果。

既有六方法金样、明确过去schedule拒绝、重复触发不重放、stop权威、cancel once删除和持久恢复测试保持；真实4s timer TS2/Rust pairSAME及AO多阶段/abort保原candidate身份。无需新采集或新矩阵。如以后必须开发机器接受入口，需另行锁完整3raw/input/hash及全部27路径，任何成功分支丢run/provider/artifact、重复副作用或错误ID都必须拒绝；本条目不实现或申请通用ignore。

## 批准记录

已批准：2026-11-14，root 在用户委托的后端完成与缺陷修复自决授权范围内，明确批准 SHA256 `1101b3ca12bd09fc51f96d306a931140cd33598f73a5b0ca8df8edf6b47693da` 私有草案的精确语义并授权本条登记。此为 root 依据用户委托作出的本条批准，不冒称用户逐字逐条直接批准；不沿用旧 AI 代批。依据 `.42cog/intent.md` 与 `docs/plan/backend-completion.md` 的既有缺陷修复范围。

批准仅登记既有 one-clock 产品修复及上述有限证据：原 self/pair DIFF27 保留，TS2/Rust 仅按现公共 comparator 规则 SAME，不新增比较器、过滤或 Original53 分数豁免。root 负责另行追加持久决定与统一冻结；本文件不修改该决定、冻结标准或其他账本。本次仅公共条目登记与已有原件离线验证，无产品修改、Cargo、产品采集、API 或远端运行。
