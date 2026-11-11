// 生成 read-messages-fixture.json。
// 用法（仓库根目录）：
//   node scripts/bbx/run.mjs ts scripts/bbx/scenarios/read-messages-tools.json /tmp/tools.ts.json
//   node scripts/bbx/scenarios/read-messages-fixture.gen.mjs /tmp/tools.ts.json scripts/bbx/scenarios/read-messages-fixture.json
// 第一个参数是基线跑 read-messages-tools 场景的采集产物：从里面取出基线自己写下的
// 两份会话记录（工具调用成功 / 被拒绝），把 model_request 里的系统提示词和工具声明
// 裁掉；再加一份手写的会话记录，走遍 readSessionMessages.ts 的各分支。
// 重新生成会换掉录下的那两份记录里的 uuid 和时间戳，属正常。
import fs from 'node:fs';
const [capturePath, outPath] = process.argv.slice(2);
const capture = JSON.parse(fs.readFileSync(capturePath, 'utf8'));
const CHATS = 'home/.pilotdeck/projects/private-tmp-pdrs-bbx-root-home-work-alpha/chats';
const HOME_CHATS = 'home/.pilotdeck/projects/private-tmp-pdrs-bbx-root-home-.pilotdeck/chats';
const jsonl = (entries) => entries.map((e) => JSON.stringify(e)).join('\n') + '\n';
const recorded = (name) => {
  const key = Object.keys(capture.tree).find((p) => p.endsWith(`/${name}.jsonl`));
  return capture.tree[key].jsonl.map((entry) => {
    if (entry.type !== 'model_request') return entry;
    return { ...entry, request: { ...entry.request, systemPrompt: 'fixture system prompt', tools: [] } };
  });
};

let seq = 0;
const at = () => `2026-11-01T00:00:${String(seq).padStart(2, '0')}.000Z`;
const entry = (turnId, body, { id = true } = {}) => {
  seq += 1;
  return { ...body, sessionId: 's-crafted', turnId, sequence: seq, createdAt: at(), ...(id ? { entryId: `e-${seq}` } : {}) };
};
const text = (t, extra = {}) => ({ type: 'text', text: t, ...extra });
const timeline = (id, order) => ({ version: 1, turnId: 't1', id, order, revision: 1 });
const crafted = [
  entry('t1', { type: 'turn_started' }),
  entry('t1', { type: 'session_metadata', metadata: { title: 'Crafted title', tag: 'fixture', firstPrompt: 'look at this', updatedAt: '2026-11-01T00:00:00.000Z' } }),
  entry('t1', {
    type: 'accepted_input',
    messages: [
      { role: 'user', content: [text('look at this '), { type: 'image', source: 'base64', mimeType: 'image/png', data: 'QUJD' }, text('and this'), { type: 'image', source: 'url', mimeType: 'image/jpeg', data: 'https://example.invalid/a.jpg' }] },
      { role: 'user', metadata: { synthetic: true }, content: [text('synthetic, hidden')] },
      { role: 'user', metadata: { queueItemId: 'q-1', moduleId: 'm-1' }, content: [text('queued follow-up')] },
      { role: 'user', metadata: { compactReplacement: true }, content: [text('replacement, hidden')] },
    ],
  }),
  entry('t1', {
    type: 'assistant_message',
    message: {
      role: 'assistant',
      metadata: { model: 'fixture-model' },
      content: [
        { type: 'thinking', text: 'thinking it over', blockId: 'b-think' },
        { type: 'thinking', text: 'no block id' },
        text('part one, ', { blockId: 'b-1', timeline: timeline('b-1', 0) }),
        text('part two', { blockId: 'b-1', timeline: timeline('b-1', 1) }),
        text('new block', { blockId: 'b-2' }),
        text('legacy a'),
        text(' legacy b'),
        { type: 'tool_call', id: 'c-grep', name: 'grep', input: { pattern: 'x' }, timeline: timeline('tool:c-grep', 2) },
        { type: 'tool_call', id: 'c-agent', name: 'Agent', input: { prompt: 'child one' } },
        { type: 'tool_call', id: 'c-task', name: 'task', input: { prompt: 'child two' } },
        { type: 'tool_call', id: 'c-plan', name: 'exit_plan_mode', input: {} },
        { type: 'image', source: 'base64', mimeType: 'image/png', data: 'QUJD', bytes: 3 },
        { type: 'pdf', source: 'base64', mimeType: 'application/pdf', data: 'UERG', bytes: 3 },
        { type: 'audio', source: 'url', mimeType: 'audio/mpeg', data: 'https://example.invalid/a.mp3' },
        { type: 'unknown_block', value: 1 },
      ],
    },
  }),
  entry('t1', {
    type: 'tool_result_message',
    message: {
      role: 'user',
      content: [
        {
          type: 'tool_result', toolCallId: 'c-grep', timeline: timeline('result:c-grep', 3),
          content: [text('3 matches'), { type: 'image', source: 'url', mimeType: 'image/png', data: 'https://example.invalid/r.png' }, { type: 'image', source: 'base64', mimeType: 'image/png', data: 'QUJDRA==' }, { type: 'pdf', source: 'base64', mimeType: 'application/pdf', data: 'UERG', bytes: 3, pages: 2 }, { type: 'pdf', source: 'base64', mimeType: 'application/pdf', data: 'UERGUERG', bytes: 6 }],
          raw: { toolName: 'Grep', data: { matches: 3, files: ['a.txt'] } },
        },
        { type: 'image', source: 'base64', mimeType: 'image/png', data: 'RUZH' },
        { type: 'image', source: 'url', mimeType: 'image/gif', data: 'https://example.invalid/b.gif' },
        { type: 'tool_result', toolCallId: 'c-agent', isError: true, content: [text('child failed')], raw: { toolName: 'Agent', error: { code: 'child_error', message: 'x' } } },
        text('between results'),
        { type: 'image', source: 'base64', mimeType: 'image/png', data: 'SElK' },
        { type: 'tool_result', toolCallId: 'c-plan', content: [], raw: { toolName: 'exit_plan_mode', data: { planFilePath: '/tmp/plan.md', planTitle: 'Plan', planSummary: 'Do it' } } },
        { type: 'tool_result_reference', toolCallId: 'c-ref', preview: 'preview text', path: '/tmp/out.txt', originalBytes: 4096, hasMore: true, mimeType: 'text/plain', reason: 'too_large', isError: false },
        { type: 'media_reference', toolCallId: 'c-media', preview: 'media preview', path: '/tmp/a.png', originalBytes: 99, hasMore: false, mimeType: 'image/png', mediaType: 'image', detail: 'low', reason: 'media' },
      ],
    },
  }),
  entry('t1', { type: 'file_artifacts', artifacts: [{ path: 'a.txt', source: 'workspace_diff' }, { path: 'b.txt', source: 'tool' }] }),
  entry('t1', { type: 'file_artifacts', artifacts: [] }),
  entry('t1', { type: 'subagent_started', subagentId: 'sa-1', transcriptRelativePath: 's-crafted/subagents/sa-1.jsonl' }),
  entry('t1', { type: 'subagent_started', subagentId: 'sa-2', subagentSessionId: 'child-session-2', transcriptRelativePath: 'unused.jsonl' }),
  entry('t1', { type: 'subagent_started', subagentId: 'sa-3' }),
  entry('t1', { type: 'subagent_completed', subagentId: 'sa-1', summaryPreview: 'child one done' }),
  entry('t1', { type: 'subagent_completed', subagentId: 'sa-2', summaryPreview: 'child two done' }),
  entry('t1', { type: 'subagent_completed', subagentId: 'sa-3', summaryPreview: 'never linked', errored: true }),
  entry('t1', { type: 'agent_status_message', event: 'context_budget', kind: 'status', text: 'context_budget', detail: { type: 'context_budget', used: 1200, displayUsed: 1300, budgetUsed: 1250, total: 65536, effectiveTotal: 57344, reservedOutputTokens: 8192, ratio: 0.02, state: 'ok' } }),
  entry('t1', { type: 'agent_status_message', event: 'notice', kind: 'error', text: 'hidden failure', detail: { visible: false, messageI18n: { key: 'errors.hidden', params: { a: 1 } }, userHintI18n: { key: 'hints.retry' } } }, { id: false }),
  entry('t1', { type: 'plan_todo_plan_changed', plan: '  1. do the thing  ' }),
  entry('t1', { type: 'plan_todo_progressed', toolName: 'grep' }),
  entry('t1', { type: 'plan_todo_written', mode: 'structured', merge: false, reason: ' start ', todos: [{ id: 'a', content: 'first', status: 'in_progress' }, { id: 'b', content: 'second', status: 'pending', priority: 'high' }] }),
  entry('t1', { type: 'plan_todo_progressed', toolName: 'grep' }),
  entry('t1', { type: 'plan_todo_progressed', toolName: 'todo_write' }),
  entry('t1', { type: 'plan_todo_written', mode: 'markdown', merge: true, markdown: '- [x] first', todos: [{ id: 'a', content: 'first', status: 'completed' }, { id: 'c', content: 'third', status: 'cancelled' }] }),
  entry('t1', { type: 'goal_changed', revision: 1, goal: { id: 'g-1', revision: 1, objective: 'ship it', phase: 'blocked', blockedReason: 'waiting', maxGoalRounds: 3 } }),
  entry('t1', { type: 'goal_changed', revision: 2, goal: null }),
  entry('t1', { type: 'file_snapshot_recorded', messageId: 'm-1', snapshot: { files: 1 } }),
  entry('t1', { type: 'file_snapshot_recorded', messageId: 'm-2', snapshot: { files: 2 } }),
  entry('t1', { type: 'file_snapshot_recorded', messageId: 'm-1', snapshot: { files: 3 } }),
  entry('t1', {
    type: 'control_boundary',
    boundary: {
      kind: 'compact', subtype: 'compact_boundary',
      compactMetadata: { timeline: timeline('compact:1', 9), compactionId: 'cmp-1', trigger: 'auto', preTokens: 1000, postTokens: 400.2, messagesSummarized: 5, targetTokens: 300, summaryGenerated: true, checkpointMerged: false, finalRatio: 0.4, level: 1, stage: 'summary', stageLabel: 'Summary' },
      replacementMessages: [{ role: 'user', content: [text('summary of earlier turns')] }],
    },
  }),
  entry('t1', { type: 'durable_message', message: { role: 'user', metadata: { compactReplacement: true }, content: [text('replacement, hidden')] } }),
  entry('t1', { type: 'turn_result', result: { type: 'success', stopReason: 'completed', usage: { inputTokens: 10, outputTokens: 2, totalTokens: 12 }, permissionDenials: [] } }),
  entry('t2', { type: 'accepted_input', messages: [{ role: 'user', content: [text('second turn')] }] }),
  entry('t2', { type: 'assistant_message', message: { role: 'assistant', content: [text('partial answer')] } }),
  entry('t2', { type: 'control_boundary', boundary: { kind: 'compact', subtype: 'microcompact' } }, { id: false }),
  entry('t2', { type: 'turn_result', result: { type: 'error', stopReason: 'model_error', usage: {}, permissionDenials: [], errors: [{ message: 'first failure' }, { message: '' }, { message: 'second failure' }] } }),
  entry('t3', { type: 'accepted_input', messages: [{ role: 'user', content: [text('third turn')] }] }),
  entry('t3', { type: 'agent_status_message', event: 'model_error', kind: 'error', text: 'visible failure', detail: { code: 'x' } }),
  entry('t3', { type: 'turn_result', result: { type: 'error', stopReason: 'aborted', usage: {}, permissionDenials: [] } }),
  entry('t4', { type: 'accepted_input', messages: [{ role: 'user', content: [text('fourth turn')] }] }),
  entry('t4', { type: 'turn_result', result: { type: 'error', stopReason: 'max_turns', usage: {}, permissionDenials: [] } }),
  entry('t5', { type: 'accepted_input', messages: [{ role: 'user', content: [text('fifth turn, never finished')] }] }),
  entry('t5', { type: 'assistant_message', message: { role: 'assistant', content: [{ type: 'tool_call', id: 'c-late', name: 'bash', input: { command: 'true' } }] } }),
  entry('t6', { type: 'durable_message', message: { role: 'assistant', content: [text('also unfinished')] } }),
];
const child = (id, entries) => entries.map((body, index) => ({ ...body, sessionId: id, turnId: 'child-turn', sequence: index + 1, createdAt: `2026-11-01T00:01:${String(index).padStart(2, '0')}.000Z`, entryId: `${id}-e-${index + 1}` }));
const sidechain = (id) => child(id, [
  { type: 'accepted_input', messages: [{ role: 'user', content: [text('fork prelude, not shown')] }] },
  { type: 'control_boundary', boundary: { kind: 'compact', subtype: 'compact_boundary', compactMetadata: { trigger: 'auto', postTokens: 1 } } },
  { type: 'assistant_message', message: { role: 'assistant', metadata: { model: 'child-model' }, content: [text(`${id} working`), { type: 'tool_call', id: 'cc-1', name: 'read_file', input: { file_path: 'x' } }] } },
  { type: 'tool_result_message', message: { role: 'user', content: [{ type: 'tool_result', toolCallId: 'cc-1', content: [text('file body')] }] } },
  { type: 'durable_message', message: { role: 'assistant', metadata: { synthetic: true }, content: [text('synthetic child, hidden')] } },
  { type: 'control_boundary', boundary: { kind: 'compact', subtype: 'compact_boundary', compactMetadata: { trigger: 'manual', postTokens: 2 } } },
  { type: 'assistant_message', message: { role: 'assistant', content: [text(`${id} finished`)] } },
]);

const alpha = '{HOME}/work/alpha';
const read = (params) => ({ ws: 'read_session_messages', params });
const sub = (params) => ({ ws: 'read_subagent_messages', params });
const scenario = {
  name: 'read-messages-fixture',
  about: '读模型对着同一份落盘记录：两份由基线录下的含工具调用的会话（读到文件 / 被拒绝；model_request 里的系统提示词和工具声明已裁掉），和一份手写的会话记录（走遍 readSessionMessages.ts 的各分支：图片、思考块、各类工具结果、文件产物、子代理、压缩边界、状态消息、失败与未完成的轮次；子代理旁路记录两份）。先只读这些文件，再在录下的两个会话上各续一轮纯文本对话，比较重新加载后写出的 13 个投影。本文件由同目录的 read-messages-fixture.gen.mjs 生成，不手改。',
  provider: [
    { name: 'title', when: ['Generate a concise title'], reply: { text: '{"title": "Fixture"}' } },
    { name: 'main', reply: { text: 'Hello from the mock.' } },
  ],
  files: {
    'home/work/alpha/notes.txt': 'first line of notes\nsecond line of notes\n',
    'home/.pilotdeck/projects/private-tmp-pdrs-bbx-root-home-work-alpha/.cwd': '{HOME}/work/alpha\n',
    [`${CHATS}/s-tool.jsonl`]: jsonl(recorded('s-tool')),
    [`${CHATS}/s-miss.jsonl`]: jsonl(recorded('s-miss')),
    [`${CHATS}/s-crafted.jsonl`]: jsonl(crafted),
    [`${CHATS}/s-crafted/subagents/sa-1.jsonl`]: jsonl(sidechain('sa-1')),
    [`${CHATS}/s-crafted/subagents/sa-2.jsonl`]: jsonl(sidechain('sa-2')),
    [`${HOME_CHATS}/s-crafted.jsonl`]: jsonl(crafted),
  },
  steps: [
    read({ sessionKey: 's-tool', projectKey: alpha }),
    read({ sessionKey: 's-miss', projectKey: alpha }),
    read({ sessionKey: 's-crafted', projectKey: alpha }),
    read({ sessionKey: 's-crafted', projectKey: alpha, limit: 5 }),
    read({ sessionKey: 's-crafted', projectKey: alpha, limit: 7, cursor: '5' }),
    read({ sessionKey: 's-crafted', projectKey: alpha, limit: 50, cursor: '12' }),
    read({ sessionKey: 's-crafted' }),
    read({ sessionKey: 'bg', sessionKind: 'background_task', relativeTranscriptPath: 's-crafted/subagents/sa-1.jsonl', projectKey: alpha }),
    sub({ sessionKey: 's-crafted', subagentId: 'sa-1', projectKey: alpha }),
    sub({ sessionKey: 's-crafted', subagentId: 'sa-2', projectKey: alpha }),
    sub({ sessionKey: 's-crafted', subagentId: 'sa-3', projectKey: alpha }),
    sub({ sessionKey: 's-crafted', subagentId: 'sa-9', projectKey: alpha }),
    sub({ sessionKey: 's-crafted', subagentId: 'sa-1' }),
    sub({ sessionKey: 'other', parentSessionId: 's-crafted', subagentId: 'sa-2', projectKey: alpha }),
    sub({ sessionKey: 'bg', sessionKind: 'background_task', relativeTranscriptPath: 's-crafted.jsonl', subagentId: 'sa-1', projectKey: alpha }),
    sub({ sessionKey: 'bg', sessionKind: 'background_task', relativeTranscriptPath: 's-crafted.jsonl', subagentId: 'sa-2', projectKey: alpha }),
    { ws: 'list_sessions', params: { projectKey: alpha } },
    { ws: 'submit_turn', params: { sessionKey: 's-tool', channelKey: 'test', message: 'thanks again', projectKey: alpha } },
    { sleep: 800 },
    read({ sessionKey: 's-tool', projectKey: alpha }),
    { ws: 'submit_turn', params: { sessionKey: 's-miss', channelKey: 'test', message: 'never mind', projectKey: alpha } },
    { sleep: 800 },
    read({ sessionKey: 's-miss', projectKey: alpha }),
    { ws: 'list_sessions', params: { projectKey: alpha } },
  ],
  volatile: ['\\.requestFingerprint$'],
};
fs.writeFileSync(outPath, JSON.stringify(scenario, null, 2) + '\n');
console.log(`wrote ${outPath}: ${fs.statSync(outPath).size} bytes, crafted entries ${crafted.length}`);
