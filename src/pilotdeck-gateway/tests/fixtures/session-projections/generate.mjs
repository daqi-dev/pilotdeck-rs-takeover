// 生成 session-projections 的输入和基线答案。
// 用法（仓库根目录）：
//   node src/pilotdeck-gateway/tests/fixtures/session-projections/generate.mjs [基线目录]
// 基线目录缺省为 resources/PilotDeck，需要已构建的 dist。
//
// 输入是 bbx 场景 read-messages-fixture 里那份手写会话记录，另加几条只有投影
// 关心的记录。答案是把这些记录逐条喂给基线的 SessionProjectionDriver（与活动
// 会话相同的 drive 路径）之后 driver.checkpoint() 的原样输出。
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../../../../..');
const baseline = path.resolve(process.argv[2] ?? path.join(repo, 'resources/PilotDeck'));
const load = (relative) => import(pathToFileURL(path.join(baseline, 'dist/src', relative)).href);
const { createDefaultSessionProjectionRegistry } = await load('session/projection/BuiltinSessionProjections.js');
const { SessionProjectionDriver } = await load('session/projection/SessionProjectionDriver.js');

const scenario = JSON.parse(fs.readFileSync(path.join(repo, 'scripts/bbx/scenarios/read-messages-fixture.json'), 'utf8'));
const craftedKey = Object.keys(scenario.files).find((name) => name.endsWith('work-alpha/chats/s-crafted.jsonl'));
const crafted = scenario.files[craftedKey].split('\n').filter(Boolean).map((line) => JSON.parse(line));

let sequence = crafted.at(-1).sequence;
const entry = (turnId, body) => {
  sequence += 1;
  return { ...body, sessionId: 's-crafted', turnId, sequence, createdAt: `2026-11-01T00:02:${String(sequence % 60).padStart(2, '0')}.000Z`, entryId: `x-${sequence}` };
};
const text = (value) => ({ type: 'text', text: value });
const extras = [
  entry('t7', { type: 'context_snapshot', step: 1, runtimeContextMessages: [{ role: 'user', content: [text('<memory-context>remembered</memory-context>')] }] }),
  entry('t7', { type: 'context_snapshot', step: 2, runtimeContextMessages: [] }),
  entry('t7', { type: 'context_snapshot', step: 3 }),
  // 没有完整替换内容的边界：带 snapshot 字段的保留在会话投影里，不带的丢掉。
  entry('t7', { type: 'control_boundary', boundary: { kind: 'compact', subtype: 'compact_boundary', compactMetadata: { trigger: 'auto', postTokens: 0 }, snapshot: { version: 2, messages: [] } } }),
  entry('t7', { type: 'control_boundary', boundary: { kind: 'compact', subtype: 'compact_boundary', compactMetadata: { trigger: 'auto', preTokens: 900 } } }),
  // 带合法 snapshot 的边界：snapshot 优先于 replacementMessages。
  entry('t7', {
    type: 'control_boundary',
    boundary: {
      kind: 'compact', subtype: 'compact_boundary',
      compactMetadata: { trigger: 'manual', preTokens: 2000, postTokens: 512, messagesSummarized: 9 },
      snapshot: { version: 1, messages: [{ role: 'user', content: [text('snapshot summary')] }, { role: 'assistant', content: [{ type: 'tool_call', id: 'c-snap', name: 'grep', input: { pattern: 'y' } }] }] },
      replacementMessages: [{ role: 'user', content: [text('ignored replacement')] }],
    },
  }),
  entry('t7', { type: 'agent_status_message', event: 'context_budget', kind: 'status', text: 'context_budget', detail: { used: 700, total: 1000 } }),
  entry('t7', { type: 'agent_status_message', event: 'context_budget', kind: 'status', text: 'context_budget', detail: { used: 0, total: 1000 } }),
  entry('t7', { type: 'file_artifacts', artifacts: [{ path: 'only-diff.txt', source: 'workspace_diff' }] }),
  entry('t7', { type: 'plan_todo_progressed', toolName: 'grep' }),
  entry('t7', { type: 'plan_todo_written', mode: 'structured', merge: false, todos: [{ content: 'no id', status: 'pending' }, { id: 'z', content: 'last', status: 'in_progress' }] }),
  entry('t7', { type: 'goal_changed', revision: 3, goal: { id: 'g-2', revision: 3, objective: 'next goal', phase: 'active', blockedReason: '  ' } }),
  entry('t7', { type: 'turn_result', result: { type: 'success', stopReason: 'completed', usage: { inputTokens: 0, outputTokens: 0, cacheReadTokens: 7 }, permissionDenials: [] } }),
];
const valid = [...crafted, ...extras];
// 第二组：在第一组后面追加一条序号不连续的 goal_changed，再追加一条合法的。
// 基线的 goal 归约器在第一条上抛错，driver 重建该格子再次失败后把它删掉。
// 另加一条清空计划的记录，plan-todo 回到初始状态。
const brokenGoal = [
  ...valid,
  entry('t8', { type: 'goal_changed', revision: 9, goal: null }),
  entry('t8', { type: 'goal_changed', revision: 4, goal: null }),
  entry('t8', { type: 'plan_todo_plan_changed', plan: null }),
];

function drive(entries) {
  let commit;
  const runtime = { subscribe(listener) { commit = listener; return { active: true, dispose() {} }; } };
  const errors = [];
  const driver = new SessionProjectionDriver({
    registry: createDefaultSessionProjectionRegistry(),
    runtime,
    onError: (error, name) => errors.push({ projection: name, message: String(error?.message ?? error) }),
  });
  for (const item of entries) commit(item);
  const checkpoint = JSON.parse(JSON.stringify(driver.checkpoint()));
  driver.dispose();
  return { entries, checkpoint, errors };
}

const out = {
  about: 'Baseline SessionProjectionDriver.checkpoint() after driving the entries one by one. Regenerate with generate.mjs; do not edit by hand.',
  baseline: 'OpenBMB/PilotDeck@cc7c4e6',
  cases: { valid: drive(valid), brokenGoal: drive(brokenGoal) },
};
fs.writeFileSync(path.join(here, 'oracle.json'), JSON.stringify(out, null, 1) + '\n');
for (const [name, value] of Object.entries(out.cases)) {
  console.log(`${name}: ${value.entries.length} entries, rows: ${Object.keys(value.checkpoint).join(', ')}; errors: ${JSON.stringify(value.errors)}`);
}
