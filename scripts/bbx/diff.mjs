#!/usr/bin/env node
// 黑盒对拍 · 比较器。
// 用法：node scripts/bbx/diff.mjs <a.json> <b.json> [--max N] [--quiet] [--self]
// 退出码：0 一致，1 有差异，2 用法错误。
//
// 归一化只有下面四条，都是「同一侧连跑两次也会变」的值，别的一律按原样比：
//   1. UUID            → <uuid:N>，N 是它在该侧输出里第一次出现的序号（保留「同一个 id」的关系）
//   2. ISO 时间戳      → <iso>
//   3. 毫秒时间戳      → <ms>（1.6e12 ~ 2.0e12 之间的整数；文本里独立出现的同范围 13 位整数同样处理（前后紧挨十六进制字符或小数点的不算，免得误伤哈希），
//                        例如 CLI 输出的 JSON 文本里的 lastModified）；数值形式的带小数的也记成 <ms>：
//                        文件系统给的 mtimeMs 是「秒 × 1000 + 纳秒 / 1e6」，纳秒部分恰好是整毫秒时 JSON 里就是整数，
//                        原来分成 <ms> / <ms.frac> 时这种巧合会让同一种写法偶发报差异（gw-projects-files-depth-limit 首次试跑碰到）。
//                        代价由一道防线补上：同一次比较里，两侧都有的同一路径上被这条规则抹掉的数值，若一侧有 3 个以上
//                        带小数、另一侧在这些位置上全是整数（或反过来），报一处 $.<ms fraction>（「一侧总把毫秒截成整数」）。
//                        单个位置小数恰好为 0 的巧合不会凑满「全是整数」；文本里的 13 位整数本来就没有小数，不参与
//   4. 进程号          → <pid>（键名为 pid / ownerPid 的整数；Node 警告行前缀 "(node:1234)"；文本里的 "pid=1234"）
//   5. requestFingerprint → <fingerprint>（键名为 requestFingerprint 的字符串；会话里已有历史时，
//                        基线两次发出的请求体逐字节相同，这个值仍然不同）
//   6. 耗时            → 键名为 duration_ms / duration_seconds / durationMs / waitedMs / delayMs（重试退避，带随机抖动）的数值，以及文本里
//                        "duration_seconds: 0.02"、"duration_ms: 12"、"durationMs":12 的数值，
//                        记成 <dur>。小数位数不比：基线把秒数取整到两位后按 JS 数字打印，0.10 会印成 0.1，
//                        位数本身逐次不同。取整和打印的写法由 Rust 侧单元测试负责
//   7. 基线 execute_code 的两处随机路径段：mkdtemp 目录 pilotdeck_execute_code_XXXXXX 的随机后缀 → <tmp>，
//      工具输出溢出目录 pilotdeck-tool-output/<进程号>/ → <pid>。只认这两个固定前缀
//      （上面第 3 条因为紧跟着「.」不抹它；只认这个前缀和后缀）
//   9. 工作区快照：快照 id 由随机的轮次 id 散列得来（键 snapshotId，以及路径段 /snapshots/<64 位十六进制>）
//      → <snap:N>，N 是它在该侧第一次出现的序号；快照恢复用的 mkdtemp 目录 .pilotdeck-restore-XXXXXX → <tmp>
//   8. 标题记录的落点：会话 .jsonl 里由 provider 生成标题的那条 session_metadata 记录（metadata.titleSource
//      存在）与循环开头的几条记录并发写入，基线两次运行的先后就不同。比较前把它的内容挪到同一轮里
//      accepted_input / 首条 session_metadata 之后；只允许越过 FLOAT_OVER 里列的记录类型，越过别的类型不挪，
//      照常报差异。FLOAT_OVER 里的 agent_loop_operation_started / agent_loop_operation_accepted（2026-11-14 加）：这两条由
//      循环起步时宿主记下，标题记录与它们的先后取决于 agent loop 程序起得多快（modules-tools-result-unknown-sidecar-binding，
//      cb8 交叉实验：基线 gateway + Rust sidecar 20/20 出现同样的 16 处差异，Rust gateway + Node sidecar 20/20 一致——
//      先后跟着 sidecar 程序走，不是 gateway 的行为）。只放这两类，别的类型照旧不越过。挪的是内容，位置上的 sequence / entryId / parentEntryId / createdAt 不动（基线里它们也只随位置变）；
//      同一会话的投影检查点里抄下来的这些位置字段跟着改
//  10. always-on 的 events.jsonl 不比记录先后（基线不等待地追加，先后随机），只比记录集合与内容
//  11. 以 "[gateway:turn-timing] " 开头的行整行去掉（连同行尾换行；最后一行没有换行也算），两侧都去：它只在某一步耗时
//      达到 200ms 时才打印（基线 InProcessGateway.ts:1629、1820，console.info 到 stdout；Rust runtime/turn/submit.rs），
//      打不打、数值多少都随机器快慢变。对产物里全部字符串做（CLI / serve 的 stdout、stderr 都在其中），不看行的其余内容；
//      行中间出现这段文字、别的 "[gateway:" 行照比。每侧摘掉的行数在差异报告头部打印。基线没有别的「达到阈值才打」的日志
//  12. 秒级时间戳      → <sec>（只认键名 created：apiServer 渠道的 OpenAI 兼容响应里 Math.floor(Date.now()/1000)，
//      1.6e9 ~ 2.0e9 之间的整数；SSE 正文文本里的 "created":<10 位整数> 同样处理）。别的键名的 10 位整数照原样比
//  13. 请求转储文件名  → pilotdeck_request_<dump:N>.json（PILOTDECK_DUMP_REQUEST=1 时 streamModel.ts:196-203 写的
//      os.tmpdir()/pilotdeck_request_<Date.now()>.json；文件名里的毫秒后面紧跟 ".json"，第 3 条不抹）。N 是它在该侧
//      第一次出现的序号，保留「同一个文件」的关系，两个文件不会被抹成同一个键
//      Rust 的文件名是 pilotdeck_request_<毫秒>_<进程号>_<序号>.json（独占创建、0600，已登记的安全偏离
//      dump-request-exclusive-private-file）：同样编号成 <dump:N>，这样两侧的转储能对上、正文照常比；
//      文件名格式本身的不同因此不在对拍里出现，由登记的说明和 tests/dump_request.rs 记着，权限的不同由场景的 accepted 放过
//  14. agent loop sidecar 的随机身份（agentLoopSidecarServer.ts:101-102、621；moduleRuntime 的 streamId）：
//      键名为 moduleInstanceId 的 "<模块>-instance-<随机>"、connectionGeneration 的 "<实例>-<随机>-<随机>…"、
//      streamId 的 "stream-<随机>"，随机段（小写字母数字）各自换成 <inst:N> / <gen:N> / <stream:N>，N 是它在该侧
//      第一次出现的序号。保留「同一个实例 / 同一代连接 / 同一条流」的关系（例如重启后是不是新实例），
//      前缀（模块名）和段数照比；格式不符的值不动
//  15. sqlite 里 *_json 列的 JSON 文本先解析再比（不是抹值，是让差异落到内部字段路径上，见 embedJson）
//  16. 记忆检索的随机轨迹 id：case_trace_<10 位十六进制>（edgeclaw-memory-core/src/service.ts:1041）→ case_trace_<case:N>，
//      trace_<10 位十六进制>（core/retrieval/reasoning-loop.ts:58-60、312；步骤 id "<traceId>:step:K" 跟着变）→ trace_<trace:N>。
//      两者都是 sha1(…:Math.random()) 的前 10 位，同一侧连跑两次也不同。N 是它在该侧第一次出现的序号，保留「同一个 id」
//      的关系。只认这两个前缀，且前面不能紧挨字母、数字或下划线（index_trace_ 由内容和时间算出，dream_trace_ 未见到，都不抹）
//  17. 会话文件大小 fileSize（SessionInfo.fileSize = stat 出的字节数，SessionList.ts:146、257；read_session_messages 的
//      session、会话列表里都有）：数值里含着被上面规则抹掉的变长值的位数（例如 11 个 durationMs 从 8 变成 13 就多 11 字节）。
//      两侧数值不同时，各自在落盘树里找这个会话的文件（…/<sessionId>.jsonl），按记录逐条累加字节数
//      （JSON.stringify(记录) + 换行），找出「文件只写到第 K 条时正好是这个大小」的 K：两侧 K 相等就当作相同——
//      那 K 条记录的内容由落盘树照常比（归一化之后），大小之差要么来自被归一化的值，要么来自落盘树里照常报出的
//      内容差异，不需要在 fileSize 上再报一次。两侧 K 不等照报。
//      有一侧拼不出 K（文件后来被整体改写、或会话文件不在树里）时退一步：两侧都大于 0、差值不超过该会话文件里
//      最短一条记录字节数的一半、且不超过 64 字节才放过。少一条记录、大小是旧值、一侧为 0 都照报。
//      放过的处数每次打印。只认对象里键名为 fileSize 的数值；CLI 输出文本里的 "fileSize": N 不处理
//  18. 运行记录文件锁 runs/registry.json.lock 有没有不比：基线停机（SIGTERM → process.exit）可能打断在持锁写
//      运行记录的过程中（锁在 RunRegistry.ts:115-116 建，withFileLock 的 finally 里 154 删），锁文件就留下来；tool-goal-permission
//      基线连跑 30 次留下 9 次。只在一侧有、一侧没有时两侧都略过（并打印）；两侧都有时内容照比，只把内容里的
//      持锁进程号 "pid":<n> 换成 "pid":<pid>（两侧都留下锁时进程号必然不同；时间戳由第 2 条抹）
//      （残留锁对之后启动的影响由场景 robust-restart-registry-lock-leftover 单独比）
//  19. 各自核对：两个由别的内容算出来的字节数，数值里含着被上面规则抹掉的变长值的位数（例如工具结果文本里
//      "duration_ms: 6" 变成 "duration_ms: 12"），不设容差，改成每侧拿本侧产物核对，两侧都核对通过才不比数值：
//      a. resultBytes（tool_call_finished 事件与 subagent_tool_result 的 detail；帧、读回的事件流、运行记录里都有）：
//         基线算法是 Buffer.byteLength(result.content.map(contentToText).join("\n"))（GatewayAgentEventProjector.ts:141-145、
//         412-427；contentToText 在 tool/protocol/result.ts:43-58）。本侧产物里找同一个 toolCallId（有 toolName 时
//         toolName 也要相同；能分出轮次时只看同一轮）的工具结果原文（带 toolCallId、toolName、content 数组的对象，
//         例如会话文件里的 tool_result 记录），按这个算法算出的字节数唯一且等于 resultBytes，这一侧就算核对通过。
//         找不到原文、原文有几份大小不同、算出来不相等的一侧不算通过，照旧比数值。
//      b. 发给假 provider 的请求头 content-length：采集器收到请求时核对它是否等于实际收到的请求体字节数，记在
//         请求记录的 'content-length-ok'（true / false；这个字段本身照常比）。两侧都是 true 时不比头的数值。
//      不比数值时类型照比；内容本身（工具结果文本、请求体）照常比。不比数值且两侧数值确实不同的处数每次打印
//  20. agent loop sidecar 原始协议里应答帧的 messageId：基线是 "<前缀>-<Math.random().toString(36).slice(2)>"
//      （agentLoopSidecarServer.ts:647-649；前缀是 nextId 的全部调用和 handshake 的 hello / capabilities）。键名为 messageId、
//      前缀在这张名单里、后面只有一段小写字母数字的值，随机段换成 <msg:N>，N 是它在该侧第一次出现的序号（与第 14 条同样的写法，
//      保留「同一个 id」的关系）。前缀、段数照比；别的格式（包括多段的）不动。
//      inReplyTo 指向的是客户端发的 messageId（场景写死），不抹
//  21. agent loop sidecar 发给宿主的 module_call 的标识（宿主一侧的各个端口各自起名，随机段都是
//      Math.random().toString(36).slice(2)）：键名为 requestId、形如 "<端口前缀>-<随机段>" 的值，前缀只认基线端口用过的
//      model / model-prepare / model-materialize / model-stream-next / model-close-stream（hostModelInvokerPort.ts:99、128、161、185、198）、
//      context-<操作>（hostContextRuntime.ts:48）、turn-<操作>（hostTurnCallbacks.ts:36）、budget-<操作>（hostModelBudgetPort.ts:37）、
//      lifecycle-dispatch（hostLifecycleRuntime.ts:42）、agent-event（hostAgentEventBridge.ts:40）、tool / tool-catalog / tool-batch
//      （hostToolPort.ts:163、190、242）、permission-decide（hostPermissionDecisionPort.ts:33），<操作> 是小写字母和下划线；
//      另有 plan-todo-<随机段>-<序号>（hostPlanTodoPort.ts:62）。键名为 preparationId、形如 "prepared-<随机段>-<序号>" 的值
//      （hostModelInvokerPort.ts:73）。只把随机段换成 <call:N>，N 是它在该侧第一次出现的序号（同一个值在这一侧各处出现编号相同，
//      所以「应答引用的是不是那个请求」「流式续取用的是不是同一个准备」照常比）；前缀、操作名、序号照比。
//      model-metadata-<provider>-<model>（pilotdeck-agent-loop-default-factory.ts:344）没有随机段，不抹；别的形状（例如 UUID 形状的
//      preparationId）不动，照原样比
//  22. gateway 内存诊断行（PILOTDECK_MEMORY_DIAGNOSTICS=1 / gateway.memoryDiagnostics，gateway/memoryDiagnostics.ts:58-74 打到 stdout 的
//      "[pilotdeck:memory] {JSON}"）：只把这一行 JSON 里 process.memoryUsage() 的五个数值 "rss" / "heapUsed" / "heapTotal" /
//      "external" / "arrayBuffers" 换成 <mem>（进程内存，基线自己连跑两次也不同）；event、sessionCount、projectKey、reason、
//      session 里的估算字节数等其余部分照比。别的行、别处同名的键不处理
// 数组按下标比。唯一的例外：元素全是带名字的对象（function.name 或 name，且名字不重复）的数组
// 按名字对齐——缺的、多的各报一条，共有元素的相对顺序不同报一条，同名元素再逐字段比。
// 这只改变差异的报法（少一个工具不再表现为后面所有工具都错位），不会让任何差异消失。
// 已接受差异（ACCEPTED，同样每次打印）：只属于 Node 运行时、与 PilotDeck 行为无关的输出。
// 场景自己可以在 "accepted" 里登记已接受差异：[{id, why, paths:[正则]}]，只用于基线自身的 bug
// （Rust 有意不照做）。命中的差异路径被略过，id、原因和略过的条数每次打印；依据记在 state/takeover.md。
// 另有一张「不比较」的清单 IGNORED，每次运行都会打印出来，不藏。
// 场景自己可以在 "volatile" 里再声明不稳定的路径（正则，匹配差异路径），同样会打印。
// 过期检查：基线 vs Rust 比较时，场景登记的每条 accepted 正则一处都没命中，打印 STALE 并计 1 处差异；
// volatile 正则一处都没命中只打印 STALE? 提示（基线这次没抖时本来就是 0）。--self（基线自比）不做这个检查。
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';

// 也可以 import（只用 diffArtifacts / IGNORED / ACCEPTED），这时不跑下面的命令行部分。
const isMain = process.argv[1] && fileURLToPath(import.meta.url) === fs.realpathSync(process.argv[1]);

// 不比较的顶层字段，以及原因。
export const IGNORED = {
  side: '采集侧标记',
  startMs: '启动耗时，性能指标，单独报告',
  serverLog: '服务进程自己的日志输出；CLI 子命令的 stdout/stderr 在 steps 里照常比较',
  failures: '超时 / 连不上的汇总，由 check.mjs 判 INVALID；对应的输出在 steps 里照常比较',
};

// 已接受差异：id → [说明, 对字符串做的处理]。新增条目要在 state/takeover.md 记依据。
export const ACCEPTED = {
  'node-runtime-warning': ['Node 运行时自己打到 stderr 的 ExperimentalWarning 及其提示行（基线用了 node:sqlite）；Rust 进程没有这个运行时',
    (s) => s.replace(/^\(node:<pid>\) ExperimentalWarning: .*\n\(Use `node --trace-warnings \.\.\.` to show where the warning was created\)\n/gm, '')],
};
const PID_KEYS = ['pid', 'ownerPid'];
const DURATION_KEYS = ['duration_ms', 'duration_seconds', 'durationMs', 'waitedMs', 'delayMs'];
const dur = () => '<dur>';
const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi;
const ISO = /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})/g;

// 规则 20 只认基线 sidecar 用过的前缀（agentLoopSidecarServer.ts 里 nextId 的全部调用，加上 handshake 的 hello / capabilities）。
const SIDECAR_MESSAGE_ID = /^(hello|capabilities|parse|validation|module-call|cancel|status|resume|ack|accepted|event|final|failed|unknown|result-unknown|execute-rejected|deadline|stream)-([a-z0-9]+)$/;
// 规则 21：module_call 的 requestId 与 preparationId。
const SIDECAR_CALL_ID = /^(model(?:-prepare|-materialize|-stream-next|-close-stream)?|context-[a-z_]+|turn-[a-z_]+|budget-[a-z_]+|lifecycle-dispatch|agent-event|tool(?:-catalog|-batch)?|permission-decide)-([a-z0-9]+)$/;
const SIDECAR_SEQUENCED_ID = /^(plan-todo|prepared)-([a-z0-9]+)-(\d+)$/;
function normalizer() {
  const ids = new Map();
  const snaps = new Map();
  const snap = (id) => { if (!snaps.has(id)) snaps.set(id, snaps.size + 1); return `<snap:${snaps.get(id)}>`; };
  // 规则 14：sidecar 身份的随机段按出现顺序编号；规则 16：记忆检索的随机轨迹 id 同样编号。
  const seq = { inst: new Map(), gen: new Map(), stream: new Map(), case: new Map(), trace: new Map(), msg: new Map(), call: new Map() };
  const tag = (kind, id) => { const m = seq[kind]; if (!m.has(id)) m.set(id, m.size + 1); return `<${kind}:${m.get(id)}>`; };
  const SIDECAR_ID = {
    moduleInstanceId: (x) => x.replace(/^(.+-instance-)([a-z0-9]+)$/, (_, pre, r) => pre + tag('inst', r)),
    connectionGeneration: (x) => x.replace(/^(.+-instance-)([a-z0-9]+)((?:-[a-z0-9]+)+)$/, (_, pre, r, rest) =>
      pre + tag('inst', r) + rest.split('-').slice(1).map((g) => '-' + tag('gen', g)).join('')),
    streamId: (x) => x.replace(/^stream-([a-z0-9]+)$/, (_, r) => 'stream-' + tag('stream', r)),
    // 规则 20：sidecar 应答帧的 messageId（agentLoopSidecarServer.ts:647-649 nextId = `${prefix}-${Math.random().toString(36).slice(2)}`）。
    messageId: (x) => x.replace(SIDECAR_MESSAGE_ID, (_, pre, r) => pre + '-' + tag('msg', r)),
    // 规则 21：module_call 的 requestId（plan-todo 带序号）与 preparationId（hostModelInvokerPort.ts:73）。
    requestId: (x) => x.replace(SIDECAR_CALL_ID, (_, pre, r) => pre + '-' + tag('call', r))
      .replace(SIDECAR_SEQUENCED_ID, (all, pre, r, n) => pre === 'plan-todo' ? `${pre}-${tag('call', r)}-${n}` : all),
    preparationId: (x) => x.replace(SIDECAR_SEQUENCED_ID, (all, pre, r, n) => pre === 'prepared' ? `${pre}-${tag('call', r)}-${n}` : all),
  };
  // turn-timing 行先删再给 UUID 编号：行里的 UUID 参与编号的话，一侧多打一行就会让后面所有序号错位。
  const dumps = new Map();
  const stripTiming = (s) => s.replace(/^\[gateway:turn-timing\] [^\n]*(?:\n|$)/gm, () => { walk.timing++; return ''; });
  const text = (s) => stripTiming(s).replace(/pilotdeck_request_(1[6-9]\d{11}(?:_\d+_\d+)?)\.json/g, (_, ms) => {
    if (!dumps.has(ms)) dumps.set(ms, dumps.size + 1);
    return `pilotdeck_request_<dump:${dumps.get(ms)}>.json`;
  }).replace(UUID, (u) => {
    const k = u.toLowerCase();
    if (!ids.has(k)) ids.set(k, ids.size + 1);
    return `<uuid:${ids.get(k)}>`;
  }).replace(ISO, '<iso>').replace(/\(node:\d+\)/g, '(node:<pid>)')
    .replace(/(?<![\da-fA-F.])1[6-9]\d{11}(?![\da-fA-F.])/g, '<ms>')
    .replace(/pilotdeck_execute_code_[A-Za-z0-9]{6}/g, 'pilotdeck_execute_code_<tmp>')
    .replace(/\.pilotdeck-restore-[A-Za-z0-9]{6}/g, '.pilotdeck-restore-<tmp>')
    .replace(/(\/snapshots\/)([0-9a-f]{64})/g, (_, pre, id) => pre + snap(id))
    .replace(/(pilotdeck-tool-output\/)\d+\//g, '$1<pid>/')
    .replace(/("created":\s?)1[6-9]\d{8}(?!\d)/g, '$1<sec>')
    .replace(/("durationMs":\s?)\d+/g, '$1<dur>')
    .replace(/(duration_ms: )\d+/g, '$1<dur>')
    .replace(/\bpid=\d+/g, 'pid=<pid>')
    .replace(/(duration_seconds: )(\d+(?:\.\d+)?)/g, (_, k, n) => k + dur(n))
    .replace(/^(\[pilotdeck:memory\] \{.*)$/gm, (line) => line.replace(/("(?:rss|heapUsed|heapTotal|external|arrayBuffers)":)\d+/g, '$1<mem>'))
    .replace(/(?<![A-Za-z0-9_])case_trace_([0-9a-f]{10})(?![0-9a-f])/g, (_, h) => 'case_trace_' + tag('case', h))
    .replace(/(?<![A-Za-z0-9_])trace_([0-9a-f]{10})(?![0-9a-f])/g, (_, h) => 'trace_' + tag('trace', h));
  const walk = (v) => {
    if (typeof v === 'string') return Object.values(ACCEPTED).reduce((s, [, apply]) => apply(s), text(v));
    if (typeof v === 'number') return v > 1.6e12 && v < 2.0e12 ? '<ms>' : v;
    if (Array.isArray(v)) return v.map(walk);
    if (v && typeof v === 'object') {
      return Object.fromEntries(Object.entries(v).map(([k, x]) => [text(k), PID_KEYS.includes(k) && Number.isInteger(x) ? '<pid>' : DURATION_KEYS.includes(k) && typeof x === 'number' ? dur(x) : k === 'snapshotId' && /^[0-9a-f]{64}$/.test(x) ? snap(x) : k === 'requestFingerprint' && typeof x === 'string' ? '<fingerprint>' : k === 'created' && Number.isInteger(x) && x > 1.6e9 && x < 2.0e9 ? '<sec>' : Object.hasOwn(SIDECAR_ID, k) && typeof x === 'string' ? walk(SIDECAR_ID[k](x)) : walk(x)]));
    }
    return v;
  };
  walk.timing = 0;
  return walk;
}

const FLOAT_OVER = ['step_started', 'context_snapshot', 'agent_instructions', 'compaction_started', 'compaction_completed', 'agent_status_message',
  'agent_loop_operation_started', 'agent_loop_operation_accepted'];
const SLOT_KEYS = ['sequence', 'entryId', 'parentEntryId', 'createdAt'];
const isTitle = (r) => r?.type === 'session_metadata' && r.metadata && typeof r.metadata === 'object' && 'titleSource' in r.metadata;
function settleTitleRecords(recs) {
  // 返回被挪动的记录：原来所在位置的 entryId → 挪动后所在位置的位置字段。投影检查点里抄了这些记录的
  // 位置字段（sequence / entryId / parentEntryId，以及等于 sequence - 1 的 index），要跟着改。
  const moved = new Map();
  for (let i = 0; i < recs.length; i++) {
    if (!isTitle(recs[i])) continue;
    let j = i;
    while (j > 0 && FLOAT_OVER.includes(recs[j - 1]?.type) && recs[j - 1].turnId === recs[i].turnId) j--;
    if (j === i) continue;
    // 内容按 [title, j..i-1] 重排，位置字段留在原位。
    const payloads = [recs[i], ...recs.slice(j, i)];
    const slots = recs.slice(j, i + 1);
    for (let k = 0; k <= i - j; k++) {
      if (typeof payloads[k].entryId === 'string') moved.set(payloads[k].entryId, slots[k]);
      recs[j + k] = Object.fromEntries(Object.keys(payloads[k]).map((key) => [key, SLOT_KEYS.includes(key) ? slots[k][key] : payloads[k][key]]));
    }
  }
  return moved;
}
function resettleProjections(v, moved) {
  if (Array.isArray(v)) { v.forEach((x) => resettleProjections(x, moved)); return; }
  if (!v || typeof v !== 'object') return;
  const slot = typeof v.entryId === 'string' && 'sequence' in v ? moved.get(v.entryId) : undefined;
  const inner = v.entry && typeof v.entry === 'object' && typeof v.entry.entryId === 'string' ? moved.get(v.entry.entryId) : undefined;
  if (inner && typeof v.index === 'number' && typeof inner.sequence === 'number') v.index = inner.sequence - 1;
  for (const x of Object.values(v)) resettleProjections(x, moved);
  if (slot) for (const key of SLOT_KEYS) if (key in v && key in slot && key !== 'createdAt') v[key] = slot[key];
}
// 规则 10：always-on 的 events.jsonl 不比记录先后。基线追加这个文件时不等上一条写完（DiscoveryFire 的
// appendEvent 是 fire-and-forget），连续发出的两条事件谁先落盘是随机的。比较前按内容排序（先抹掉 id 和时间），
// 记录的集合和每条内容照比。
const UNORDERED_JSONL = /\/always-on\/projects\/[^/]+\/events\.jsonl$/;
const orderKey = (r) => JSON.stringify(r).replace(UUID, 'U').replace(ISO, 'T').replace(/\d{13}(\.\d+)?/g, 'M');
// 规则 15：sqlite 里列名以 _json 结尾、内容是 JSON 对象 / 数组的文本单元格，先解析再比，差异落到内部的
// 字段路径上（…state_json.<json>.recentCaseTraces[0].caseId），场景因此能只放过真正随机的子字段。
// 文本的写法不丢：与 JSON.stringify(解析结果) 逐字相同时记成 {"<json>": 解析结果}，否则另带原文
// {"<json>": …, "<raw>": 原文}，原文照整串比。
function embedJson(row) {
  for (const [col, v] of Object.entries(row)) {
    if (!/_json$/.test(col) || typeof v !== 'string') continue;
    let parsed; try { parsed = JSON.parse(v); } catch { continue; }
    if (!parsed || typeof parsed !== 'object') continue;
    row[col] = JSON.stringify(parsed) === v ? { '<json>': parsed } : { '<json>': parsed, '<raw>': v };
  }
}
// 规则 17：会话文件大小。两侧原始产物一起看（归一化之前），把能解释的 fileSize 差改成相同，返回放过的处数。
const lineBytes = (r) => Buffer.byteLength(JSON.stringify(r)) + 1;
function sessionFiles(raw, sessionId) {
  return Object.entries(raw.tree ?? {}).filter(([k, e]) => k.endsWith(`/${sessionId}.jsonl`) && Array.isArray(e?.jsonl)).map(([, e]) => e);
}
// 文件只写到第 K 条记录时大小正好是 size 的 K（0 条即 0 字节）；拼不出返回 null。
function recordsAtSize(files, size) {
  for (const e of files) {
    if (e.raw && (e.raw.lines !== e.jsonl.length || (e.jsonl.length && !e.raw.finalNewline))) continue;
    let sum = 0;
    if (sum === size) return 0;
    for (let k = 0; k < e.jsonl.length; k++) { sum += lineBytes(e.jsonl[k]); if (sum === size) return k + 1; }
  }
  return null;
}
function settleFileSizes(rawA, rawB) {
  const count = { records: 0, tolerance: 0 };
  const visit = (a, b) => {
    if (!a || !b || typeof a !== 'object' || typeof b !== 'object' || Array.isArray(a) !== Array.isArray(b)) return;
    if (!Array.isArray(a) && typeof a.fileSize === 'number' && typeof b.fileSize === 'number' && a.fileSize !== b.fileSize
      && typeof a.sessionId === 'string' && typeof b.sessionId === 'string') {
      const fa = sessionFiles(rawA, a.sessionId), fb = sessionFiles(rawB, b.sessionId);
      const ka = recordsAtSize(fa, a.fileSize), kb = recordsAtSize(fb, b.fileSize);
      if (ka !== null && kb !== null) {
        if (ka === kb) { b.fileSize = a.fileSize; count.records++; }
      } else {
        const lens = [...fa, ...fb].flatMap((e) => e.jsonl.map(lineBytes));
        const tol = lens.length ? Math.min(64, Math.floor(Math.min(...lens) / 2)) : -1;
        if (a.fileSize > 0 && b.fileSize > 0 && Math.abs(a.fileSize - b.fileSize) <= tol) { b.fileSize = a.fileSize; count.tolerance++; }
      }
    }
    for (const k of Object.keys(a)) if (k !== 'tree' && k in b) visit(a[k], b[k]);
  };
  visit(rawA, rawB);
  return count;
}
// 规则 19：各自核对。一侧的值能由本侧产物里的东西算出来时，在这个对象上记一个标记 SELF_CHECKED（列出核对过的键）；
// compare 在两侧对象都标了同一个键时不比它的数值（类型照比）。标记本身不参与比较。
const SELF_CHECKED = '<self-checked>';
const markChecked = (obj, key) => { obj[SELF_CHECKED] = [...(obj[SELF_CHECKED] ?? []), key]; };
// 基线 tool/protocol/result.ts:43-58 的 contentToText；认不出的内容项返回 null（不核对）。
function contentText(c) {
  if (!c || typeof c !== 'object') return null;
  if (c.type === 'text' && typeof c.text === 'string') return c.text;
  if (c.type === 'json' && 'value' in c) return JSON.stringify(c.value);
  if (c.type === 'image' && typeof c.data === 'string') return `[Image: ${c.mimeType}, ${c.data.length} base64 characters]`;
  if (c.type === 'pdf' && typeof c.data === 'string') return `[PDF: ${c.mimeType}, ${c.data.length} base64 characters${c.pages ? `, ${c.pages} pages` : ''}]`;
  if (c.type === 'file' && typeof c.path === 'string') return `[File: ${c.path}${c.mimeType ? `, ${c.mimeType}` : ''}${c.description ? `, ${c.description}` : ''}]`;
  return null;
}
function markSelfChecked(raw) {
  // 工具结果原文：形如 PilotDeckToolResult 的对象（toolCallId、toolName、content 数组都在），按 toolCallId 收集，
  // 带上它所在的轮次（最近一层祖先的 turnId）。resultBytes 的算法是 GatewayAgentEventProjector.ts:141-145、412-427：
  // Buffer.byteLength(content.map(contentToText).join("\n"))。
  const results = new Map(), holders = [];
  const visit = (v, turnId) => {
    if (Array.isArray(v)) { for (const x of v) visit(x, turnId); return; }
    if (!v || typeof v !== 'object') return;
    const t = typeof v.turnId === 'string' ? v.turnId : turnId;
    if (typeof v.toolCallId === 'string' && typeof v.toolName === 'string' && Array.isArray(v.content)) {
      const parts = v.content.map(contentText);
      if (!parts.includes(null)) {
        if (!results.has(v.toolCallId)) results.set(v.toolCallId, []);
        results.get(v.toolCallId).push({ toolName: v.toolName, turnId: t, bytes: Buffer.byteLength(parts.join('\n')) });
      }
    }
    if (typeof v.resultBytes === 'number' && typeof v.toolCallId === 'string') holders.push([v, typeof v.timeline?.turnId === 'string' ? v.timeline.turnId : t]);
    for (const x of Object.values(v)) visit(x, t);
  };
  visit(raw, undefined);
  for (const [v, turnId] of holders) {
    let cands = (results.get(v.toolCallId) ?? []).filter((c) => typeof v.toolName !== 'string' || c.toolName === v.toolName);
    if (turnId && cands.some((c) => c.turnId === turnId)) cands = cands.filter((c) => c.turnId === turnId);
    const sizes = new Set(cands.map((c) => c.bytes));
    // 同一个调用 id 在本侧有几份不同的原文（例如 call_bbx_1 在不同轮里复用、又分不出轮次）时不核对，照比数值。
    if (sizes.size === 1 && sizes.has(v.resultBytes)) markChecked(v, 'resultBytes');
  }
  // 发给假 provider 的请求：采集器收到时核对过 content-length（记在 'content-length-ok'）。
  for (const list of Object.values(raw.provider ?? {})) {
    for (const r of Array.isArray(list) ? list : []) if (r && r['content-length-ok'] === true && r.headers && typeof r.headers === 'object') markChecked(r.headers, 'content-length');
  }
}
let selfChecked = 0;

// 规则 18：运行记录文件锁只在一侧有时，两侧都略过。
const REGISTRY_LOCK = /\/runs\/registry\.json\.lock$/;
function settleRegistryLock(rawA, rawB) {
  const dropped = [];
  for (const k of new Set([...Object.keys(rawA.tree ?? {}), ...Object.keys(rawB.tree ?? {})])) {
    if (!REGISTRY_LOCK.test(k)) continue;
    if ((k in (rawA.tree ?? {})) === (k in (rawB.tree ?? {}))) {
      // 两侧都有：锁文件内容 {"pid":<持锁进程号>,"acquiredAt":…}（RunRegistry.ts:116）里的进程号归一化，其余照比。
      for (const e of [rawA.tree?.[k], rawB.tree?.[k]]) if (e && typeof e.text === 'string') e.text = e.text.replace(/("pid":)\d+(?=[,}])/g, '$1<pid>');
      continue;
    }
    delete rawA.tree?.[k]; delete rawB.tree?.[k]; dropped.push(k);
  }
  return dropped;
}
function load(raw) {
  for (const [name, entry] of Object.entries(raw.tree ?? {})) {
    if (name.endsWith('.jsonl') && Array.isArray(entry?.jsonl)) {
      const moved = settleTitleRecords(entry.jsonl);
      const projections = raw.tree[`${name}.projections.json`];
      if (moved.size && projections?.json) resettleProjections(projections.json, moved);
    }
    if (UNORDERED_JSONL.test(name) && Array.isArray(entry?.jsonl)) entry.jsonl.sort((x, y) => (orderKey(x) < orderKey(y) ? -1 : orderKey(x) > orderKey(y) ? 1 : 0));
    if (entry?.sqlite?.tables) for (const rows of Object.values(entry.sqlite.tables)) for (const row of rows) embedJson(row);
  }
  for (const k of Object.keys(IGNORED)) delete raw[k];
  const walk = normalizer();
  const out = walk(raw);
  timingLines.push(walk.timing);
  return out;
}
let timingLines = [];

const show = (v) => { const s = JSON.stringify(v); return s === undefined ? '(absent)' : s.length > 200 ? s.slice(0, 200) + `… (${s.length} chars)` : s; };
// 数组元素的名字表；不是「全部元素都有不重复的名字」就返回 null。
function names(arr) {
  const out = arr.map((x) => (x && typeof x === 'object' && !Array.isArray(x)) ? (typeof x.function?.name === 'string' ? x.function.name : typeof x.name === 'string' ? x.name : null) : null);
  return out.every((n) => n !== null) && new Set(out).size === out.length ? out : null;
}
function compare(a, b, at, outList) {
  if (a === b) return;
  const ta = Array.isArray(a) ? 'array' : a === null ? 'null' : typeof a;
  const tb = Array.isArray(b) ? 'array' : b === null ? 'null' : typeof b;
  if (ta !== tb || (ta !== 'object' && ta !== 'array')) { outList.push({ at, a, b }); return; }
  if (ta === 'array') {
    const na = names(a), nb = names(b);
    if (na && nb && (a.length || b.length) && JSON.stringify(na) !== JSON.stringify(nb)) {
      for (const n of na) if (!nb.includes(n)) outList.push({ at: `${at}[name=${n}]`, a: a[na.indexOf(n)], b: undefined });
      for (const n of nb) if (!na.includes(n)) outList.push({ at: `${at}[name=${n}]`, a: undefined, b: b[nb.indexOf(n)] });
      const sa = na.filter((n) => nb.includes(n)), sb = nb.filter((n) => na.includes(n));
      if (JSON.stringify(sa) !== JSON.stringify(sb)) outList.push({ at: at + '.<name order>', a: sa, b: sb });
      for (const n of sa) compare(a[na.indexOf(n)], b[nb.indexOf(n)], `${at}[name=${n}]`, outList);
      return;
    }
    if (a.length !== b.length) outList.push({ at: at + '.length', a: a.length, b: b.length });
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      if (i >= a.length) outList.push({ at: `${at}[${i}]`, a: undefined, b: b[i] });
      else if (i >= b.length) outList.push({ at: `${at}[${i}]`, a: a[i], b: undefined });
      else compare(a[i], b[i], `${at}[${i}]`, outList);
    }
    return;
  }
  // 键的顺序也是可观察的（JSON 序列化顺序），单独报一条，再按键比值。
  const ka = Object.keys(a).filter((k) => k !== SELF_CHECKED), kb = Object.keys(b).filter((k) => k !== SELF_CHECKED);
  // 规则 19：两侧都各自核对过的键只比类型，不比数值。
  const checked = Array.isArray(a[SELF_CHECKED]) && Array.isArray(b[SELF_CHECKED]) ? a[SELF_CHECKED].filter((k) => b[SELF_CHECKED].includes(k)) : [];
  const shared = ka.filter((k) => k in b);
  if (JSON.stringify(shared) !== JSON.stringify(kb.filter((k) => k in a))) outList.push({ at: at + '.<key order>', a: shared, b: kb.filter((k) => k in a) });
  for (const k of ka) if (!(k in b)) outList.push({ at: `${at}.${k}`, a: a[k], b: undefined });
  for (const k of kb) if (!(k in a)) outList.push({ at: `${at}.${k}`, a: undefined, b: b[k] });
  for (const k of shared) {
    if (checked.includes(k) && typeof a[k] === typeof b[k] && (typeof a[k] === 'number' || typeof a[k] === 'string')) { if (a[k] !== b[k]) selfChecked++; continue; }
    compare(a[k], b[k], `${at}.${k}`, outList);
  }
}

// 规则 3 的防线：被抹成 <ms> 的数值按路径记下是否整数；两侧共有路径上，一侧 3 个以上带小数而另一侧在这些位置全是整数，
// 返回一处差异（只报一处，写出两侧的计数）。在归一化之前、两侧原始产物上做。
function msValues(raw) {
  const out = new Map();
  const visit = (v, at) => {
    if (typeof v === 'number') { if (v > 1.6e12 && v < 2.0e12) out.set(at, Number.isInteger(v)); return; }
    if (Array.isArray(v)) { v.forEach((x, i) => visit(x, `${at}[${i}]`)); return; }
    if (v && typeof v === 'object') for (const [k, x] of Object.entries(v)) visit(x, `${at}.${k}`);
  };
  visit(raw, '$');
  return out;
}
function msTruncation(rawA, rawB) {
  const a = msValues(rawA), b = msValues(rawB);
  const shared = [...a.keys()].filter((k) => b.has(k));
  for (const [x, y, side] of [[a, b, 'a'], [b, a, 'b']]) {
    const frac = shared.filter((k) => !x.get(k));
    if (frac.length >= 3 && frac.every((k) => y.get(k))) {
      const note = `${frac.length} 处毫秒时间戳带小数，另一侧对应位置全是整数（例 ${frac[0]}）`;
      return { at: '$.<ms fraction>', a: side === 'a' ? note : '整数', b: side === 'b' ? note : '整数' };
    }
  }
  return null;
}
// 两份原始产物 → 全部差异（场景的 volatile / accepted 之前）与两条成对规则的记录。供下面的命令行和别的脚本用。
export function diffArtifacts(rawA, rawB) {
  const sizes = settleFileSizes(rawA, rawB);
  const locks = settleRegistryLock(rawA, rawB);
  markSelfChecked(rawA); markSelfChecked(rawB);
  const all = [];
  const truncated = msTruncation(rawA, rawB);
  if (truncated) all.push(truncated);
  timingLines = [];
  const a = load(rawA), b = load(rawB);
  const timing = timingLines;
  selfChecked = 0;
  compare(a, b, '$', all);
  return { all, sizes, locks, checked: selfChecked, timing, a, b };
}
function main() {
  const args = process.argv.slice(2);
  const files = args.filter((a) => !a.startsWith('--'));
  const max = args.includes('--max') ? Number(args[args.indexOf('--max') + 1]) : 60;
  const quiet = args.includes('--quiet');
  const selfCompare = args.includes('--self');
  if (files.length < 2) { console.error('usage: diff.mjs <a.json> <b.json> [--max N] [--quiet] [--self]'); process.exit(2); }
  const [fa, fb] = files;
  const scenarioVolatile = (JSON.parse(fs.readFileSync(fa, 'utf8')).volatile ?? []).map((p) => new RegExp(p));
  const { all, sizes, locks, checked, timing } = diffArtifacts(JSON.parse(fs.readFileSync(fa, 'utf8')), JSON.parse(fs.readFileSync(fb, 'utf8')));
  const scenarioAccepted = (JSON.parse(fs.readFileSync(fa, 'utf8')).accepted ?? []).map((x) => ({ ...x, res: x.paths.map((p) => new RegExp(p)), hits: 0 }));
  const stable = all.filter((d) => !scenarioVolatile.some((re) => re.test(d.at)));
  const diffs = stable.filter((d) => !scenarioAccepted.some((x) => x.res.some((re) => re.test(d.at)) && ++x.hits));
  const unused = (res, list) => res.filter((re) => !list.some((d) => re.test(d.at)));
  const stale = selfCompare ? [] : [
    ...unused(scenarioVolatile, all).map((re) => ({ tag: 'STALE?', what: `volatile ${re}` })),
    ...scenarioAccepted.flatMap((x) => unused(x.res, stable).map((re) => ({ tag: 'STALE', what: `accepted ${x.id} ${re}` }))),
  ];
  const count = diffs.length + stale.filter((x) => x.tag === 'STALE').length;
  if (!quiet) {
    console.log(`bbx diff: ${fa}  vs  ${fb}`);
    console.log(`  不比较的字段: ${Object.entries(IGNORED).map(([k, why]) => `${k}（${why}）`).join('；')}`);
    console.log(`  已接受差异: ${Object.entries(ACCEPTED).map(([k, [why]]) => `${k}（${why}）`).join('；')}`);
    if (sizes.records || sizes.tolerance) console.log(`  会话文件大小 fileSize（规则 17）: 两侧写到同样条数的记录，放过 ${sizes.records} 处；拼不出条数、差值在固定容差内，放过 ${sizes.tolerance} 处`);
    if (timing[0] || timing[1]) console.log(`  耗时日志行 [gateway:turn-timing]（规则 11）: 摘掉 a 侧 ${timing[0]} 行、b 侧 ${timing[1]} 行`);
    if (checked) console.log(`  resultBytes / content-length（规则 19）: 两侧各自核对通过、数值不同，不比数值 ${checked} 处`);
    for (const k of locks) console.log(`  运行记录文件锁（规则 18）: ${k} 只在一侧有，两侧都略过`);
    if (scenarioVolatile.length) console.log(`  场景声明的不稳定路径: ${scenarioVolatile.map(String).join(' ')}（因此略过 ${all.length - stable.length} 处）`);
    for (const x of scenarioAccepted) console.log(`  场景登记的已接受差异: ${x.id}（${x.why}）略过 ${x.hits} 处`);
    for (const d of diffs.slice(0, max)) console.log(`  ${d.at}\n      a: ${show(d.a)}\n      b: ${show(d.b)}`);
    if (diffs.length > max) console.log(`  … 另有 ${diffs.length - max} 处`);
    for (const x of stale) console.log(`  ${x.tag} ${x.what}：一处都没命中${x.tag === 'STALE' ? '，计 1 处差异' : '（只提示）'}`);
  }
  console.log(count ? `DIFF ${count}` : 'SAME');
  process.exit(count ? 1 : 0);
}
if (isMain) main();
