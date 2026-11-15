# Runtime / native process 上游增量审阅

本报告审阅固定共同祖先 `8a7b0626a068b011c9ed70025c2231f3fb725bd8` → 上游 `ca7c7cc33db3c42cc81d78239657f3dfbdc422ca` 的增量，再对照产品起点 backend62 `33b23a773b437aaea33696292810cf67aa54fdef` 的现有实现。目标工作树为本轮独立候选；结论不把 main 的旧验收记录、差异批准或测试状态继承为本候选事实。原计划 main `a45c9403edb45a0a53a518f9bb5ea709b1ac7268` 到当前 main 在本组没有新增生产逻辑。

本组共 30 个路径，完整逐项列在下表；`src/` 前缀均为 `src/pilotdeck-gateway/src/`。审阅深度是源码和语义对照，未运行本组 Cargo 测试。上传和附件修复已经另行实际测试，本报告不将其通过推及生命周期或会话身份处理。其他 agent 负责渠道实现和 config/foundation/storage 提供者；本组只审这些提供者在运行时的消费点。

## 逐项接收决定

| 路径（相对 gateway src） | 上游增量与 backend62 对照 | 结论及所需验证 |
|---|---|---|
| `host/modules/context/request.rs` | `string_items` 的手写 UTF-16 代理对遍历抽取为 `js_value::code_point_chunks`；backend62 已按同样规则保留合法代理对及单独代理字符。 | 等价抽取，非能力缺口。暂缓机械重构；共享 helper 若接收可单独替换并复用 UTF-16 夹具。 |
| `native_process.rs` | 追加 stderr 尽力写入；另重导出会话代理字符判定及错误文案。 | 日志安全改动应窄接收；身份拒绝须独立处理，不能因 re-export 一并启用。待坏 stderr 真实进程回归。 |
| `native_process/assembly.rs` | 日志尽力写；automation slot 在单次 start poll 前登记；flush 增加 observer；cron 注释声称失败后共享 stop latch。backend62 装配已有大量新增正式能力。 | slot 先登记是半启动清理所必需的窄接线；日志候选接收；observer 只改善证据可观察性；不继承 cron latch 注释。不可整体回退 main 装配。 |
| `native_process/assembly/automation.rs` | 上游在首次 await 前记录 attempted，使单次 poll 放弃的 start 仍可被补偿 stop 清理。backend62 仅 started 成功后置位，存在真实半启动泄漏窗口。 | 必须修复；授权窄实现，待本候选执行失败/取消/补偿停止用例。不能在清理尚未完成时清除可恢复的状态。 |
| `native_process/assembly/mcp.rs` | 强制持有 `RuntimeLease` 改为 Option，为 mutant 故意关闭 lease；测试增加及时 lease 断言。 | 默认能力已覆盖；保留 backend62 强类型所有权，不为测试注入放宽生产不变量。可复用直接 lease 测试。 |
| `native_process/channels.rs` | console writer 公开；提取同字段 `start_deps`；补 reload 未接线历史说明。 | writer 必要调用方可窄公开；deps 抽取等价暂缓；不能复制 main 的调用方盘点结论到已扩展 backend62。渠道真实接线另由渠道组审查。 |
| `native_process/methods/config.rs` | 会话键代理字符拒绝；projectKey nullish fallback helper。backend62 已明确使用 dialog_project，缺失/null 是 TS resolveProjectKey 的类型错误。 | 身份安全仍未闭合；不接收 main 的 project fallback，会撤回 backend62 的更准确产品行为。混合错误需保留原有先后顺序。 |
| `native_process/methods/mod.rs` | 添加 UTF-16 判定及 require_session_key 包装、nullish project helper；RUN_METHODS 的测试导出。 | 身份 helper 候选待完整入口方案；project helper 与 backend62 冲突不接收；测试导出非生产缺口。 |
| `native_process/methods/runs.rs` | METHODS 可见性由 super 放宽为 crate，支持测试覆盖筛选。 | 无运行行为增量，暂缓测试装置改造。 |
| `native_process/methods/session_dir.rs` | new_session 采用 JS truthiness/UTF-16 插值；多处拒绝单独代理字符；project fallback。backend62 session_new 已使用 to_string_units 和 js_truthy；close_session 已按 TS Map 语义允许非字符串 no-op。 | new_session 已覆盖；拒绝族需按各入口现状重新审计，不能通套 require_text；project fallback 不接收。 |
| `native_process/methods/tools.rs` | rewind_files 改用代理字符拒绝 gate；增加合法代理对及 U+FFFD 对照。 | 安全缺口候选，需正式 rewind 入口证明文件不会跨会话回放，不能继承 main 的历史批准。 |
| `native_process/services.rs` | main 的附件读取仍以其旧 upload lease 路径为前提；新增 regular fd / bounded read。backend62 必须保留 TS 的普通路径和合法 symlink。 | 已窄适配句柄及有界读取，不移植旧路径限制；保持既有文本/图像上限。见 IFACE-UPLOAD-SAFE-HANDLE-001 与 R1。父协调者报告本候选上传组 86 PASS / 3 ignored，独立 services 的实际记录由根账本登记。 |
| `runtime/always_on/manager.rs` | 上游保留 watch 初始 receiver，并尝试用 stop_abandoned 允许重试。backend62 send(true) 在无 receiver 时不保留 true，第二次 stop 会永久等 false；取消首次 stop 还会丢失已 take 的 scheduler/drain 所有权。 | 必须修复。采用实际排空任务持有所有权，调用者仅等待共享 watch，完成后 send_replace(true)。不照搬只能重入、无法保证已有等待者唤醒的 abandoned 标志。 |
| `runtime/always_on/mod.rs` | 只注册新 cfg(test) manifest 模块。 | 非产品增量，连同其装置另行审阅；无需为运行时接入加入。 |
| `runtime/always_on/negative_controls_manifest.rs` | 新增负向控制行表/文件摘要闭合集合测试。 | 测试装置可保留为参考，不能把 main 行数/摘要冻结为本候选分母；未接收不影响生产能力，装置复用待单独验证。 |
| `runtime/cron/manager.rs` | stop_attempted 一次闩锁使第一次 stop 即使 Err，后续 stop 也直接 Ok；start 重置。backend62 会保留失败 runtime 供后续停止尝试。 | 冲突暂缓。不能把未排空伪装为停止成功；须证明失败时任务已消亡或保留真实失败，结合正式 S1/teardown 合同再决定。 |
| `runtime/memory/edgeclaw/file_memory.rs` | 仅测试 scratch 在 fallible I/O 前建立 Drop guard，及子进程测试 stdout receipt 改 writer。 | 没有内存模块生产增量。RAII 测试清理可单独复用；不计为产品待实现。 |
| `runtime/memory/maintenance.rs` | 默认 diagnostic 的 eprintln 改尽力写，避免诊断失败终止后续维护。backend62 保留易 panic 的默认 sink。 | 必要日志安全候选；保持文案/换行/目的流，测试坏 stderr 后仍运行下一轮。 |
| `runtime/model/http.rs` | debug dump 成功 stdout 与失败 stderr 改尽力写，模型请求/落盘算法未改变。backend62 同类调用仍在，另有自己的诊断点。 | 必要日志安全候选；不能顺带改 provider 请求。验证坏流下 HTTP mock 仍收到原请求，dump 错误不会中断调用。 |
| `runtime/model/http/tls.rs` | CA 解析/读取/目录诊断全部改尽力写；原有 fd 类型检查及 MAX_CA_FILE_BYTES 不变。backend62 已具有这些 CA 安全机制。 | CA 文件安全能力已覆盖；只接收非 panic 日志候选。需要损坏 CA + 坏 stderr 真实 HTTPS 配置构造回归。 |
| `runtime/plugins.rs` | 命令目录默认 warn 改尽力写。 | 必要日志安全候选；维持目录返回/警告内容，测试错误插件仍可返回其余目录。 |
| `runtime/project/interaction_notice.rs` | 持交互锁时的 log 改尽力写，避免锁中毒及 notice 丢失。 | 必要日志安全候选；测试坏 stderr 后 notice 保留且锁可再次取得。 |
| `runtime/refresh.rs` | ConsoleLog stdout/stderr 改尽力写，detail 格式不变。 | 必要日志安全候选；正式 refresh 的失败/成功输出正常流需保持字节一致。 |
| `runtime/replacement.rs` | 默认 warn 闭包改尽力写。 | 必要日志安全候选；坏 stderr 不应取消 reservation 清理。 |
| `runtime/session/factory.rs` | message.content.some 检验抽共享 message_contains_tool_result；permission denial 字符串遍历抽 code_point_chunks。backend62 另有较新的 file_artifacts.length 按日志顺序校验。 | 两处抽取等价，可暂缓；不得用 main 整函数覆盖 backend62 的 artifacts 校验。基础组发现 malformed artifacts projection 缺口，应补现有 storage projection，不建立第二份历史所有者。 |
| `runtime/session/transcript_locks.rs` | 仅增加单独代理字符与 U+FFFD 因 lossy 路径别名而共锁的测试。 | 生产未改变；测试证明风险，不能把别名行为当已批准接口。身份修复要同时验证隔离及已有合法文件访问。 |
| `runtime/tools/scheduler.rs` | required 字符串遍历抽 code_point_chunks。 | 与 backend62 手写代理对遍历等价；暂缓纯重构。 |
| `runtime/turn/runner_tail.rs` | 默认 debug sink 改尽力写。 | 必要日志安全候选；尾部存储/释放须在坏 stderr 后完成。 |
| `runtime/turn/submit.rs` | 预算状态失败/超时日志改尽力写；validate 最后拒绝单独代理字符。backend62 有更多参数及权限校验。 | 日志安全候选；身份拒绝保持所有原有字段错误优先，先核对当前 validate 尾部及 busy admission，不能照旧 main 行号拼贴。 |
| `test_process.rs` | 两个 mode-000 权限测试把手写 require/skip 合成 skip_or_require；调用门槛与原错误/skip 文案相同。 | 仅 cfg(test) 共享辅助，生产无增量；依赖 helper 一起审阅，不能导致受限平台静默跳过必须执行的测试。 |

## 需要实施的两个生命周期修复

AlwaysOn 的问题由当前源码可直接推出：构造时 `watch::channel(false)` 的 receiver 立即丢弃；唯一一次停止完成调用 `Sender::send(true)` 失败而不保存值，随后新 receiver 永久收到 false。仅保留 receiver 或 `send_replace` 可以解决连续停止，但不能解决 stop future 被取消后已经从共享表取出的 scheduler JoinHandle 和 active-run receivers 丢失。上游 `stop_abandoned` 重试不能保证已有等待者被唤醒，也不能重新找回已取走的真实 scheduler 排空任务。

本轮窄实现应在第一次 stop 开始时同步标记 stopping，随后由独立 drain task 持有真实 scheduler 和 control-run receivers，直到全部结束再清空 fire、删除 project contexts 并保存 true。每次 stop 都等待这一完成值。取消调用者只是取消等待，不取消实际停止；不能在 Drop、超时或仍有 run 时发送成功。验证至少覆盖：第一次完成后连续两次 stop；首次等待者取消时另一等待者保持未完成；释放在途真实控制调用后所有等待者完成，后续 stop 立即完成。

Automation 的问题位于 `start` 在成功前不设置 started，而 `stop` 只检查 started。同步 builder 用 `now_or_never` 单次轮询 `start`，启动失败后的停止可能 yield；该 future 被丢弃后外部 stop 直接无操作。需在首次 await 前记录 attempted，并在真实清理完成时消除该待清理状态；同时必须在 builder 首次 start poll 前把 bundle 填进 cleanup slot。单改 bundle 而不接线无法保证真实宿主清理。验证使用可观测 ticker/provider：启动 A 成功、B 失败、A 停止等待；丢弃 start future；补偿 stop 必须等待 A 实际消亡。正常 await start 仍应保留 B 的真实启动错误。

## 未闭合项与 P0 出口

本组文件级审阅覆盖已经完成，生产候选还未闭合。AlwaysOn/automation 修复尚需实施、受影响测试及正式装配失败路径证据；非 panic 日志修复是已识别安全缺口，尚需窄接收和坏输出流验证；单独代理字符跨会话别名风险需要覆盖所有读写/删除/rewind/turn 入口的统一方案、差异登记和实测；cron 失败停止的一次闩锁与 backend62 失败保留语义冲突，必须保留为明确未决项。

等价 helper、纯测试装置和已经覆盖的能力可以给出有依据的暂缓结论，但上述安全/生命周期问题不能用“暂缓”宣布产品接入已完成。没有修改冻结判分文件，没有把 upstream 的测试数算成本轮通过数，没有修改全局 `state/iface-upstream-review.json`；其归并由协调者完成。

## 本轮窄补丁交接

2026-11-15，协调者在前一候选 `b3511cce` 的编译/目标执行结束后授权修改。已实现三个文件：

- `runtime/always_on/manager.rs`：首次 stop 同步置 stopping，由单一 Tokio task 持有真实 drain；所有 caller 等待同一 watch；仅实际排空后 `send_replace(true)`。取消 caller 不取消 drain。未接收 main 的 abandoned 标志。
- `native_process/assembly/automation.rs`：首次 start await 前记录 attempted；完整清理返回前保留补偿义务；普通 stop 在停止实际返回后才清 started/attempted 并 unpublish。保留既有 TS 失败聚合和 finally 行为，不添加 cron stop 失败吞掉的一次闩锁。
- `native_process/assembly.rs`：仅将 cleanup slot 登记移到 start 单次轮询之前，没有改变其它装配路径。

新增直接回归五项：AlwaysOn 连续 stop、取消首个 waiter 后仍实际等待 run；automation 丢弃 start 清理并再次丢弃补偿 stop、完整 await 保留真实 provider 错误、丢弃普通 stop 后恢复排空。原在途控制调用测试还补了真实 timeout 与后续连续 stop。Automation 的测试 provider 拥有真实 Tokio 任务，借用 Mutex 中的 JoinHandle 等待；取消测试 future 不会丢弃该句柄，断言 stop 返回时任务已经结束，没有“先宣告完成、再 yield 等退出”的宽松检查。

此补丁已定点 rustfmt（skip_children）及 `git diff --check` 通过。尚未编译或运行这些新回归；协调者取得稳定候选后统一执行。建议过滤 `runtime::always_on::manager::tests::`、`native_process::assembly::automation::tests::`，随后在正式 native assembly 入口验证 startup failure 的 initialization cleanup。上述 P0 生命周期缺口状态改为“已实施、待本候选验证”，其余未闭合项保持。
