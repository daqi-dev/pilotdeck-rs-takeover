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
    // SubagentStart / SubagentStop 的执行事件投影在父会话的流上，与子代理状态事件的先后在基线里不稳定，
    // 所以这里不配；它们由手工对比验证（见报告）。子代理自己的 hook 事件带子会话 id，不投影。
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
  };
}

const outDir = here;
for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(outDir, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
  console.log('wrote', name);
}
