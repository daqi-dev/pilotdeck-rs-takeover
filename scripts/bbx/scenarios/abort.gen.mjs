// 生成 abort-*.json 场景：中止点矩阵（specs/delivery/abort-matrix.md）里没有现成场景的格子。
// 用法（仓库根目录）：node scripts/bbx/scenarios/abort.gen.mjs
// 改场景改这里，再重新生成；生成的 JSON 一并提交。
//
// 行是「中止到达时这一轮正在做什么」，列是「中止的来源」。每个场景覆盖一行（或相近的几行），
// 一列一个会话：abort_turn（带 / 不带 runId）、close_session、submit_turn.timeoutMs，
// 个别场景另有插话、同会话再提交、断开连接。
//
// 子进程是否被杀、在途工具有没有跑完，靠落盘看：被测命令在睡眠之后写一个 late-*.txt，
// 被杀就没有这个文件；hook 用同一个小脚本 hook.mjs 把收到的输入追加到 {HOME}/hooklog/<名>.jsonl，
// 睡眠结束再追加 {done:true}，被杀的 hook 没有这一行。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);

// node hook.mjs <log> <exit> [stdout] [stderr] [sleepMs]
const HOOK_SCRIPT = `// bbx hook fixture: node hook.mjs <log> <exit> [stdout] [stderr] [sleepMs]
import fs from 'node:fs';
const [log, exit = '0', out = '', err = '', sleep = '0'] = process.argv.slice(2);
const dir = process.env.HOME + '/hooklog';
const append = (row) => { fs.mkdirSync(dir, { recursive: true }); fs.appendFileSync(dir + '/' + log + '.jsonl', JSON.stringify(row) + '\\n'); };
let input = '';
process.stdin.on('data', (d) => { input += d; });
process.stdin.on('end', () => {
  let parsed; try { parsed = JSON.parse(input); } catch { parsed = { raw: input }; }
  const keep = {};
  for (const k of ['hook_event_name', 'hookEventName', 'tool_name', 'toolName', 'error', 'isInterrupt', 'is_interrupt', 'prompt', 'reason', 'stopHookActive']) if (k in parsed) keep[k] = parsed[k];
  append({ argv: process.argv.slice(2), input: keep });
  setTimeout(() => {
    append({ argv: process.argv.slice(2), done: true });
    if (out) process.stdout.write(out);
    if (err) process.stderr.write(err);
    process.exit(Number(exit));
  }, Number(sleep));
});
`;

// 慢 MCP 服务：tools/call 等 argv[3] 毫秒再回；收到的 tools/call 与 notifications/cancelled 记到 {HOME}/hooklog/mcp-<label>.jsonl
// （不记「已回答」：服务进程可能在回答前就随会话关闭被结束，那一行的有无取决于时序）。
const MCP_SCRIPT = `// bbx fixture: slow MCP stdio server. node slow-mcp.mjs <label> <delayMs>
import fs from 'node:fs';
import readline from 'node:readline';
const label = process.argv[2] ?? 'slow';
const delay = Number(process.argv[3] ?? '2000');
const dir = process.env.HOME + '/hooklog';
const log = (row) => { fs.mkdirSync(dir, { recursive: true }); fs.appendFileSync(dir + '/mcp-' + label + '.jsonl', JSON.stringify(row) + '\\n'); };
const send = (message) => process.stdout.write(JSON.stringify(message) + '\\n');
const tool = {
  name: 'slow',
  description: 'Wait a while, then echo the given text.',
  inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'], additionalProperties: false },
};
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  if (request.method === 'tools/call' || (request.method ?? '').startsWith('notifications/cancelled')) log({ method: request.method, params: request.params ?? null });
  if (request.id === undefined) return;
  if (request.method === 'initialize') {
    send({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: request.params?.protocolVersion ?? '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'bbx-slow-' + label, version: '1.0.0' } } });
  } else if (request.method === 'tools/list') {
    send({ jsonrpc: '2.0', id: request.id, result: { tools: [tool] } });
  } else if (request.method === 'tools/call') {
    const text = String(request.params?.arguments?.text ?? '');
    setTimeout(() => { send({ jsonrpc: '2.0', id: request.id, result: { content: [{ type: 'text', text: 'slow[' + label + ']: ' + text }] } }); }, delay);
  } else if (request.method === 'ping') {
    send({ jsonrpc: '2.0', id: request.id, result: {} });
  } else {
    send({ jsonrpc: '2.0', id: request.id, error: { code: -32601, message: 'Method not found: ' + request.method } });
  }
});
`;

const q = (s) => `'${s.replace(/'/g, `'\\''`)}'`;
const cmd = (log, { exit = 0, out = '', err = '', sleep = 0 } = {}) => ({
  type: 'command',
  command: ['node hook.mjs', log, String(exit), q(out), q(err), String(sleep)].join(' '),
});
const plugin = (name, hooks) => ({
  [`home/.pilotdeck/plugins/${name}/plugin.json`]: { name, version: '0.1.0', description: `bbx ${name}`, hooks },
  [`home/.pilotdeck/plugins/${name}/hook.mjs`]: HOOK_SCRIPT,
});

const TITLE = { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Abort run"}' } };
const bypass = (session) => ({ ws: 'set_permission_mode', params: { sessionKey: session, mode: 'bypassPermissions' } });
const submit = (session, message, { runId, timeoutMs, answer, timeout = 20000, expectTimeout } = {}) => ({
  ws: 'submit_turn',
  params: { sessionKey: session, channelKey: 'test', ...(runId ? { runId } : {}), message, ...(timeoutMs ? { timeoutMs } : {}) },
  timeout, ...(expectTimeout ? { expectTimeout: true } : {}), ...(answer ? { answer } : {}),
});
const read = (session) => ({ ws: 'read_session_messages', params: { sessionKey: session } });
const on = (event) => (typeof event === 'string' ? { type: event } : event);
const HOOK_STARTED = { type: 'agent_status', event: 'hook_execution_started' };
// 一列一个动作：在 trigger 帧到来时发出。
const act = {
  abort: (session, trigger) => [{ on: on(trigger), ws: 'abort_turn', params: { sessionKey: session }, count: 1 }],
  abortRun: (session, runId, trigger) => [{ on: on(trigger), ws: 'abort_turn', params: { sessionKey: session, runId }, count: 1 }],
  close: (session, trigger) => [{ on: on(trigger), ws: 'close_session', params: { sessionKey: session }, count: 1 }],
  steer: (session, runId, trigger) => [{ on: on(trigger), ws: 'steer_turn', params: { sessionKey: session, runId, itemId: `item-${session}`, message: `steer-${session}: also say hi` }, count: 1 }],
  busy: (session, trigger) => [{ on: on(trigger), ws: 'submit_turn', params: { sessionKey: session, channelKey: 'test', message: `busy-${session}: second` }, count: 1 }],
};

// 一行的标准四列：abort_turn 带 runId、abort_turn 不带 runId、close_session、timeoutMs。
// make(session, marker) 给出该会话的 provider 规则；trigger 是触发中止的帧；timeoutMs 让超时落在同一个位置。
// 每列之后同会话再提交一轮（close_session 之后也是，会话会被重新打开），看下一轮能否正常开始、带着什么历史。
function columns(prefix, { trigger, timeoutMs, settle = 3500, nextTurn = true, cols = ['run', 'bare', 'close', 'timeout'] }) {
  const steps = [];
  const sessions = [];
  for (const col of cols) {
    const s = `s-${prefix}-${col}`;
    const marker = `${prefix}-${col}`;
    sessions.push({ s, marker, col });
    steps.push(bypass(s));
    if (col === 'run') steps.push(submit(s, `${marker}: go`, { runId: `run-${marker}`, answer: act.abortRun(s, `run-${marker}`, trigger) }));
    else if (col === 'bare') steps.push(submit(s, `${marker}: go`, { answer: act.abort(s, trigger) }));
    else if (col === 'close') steps.push(submit(s, `${marker}: go`, { answer: act.close(s, trigger) }));
    else if (col === 'timeout') steps.push(submit(s, `${marker}: go`, { timeoutMs }));
    else if (col === 'steer') steps.push(submit(s, `${marker}: go`, { runId: `run-${marker}`, answer: act.steer(s, `run-${marker}`, trigger) }));
    else if (col === 'busy') steps.push(submit(s, `${marker}: go`, { answer: act.busy(s, trigger) }));
    steps.push({ sleep: settle });
    if (nextTurn) steps.push(submit(s, `next-${marker}: again`));
  }
  for (const { s } of sessions) steps.push(read(s));
  return { steps, sessions };
}
// 每个会话：第一次请求回 reply(marker)，带着本会话工具结果的请求回 after，next- 开头的下一轮回一句话。
const perSession = (sessions, reply, { afterId, after = (m) => ({ text: `Done ${m}.` }) } = {}) => sessions.flatMap(({ marker }) => [
  { name: `next-${marker}`, when: [`next-${marker}:`], reply: { text: `Next ${marker}.` } },
  ...(afterId ? [{ name: `${marker}-after`, when: [`${marker}:`, `"tool_call_id":"${afterId(marker)}"`], reply: after(marker) }] : []),
  { name: marker, when: [`${marker}:`], reply: reply(marker) },
]);
const callId = (m) => `call_${m.replace(/-/g, '_')}`;
const toolReply = (name, args) => (m) => ({ tool: { id: callId(m), name, arguments: typeof args === 'function' ? args(m) : args } });

const scenarios = {};
// 超时列的子代理继承本轮剩余期限，摘要里带实际毫秒数（「Subagent timed out after 1967ms.」），基线两次自比就差几毫秒。
const subagentTimeoutVolatile = (session) => [
  `/chats/${session}\\.jsonl\\.(jsonl\\[\\d+\\]|projections\\.json\\.json\\.projections\\.agent-transcript\\.subagent-references\\.state\\.completed\\[0\\])\\.summaryPreview$`,
];

// ── 工具执行中：bash 子进程 ──
// 命令先睡 2 秒再写 late-<marker>.txt：被杀就没有这个文件。PostToolUse / PostToolUseFailure 记录 hook 有没有跑、isInterrupt。
{
  const hooks = {
    PostToolUse: [{ matcher: 'bash', hooks: [cmd('post')] }],
    PostToolUseFailure: [{ matcher: 'bash', hooks: [cmd('post-failure')] }],
  };
  const { steps, sessions } = columns('tb', { trigger: { type: 'tool_call_started', name: 'bash' }, timeoutMs: 1500, cols: ['run', 'bare', 'close', 'timeout', 'steer', 'busy'] });
  scenarios['abort-tool-bash'] = {
    about: '中止落在 bash 工具执行中（子进程在睡眠）：abort_turn 带 / 不带 runId、close_session、timeoutMs 超时、插话、同会话再提交。比较：子进程是否被杀（late-*.txt）、PostToolUse / PostToolUseFailure hook 是否跑与 isInterrupt、帧序列与 finishReason、turn_aborted 状态、会话记录、下一轮请求带着的历史。',
    files: plugin('bbx-abort', hooks),
    provider: [TITLE, ...perSession(sessions, toolReply('bash', (m) => ({ command: `sleep 2; echo late > late-${m}.txt`, description: m })), { afterId: callId })],
    steps,
  };
}

// ── 工具执行中：bash，客户端断开（关闭帧 / RST）后重连看结果 ──
{
  const steps = [];
  const provider = [TITLE];
  for (const [col, how] of [['closeframe', true], ['reset', 'reset']]) {
    const s = `s-td-${col}`;
    const m = `td-${col}`;
    provider.push({ name: `next-${m}`, when: [`next-${m}:`], reply: { text: `Next ${m}.` } });
    provider.push({ name: `${m}-after`, when: [`${m}:`, `"tool_call_id":"${callId(m)}"`], reply: { text: `Done ${m}.` } });
    provider.push({ name: m, when: [`${m}:`], reply: { tool: { id: callId(m), name: 'bash', arguments: { command: `sleep 2; echo late > late-${m}.txt`, description: m } } } });
    steps.push(bypass(s));
    steps.push(submit(s, `${m}: go`, { runId: `run-${m}`, timeout: 1200, expectTimeout: true }));
    steps.push({ disconnect: how });
    steps.push({ sleep: 3500 });
    steps.push({ connect: true });
    steps.push({ ws: 'run_get', params: { runId: `run-${m}` } });
    steps.push(read(s));
    steps.push(submit(s, `next-${m}: again`));
  }
  scenarios['abort-tool-bash-disconnect'] = {
    about: '中止来源是客户端断开（GatewayWsConnection.ts:48-62：断开时对本连接在途的会话 abortTurn，原因 aborted）：bash 工具执行中先后以关闭帧、RST 断开，重连后看子进程是否被杀（late-*.txt）、运行记录、会话记录与下一轮。',
    files: plugin('bbx-abort', { PostToolUseFailure: [{ matcher: 'bash', hooks: [cmd('post-failure')] }] }),
    provider,
    steps,
  };
}

// ── 工具执行中：execute_code（Python 子进程）──
{
  const { steps, sessions } = columns('tx', { trigger: { type: 'tool_call_started', name: 'execute_code' }, timeoutMs: 1200, cols: ['run', 'close', 'timeout'] });
  scenarios['abort-tool-execute-code'] = {
    about: '中止落在 execute_code 执行中（Python 子进程睡 2 秒后写 late-*.txt）：abort_turn、close_session、timeoutMs。比较子进程是否被杀、PostToolUseFailure、帧与会话记录、下一轮。',
    files: plugin('bbx-abort', { PostToolUseFailure: [{ matcher: 'execute_code', hooks: [cmd('post-failure')] }] }),
    provider: [TITLE, ...perSession(sessions, toolReply('execute_code', (m) => ({ code: `import time\ntime.sleep(2)\nopen('late-${m}.txt', 'w').write('late')\nprint('slept')` })), { afterId: callId })],
    steps,
  };
}

// ── 工具执行中：MCP 工具（stdio 服务 2 秒后才回）──
{
  const { steps, sessions } = columns('tm', { trigger: { type: 'tool_call_started' }, timeoutMs: 1200, cols: ['run', 'close', 'timeout'] });
  scenarios['abort-tool-mcp'] = {
    about: '中止落在 MCP 工具调用中（stdio 服务 2 秒后才回 tools/call）：abort_turn、close_session、timeoutMs。比较 MCP 服务收到的消息（有没有 notifications/cancelled）、PostToolUseFailure、帧与会话记录、下一轮。',
    files: {
      'home/.pilotdeck/mcp/slow-mcp.mjs': MCP_SCRIPT,
      'home/.pilotdeck/mcp.json': { mcpServers: { 'bbx-slow': { command: 'node', args: ['{PILOT_HOME}/mcp/slow-mcp.mjs', 'slow', '2000'] } } },
      ...plugin('bbx-abort', { PostToolUseFailure: [{ hooks: [cmd('post-failure')] }] }),
    },
    provider: [TITLE, ...perSession(sessions, toolReply('mcp__bbx-slow__slow', (m) => ({ text: m })), { afterId: callId })],
    steps,
  };
}

// ── 工具执行中：一次性子代理（agent），子代理的模型请求还挂着 ──
{
  const { steps, sessions } = columns('ta', { trigger: { type: 'tool_call_started', name: 'agent' }, timeoutMs: 2000, cols: ['run', 'close', 'timeout'] });
  const provider = [TITLE];
  for (const { marker } of sessions) {
    provider.push({ name: `child-${marker}`, when: [`CHILD-${marker}`], reply: { text: 'Scope: x.\nResult: y.\nKey files: none\nFiles changed: none\nIssues: none', delayMs: 3000 } });
  }
  provider.push(...perSession(sessions, toolReply('agent', (m) => ({ description: 'Look', prompt: `CHILD-${m}: look.`, subagent_type: 'general-purpose' })), { afterId: callId }));
  scenarios['abort-tool-agent'] = {
    about: '中止落在一次性子代理（agent 工具）执行中，子代理的模型请求还没回（3 秒）：abort_turn、close_session、timeoutMs。比较子代理事件（subagent_started / subagent_completed）、PostToolUseFailure 与 SubagentStop hook、帧、会话与子代理记录、下一轮。',
    files: plugin('bbx-abort', {
      PostToolUseFailure: [{ matcher: 'agent', hooks: [cmd('post-failure')] }],
      SubagentStop: [{ hooks: [cmd('subagent-stop')] }],
    }),
    provider,
    steps,
    volatile: subagentTimeoutVolatile('s-ta-timeout'),
  };
}

// ── 并行 / 分组的工具批次进行中（部分已完成）──
// 一次回复三个调用：read_file（并发安全，先跑完）、bash 睡 2 秒（顺序）、write_file（顺序，排在 bash 之后）。
{
  // 中止在 bash 第一行输出（tool_progress）时发出：bash 已经在跑。若在它的 PreToolUse hook 期间中止，基线会在已中止的信号下照样把 bash 跑完（见 hook-cancelled 的已接受差异），那是另一个格子。
  const { steps, sessions } = columns('tp', { trigger: { type: 'tool_progress' }, timeoutMs: 1800, cols: ['run', 'close', 'timeout'] });
  const provider = [TITLE];
  for (const { marker } of sessions) {
    provider.push({ name: `next-${marker}`, when: [`next-${marker}:`], reply: { text: `Next ${marker}.` } });
    provider.push({ name: `${marker}-after`, when: [`${marker}:`, '"tool_call_id"'], reply: { text: `Done ${marker}.` } });
    provider.push({ name: marker, when: [`${marker}:`], reply: { tools: [
      { id: `r_${marker.replace(/-/g, '_')}`, name: 'read_file', arguments: { path: 'seed.txt' } },
      { id: `b_${marker.replace(/-/g, '_')}`, name: 'bash', arguments: { command: `echo started; sleep 2; echo late > late-${marker}.txt`, description: marker } },
      { id: `w_${marker.replace(/-/g, '_')}`, name: 'write_file', arguments: { path: `written-${marker}.txt`, content: 'w' } },
    ] } });
  }
  scenarios['abort-tool-batch'] = {
    about: '中止落在一批工具执行中（read_file 已完成、bash 在睡、write_file 还没开始）：abort_turn、close_session、timeoutMs。比较每个调用的 PreToolUse / PostToolUse / PostToolUseFailure、late-*.txt 与 written-*.txt、帧、会话记录、下一轮请求里留下的工具结果。',
    files: {
      'proj/seed.txt': 'seed\n',
      ...plugin('bbx-abort', {
        PreToolUse: [{ hooks: [cmd('pre')] }],
        PostToolUse: [{ hooks: [cmd('post')] }],
        PostToolUseFailure: [{ hooks: [cmd('post-failure')] }],
      }),
    },
    provider,
    steps,
  };
}

// ── 模型请求与流式输出中 ──
{
  const steps = [];
  const provider = [TITLE];
  const cases = [
    // 已发出、还没有响应头：假 provider 先等 3 秒再回。
    { m: 'sh-run', trigger: 'model_selection_changed', col: 'run', reply: { text: 'late headers', delayMs: 3000 } },
    { m: 'sh-close', trigger: 'model_selection_changed', col: 'close', reply: { text: 'late headers', delayMs: 3000 } },
    { m: 'sh-timeout', col: 'timeout', timeoutMs: 1500, reply: { text: 'late headers', delayMs: 3000 } },
    // 推理输出到一半。
    { m: 'sr-run', trigger: 'assistant_thinking_delta', col: 'run', reply: { reasoning: 'thinking about it', chunks: ['never ', 'shown'], hangAfter: 1, hangMs: 4000 } },
    { m: 'sr-close', trigger: 'assistant_thinking_delta', col: 'close', reply: { reasoning: 'thinking about it', chunks: ['never ', 'shown'], hangAfter: 1, hangMs: 4000 } },
    // 文本之后工具调用已收到、结束原因还没到。
    { m: 'sc-run', trigger: 'assistant_text_delta', col: 'run', reply: { text: 'let me look ', tool: { id: 'call_sc', name: 'bash', arguments: { command: 'echo never > never-sc.txt' } }, hangAfter: 2, hangMs: 4000 } },
    { m: 'sc-close', trigger: 'assistant_text_delta', col: 'close', reply: { text: 'let me look ', tool: { id: 'call_scc', name: 'bash', arguments: { command: 'echo never > never-scc.txt' } }, hangAfter: 2, hangMs: 4000 } },
    // 文本输出到一半：close_session 与超时（abort_turn 见 abort-mid-loop）。
    { m: 'st-close', trigger: 'assistant_text_delta', col: 'close', reply: { chunks: ['partial ', 'never'], hangAfter: 1, hangMs: 4000 } },
    { m: 'st-timeout', col: 'timeout', timeoutMs: 1500, reply: { chunks: ['partial ', 'never'], hangAfter: 1, hangMs: 4000 } },
  ];
  for (const c of cases) {
    const s = `s-${c.m}`;
    provider.push({ name: `next-${c.m}`, when: [`next-${c.m}:`], reply: { text: `Next ${c.m}.` } });
    provider.push({ name: c.m, when: [`${c.m}:`], reply: c.reply });
    steps.push(bypass(s));
    if (c.col === 'run') steps.push(submit(s, `${c.m}: go`, { runId: `run-${c.m}`, answer: act.abortRun(s, `run-${c.m}`, c.trigger) }));
    else if (c.col === 'close') steps.push(submit(s, `${c.m}: go`, { answer: act.close(s, c.trigger) }));
    else steps.push(submit(s, `${c.m}: go`, { timeoutMs: c.timeoutMs }));
    steps.push({ sleep: 4500 });
    steps.push(submit(s, `next-${c.m}: again`));
    // 紧跟着读：读历史带回的上一轮保留流只保留 30 秒（GatewayTurnReplayStore.ts:10、81），
    // 全部读放到最后时第一个会话的读正好落在这 30 秒的边上（基线约 27.7 秒、Rust 约 30.3 秒）。
    steps.push(read(s));
  }
  scenarios['abort-model-stream'] = {
    about: '中止落在模型请求中：已发出还没有响应头、推理输出到一半、文本之后工具调用已到但结束原因未到、文本输出到一半（abort_turn 带 runId / close_session / timeoutMs）。比较帧、部分 assistant 消息是否落盘、工具是否执行、下一轮请求带着的历史。',
    provider,
    steps,
  };
}

// ── 权限询问未答 ──
{
  // 默认权限模式（不 bypass）。插话 / 同会话再提交在询问发出时做，随后批准询问让这一轮走完。
  const steps = [];
  const sessions = [];
  const allow = (s) => ({ on: { type: 'permission_request' }, ws: 'permission_decide', params: { sessionKey: s, requestId: '{event.requestId}', decision: 'allow' }, count: 1 });
  for (const col of ['close', 'timeout', 'steer', 'busy']) {
    const s = `s-pa-${col}`;
    const marker = `pa-${col}`;
    sessions.push({ s, marker });
    if (col === 'close') steps.push(submit(s, `${marker}: go`, { answer: act.close(s, 'permission_request') }));
    else if (col === 'timeout') steps.push(submit(s, `${marker}: go`, { timeoutMs: 1500 }));
    else if (col === 'steer') steps.push(submit(s, `${marker}: go`, { runId: `run-${marker}`, answer: [...act.steer(s, `run-${marker}`, 'tool_call_started'), allow(s)] }));
    else steps.push(submit(s, `${marker}: go`, { answer: [...act.busy(s, 'tool_call_started'), allow(s)] }));
    steps.push({ sleep: 2500 });
    steps.push({ ws: 'permission_list', params: { sessionKey: s } });
    steps.push(submit(s, `next-${marker}: again`));
  }
  for (const { s } of sessions) steps.push(read(s));
  scenarios['abort-permission-pending'] = {
    about: '中止落在权限询问未答时（abort_turn 见 params-abort-reason-snapshot，断开见 robust-drop-*-permission）：close_session、timeoutMs；对照：询问期间插话、同会话再提交（之后批准）。比较询问是否被撤回（permission_list）、工具是否执行（ran-*.txt）、帧、会话记录、下一轮。',
    provider: [TITLE, ...perSession(sessions, toolReply('bash', (m) => ({ command: `echo ran > ran-${m}.txt`, description: m })), { afterId: callId })],
    steps,
  };
}

// ── hook 执行中 ──
// 每个 hook 是一段只睡 3 秒的 shell；中止由 hook_execution_started 触发。hook 是否被杀看它的
// hook_execution_completed（outcome cancelled、没有 exitCode）。这里不让 hook 记日志：被测 hook
// 刚启动就收到 SIGTERM，日志行写没写出来取决于进程启动与中止往返谁快，不是要比的东西。
function hookRow(name, event, matcher, { provider, timeoutMs = 1500, cols = ['run', 'close', 'timeout'], trigger = HOOK_STARTED, extraHooks = {} }) {
  const { steps, sessions } = columns(name, { trigger, timeoutMs, cols });
  return {
    files: plugin('bbx-abort', { [event]: [{ ...(matcher ? { matcher } : {}), hooks: [{ type: 'command', command: 'cat >/dev/null; sleep 3' }] }], ...extraHooks }),
    provider: [TITLE, ...provider(sessions)],
    steps,
  };
}
scenarios['abort-hook-user-prompt'] = {
  about: '中止落在 UserPromptSubmit hook 执行中（TurnRunner.ts:229-241）：abort_turn、close_session、timeoutMs。比较 hook 是否被杀、user_prompt_submitted 之后的走向、turn_aborted 状态、会话记录、下一轮。',
  ...hookRow('hu', 'UserPromptSubmit', null, { provider: (ss) => perSession(ss, (m) => ({ text: `answer ${m}` })) }),
};
// 与 hook-cancelled 是同一个基线 bug：被中止杀掉的 PreToolUse hook 不阻断，工具在已中止的信号上照常执行。
// 沿用 hook-cancelled 的已接受差异，每列只放过它自己的 ran-*.txt（超时列拆出去后放过 ran-hp-timeout.txt）。
const abortedHookRunsTool = (marker) => ({
  id: 'aborted-hook-does-not-run-tool',
  why: '基线把被中止杀掉的 PreToolUse hook（outcome cancelled）当作非阻断，中止这一轮之后仍然走权限并真的执行了工具（落盘里有 ran-*.txt）：已中止的信号上 abort 监听器不会再触发，bash 跑完（commandRunner.ts:96-101）。Rust 同样走权限、会话记录与基线一致，但工具见到已中止的信号不再执行。与 hook-cancelled 同一条，只放过工具执行留下的 ran-*.txt',
  paths: [`^\\$\\.tree\\./home/\\.pilotdeck/ran-${marker}\\.txt$`],
});
scenarios['abort-hook-pre-tool'] = {
  about: '中止落在 PreToolUse hook 执行中（ToolRuntime.ts:188）：close_session、timeoutMs（abort_turn 见 hook-cancelled）。比较 hook 是否被杀、工具是否仍执行（ran-*.txt）、帧、会话记录、下一轮。',
  ...hookRow('hp', 'PreToolUse', 'bash', { cols: ['close', 'timeout'], provider: (ss) => perSession(ss, toolReply('bash', (m) => ({ command: `echo ran > ran-${m}.txt`, description: m })), { afterId: callId }) }),
  accepted: [abortedHookRunsTool('hp-close'), abortedHookRunsTool('hp-timeout')],
};
scenarios['abort-hook-post-tool'] = {
  about: '中止落在 PostToolUse hook 执行中（ToolRuntime.ts:387-394）：abort_turn、close_session、timeoutMs。比较 hook 是否被杀、工具结果是否保留、帧、会话记录、下一轮。',
  ...hookRow('ho', 'PostToolUse', 'bash', { provider: (ss) => perSession(ss, toolReply('bash', (m) => ({ command: `echo ran > ran-${m}.txt`, description: m })), { afterId: callId }) }),
};
scenarios['abort-hook-post-tool-failure'] = {
  about: '中止落在 PostToolUseFailure hook 执行中（ToolRuntime.ts:418-423；bash 以非零退出码失败）：abort_turn、close_session、timeoutMs。',
  ...hookRow('hf', 'PostToolUseFailure', 'bash', { provider: (ss) => perSession(ss, toolReply('bash', (m) => ({ command: 'echo failing 1>&2; exit 7', description: m })), { afterId: callId }) }),
};
scenarios['abort-hook-stop'] = {
  about: '中止落在 Stop hook 执行中（AgentLoop.ts:1884-1929：Stop 之后没有中止检查）：abort_turn、close_session、timeoutMs。比较 finishReason、turn_failed / error 有无、帧、会话记录、下一轮。',
  ...hookRow('hs', 'Stop', null, { provider: (ss) => perSession(ss, (m) => ({ text: `answer ${m}` })) }),
};
scenarios['abort-hook-stop-failure'] = {
  about: '中止落在 StopFailure hook 执行中（模型回 500，AgentLoop.ts:1677）：abort_turn、close_session、timeoutMs。',
  ...hookRow('hz', 'StopFailure', null, { provider: (ss) => perSession(ss, () => ({ status: 500, body: { error: { message: 'boom', type: 'server_error' } } })) }),
};
{
  const sub = (ss) => {
    const rules = [];
    for (const { marker } of ss) rules.push({ name: `child-${marker}`, when: [`CHILD-${marker}`], reply: { text: 'Scope: x.\nResult: y.\nKey files: none\nFiles changed: none\nIssues: none' } });
    return [...rules, ...perSession(ss, toolReply('agent', (m) => ({ description: 'Look', prompt: `CHILD-${m}: look.`, subagent_type: 'general-purpose' })), { afterId: callId })];
  };
  scenarios['abort-hook-subagent-start'] = {
    about: '中止落在 SubagentStart hook 执行中（OneShotSubagentPort.ts:139）：abort_turn、close_session、timeoutMs。比较 subagent_started / subagent_completed、hook 是否被杀、子代理是否仍发模型请求、帧、会话与子代理记录、下一轮。',
    ...hookRow('ss', 'SubagentStart', null, { provider: sub }),
    volatile: subagentTimeoutVolatile('s-ss-timeout'),
  };
  scenarios['abort-hook-subagent-stop'] = {
    about: '中止落在 SubagentStop hook 执行中（OneShotSubagentPort.ts:206）：abort_turn、close_session、timeoutMs。比较 subagent_completed 与 hook 完成帧的先后、帧、会话与子代理记录、下一轮。',
    ...hookRow('sp', 'SubagentStop', null, { provider: sub }),
    volatile: subagentTimeoutVolatile('s-sp-timeout'),
  };
  // 断开列（GatewayWsConnection.ts:48-62，原因 aborted，同 abort_turn）：SubagentStart hook 睡 3 秒，
  // 提交后 1.2 秒以关闭帧 / RST 断开，重连后看运行记录、父会话与子代理记录、下一轮。
  const steps = [];
  const ss = [];
  for (const [col, how] of [['closeframe', true], ['reset', 'reset']]) {
    const s = `s-sd-${col}`;
    const m = `sd-${col}`;
    ss.push({ s, marker: m });
    steps.push(bypass(s));
    steps.push(submit(s, `${m}: go`, { runId: `run-${m}`, timeout: 1200, expectTimeout: true }));
    steps.push({ disconnect: how });
    steps.push({ sleep: 3500 });
    steps.push({ connect: true });
    steps.push({ ws: 'run_get', params: { runId: `run-${m}` } });
    steps.push(read(s));
    steps.push(submit(s, `next-${m}: again`));
  }
  scenarios['abort-hook-subagent-start-disconnect'] = {
    about: '中止来源是客户端断开（GatewayWsConnection.ts:48-62：断开时对本连接在途的会话 abortTurn，原因 aborted）：SubagentStart hook 执行中先后以关闭帧、RST 断开，重连后看运行记录、父会话与子代理记录（子代理在已中止的信号上照样写出自己的会话文件）、下一轮。',
    files: plugin('bbx-abort', { SubagentStart: [{ hooks: [{ type: 'command', command: 'cat >/dev/null; sleep 3' }] }] }),
    provider: [TITLE, ...sub(ss)],
    steps,
  };
}

// ── 被杀的 advisory hook 晚退出（PreModelRequest，收到 SIGTERM 后再活 N 毫秒）──
// hook 是一段 shell：一启动就装好 TERM 的 trap（node 装处理器太慢，中止会先到），
// 收到 TERM 后再睡 late 毫秒、记一行、以 143 退出；没被杀则 5 秒后记 done。
for (const late of [50, 300, 900, 2000]) {
  const s = `s-al-${late}`;
  const m = `al-${late}`;
  const log = `"$HOME/hooklog/${m}.log"`;
  const command = `trap 'sleep ${late / 1000}; echo term-exit >> ${log}; exit 143' TERM; mkdir -p "$HOME/hooklog"; echo start >> ${log}; cat >/dev/null; sleep 5 & wait; echo done >> ${log}`;
  scenarios[`abort-hook-advisory-late-${late}`] = {
    about: `PreModelRequest hook（不等待的 advisory hook，AgentLoop.ts:699-702）执行中（模型已开始流式输出）abort_turn，hook 收到 SIGTERM 后再活 ${late} 毫秒才退出（commandRunner.ts:96-101 先 TERM、3 秒后 KILL，结果等子进程 close）。模型请求挂着。比较 hook 完成帧是否上流、与 turn_completed 的先后、hook 日志、会话记录。`,
    files: {
      [`home/.pilotdeck/plugins/bbx-abort/plugin.json`]: { name: 'bbx-abort', version: '0.1.0', description: 'bbx bbx-abort', hooks: { PreModelRequest: [{ hooks: [{ type: 'command', command }] }] } },
    },
    // 中止在第一块文本到达时发出（模型先等 300 毫秒），保证 hook 的 trap 已经装好。
    provider: [TITLE, { name: m, when: [`${m}:`], reply: { chunks: ['partial ', 'never'], delayMs: 300, hangAfter: 1, hangMs: 6000 } }],
    steps: [
      bypass(s),
      submit(s, `${m}: go`, { answer: act.abort(s, 'assistant_text_delta') }),
      { sleep: 3500 },
      read(s),
    ],
  };
}

// ── 轮次收尾：标题生成还没回 ──
{
  const { steps, sessions } = columns('fn', { trigger: { type: 'assistant_block' }, cols: ['run', 'close'] });
  scenarios['abort-turn-finishing'] = {
    about: '中止落在轮次收尾：回复已完整、标题生成请求挂着（假 provider 3 秒后才回标题）。abort_turn、close_session（超时列不适用：标题生成不在轮次内，轮次先于任何超时结束）。比较 turn_completed、会话标题记录、下一轮。',
    provider: [{ ...TITLE, reply: { ...TITLE.reply, delayMs: 3000 } }, ...perSession(sessions, (m) => ({ text: `answer ${m}` }))],
    steps,
  };
}

// 超时列单独成场景（abort-timeout-*）：超时之后基线不等这一轮收尾，被中止的循环在后台继续跑完
// （InProcessGateway.ts:1386-1433、1587、1824-1836），它的副作用与时序和其他列不同，分开才看得清各列各自是否一致。
// 按会话把步骤分开：一个步骤属于它 params.sessionKey 的会话，sleep 属于前一个会话。
// 超时之后，被中止的循环的即时反应（被杀 hook 的完成、被杀工具的失败 hook 开始）与网关写超时事件
// 进运行记录谁先谁后，基线取决于两件异步的事谁先完成：InProcessGateway.ts:1405 等超时状态写进会话记录
// （JsonlTranscriptWriter.ts:304-308 排在写入队列之后，落在 libuv 线程池上），之后 1414/1426 才把
// agent_status / error 追加进运行记录；另一边是被 SIGTERM 的子进程 close 之后 hook 的完成经
// GatewayTurnEventCoordinator.emit（GatewayTurnEventCoordinator.ts:38-50）追加进运行记录。
// 基线各自连跑 20 次的分布写在 note 里；只放过该会话这几条运行记录事件和运行记录文件的排版骨架。
const TIMEOUT_ORDER = {
  'abort-timeout-hook-post-tool-failure': { session: 's-hf-timeout', from: 7, to: 9, note: '基线 20 次：PostToolUseFailure 的完成在 turn_timeout 之前 19 次、在 error 之后 1 次。' },
  'abort-timeout-hook-pre-tool': { session: 's-hp-timeout', from: 7, to: 9, note: '基线 20 次：PreToolUse 的完成在 turn_timeout 之前 19 次、在 error 之后 1 次。' },
  'abort-timeout-hook-subagent-start': { session: 's-ss-timeout', from: 7, to: 9, note: '基线 20 次：SubagentStart 的完成在 turn_timeout 之前 19 次、在 error 之后 1 次。' },
  'abort-timeout-hook-stop': { session: 's-hs-timeout', from: 8, to: 10, note: '基线 20 次：Stop 的完成在 turn_timeout 之前 11 次、在 error 之后 9 次。' },
  'abort-timeout-hook-subagent-stop': { session: 's-sp-timeout', from: 10, to: 12, note: '基线 20 次：SubagentStop 的完成在 turn_timeout 之前 11 次、在 error 之后 9 次。' },
  'abort-timeout-tool-bash': { session: 's-tb-timeout', from: 6, to: 8, note: '基线 20 次：bash 被杀后 PostToolUseFailure 的开始在 turn_timeout 之前 19 次、在 error 之后 1 次。' },
  'abort-timeout-tool-batch': { session: 's-tp-timeout', from: 10, to: 12, note: '基线 20 次：bash 被杀后 PostToolUseFailure 的开始在 error 之后 11 次、在 turn_timeout 之前 9 次。' },
};
const timeoutOrderVolatile = ({ session, from, to }) => {
  const indices = Array.from({ length: to - from + 1 }, (_, i) => from + i).join('|');
  return [
    // 运行记录事件的键是「\\0会话\\0runId」（RunRegistry 的 eventKey），差异路径里照原样带着 NUL。
    `^\\$\\.tree\\./home/\\.pilotdeck/runs/registry\\.json\\.json\\.events\\.\\u0000${session}\\u0000<uuid:\\d+>\\[(${indices})\\]\\.event\\.`,
    '^\\$\\.tree\\./home/\\.pilotdeck/runs/registry\\.json\\.raw\\.skeletons\\.',
  ];
};

function splitTimeout(name, body) {
  const isTimeout = (key) => /-timeout$/.test(key ?? '');
  const keep = [];
  const moved = [];
  let current;
  for (const step of body.steps) {
    const key = step.params?.sessionKey;
    if (key) current = key;
    (isTimeout(key ?? current) ? moved : keep).push(step);
  }
  if (moved.length === 0) return { [name]: body };
  const volatile = body.volatile ?? [];
  const pick = (want) => volatile.filter((v) => /-timeout/.test(v) === want);
  // accepted 按路径归列：路径里带 -timeout 的跟超时列走。
  const accepted = body.accepted ?? [];
  const pickAccepted = (want) => accepted.filter((a) => a.paths.every((p) => /-timeout/.test(p) === want));
  const base = { ...body, steps: keep, ...(pick(false).length ? { volatile: pick(false) } : {}), ...(pickAccepted(false).length ? { accepted: pickAccepted(false) } : {}) };
  if (!pick(false).length) delete base.volatile;
  if (!pickAccepted(false).length) delete base.accepted;
  const timeout = {
    ...body,
    about: `超时列（submit_turn.timeoutMs，从 ${name} 拆出）：${body.about}`,
    steps: moved,
    ...(pick(true).length ? { volatile: pick(true) } : {}),
    ...(pickAccepted(true).length ? { accepted: pickAccepted(true) } : {}),
  };
  if (!pick(true).length) delete timeout.volatile;
  if (!pickAccepted(true).length) delete timeout.accepted;
  return { [name]: base, [name.replace(/^abort-/, 'abort-timeout-')]: timeout };
}

for (const [original, whole] of Object.entries(scenarios)) {
  for (const [name, body] of Object.entries(splitTimeout(original, whole))) {
    const order = TIMEOUT_ORDER[name];
    if (order) {
      body.volatile = [...(body.volatile ?? []), ...timeoutOrderVolatile(order)];
      body.about = `${body.about}\n运行记录里超时事件的先后（volatile 的依据）：${order.note}`;
    }
    fs.writeFileSync(path.join(here, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
    console.log('wrote', name);
  }
}
