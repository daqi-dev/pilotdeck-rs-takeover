// 生成 hook-*.json 场景。用法（仓库根目录）：node scripts/bbx/scenarios/hooks.gen.mjs
// 改场景改这里，再重新生成；生成的 JSON 一并提交。
//
// 所有 command hook 都跑同一个小脚本 hook.mjs（放在插件目录里，插件 hook 的工作目录就是插件根）：
//   node hook.mjs <日志名> <退出码> [stdout] [stderr] [延迟毫秒]
// 它把收到的 stdin（解析后）、工作目录、环境变量（PILOT* 记值，其余只记名字）追加到
// {HOME}/hooklog/<日志名>.jsonl，延迟后写 stdout / stderr 并以给定退出码退出；
// 延迟结束时再追加一行 {done:true}，被超时杀掉的 hook 没有这一行。落盘比较因此能看到
// 「hook 收到了什么、以什么顺序被调用、有没有被杀掉」。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);

const HOOK_SCRIPT = `// bbx hook fixture: node hook.mjs <log> <exit> [stdout] [stderr] [sleepMs]
import fs from 'node:fs';
const [log, exit = '0', out = '', err = '', sleep = '0'] = process.argv.slice(2);
const dir = process.env.HOME + '/hooklog';
const append = (row) => { fs.mkdirSync(dir, { recursive: true }); fs.appendFileSync(dir + '/' + log + '.jsonl', JSON.stringify(row) + '\\n'); };
let input = '';
process.stdin.on('data', (d) => { input += d; });
process.stdin.on('end', () => {
  let parsed; try { parsed = JSON.parse(input); } catch { parsed = { raw: input }; }
  const env = {};
  for (const k of Object.keys(process.env).sort()) env[k] = /^PILOT/.test(k) && !/BUNDLED/.test(k) ? process.env[k] : true;
  append({ argv: process.argv.slice(2), cwd: process.cwd(), input: parsed, env });
  // exit "@": the behaviour is the JSON between HOOK<< and >> in the prompt (or the last assistant message).
  let spec = { exit, out, err, sleep };
  if (exit === '@') {
    const m = /HOOK<<(.*?)>>/s.exec(String(parsed.prompt ?? parsed.lastAssistantMessage ?? ''));
    spec = { exit: 0, out: '', err: '', sleep: 0, ...(m ? JSON.parse(m[1]) : {}) };
  }
  setTimeout(() => {
    append({ argv: process.argv.slice(2), done: true });
    if (spec.out) process.stdout.write(spec.out);
    if (spec.err) process.stderr.write(spec.err);
    process.exit(Number(spec.exit));
  }, Number(spec.sleep));
});
`;

const q = (s) => `'${s.replace(/'/g, `'\\''`)}'`;
// 一个 command hook：node hook.mjs <log> <exit> [stdout] [stderr] [sleep]
const cmd = (log, { exit = 0, out = '', err = '', sleep = 0, tag, ...rest } = {}) => ({
  type: 'command',
  command: ['node hook.mjs', log, String(exit), q(out), q(err), String(sleep), ...(tag ? [tag] : [])].join(' '),
  ...rest,
});
const json = (value) => JSON.stringify(value) + '\n';

const plugin = (name, hooks, where = 'home/.pilotdeck/plugins') => ({
  [`${where}/${name}/plugin.json`]: { name, version: '0.1.0', description: `bbx ${name}`, hooks },
  [`${where}/${name}/hook.mjs`]: HOOK_SCRIPT,
});

// 收到 SIGTERM 后 300ms 才退出，其余行为与 hook.mjs 相同（参数照传）。
const TERM_LATE = `// bbx hook fixture: hook.mjs that outlives SIGTERM by 300ms
process.on('SIGTERM', () => { setTimeout(() => process.exit(143), 300); });
await import('./hook.mjs');
`;
const TITLE = { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Hook run"}' } };
// 每个用例一个会话：消息里带标记，模型先发一次工具调用，拿到结果后回一句话。
const toolCase = (marker, tool, args, id = `call_${marker.replace(/-/g, '_')}`) => [
  { name: `${marker}-after`, when: [`${marker}:`, `"tool_call_id":"${id}"`], reply: { text: `done ${marker}` } },
  { name: marker, when: [`${marker}:`], reply: { tool: { id, name: tool, arguments: args } } },
];
const bypass = (session) => ({ ws: 'set_permission_mode', params: { sessionKey: session, mode: 'bypassPermissions' } });
const submit = (session, message, extra = {}) => ({
  ws: 'submit_turn', params: { sessionKey: session, channelKey: 'test', message, ...(extra.params ?? {}) },
  timeout: extra.timeout ?? 20000, ...(extra.answer ? { answer: extra.answer } : {}),
});
const read = (session) => ({ ws: 'read_session_messages', params: { sessionKey: session } });

const scenarios = {};

// ── PreToolUse ──
{
  const cases = ['order', 'block', 'deny-reason', 'deny-bare', 'json-block', 'stop', 'rewrite-ok', 'rewrite-bad', 'garbage', 'fail', 'nomatch'];
  const when = (c) => `bash(case-${c})`;
  const hooks = {
    PreToolUse: [
      // 顺序：同一个 matcher 里两条，再一条正则 matcher、一条多选 matcher；write_file 那条不该跑。
      { matcher: 'bash', hooks: [cmd('pre', { if: when('order'), tag: 'first' }), cmd('pre', { if: when('order'), tag: 'second' })] },
      { matcher: 'write_file', hooks: [cmd('pre', { if: when('order'), tag: 'write-file-never' })] },
      { matcher: '/^ba/', hooks: [cmd('pre', { if: when('order'), tag: 'regex' })] },
      { matcher: 'edit_file|bash', hooks: [cmd('pre', { if: when('order'), tag: 'alternatives' })] },
      { matcher: 'bash', hooks: [
        cmd('pre', { if: when('block'), exit: 2, err: 'blocked by hook\n' }),
        cmd('pre', { if: when('deny-reason'), out: json({ hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: 'denied by hook' } }) }),
        cmd('pre', { if: when('deny-bare'), out: json({ hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny' } }) }),
        cmd('pre', { if: when('json-block'), out: json({ decision: 'block', reason: 'json block reason' }) }),
        cmd('pre', { if: when('stop'), out: json({ continue: false, stopReason: 'stop reason only' }) }),
        cmd('pre', { if: when('rewrite-ok'), out: json({ hookSpecificOutput: { hookEventName: 'PreToolUse', updatedInput: { command: 'printf rewritten > rewritten.txt', description: 'rewritten by hook' } } }) }),
        cmd('pre', { if: when('rewrite-bad'), out: json({ hookSpecificOutput: { hookEventName: 'PreToolUse', updatedInput: { cmd: 1 } } }) }),
        cmd('pre', { if: when('garbage'), out: 'not json\n{broken\n' }),
        cmd('pre', { if: when('fail'), exit: 1, out: 'partial', err: 'boom\n' }),
      ] },
      // matcher 不匹配：只对 read_file 生效。
      { matcher: 'read_file', hooks: [cmd('pre', { if: 'read_file(case-nomatch)' })] },
    ],
  };
  const provider = [TITLE];
  const steps = [];
  for (const c of cases) {
    provider.push(...toolCase(`case-${c}`, 'bash', { command: `printf case-${c} > out-${c}.txt`, description: `case ${c}` }));
    steps.push(bypass(`s-${c}`), submit(`s-${c}`, `case-${c}: run the tool`), { sleep: 300 });
  }
  steps.push(read('s-rewrite-ok'));
  scenarios['hook-pre-tool-use'] = {
    about: '插件的 PreToolUse hook（bash，bypassPermissions）：多条 hook 的执行顺序（同 matcher 两条、正则 matcher、多选 matcher；write_file 的 matcher 不跑）、退出码 2 阻止、JSON permissionDecision deny（带 / 不带理由）、decision:block、continue:false、updatedInput 改写（改写后的命令真的执行）与改写成非法输入、stdout 非法、退出码 1、matcher 不匹配。hook 收到的 stdin、工作目录、环境变量落盘在 home/hooklog。',
    files: plugin('bbx-pre', hooks),
    provider,
    steps,
  };
}

// ── PostToolUse / PostToolUseFailure ──
{
  const hooks = {
    PostToolUse: [
      { matcher: 'bash', hooks: [
        cmd('post', { if: 'bash(case-pass)' }),
        cmd('post', { if: 'bash(case-context)', out: json({ hookSpecificOutput: { hookEventName: 'PostToolUse', additionalContext: 'context from post hook' } }) }),
        cmd('post', { if: 'bash(case-block)', exit: 2, err: 'post hook blocked\n' }),
        cmd('post', { if: 'bash(case-json-block)', out: json({ decision: 'block', reason: 'post json block' }) }),
        cmd('post', { if: 'bash(case-garbage)', out: '{"unterminated' }),
        cmd('post', { if: 'bash(case-fail)', exit: 3, err: 'post failed\n' }),
      ] },
      { matcher: 'write_file', hooks: [cmd('post', { if: 'bash(case-pass)' })] },
    ],
    PostToolUseFailure: [
      { matcher: 'bash', hooks: [cmd('post-failure', {}), cmd('post-failure', { out: json({ decision: 'block', reason: 'ignored' }) })] },
    ],
  };
  const cases = ['pass', 'context', 'block', 'json-block', 'garbage', 'fail'];
  const provider = [TITLE];
  const steps = [];
  for (const c of cases) {
    provider.push(...toolCase(`case-${c}`, 'bash', { command: `printf case-${c}`, description: `case ${c}` }));
    steps.push(bypass(`s-${c}`), submit(`s-${c}`, `case-${c}: run the tool`), { sleep: 300 });
  }
  provider.push(...toolCase('case-error', 'bash', { command: 'echo failing 1>&2; exit 7', description: 'case error' }));
  steps.push(bypass('s-error'), submit('s-error', 'case-error: run the tool'), { sleep: 300 });
  steps.push(read('s-context'), read('s-block'));
  scenarios['hook-post-tool-use'] = {
    about: '插件的 PostToolUse / PostToolUseFailure hook：放行、additionalContext（进结果 metadata.lifecycle，再到下一次模型请求）、退出码 2 与 decision:block（工具结果带 lifecycle.blocked，循环以 lifecycle_blocked 结束这一轮）、stdout 非法、非零退出；bash 失败时 PostToolUseFailure 收到 error / isInterrupt。',
    files: plugin('bbx-post', hooks),
    provider,
    steps,
  };
}

// ── UserPromptSubmit ──
{
  // 第一条 hook 的行为由提示词里的 HOOK<<…>> 决定；第二条总是放行（看阻止之后后面的 hook 还跑不跑）；
  // matcher 与事件名相同的执行，别的 matcher 不执行；`if` 条件在没有工具的事件上：工具名缺省不比，
  // toolInput 缺省按 {} 序列化，所以 X(nothing) 不成立、X 成立。
  const hooks = {
    UserPromptSubmit: [
      { hooks: [cmd('prompt', { exit: '@' })] },
      { matcher: 'UserPromptSubmit', hooks: [cmd('prompt-second', {}), cmd('prompt-if-no', { if: 'X(nothing)' }), cmd('prompt-if-yes', { if: 'X' })] },
      { matcher: 'Other', hooks: [cmd('prompt-nomatch', {})] },
    ],
  };
  const directive = (spec) => `HOOK<<${JSON.stringify(spec)}>>`;
  const cases = {
    pass: {},
    context: { out: json({ hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext: 'extra context <&> "q"\nline two' } }) },
    'context-system': { out: json({ systemMessage: 'system note', hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext: 'second context' } }) },
    block: { exit: 2, err: 'prompt blocked\n' },
    'json-block': { out: json({ decision: 'block', reason: 'json said no' }) },
    fail: { exit: 1, err: 'non blocking\n', out: 'garbage {' },
    garbage: { out: '[1,2]\n{"a":' },
  };
  const steps = [];
  for (const [c, spec] of Object.entries(cases)) steps.push(submit(`s-${c}`, `case-${c}: ${directive(spec)}`), { sleep: 300 });
  steps.push(read('s-block'), read('s-context'));
  scenarios['hook-user-prompt-submit'] = {
    about: '插件的 UserPromptSubmit hook：放行、additionalContext（含要转义的字符与换行）作为 hook_context 消息进模型请求、systemMessage、退出码 2 与 decision:block 阻止本轮（turn_failed，之后的 hook 照跑）、非零退出与非法输出不影响；matcher 与事件名相同的执行、别的不执行；`if` 条件在没有工具的事件上。',
    files: plugin('bbx-prompt', hooks),
    provider: [TITLE, { name: 'main', reply: { text: 'answer' } }],
    steps,
  };
}

// ── PermissionRequest / PermissionDenied / FileChanged ──
{
  const decision = (value) => json({ hookSpecificOutput: { hookEventName: 'PermissionRequest', decision: value } });
  const hooks = {
    PermissionRequest: [{ matcher: 'write_file', hooks: [
      cmd('perm', { if: 'write_file(perm-allow)', out: decision({ behavior: 'allow' }) }),
      cmd('perm', { if: 'write_file(perm-deny)', out: decision({ behavior: 'deny', message: 'hook says no' }) }),
      cmd('perm', { if: 'write_file(perm-bare)', out: decision({ behavior: 'deny' }) }),
      cmd('perm', { if: 'write_file(perm-update)', out: decision({ behavior: 'allow', updatedInput: { file_path: 'updated-by-hook.txt', content: 'updated by hook\n' } }) }),
      cmd('perm', { if: 'write_file(perm-pass)' }),
      cmd('perm', { if: 'write_file(perm-exit2)', exit: 2, err: 'blocking is not a decision\n' }),
      cmd('perm', { if: 'write_file(perm-twice)', out: decision({ behavior: 'deny', message: 'first decision wins' }) }),
      cmd('perm', { if: 'write_file(perm-twice)', out: decision({ behavior: 'allow' }) }),
    ] }],
    PermissionDenied: [{ hooks: [cmd('denied')] }],
    FileChanged: [
      { matcher: '/perm-[a-z0-9]+\\.txt$/', hooks: [cmd('changed')] },
      { matcher: '/updated-by-hook/', hooks: [cmd('changed', { exit: 1, err: 'changed hook failed\n' }), cmd('changed', { exit: 2, err: 'changed hook blocked\n' })] },
    ],
  };
  // 采集器对权限询问的回答：hook 的决定在前，网关回调在后，取第一个决定。
  const cases = { allow: 'deny', deny: 'allow', bare: 'allow', update: 'deny', pass: 'deny', exit2: 'allow', twice: 'allow' };
  const provider = [TITLE];
  const steps = [];
  for (const [c, answer] of Object.entries(cases)) {
    provider.push(...toolCase(`perm-${c}`, 'write_file', { file_path: `perm-${c}.txt`, content: `perm-${c}\n` }));
    steps.push(submit(`s-${c}`, `perm-${c}: write the file`, { answer: [{
      on: { type: 'permission_request' }, ws: 'permission_decide',
      params: { sessionKey: `s-${c}`, requestId: '{event.requestId}', decision: answer }, count: 1,
    }] }), { sleep: 300 });
  }
  steps.push(read('s-update'));
  scenarios['hook-permission-request'] = {
    about: '插件的 PermissionRequest / PermissionDenied / FileChanged hook（默认权限模式，write_file 需要询问）：配置的 hook 先于网关的权限回调执行，第一个 decision 生效——hook allow 而用户拒绝时照样写、hook deny（带 / 不带 message）而用户批准时照样拒、allow 带 updatedInput 时写的是改过的文件；hook 不表态时用户的回答生效；退出码 2 不是决定；两条 hook 先 deny 后 allow 取 deny。被拒时 PermissionDenied 收到 reason。写成功后 FileChanged 按绝对路径匹配，失败与阻止只变成 warning 事件，不影响工具结果。',
    files: plugin('bbx-perm', hooks),
    provider,
    steps,
  };
}

// ── SessionStart / Setup / Stop / StopFailure / SessionEnd ──
{
  const hooks = {
    SessionStart: [{ hooks: [cmd('session-start', { out: json({ hookSpecificOutput: { hookEventName: 'SessionStart', additionalContext: 'ignored by the session' } }) })] }],
    Setup: [{ hooks: [cmd('setup', { exit: 2, err: 'setup blocking is ignored\n' })] }],
    Stop: [{ hooks: [cmd('stop', { exit: '@' }), cmd('stop-second', {})] }],
    StopFailure: [{ hooks: [cmd('stop-failure', {})] }],
    SessionEnd: [{ matcher: 'SessionEnd', hooks: [cmd('session-end', { exit: '@' })] }, { matcher: 'Other', hooks: [cmd('session-end-nomatch')] }],
  };
  // Stop 与 SessionEnd 的第一条 hook 按助手最后一句话里的 HOOK<<…>> 行事（SessionEnd 收不到这句话，
  // 走默认放行；超时用专门的会话：Stop 回 {"sleep":…} 没用，所以 SessionEnd 的超时靠 session-end-slow）。
  hooks.SessionEnd.push({ matcher: 'SessionEnd', hooks: [cmd('session-end-slow', { sleep: 4000 })] });
  const directive = (spec) => `HOOK<<${JSON.stringify(spec)}>>`;
  const replies = {
    pass: 'all good',
    'stop-block': `blocked ${directive({ exit: 2, err: 'stop hook blocked\n' })}`,
    'stop-json': `json ${directive({ out: json({ decision: 'block', reason: 'stop json block' }) })}`,
    'stop-context': `context ${directive({ out: json({ hookSpecificOutput: { hookEventName: 'Stop', additionalContext: 'stop context' } }) })}`,
    'stop-fail': `fail ${directive({ exit: 1, err: 'stop failed\n' })}`,
  };
  const provider = [TITLE, { name: 'model-error', when: ['case-model-error:'], reply: { status: 400, body: { error: { message: 'bad request from mock', type: 'invalid_request_error' } } } }];
  const steps = [];
  for (const [c, text] of Object.entries(replies)) {
    provider.push({ name: `case-${c}`, when: [`case-${c}:`], reply: { text } });
    steps.push(submit(`s-${c}`, `case-${c}: say something`), { sleep: 300 });
  }
  steps.push(submit('s-model-error', 'case-model-error: fail please'), { sleep: 4500 });
  steps.push(read('s-stop-block'));
  scenarios['hook-session-lifecycle'] = {
    about: '插件的 SessionStart / Setup / Stop / StopFailure / SessionEnd hook：每次提交都跑 SessionStart、Setup、SessionEnd（gateway 每轮一个会话周期），它们的效果与退出码不影响本轮；Stop 收到最后一句助手消息，退出码 2 与 decision:block 让本轮以 lifecycle_blocked（stage stop）失败，additionalContext、非零退出不影响；模型请求失败时 StopFailure 收到 error；SessionEnd 的 hook 1.5 秒超时被杀（hooklog 里没有 done 行），matcher 与事件名不同的不跑。',
    files: plugin('bbx-life', hooks),
    provider,
    steps,
    settleMs: 1000,
  };
}

// ── SDK 会话配置的 HTTP hook ──
{
  // hook 请求发到假 provider 的 /hooks（按请求体匹配规则；只有 hook 请求体里有 transcriptPath）。
  const hookReply = (name, when, body, status = 200) => ({ name, when: ['"transcriptPath"', ...when], reply: { status, body } });
  const sdkHooks = (events) => ({
    hooks: { url: '{MOCK}/hooks?keep=1&event=stale', headers: { 'x-bbx-hook': 'header-value', Authorization: 'Bearer sdk' }, events },
  });
  const provider = [
    TITLE,
    hookReply('hook-deny', ['"hookEventName":"PreToolUse"', 'sdk-deny'], { hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: 'sdk hook denied' } }),
    hookReply('hook-500', ['"hookEventName":"PostToolUse"', 'sdk-plain'], { error: 'down' }, 500),
    hookReply('hook-context', ['"hookEventName":"UserPromptSubmit"', 'sdk-plain'], { hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext: 'context over http' } }),
    hookReply('hook-async-pre', ['"hookEventName":"PreToolUse"', 'sdk-async'], { async: true, asyncHookId: 'inv-pre', asyncTimeout: 60 }),
    hookReply('hook-async-prompt', ['"hookEventName":"UserPromptSubmit"', 'sdk-async'], { async: true, asyncHookId: 'inv-prompt', asyncTimeout: 1 }),
    hookReply('hook-async-stop', ['"hookEventName":"Stop"', 'done sdk-async'], { async: true, asyncHookId: 'inv-stop', asyncTimeout: 60 }),
    hookReply('hook-async-noid', ['"hookEventName":"PostToolUse"', 'sdk-async'], { async: true }),
    hookReply('hook-other', [], {}),
    { name: 'sdk-second', when: ['sdk-second:'], reply: { text: 'second answer' } },
    ...toolCase('sdk-plain', 'write_file', { file_path: 'sdk-plain.txt', content: 'sdk-plain\n' }),
    ...toolCase('sdk-deny', 'bash', { command: 'printf sdk-deny > denied.txt', description: 'deny' }),
    ...toolCase('sdk-async', 'bash', { command: 'sleep 2; printf sdk-async', description: 'async' }),
  ];
  const events = {
    UserPromptSubmit: [{}],
    PreToolUse: [{ matcher: 'write_file' }, { matcher: 'bash', timeout: 5 }],
    PostToolUse: [{ matcher: '' }],
    Stop: [{}],
  };
  const asyncAnswer = (session) => [
    // PreToolUse 声明 async 之后、bash 还在 sleep 时交结果：delivered，下一次模型请求带上这段上下文。
    { on: { type: 'hook_response', hookEvent: 'PreToolUse' }, ws: 'hook_async_result', count: 1,
      params: { sessionKey: session, invocationId: '{event.asyncInvocationId}', output: { hookSpecificOutput: { hookEventName: 'PreToolUse', additionalContext: 'late context from async hook' } } } },
    // 同一个 invocation 再交一次：duplicate。
    { on: { type: 'hook_started', hookEvent: 'PostToolUse' }, ws: 'hook_async_result', count: 1,
      params: { sessionKey: session, invocationId: 'inv-pre', output: { additionalContext: 'again' } } },
    // UserPromptSubmit 的 1 秒超时到了（includeHookEvents 时有 hook_async_result expired 事件）：再交是 expired。
    { on: { type: 'hook_async_result', status: 'expired' }, ws: 'hook_async_result', count: 1,
      params: { sessionKey: session, invocationId: '{event.invocationId}', output: { additionalContext: 'too late' } } },
  ];
  scenarios['hook-sdk-http'] = {
    about: 'sdkSessionConfig.hooks（HTTP hook）：URL 上的 event / matcher 序号覆盖原有同名参数、请求头与请求体、matcher 选择；includeHookEvents 时投影为 hook_started / hook_response（带 stdout / 退出码 / async 字段），否则为 agent_status；HTTP 500 是 non_blocking_error；HTTP 回 deny 阻止工具；additionalContext 进模型请求。异步：返回 async + asyncHookId 的登记到网关，hook_async_result 在轮次进行中交结果 → delivered（作为 steer 进下一次模型请求）、重复 → duplicate、超时 → expired（事件与结果）、轮次结束后才交（Stop 登记的）→ expired、别的会话交 → unknown、未知 id → unknown；不带 asyncHookId 的只挂在会话自己的运行时里，下一轮不受影响。',
    provider,
    steps: [
      bypass('s-plain'),
      submit('s-plain', 'sdk-plain: run', { params: { sdkSessionConfig: { ...sdkHooks(events) } } }), { sleep: 300 },
      bypass('s-deny'),
      submit('s-deny', 'sdk-deny: run', { params: { sdkSessionConfig: { ...sdkHooks(events), includeHookEvents: true } } }), { sleep: 300 },
      bypass('s-async'),
      submit('s-async', 'sdk-async: run', { params: { sdkSessionConfig: { ...sdkHooks(events), includeHookEvents: true } }, answer: asyncAnswer('s-async'), timeout: 30000 }),
      { sleep: 500 },
      { ws: 'hook_async_result', params: { sessionKey: 's-async', invocationId: 'inv-stop', output: {} } },
      { ws: 'hook_async_result', params: { sessionKey: 's-async', invocationId: 'inv-pre' } },
      { ws: 'hook_async_result', params: { sessionKey: 's-other', invocationId: 'inv-pre' } },
      { ws: 'hook_async_result', params: { sessionKey: 's-async', invocationId: 'inv-never' } },
      // 同一会话的下一轮：上一轮 PostToolUse 留下的不带 id 的 async（只在会话自己的运行时里挂起）不影响这一轮。
      submit('s-async', 'sdk-second: hi', { params: { sdkSessionConfig: { ...sdkHooks(events), includeHookEvents: true } } }), { sleep: 300 },
      read('s-async'),
    ],
  };
}

// ── 异步 hook 的 invocation id 冲突 ──
{
  const hooks = { url: '{MOCK}/hooks', events: { Stop: [{}] } };
  scenarios['hook-sdk-async-conflict'] = {
    about: 'SDK HTTP hook 的 Stop 返回 async + 同一个 asyncHookId：第一个会话登记后一直挂起（轮次结束不会让它过期），第二个会话的 Stop 再登记同一个 id 时网关拒绝（ASYNC_HOOK_INVOCATION_CONFLICT），这次 lifecycle 派发失败，本轮以 agent_invalid_state 失败，消息是冲突的原文。',
    provider: [
      TITLE,
      { name: 'hook-stop', when: ['"transcriptPath"', '"hookEventName":"Stop"'], reply: { status: 200, body: { async: true, asyncHookId: 'inv-shared', asyncTimeout: 60 } } },
      { name: 'main', reply: { text: 'answer' } },
    ],
    steps: [
      submit('s-first', 'first: hi', { params: { sdkSessionConfig: { hooks } } }), { sleep: 300 },
      submit('s-second', 'second: hi', { params: { sdkSessionConfig: { hooks } } }), { sleep: 300 },
      { ws: 'hook_async_result', params: { sessionKey: 's-first', invocationId: 'inv-shared', output: {} } },
    ],
  };
}

// ── 来源：配置文件不是 hook 来源；插件的 hooks/hooks.json、自定义路径；其他 hook 类型 ──
{
  const config = `schemaVersion: 1
agent:
  model: bbx/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    bbx:
      protocol: openai
      url: {MOCK}/v1
      apiKey: bbx-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
hooks:
  UserPromptSubmit:
    - hooks:
        - type: command
          command: "mkdir -p $HOME/hooklog && echo ran >> $HOME/hooklog/config-file.log"
`;
  const promptHook = (log) => ({ UserPromptSubmit: [{ hooks: [cmd(log)] }] });
  scenarios['hook-sources'] = {
    about: '`pilotdeck.yaml` 顶层的 hooks 段不是 hook 来源（基线报未知字段，照常启动，hook 不跑）；插件 hooks 的三种写法：plugin.json 里的对象、缺省的 hooks/hooks.json、plugin.json 里写的自定义路径；按插件目录名顺序合并。不支持的事件、callback 类型被丢弃；prompt / agent 类型没有执行器，结果是 non_blocking_error；插件里的 http hook 发到给定地址。',
    config,
    files: {
      // 目录名决定加载顺序：a-inline, b-default-file, c-custom-path, d-kinds。
      ...plugin('a-inline', promptHook('inline')),
      'home/.pilotdeck/plugins/b-default-file/plugin.json': { name: 'b-default-file', version: '0.1.0', description: 'bbx' },
      'home/.pilotdeck/plugins/b-default-file/hooks/hooks.json': promptHook('default-file'),
      'home/.pilotdeck/plugins/b-default-file/hook.mjs': HOOK_SCRIPT,
      'home/.pilotdeck/plugins/c-custom-path/plugin.json': { name: 'c-custom-path', version: '0.1.0', description: 'bbx', hooks: 'conf/my-hooks.json' },
      'home/.pilotdeck/plugins/c-custom-path/conf/my-hooks.json': promptHook('custom-path'),
      'home/.pilotdeck/plugins/c-custom-path/hook.mjs': HOOK_SCRIPT,
      ...plugin('d-kinds', {
        NotAnEvent: [{ hooks: [cmd('never')] }],
        UserPromptSubmit: [{ hooks: [
          { type: 'callback', name: 'x' },
          { type: 'prompt', prompt: 'judge $ARGUMENTS' },
          { type: 'agent', prompt: 'agent $ARGUMENTS' },
          { type: 'http', url: '{MOCK}/plugin-hook', headers: { 'x-env': 'pre-$HOME-post', 'x-allowed': '${PILOT_HOME}' }, allowedEnvVars: ['PILOT_HOME'] },
          { type: 'mystery' },
          cmd('kinds-last'),
        ] }],
      }),
    },
    provider: [
      TITLE,
      { name: 'plugin-http-hook', when: ['"transcriptPath"', '"hookEventName":"UserPromptSubmit"'], reply: { status: 200, body: { hookSpecificOutput: { hookEventName: 'UserPromptSubmit', additionalContext: 'from plugin http hook' } } } },
      { name: 'main', reply: { text: 'answer' } },
    ],
    steps: [submit('s-src', 'sources: hello'), { sleep: 300 }],
  };
}

// ── 子代理：SubagentStart / SubagentStop，子代理自己的工具调用 ──
{
  const hooks = {
    // SubagentStart / SubagentStop 在 hook-subagent-lifecycle 里单独配（那里把 hook 时长摆开，让帧的先后确定）。
    // 子代理自己的 hook 事件带子会话 id，不投影。
    SessionStart: [{ hooks: [cmd('session', {})] }],
    Setup: [{ hooks: [cmd('session', {})] }],
    SessionEnd: [{ hooks: [cmd('session', {})] }],
    UserPromptSubmit: [{ hooks: [cmd('session', {})] }],
    PreToolUse: [{ matcher: 'glob', hooks: [cmd('pre', {})] }],
    PostToolUse: [{ matcher: 'glob', hooks: [cmd('post', {})] }],
    Stop: [{ hooks: [cmd('stop', {})] }],
  };
  scenarios['hook-subagent'] = {
    about: '插件 hook 与一次性子代理（agent 工具）：子代理自己的工具调用（glob）派发 PreToolUse / PostToolUse、子代理结束派发 Stop，用的是父会话的 hook 配置、子会话的 id 与轮次环境，不投影到网关流；SessionStart / Setup / UserPromptSubmit / SessionEnd 只对父会话派发。',
    files: plugin('bbx-sub', hooks),
    provider: [
      TITLE,
      { name: 'parent-after', when: ['turn-1:', '"tool_call_id":"call_parent"'], reply: { text: 'Parent done.' } },
      { name: 'child-after', when: ['CHILD-HOOK-7', '"tool_call_id":"call_child"'], reply: { text: 'Scope: looked.\nResult: nothing.\nKey files: none\nFiles changed: none\nIssues: none' } },
      { name: 'child', when: ['CHILD-HOOK-7'], reply: { tool: { id: 'call_child', name: 'glob', arguments: { pattern: '*.nothing' } } } },
      { name: 'parent', when: ['turn-1:'], reply: { tool: { id: 'call_parent', name: 'agent', arguments: { description: 'Look around', prompt: 'CHILD-HOOK-7: look for files.', subagentType: 'Explorer' } } } },
    ],
    steps: [submit('s-sub', 'turn-1: delegate'), { sleep: 300 }],
    volatile: ['\\.(used|ratio|displayUsed|usageTokens|localEstimateTokens|calibrationEstimatedInputTokens|fileSize|breakdown\\.total|breakdown\\.messages|resultBytes|content-length)$'],
    volatileWhy: '子代理结果文本里带位数不定的耗时，token 估算随之变；基线自比已证明',
  };
  // SubagentStart / SubagentStop 的执行帧直接发到父会话流上，子代理自己的事件（subagent_started、
  // subagent_tool_*、subagent_text_delta ……）进宿主事件缓冲，sidecar 来帧或 500ms 无帧时才冲刷
  // （基线 agentLoopSidecarClient.ts:379-395；进程内循环是 AgentLoop.ts:2908-2919 的同一种泵）。
  // 帧的先后因此取决于 hook 时长：SubagentStart 不睡、SubagentStop 睡 1000ms，子代理运行期间攒下的事件
  // 确定地在 SubagentStop 的 started 与 completed 之间冲刷，subagent_completed 在 completed 之后。
  scenarios['hook-subagent-lifecycle'] = {
    about: 'SubagentStart / SubagentStop hook（一次性子代理）：hook 收到的输入与环境、matcher 按事件名匹配（按子代理类型写的 matcher 不跑），以及父会话 WS 流上 hook_execution 帧与子代理事件的先后。基线的子代理事件进宿主事件缓冲，只在 sidecar 来帧或 500ms 无帧时冲刷（agentLoopSidecarClient.ts:379-395），hook 执行帧直接发；SubagentStop hook 睡 1000ms，让子代理运行期间攒下的事件确定地落在 SubagentStop 的 started 与 completed 之间，subagent_completed 落在 completed 之后。',
    files: plugin('bbx-sub', {
      SubagentStart: [
        { hooks: [cmd('substart', {})] },
        { matcher: 'explore', hooks: [cmd('never', {})] },
        { matcher: 'SubagentStart', hooks: [cmd('substart-matched', {})] },
      ],
      SubagentStop: [
        { hooks: [cmd('substop', { sleep: 1000 })] },
        { matcher: 'explore', hooks: [cmd('never', {})] },
      ],
    }),
    provider: scenarios['hook-subagent'].provider,
    steps: [submit('s-sub', 'turn-1: delegate'), { sleep: 300 }],
    volatile: scenarios['hook-subagent'].volatile,
    volatileWhy: scenarios['hook-subagent'].volatileWhy,
  };
}

// ── 工具 hook 输出里其余的字段：只解析不生效的、只进 metadata 的 ──
{
  const specific = (event, rest) => json({ hookSpecificOutput: { hookEventName: event, ...rest } });
  const hooks = {
    PreToolUse: [
      { matcher: 'bash', hooks: [
        cmd('pre', { if: 'bash(case-pre-ask)', out: specific('PreToolUse', { permissionDecision: 'ask', permissionDecisionReason: 'ask reason' }) }),
        cmd('pre', { if: 'bash(case-pre-passthrough)', out: specific('PreToolUse', { permissionDecision: 'passthrough' }) }),
        cmd('pre', { if: 'bash(case-approve)', out: json({ decision: 'approve', reason: 'approved', suppressOutput: true, systemMessage: 'pre system note' }) }),
      ] },
      { matcher: 'write_file', hooks: [
        cmd('pre', { if: 'write_file(perm-pre-allow)', out: specific('PreToolUse', { permissionDecision: 'allow', permissionDecisionReason: 'allowed by pre hook' }) }),
      ] },
    ],
    PostToolUse: [{ matcher: 'bash', hooks: [
      cmd('post', { if: 'bash(case-mcp-output)', out: specific('PostToolUse', { updatedMCPToolOutput: { content: [{ type: 'text', text: 'replaced by hook' }] } }) }),
      cmd('post', { if: 'bash(case-mcp-null)', out: specific('PostToolUse', { updatedMCPToolOutput: null }) }),
      cmd('post', { if: 'bash(case-mismatch)', out: specific('SomethingElse', { additionalContext: 'context under another event name' }) }),
      cmd('post', { if: 'bash(case-noname)', out: json({ hookSpecificOutput: { additionalContext: 'context without event name' } }) }),
      cmd('post', { if: 'bash(case-approve)', out: json({ suppressOutput: true, systemMessage: 'post system note', hookSpecificOutput: { hookEventName: 'PostToolUse', updatedMCPToolOutput: 'plain string output' } }) }),
    ] }],
    PermissionRequest: [{ hooks: [cmd('perm')] }],
    PermissionDenied: [{ hooks: [cmd('denied', { out: specific('PermissionDenied', { retry: true }) })] }],
  };
  const cases = ['pre-ask', 'pre-passthrough', 'approve', 'mcp-output', 'mcp-null', 'mismatch', 'noname'];
  const provider = [TITLE];
  const steps = [];
  for (const c of cases) {
    provider.push(...toolCase(`case-${c}`, 'bash', { command: `printf case-${c}`, description: `case ${c}` }));
    steps.push(bypass(`s-${c}`), submit(`s-${c}`, `case-${c}: run the tool`), { sleep: 300 });
  }
  provider.push(...toolCase('perm-pre-allow', 'write_file', { file_path: 'perm-pre-allow.txt', content: 'perm-pre-allow\n' }));
  steps.push(submit('s-perm-pre-allow', 'perm-pre-allow: write the file', { answer: [{
    on: { type: 'permission_request' }, ws: 'permission_decide',
    params: { sessionKey: 's-perm-pre-allow', requestId: '{event.requestId}', decision: 'deny' }, count: 2,
  }] }), { sleep: 300 });
  steps.push(read('s-mcp-output'), read('s-mcp-null'), read('s-mismatch'), read('s-approve'));
  scenarios['hook-tool-effects'] = {
    about: '工具 hook 输出里不改变流程的字段：PreToolUse 的 permissionDecision ask / passthrough 不影响执行，allow 不跳过权限询问（用户拒绝照样拒）；PermissionDenied 的 retry 不触发重试；顶层 decision:approve、reason、suppressOutput、systemMessage 不起作用；PostToolUse 的 updatedMCPToolOutput（对象、null、字符串）只进工具结果的 metadata.lifecycle；hookSpecificOutput 的 hookEventName 与事件不同照样生效、缺了整个忽略。',
    files: plugin('bbx-effects', hooks),
    provider,
    steps,
  };
}

// ── 会话 hook 的输出与只解析不使用的配置字段；没有触发点的事件 ──
{
  const never = ['Notification', 'CwdChanged', 'WorktreeCreate', 'WorktreeRemove', 'TeammateIdle', 'TaskCreated', 'TaskCompleted'];
  const hooks = {
    // 插件 hook 的 pluginName / pluginId / pluginRoot 由加载器按插件覆盖（hook 的工作目录仍是插件根）。
    SessionStart: [{ pluginName: 'spoofed', pluginId: 'spoofed@nowhere', pluginRoot: '/nonexistent-root', hooks: [
      // 命令 hook 的 timeout 不生效（只有 SessionEnd 有自己的超时）：睡 1.5 秒也照样跑完。
      cmd('session-start', { statusMessage: 'starting up', once: true, timeout: 1, sleep: 1500, out: json({
        systemMessage: 'start note', suppressOutput: true,
        hookSpecificOutput: { hookEventName: 'SessionStart', initialUserMessage: 'initial message from hook', watchPaths: ['watched.txt', 7], additionalContext: 'ignored' },
      }) }),
    ] }],
    Setup: [{ hooks: [cmd('setup', { shell: 'powershell', async: true, asyncRewake: true, out: json({ hookSpecificOutput: { hookEventName: 'Setup', worktreePath: '/tmp/nowhere' } }) })] }],
    UserPromptSubmit: [{ hooks: [cmd('prompt', { once: true, statusMessage: 'checking prompt' })] }],
    ...Object.fromEntries(never.map((event) => [event, [{ hooks: [cmd(`never-${event}`)] }]])),
  };
  scenarios['hook-session-effects'] = {
    about: 'SessionStart / Setup 的输出（initialUserMessage、watchPaths、worktreePath、systemMessage、suppressOutput）都不影响会话；配置字段 statusMessage、once（同一会话第二轮照样执行）、命令 hook 的 timeout、shell、async、asyncRewake 只解析不使用；插件 matcher 上自带的 pluginName / pluginId / pluginRoot 被加载器覆盖。Notification、CwdChanged、WorktreeCreate、WorktreeRemove 没有触发点，TeammateIdle、TaskCreated、TaskCompleted 不是 hook 事件：配置了也从不执行。',
    files: plugin('bbx-sess', hooks),
    provider: [TITLE, { name: 'main', reply: { text: 'answer' } }],
    steps: [
      submit('s-sess', 'first: hello'), { sleep: 300 },
      submit('s-sess', 'second: again'), { sleep: 300 },
      read('s-sess'),
    ],
  };
}

// ── PreModelRequest / InstructionsLoaded：每次模型请求前，不等待 ──
{
  const hooks = {
    // hook 睡 1.5 秒：派发不等待，整轮在 hook 结束前就跑完，执行完成事件因此不在轮次的流上。
    PreModelRequest: [{ matcher: 'PreModelRequest', hooks: [cmd('pre-model', { sleep: 1500, out: json({ decision: 'block', reason: 'not honoured' }) })] }],
    InstructionsLoaded: [{ hooks: [cmd('instructions', { sleep: 1500, exit: 2, err: 'not honoured either\n' })] }],
  };
  scenarios['hook-model-request'] = {
    about: '插件的 PreModelRequest 与 InstructionsLoaded hook：一轮里每次模型请求前各派发一次（工具调用的一轮两次），收到 provider / model 与 hasSystemPrompt；派发不等待、结果不影响请求（阻止也不起作用）。',
    files: plugin('bbx-model', hooks),
    provider: [TITLE, ...toolCase('model-case', 'bash', { command: 'printf model-case', description: 'model case' })],
    steps: [bypass('s-model'), submit('s-model', 'model-case: run the tool'), { sleep: 2500 }],
    settleMs: 500,
  };
  // 用户中止这一轮时 hook 还在跑：基线派发时带着轮次的中止信号
  // （sidecarDefaultModuleDispatcher.ts:378-391、AgentLoop.ts:699-702 / 2281-2283），
  // 中止即杀 hook 进程（hooklog 没有 done 行）。模型 0.8 秒后才吐第一块文本然后挂住，收到这块文本就中止：
  // 那时 hook 进程已经起来并记下开始行（立刻中止的话进程可能还没起来，开始行有无取决于时机），
  // 中止落在流进行中、hook 睡完之前。之后同一会话再跑一轮：hook 照常派发、跑完。
  // hook 进程收到 SIGTERM 后再活 300ms 才退出（term.mjs 包一层 hook.mjs）：被杀的 hook 的执行结果因此确定地落在
  // 中止的一轮收尾之后、不上这一轮的流。不包这层时它与被截断的 assistant_block 只差几毫秒，Rust 侧五次里有一次先后颠倒。
  // 两个事件各一个场景：同时杀两个 hook 时两条 hook_execution_completed 的先后取决于进程退出的先后，基线自比不稳。
  for (const [name, event, log] of [
    ['hook-model-request-cancelled', 'PreModelRequest', 'pre-model'],
    ['hook-instructions-loaded-cancelled', 'InstructionsLoaded', 'instructions'],
  ]) {
    const session = `s-${log}-cancel`;
    scenarios[name] = {
      about: `${event} hook 执行中用户中止这一轮：hook 进程被杀（hooklog 没有 done 行）、中止的一轮照常收尾；同一会话下一轮的 hook 照常派发并跑完。`,
      files: {
        ...plugin('bbx-model-cancel', { [event]: [{ hooks: [{ ...cmd(log, { sleep: 3000 }), command: cmd(log, { sleep: 3000 }).command.replace('hook.mjs', 'term.mjs') }] }] }),
        'home/.pilotdeck/plugins/bbx-model-cancel/term.mjs': TERM_LATE,
      },
      provider: [
        TITLE,
        { name: 'next', when: ['next-case:'], reply: { text: 'next reply' } },
        { name: 'slow', when: ['slow-case:'], reply: { chunks: ['partial ', 'never sent'], delayMs: 800, hangAfter: 1, hangMs: 6000 } },
      ],
      steps: [
        bypass(session),
        submit(session, 'slow-case: answer slowly', { answer: [{
          on: { type: 'assistant_text_delta' }, ws: 'abort_turn', params: { sessionKey: session }, count: 1,
        }] }),
        { sleep: 4000 },
        submit(session, 'next-case: answer'),
        { sleep: 3800 },
        read(session),
      ],
      settleMs: 500,
    };
  }
}

// 同上两类 hook，打断运行中轮次的是 dispose 而不是用户中止：基线 AgentSession.dispose 在轮次运行中时先
// abort("session_closed")（src/agent/session/AgentSession.ts:274-281），中止的正是派发这两类事件时带的那个
// 中止信号（sidecarDefaultModuleDispatcher.ts:378-391），所以 hook 进程同样被杀。两个入口：
// close_session（InProcessGateway.closeSession → router.close，src/gateway/client/InProcessGateway.ts:2049-2054）；
// 停机（PilotDeckServerShutdownBundle 先 closeServer 再 disposeGateway，src/cli/PilotDeckServerShutdownBundle.ts:30-38）。
// 停机那一例：轮次先停在权限询问上，客户端发关闭帧断开（有挂起的询问，preserveTurn，这一轮不被中止），
// 再发 SIGTERM；serve 之后多等 4 秒再采落盘，没被杀的 hook 会来得及写下 done 行。
for (const [event, log] of [['PreModelRequest', 'pre-model'], ['InstructionsLoaded', 'instructions']]) {
  const short = log === 'pre-model' ? 'model-request' : 'instructions-loaded';
  const session = `s-${log}-dispose`;
  const files = (sleep) => ({
    ...plugin('bbx-model-dispose', { [event]: [{ hooks: [{ ...cmd(log, { sleep }), command: cmd(log, { sleep }).command.replace('hook.mjs', 'term.mjs') }] }] }),
    'home/.pilotdeck/plugins/bbx-model-dispose/term.mjs': TERM_LATE,
  });
  scenarios[`hook-${short}-close-session`] = {
    about: `${event} hook 执行中 close_session：会话 dispose 中止运行中的轮次，hook 进程被杀（hooklog 没有 done 行），与用户中止相同。`,
    files: files(3000),
    provider: [
      TITLE,
      { name: 'slow', when: ['slow-case:'], reply: { chunks: ['partial ', 'never sent'], delayMs: 800, hangAfter: 1, hangMs: 6000 } },
    ],
    steps: [
      bypass(session),
      submit(session, 'slow-case: answer slowly', { answer: [{
        on: { type: 'assistant_text_delta' }, ws: 'close_session', params: { sessionKey: session }, count: 1,
      }] }),
      { sleep: 4000 },
      read(session),
    ],
    settleMs: 500,
  };
  scenarios[`hook-${short}-server-stop`] = {
    about: `${event} hook 执行中停机：轮次停在权限询问上、客户端发关闭帧断开（有挂起的询问，这一轮不被中止）后 SIGTERM，disposeGateway 中止运行中的轮次，hook 进程被杀（hooklog 没有 done 行）。`,
    server: false,
    files: files(5000),
    provider: [TITLE, ...toolCase('stop-case', 'bash', { command: 'echo ran > ran.txt', description: 'stop case' })],
    steps: [{
      serve: ['server'], timeout: 60000,
      then: [
        { ...submit(session, 'stop-case: run the tool'), timeout: 1500, expectTimeout: true },
        { disconnect: true },
        { sleep: 300 },
      ],
    }],
    settleMs: 4000,
  };
}

// ── Elicitation / ElicitationResult：提问发出与得到回答时 ──
{
  const hooks = {
    // hook 睡 1.5 秒：派发不等待，提问照常发出、回答照常送达，整轮在 hook 结束前就跑完。
    Elicitation: [{ matcher: 'Elicitation', hooks: [cmd('elicitation', { sleep: 1500, out: json({ decision: 'block', reason: 'not honoured' }) })] }],
    ElicitationResult: [{ hooks: [cmd('elicitation-result', { sleep: 1500, exit: 2, err: 'not honoured either\n' })] }],
  };
  const question = { questions: [{ question: 'Which color?', header: 'Color', options: [{ label: 'Red', description: 'warm' }, { label: 'Blue', description: 'cool' }], multiSelect: false }] };
  scenarios['hook-elicitation'] = {
    about: '插件的 Elicitation / ElicitationResult hook：ask_user_question 的提问发给客户端时派发 Elicitation（requestId、toolName、toolCallId），回答送达后派发 ElicitationResult（requestId、delivered）；都不等待，阻止不起作用。',
    files: plugin('bbx-ask', hooks),
    provider: [TITLE, ...toolCase('ask-case', 'ask_user_question', question)],
    steps: [
      submit('s-ask', 'ask-case: ask me', { answer: [{
        on: { type: 'elicitation_request' }, ws: 'elicitation_respond', count: 1,
        params: { sessionKey: 's-ask', requestId: '{event.requestId}', answer: { type: 'answered', answers: { 'Which color?': 'Red' } } },
      }] }),
      { sleep: 2500 },
      read('s-ask'),
    ],
  };
}

// ── hook 被取消：轮次在 hook 执行中被中止 ──
{
  const hooks = { PreToolUse: [{ matcher: 'bash', hooks: [cmd('pre-slow', { sleep: 3000 })] }] };
  scenarios['hook-cancelled'] = {
    about: 'PreToolUse hook 执行中轮次被中止：hook 进程被杀（hooklog 没有 done 行），执行结果 outcome 为 cancelled，工具不执行。',
    files: plugin('bbx-cancel', hooks),
    provider: [TITLE, ...toolCase('cancel-case', 'bash', { command: 'printf ran > ran.txt', description: 'cancel case' })],
    steps: [
      bypass('s-cancel'),
      submit('s-cancel', 'cancel-case: run the tool', { answer: [{
        on: { type: 'agent_status', event: 'hook_execution_started' }, ws: 'abort_turn', params: { sessionKey: 's-cancel' }, count: 1,
      }] }),
      { sleep: 3500 },
      read('s-cancel'),
    ],
    accepted: [{
      id: 'aborted-hook-does-not-run-tool',
      why: '基线把被中止杀掉的 PreToolUse hook（outcome cancelled）当作非阻断，用户中止这一轮之后仍然走权限并真的执行了工具（落盘里有 ran.txt，会话记录多出 permission_started / permission_completed）；Rust 中止后不执行工具',
      paths: [
        '^\\$\\.tree\\./home/\\.pilotdeck/ran\\.txt$',
        '/chats/s-cancel\\.jsonl\\.jsonl(\\.length|\\[2\\d\\].*)$',
        '/chats/s-cancel\\.jsonl\\.(raw|projections\\.json)\\.',
        '^\\$\\.steps\\[3\\]\\.frames\\[0\\]\\.result\\.session\\.fileSize$',
      ],
    }],
  };
}

// ── modules：不需要外部服务的绑定 ──
{
  const base = `schemaVersion: 1
agent:
  model: bbx/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    bbx:
      protocol: openai
      url: {MOCK}/v1
      apiKey: bbx-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
`;
  const skill = { 'home/.pilotdeck/skills/bbx-user-skill/SKILL.md': '---\nname: bbx-user-skill\ndescription: A user-level skill for the modules scenario.\n---\n\nSay USER-SKILL.\n' };
  const steps = [
    { ws: 'skill_list', params: {} },
    { ws: 'skill_read', params: { scope: 'user', slug: 'bbx-user-skill' } },
    { ws: 'skill_create', params: { scope: 'user', slug: 'made', name: 'Made', description: 'Created while the module is selected.', body: 'x' } },
    { ws: 'skill_validate', params: {} },
    submit('s-mod', 'modules: hello'), { sleep: 300 },
  ];
  const provider = [TITLE, { name: 'main', reply: { text: 'answer' } }];
  scenarios['modules-builtin'] = {
    about: '`modules` 段里不需要外部服务的写法：agentLoop / modelProvider / tools / context 绑到内置实现（enabled: true, provider: pilotdeck），skills / knowledge 照常——与不写 modules 时行为相同（skill 管理、系统提示词里的 skills 段、发给模型的请求）。',
    config: `${base}modules:
  agentLoop:
    enabled: true
    provider: pilotdeck
  modelProvider:
    enabled: true
    provider: pilotdeck
  tools:
    enabled: true
    provider: pilotdeck
  context:
    enabled: true
    provider: pilotdeck
  skills:
    enabled: true
    provider: pilotdeck
`,
    files: skill,
    provider,
    steps,
  };
  scenarios['modules-skills-disabled'] = {
    about: '`modules.skills.enabled: false`（与 `modules.knowledge.enabled: false`）：skill 管理方法全部回 SKILL_MODULE_DISABLED，会话的 skills 贡献清空（系统提示词里没有 skills 段，发给模型的请求随之变化）；knowledge 关掉不注册任何工具。',
    config: `${base}modules:
  skills:
    enabled: false
  knowledge:
    enabled: false
`,
    files: skill,
    provider,
    steps,
  };
}

const outDir = here;
for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(outDir, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
  console.log('wrote', name);
}
