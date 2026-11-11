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
  // exit "@": the behaviour is the JSON between HOOK<< and >> in the prompt.
  let spec = { exit, out, err, sleep };
  if (exit === '@') {
    const m = /HOOK<<(.*?)>>/s.exec(String(parsed.prompt ?? ''));
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

const outDir = here;
for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(outDir, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
  console.log('wrote', name);
}
