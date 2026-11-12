// 生成 compact-*.json 场景。用法（仓库根目录）：node scripts/bbx/scenarios/compaction.gen.mjs
// 改场景改这里，再重新生成；生成的 JSON 一并提交。
//
// 上下文压缩要在超过预算时才发生。场景把模型的上下文窗口配得很小（maxContextTokens 几千到一万多，
// maxOutputTokens 1000），每轮发一段固定的长文本，几轮之后越过阈值。文本由下面的 filler 生成，
// 内容固定、没有随机值，token 数两侧一样。
//
// 注意基线的预算估计会用上一轮 provider 报的 usage 校准（假 provider 固定报 prompt_tokens 12），
// 校准把估计值最多压低一半，所以第二轮起「用量」约是本地估计的一半。阈值按这个算。
import fs from 'node:fs';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);

const config = ({ context = 14000, output = 1000 } = {}) => `schemaVersion: 1
agent:
  model: bbx/deterministic
  maxContextTokens: ${context}
  maxOutputTokens: ${output}
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
            maxContextTokens: ${context}
            maxOutputTokens: ${output}
telemetry:
  enabled: false
`;

const WORDS = 'alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa';
// 一段固定文本：lines 行，每行约 20 个 token。
const filler = (tag, lines) => Array.from({ length: lines }, (_, j) => `${tag} line ${j + 1}: ${WORDS}.`).join('\n');

const TITLE = { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Compaction run"}' } };
const SUMMARY_TEXT = [
  '## Objective',
  'Keep the long notes conversation going.',
  '## Current State',
  'Several long notes were exchanged.',
  '## Remaining',
  'Answer the next note.',
  '## Files And Artifacts',
  'None',
].join('\n');
const SUMMARY = { name: 'summary', when: ['internal-compaction-control'], reply: { text: SUMMARY_TEXT } };
const MAIN = { name: 'main', reply: { text: 'Noted.' } };

const turn = (session, message, extra = {}) => ({
  ws: 'submit_turn',
  params: { sessionKey: session, channelKey: 'test', message },
  ...extra,
});

const scenarios = {};

// 自动全量压缩：六轮长文本把用量推过 0.9，第六轮开模型请求前先压缩（摘要请求 + 替换历史），
// 第七轮看压缩后的请求内容；然后读会话、分叉、导出。
{
  const steps = [];
  for (let i = 1; i <= 6; i += 1) {
    steps.push(turn('s-cmp', `note ${i}\n${filler(`n${i}`, 140)}`), { sleep: 300 });
  }
  steps.push(turn('s-cmp', 'note 7: short follow-up'), { sleep: 300 });
  steps.push(
    {
      ws: 'read_session_messages',
      params: { sessionKey: 's-cmp' },
      save: { note7: '"text":"note 7: short follow-up","entryId":"([0-9a-f-]{36})"' },
    },
    { ws: 'export_session_transcript', params: { sessionKey: 's-cmp' } },
    // 从第七轮的用户消息分叉：新会话带着压缩后的历史；在分叉上再问一轮。
    {
      ws: 'fork_session',
      params: { sessionKey: 's-cmp', fromEntryId: '{saved.note7}' },
      save: { forked: '"newSessionKey":"([^"]+)"' },
    },
    turn('{saved.forked}', 'note 8: on the fork'),
    { sleep: 300 },
    { ws: 'read_session_messages', params: { sessionKey: '{saved.forked}' } },
  );
  scenarios['compact-auto'] = {
    about: '自动全量压缩：上下文窗口配小，几轮长文本后用量越过 0.9，开模型请求前先发摘要请求（摘要提示词、裁剪后的输入），用摘要和保留的消息替换历史；之后一轮的请求带 compact-boundary 与摘要；读会话、导出会话都看到压缩后的结果。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 20000 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

// 五轮普通长度、一轮很长的消息：摘要后保留的尾部（第五、六轮）仍超过目标的 60%，
// 摘要之后再按 snip 裁掉中间的轮次（请求里出现 <snip-boundary>）。
{
  const steps = [];
  for (let i = 1; i <= 5; i += 1) {
    steps.push(turn('s-snip', `note ${i}\n${filler(`n${i}`, 140)}`), { sleep: 300 });
  }
  steps.push(turn('s-snip', `note 6\n${filler('n6', 500)}`), { sleep: 300 });
  steps.push(turn('s-snip', 'note 7: short follow-up'), { sleep: 300 });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-snip' } });
  scenarios['compact-snip'] = {
    about: '全量摘要之后的 snip：最后一轮消息很长，摘要后保留的尾部仍高于目标（有效窗口的 60%），按基线再裁掉中间轮次；看摘要请求、压缩后的模型请求（snip-boundary）、会话记录与读会话。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 20000 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

// 窗口小到系统提示词加工具定义就占了大半：第一轮就越过阈值，摘要无可摘，
// 应急压缩（reactive）也降不下来，轮次以 context_overflow_after_emergency_compaction 失败；
// 第二轮再来一次，摘要失败冷却等状态在会话里延续。
{
  scenarios['compact-overflow'] = {
    about: '应急压缩后仍超窗：上下文窗口只有 14000，第一轮就越过 0.9；摘要无可摘（no_content）、reactive 摘要、工具结果投影、截头都做完仍超出，轮次以 agent_context_recovery_failed / context_overflow_after_emergency_compaction 结束；第二轮同样。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 14000 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps: [
      turn('s-over', `note 1\n${filler('n1', 100)}`), { sleep: 300 },
      turn('s-over', `note 2\n${filler('n2', 100)}`), { sleep: 300 },
      { ws: 'read_session_messages', params: { sessionKey: 's-over' } },
    ],
  };
}

// 摘要请求失败：假 provider 对摘要请求回 500。第六轮的自动压缩摘要失败（compact_completed 为 failed），
// 应急路径里的 reactive 摘要命中失败冷却，不再发请求；第七轮仍在冷却内。
{
  const steps = [];
  for (let i = 1; i <= 6; i += 1) {
    steps.push(turn('s-fail', `note ${i}\n${filler(`n${i}`, 140)}`, { timeout: 60000 }), { sleep: 300 });
  }
  steps.push(turn('s-fail', 'note 7: short follow-up', { timeout: 60000 }), { sleep: 300 });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-fail' } });
  scenarios['compact-summary-fail'] = {
    about: '摘要请求失败：摘要请求回 500，自动压缩的摘要失败（compact_summary_failed、compact_completed status failed），随后的 reactive 摘要命中 60 秒失败冷却、不发请求，再走应急投影与截头；下一轮仍在冷却内。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 20000 }),
    provider: [
      TITLE,
      { name: 'summary', when: ['internal-compaction-control'], reply: { status: 500, body: { error: { message: 'summary backend exploded', type: 'server_error' } } } },
      MAIN,
    ],
    steps,
  };
}

// micro 单独触发：一轮里模型连续读五个文件，每个结果约 3000 token；第六次请求前用量越过 0.8，
// micro 把最老的一个读文件结果改写成预览（保留最新四个），用量回到 0.9 以下，不发摘要请求。
{
  const files = {};
  const provider = [TITLE];
  for (let i = 1; i <= 5; i += 1) files[`home/.pilotdeck/notes/f${i}.txt`] = filler(`f${i}`, 110) + '\n';
  provider.push({ name: 'done', when: ['micro run:', '"tool_call_id":"call_f5"'], reply: { text: 'All five notes read.' } });
  for (let i = 4; i >= 1; i -= 1) {
    provider.push({
      name: `read-f${i + 1}`,
      when: ['micro run:', `"tool_call_id":"call_f${i}"`],
      reply: { tool: { id: `call_f${i + 1}`, name: 'read_file', arguments: { file_path: `{PILOT_HOME}/notes/f${i + 1}.txt` } } },
    });
  }
  provider.push({ name: 'read-f1', when: ['micro run:'], reply: { tool: { id: 'call_f1', name: 'read_file', arguments: { file_path: '{PILOT_HOME}/notes/f1.txt' } } } });
  provider.push(SUMMARY, MAIN);
  scenarios['compact-micro'] = {
    about: 'micro 单独触发：一轮里连续五次 read_file，结果累积到用量越过 0.8 时，micro 把最老的读文件结果改写成截断预览（保留最新四个），用量回落到 0.9 以下即止，不发摘要请求；看之后模型请求里的改写结果与会话记录。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 16000 }),
    files,
    provider,
    steps: [
      turn('s-micro', 'micro run: read the five notes'), { sleep: 300 },
      { ws: 'read_session_messages', params: { sessionKey: 's-micro' } },
    ],
  };
}

// PreCompact / PostCompact 插件 hook：自动压缩时各跑一次，hook 收到的 stdin 记到 hooklog。
{
  const HOOK = `import fs from 'node:fs';
const [log] = process.argv.slice(2);
let input = '';
process.stdin.on('data', (d) => { input += d; });
process.stdin.on('end', () => {
  let parsed; try { parsed = JSON.parse(input); } catch { parsed = { raw: input }; }
  const dir = process.env.HOME + '/hooklog';
  fs.mkdirSync(dir, { recursive: true });
  fs.appendFileSync(dir + '/' + log + '.jsonl', JSON.stringify({ argv: process.argv.slice(2), cwd: process.cwd(), input: parsed }) + '\\n');
});
`;
  const hook = (log) => [{ hooks: [{ type: 'command', command: `node hook.mjs ${log}` }] }];
  const steps = [];
  for (let i = 1; i <= 6; i += 1) {
    steps.push(turn('s-hook', `note ${i}\n${filler(`n${i}`, 140)}`), { sleep: 300 });
  }
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-hook' } });
  scenarios['compact-hooks'] = {
    about: 'PreCompact / PostCompact 插件 hook：自动全量压缩时摘要前后各派发一次（matcher 为事件名，cwd 为项目根，permissionMode default），hook 收到的输入写进 hooklog；hook 执行过程投影成 agent_status。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 20000 }),
    files: {
      'home/.pilotdeck/plugins/cmp-hooks/plugin.json': {
        name: 'cmp-hooks',
        version: '0.1.0',
        description: 'bbx cmp-hooks',
        hooks: { PreCompact: hook('pre-compact'), PostCompact: hook('post-compact') },
      },
      'home/.pilotdeck/plugins/cmp-hooks/hook.mjs': HOOK,
    },
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

// 手动压缩：/compact 带参数只回用法；会话还没在内存里时回 session_busy；
// 两轮长文本之后 /compact 强制摘要（不看阈值），之后一轮的请求带摘要；读会话看维护轮次的记录。
{
  const steps = [
    turn('s-man', '/compact now please'),
    turn('s-man', '/compact'),
  ];
  for (let i = 1; i <= 3; i += 1) {
    steps.push(turn('s-man', `note ${i}\n${filler(`n${i}`, 60)}`), { sleep: 300 });
  }
  steps.push(turn('s-man', '/compact'), { sleep: 300 });
  steps.push(turn('s-man', 'note 4: after manual compaction'), { sleep: 300 });
  steps.push(turn('s-man', '/compact'), { sleep: 300 });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-man' } });
  scenarios['compact-manual'] = {
    about: '手动压缩 /compact：带参数回用法文本；会话尚未载入时回 session_busy；三轮之后 /compact 不看阈值强制摘要（维护轮次：turn_started、compaction_* 记录、替换边界、turn_result），回 manual_compaction 状态与「Compacted N history items」；之后一轮的请求带摘要；再 /compact 一次（滚动摘要合并上一份摘要）。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 65536, output: 8192 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

// 手动压缩之后：续聊一轮，读会话、导出会话，从续聊那条用户消息分叉，在分叉上再聊一轮。
{
  const steps = [];
  for (let i = 1; i <= 3; i += 1) {
    steps.push(turn('s-mfork', `note ${i}\n${filler(`n${i}`, 60)}`), { sleep: 300 });
  }
  steps.push(turn('s-mfork', '/compact'), { sleep: 300 });
  steps.push(turn('s-mfork', 'note 4: after manual compaction'), { sleep: 300 });
  steps.push(
    {
      ws: 'read_session_messages',
      params: { sessionKey: 's-mfork' },
      save: { note4: '"text":"note 4: after manual compaction","entryId":"([0-9a-f-]{36})"' },
    },
    { ws: 'export_session_transcript', params: { sessionKey: 's-mfork' } },
    {
      ws: 'fork_session',
      params: { sessionKey: 's-mfork', fromEntryId: '{saved.note4}' },
      save: { forked: '"newSessionKey":"([^"]+)"' },
    },
    turn('{saved.forked}', 'note 5: on the fork'),
    { sleep: 300 },
    { ws: 'read_session_messages', params: { sessionKey: '{saved.forked}' } },
  );
  scenarios['compact-manual-fork'] = {
    about: '手动压缩之后：续聊的请求带摘要；读会话、导出会话看到维护轮次与替换边界；从续聊的用户消息分叉，分叉上的请求带着压缩后的历史。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 65536, output: 8192 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

// 手动压缩时的 PreCompact / PostCompact 插件 hook：hook 收到的输入（trigger 等）写进 hooklog。
{
  const HOOK = `import fs from 'node:fs';
const [log] = process.argv.slice(2);
let input = '';
process.stdin.on('data', (d) => { input += d; });
process.stdin.on('end', () => {
  let parsed; try { parsed = JSON.parse(input); } catch { parsed = { raw: input }; }
  const dir = process.env.HOME + '/hooklog';
  fs.mkdirSync(dir, { recursive: true });
  fs.appendFileSync(dir + '/' + log + '.jsonl', JSON.stringify({ argv: process.argv.slice(2), cwd: process.cwd(), input: parsed }) + '\\n');
});
`;
  const hook = (log) => [{ hooks: [{ type: 'command', command: `node hook.mjs ${log}` }] }];
  const steps = [];
  for (let i = 1; i <= 3; i += 1) {
    steps.push(turn('s-mhook', `note ${i}\n${filler(`n${i}`, 60)}`), { sleep: 300 });
  }
  steps.push(turn('s-mhook', '/compact'), { sleep: 500 });
  steps.push(turn('s-mhook', 'note 4: after manual compaction'), { sleep: 300 });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-mhook' } });
  scenarios['compact-manual-hooks'] = {
    about: '手动压缩 /compact 时的 PreCompact / PostCompact 插件 hook：摘要前后各派发一次（trigger 为 manual），hook 收到的输入写进 hooklog；之后一轮照常。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 65536, output: 8192 }),
    files: {
      'home/.pilotdeck/plugins/cmp-hooks/plugin.json': {
        name: 'cmp-hooks',
        version: '0.1.0',
        description: 'bbx cmp-hooks',
        hooks: { PreCompact: hook('pre-compact'), PostCompact: hook('post-compact') },
      },
      'home/.pilotdeck/plugins/cmp-hooks/hook.mjs': HOOK,
    },
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

// 手动压缩的摘要请求失败：基线回 No compactable history yet（编排把失败的摘要当作 skipped）；
// 再 /compact 一次命中摘要失败冷却；之后的普通一轮照常。
{
  const steps = [];
  for (let i = 1; i <= 3; i += 1) {
    steps.push(turn('s-mfail', `note ${i}\n${filler(`n${i}`, 60)}`, { timeout: 60000 }), { sleep: 300 });
  }
  steps.push(turn('s-mfail', '/compact', { timeout: 60000 }), { sleep: 300 });
  steps.push(turn('s-mfail', '/compact', { timeout: 60000 }), { sleep: 300 });
  steps.push(turn('s-mfail', 'note 4: after failed compaction', { timeout: 60000 }), { sleep: 300 });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-mfail' } });
  scenarios['compact-manual-fail'] = {
    about: '手动压缩的摘要请求回 500：基线的编排把失败的摘要当作没压缩（compaction_completed skipped），维护轮次照常结束，回 No compactable history yet；紧接着再 /compact 一次（摘要失败冷却内）；之后的普通一轮照常。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 65536, output: 8192 }),
    provider: [
      TITLE,
      { name: 'summary', when: ['internal-compaction-control'], reply: { status: 500, body: { error: { message: 'summary backend exploded', type: 'server_error' } } } },
      MAIN,
    ],
    steps,
  };
}

// 会话不在内存里：close_session 之后 /compact 回 session_busy；再聊一轮（会话重新载入）后 /compact 正常压缩。
// 另一个会话只有一轮短对话：/compact 回 No compactable history yet。
{
  const steps = [];
  for (let i = 1; i <= 2; i += 1) {
    steps.push(turn('s-mclose', `note ${i}\n${filler(`n${i}`, 60)}`), { sleep: 300 });
  }
  steps.push({ ws: 'close_session', params: { sessionKey: 's-mclose' } }, { sleep: 300 });
  steps.push(turn('s-mclose', '/compact'), { sleep: 300 });
  steps.push(turn('s-mclose', 'note 3: reloaded'), { sleep: 300 });
  steps.push(turn('s-mclose', '/compact'), { sleep: 300 });
  steps.push(turn('s-mshort', 'hello'), { sleep: 300 });
  steps.push(turn('s-mshort', '/compact'), { sleep: 300 });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-mclose' } });
  steps.push({ ws: 'read_session_messages', params: { sessionKey: 's-mshort' } });
  scenarios['compact-manual-unloaded'] = {
    about: '会话不在内存里时 /compact：close_session 之后回 session_busy（不新建会话）；再聊一轮把会话载入后 /compact 正常；只有一轮短对话的会话 /compact 的结果。本文件由同目录的 compaction.gen.mjs 生成，不手改。',
    config: config({ context: 65536, output: 8192 }),
    provider: [TITLE, SUMMARY, MAIN],
    steps,
  };
}

const outDir = here;
for (const [name, body] of Object.entries(scenarios)) {
  fs.writeFileSync(path.join(outDir, `${name}.json`), JSON.stringify({ name, ...body }, null, 2) + '\n');
}
console.log(`wrote ${Object.keys(scenarios).length} scenarios`);
