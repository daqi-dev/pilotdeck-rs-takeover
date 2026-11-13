import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { lifecycleContract, lifecyclePair } from './observer-lifecycle-policy.mjs';
const load = () => structuredClone(JSON.parse(fs.readFileSync(new URL('./fixtures/observer-lifecycle-policy/paired-contract.json', import.meta.url))).pair);
const parent = a => Object.entries(a.tree).find(([p]) => p.endsWith('/chats/agent03-parent.jsonl'));
const reg = a => Object.values(a.tree).find(v => v.json?.events && v.json?.records).json;
const reviewer = a => parent(a)[1].jsonl.find(r => r.type === 'subagent_started' && r.subagentType === 'reviewer');
const events = a => a.steps[0].frames.filter(f => ['subagent_text_delta', 'subagent_assistant_block', 'subagent_completed'].includes(f.event?.event) && f.event.detail?.subagentId === reviewer(a).subagentId);
function projectedEvent(a, index, edit) {
 const f = a.steps[0].frames[index]; edit(f.event);
 const key = '\0agent03-parent\0' + a.steps[0].frames[0].event.runId;
 edit(reg(a).events[key][index].event);
}
function fixParentProjection(a) {
 const [p, file] = parent(a), rows = file.jsonl;
 rows.forEach((row, i) => { row.sequence = i + 1; if (i) row.parentEntryId = rows[i - 1].entryId; });
 const byId = new Map(rows.map(row => [row.entryId, row]));
 function walk(v) { if (Array.isArray(v)) return v.forEach(walk); if (!v || typeof v !== 'object') return; if (v.entryId && v.sequence && byId.has(v.entryId)) for (const key of ['sequence', 'parentEntryId', 'turnId', 'sessionId', 'type', 'createdAt']) if (Object.hasOwn(v, key)) v[key] = byId.get(v.entryId)[key]; Object.values(v).forEach(walk); }
 walk(a.tree[p + '.projections.json'].json);
}
for (const name of ['synthetic fixed-scene pair']) test(`actual ${name} lifecycle groups validated; no raw mutation or formal SAME claim`, () => {
 const [a, b] = load(name), before = JSON.stringify([a, b]), p = lifecyclePair(a, b);
 assert.equal(p.approved, false); assert.equal(p.formalSame, false);
 assert.equal(p.left.startWindow, 'before-tool-result'); assert.equal(p.right.startWindow, 'after-step-completed');
 assert.equal(p.left.completionWindow, 3); assert.equal(p.right.completionWindow, 0);
 assert.equal(p.comparisons.completed.length, 3); assert.ok(p.nonTargetComparisons.length > 20);
 assert.equal(JSON.stringify([a, b]), before);
});
const faults = [
 ['missing reviewer event', a => { a.steps[0].frames.splice(a.steps[0].frames.indexOf(events(a)[0]), 1); }],
 ['duplicate reviewer event', a => { a.steps[0].frames.splice(a.steps[0].frames.indexOf(events(a)[0]), 0, structuredClone(events(a)[0])); }],
 ['wrong child', a => { events(a)[0].event.detail.subagentId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'; }],
 ['wrong producer block', a => { const i = a.steps[0].frames.indexOf(events(a)[0]); projectedEvent(a, i, e => e.timeline.id = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee:text:0'); }],
 ['extra target payload field', a => { const i = a.steps[0].frames.indexOf(events(a)[0]); projectedEvent(a, i, e => e.detail.unapproved = true); }],
 ['wrong session', a => { reviewer(a).sessionId = 'agent03-second'; }],
 ['wrong actual run', a => { const i = a.steps[0].frames.indexOf(events(a)[0]); projectedEvent(a, i, e => e.runId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'); }],
 ['wrong reviewer text in both wire and registry', a => { const i = a.steps[0].frames.indexOf(events(a)[0]); projectedEvent(a, i, e => e.detail.text = 'WRONG'); }],
 ['registry seq gap', a => { const key = Object.keys(reg(a).events)[0]; reg(a).events[key][2].seq += 2; }],
 ['history missing witness', a => { delete a.steps[6].frames[0].result.stream; }],
 ['missing child terminal witness', a => { const r = reviewer(a); const value = Object.entries(a.tree).find(([p, v]) => p.endsWith('/' + r.transcriptRelativePath) && v.jsonl)[1]; value.jsonl.at(-1).type = 'not-terminal'; }],
 ['start crosses tool_result_message', a => { const rows = parent(a)[1].jsonl, i = rows.indexOf(reviewer(a)); const [r] = rows.splice(i, 1); rows.splice(i + 1, 0, r); fixParentProjection(a); }],
 ['three events reordered with internally consistent registry', a => { const ev = events(a), indexes = ev.map(f => a.steps[0].frames.indexOf(f)); const key = '\0agent03-parent\0' + a.steps[0].frames[0].event.runId; [a.steps[0].frames[indexes[0]], a.steps[0].frames[indexes[1]]] = [a.steps[0].frames[indexes[1]], a.steps[0].frames[indexes[0]]]; a.steps[0].frames.forEach((f, i) => f.seq = i); for (const i of indexes) reg(a).events[key][i].event = structuredClone(a.steps[0].frames[i].event); }],
 ['non-target parent body changed despite consistent wire registry', a => { const i = a.steps[0].frames.findIndex(f => f.event.type === 'assistant_text_delta'); projectedEvent(a, i, e => e.text = 'WRONG'); }],
 ['non-target event ordering changed', a => { const fs = a.steps[0].frames; [fs[0], fs[1]] = [fs[1], fs[0]]; fs.forEach((f, i) => f.seq = i); const key = '\0agent03-parent\0' + fs[0].event.runId; reg(a).events[key][0].event = structuredClone(fs[0].event); reg(a).events[key][1].event = structuredClone(fs[1].event); }],
 ['non-target durable order changed', a => { const rows = parent(a)[1].jsonl; [rows[0], rows[1]] = [rows[1], rows[0]]; fixParentProjection(a); }],
 ['fixed scene hash missing', a => { delete a.evidence.scenario.sha256; }],
 ['tool schema/arguments changed', a => { a.provider['child-beta'][0].body.tools[0].function.parameters = { type: 'object', forbidden: true }; }],
 ['complete digest body change', a => { a.provider.observer[0].body.messages[1].content += '\nWRONG'; }],
];
for (const [name, mutate] of faults) test(`reject ${name}`, () => { const [ts, rust] = load('fork'); mutate(rust); assert.throws(() => lifecyclePair(ts, rust)); });
function runDiff(mutate) {
 const pair = load(); if (mutate) mutate(pair);
 const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'observer-lifecycle-'));
 try {
  pair.forEach((value, i) => fs.writeFileSync(path.join(dir, `${i}.json`), JSON.stringify(value)));
  return spawnSync(process.execPath, [fileURLToPath(new URL('./fixtures/observer-lifecycle-policy/layer-cli.mjs', import.meta.url)), path.join(dir, '0.json'), path.join(dir, '1.json'), '--max', '2000'], { encoding: 'utf8', env: { ...process.env, HOME: dir, PILOT_HOME: path.join(dir, 'pilot') } });
 } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}
test('lifecycle layer CLI maps only proven leaves, preserves alias residuals and reports typed expansion', () => {
 const r = runDiff(); assert.equal(r.status, 1, r.stderr); assert.match(r.stdout, /COMPATIBILITY-VALUE observerLifecycleWindows/); assert.match(r.stdout, /LIFECYCLE-LEAF-EXPANSION/); assert.match(r.stdout, /^  \$[^\n]*\.entryId\n/m); assert.match(r.stdout, /DIFF/);
});
test('unknown field in an extra non-target record remains a leaf DIFF', () => {
 const r = runDiff(([ts]) => { ts.steps[3].frames.at(-1).unapprovedField = 'MUST-STAY-DIFF'; });
 assert.equal(r.status, 1, r.stderr); assert.match(r.stdout, /^  \$\.steps\[3\]\.frames\[11\]\.unapprovedField\n/m); assert.match(r.stdout, /MUST-STAY-DIFF/);
});
test('lifecycle layer CLI rejects missing registry witness as INVALID', () => {
 const r = runDiff(([, rust]) => { delete reg(rust).events[Object.keys(reg(rust).events)[0]]; });
 assert.equal(r.status, 2); assert.match(r.stderr, /INVALID observer lifecycle/);
});
test('lifecycle layer CLI rejects unknown target payload instead of exempting its record', () => {
 const r = runDiff(([, rust]) => { const i = rust.steps[0].frames.indexOf(events(rust)[0]); projectedEvent(rust, i, e => e.detail.unapproved = true); });
 assert.equal(r.status, 2); assert.match(r.stderr, /INVALID observer lifecycle/);
});
test('typed extra-record expansion preserves unknown object keys, array length and every value', () => {
 const r = runDiff(([ts]) => { ts.steps[3].frames.at(-1).unknownNested = { list: [101, 202] }; });
 assert.equal(r.status, 1, r.stderr);
 for (const suffix of ['unknownNested.<type>', 'unknownNested.<key order>', 'unknownNested.list.<type>', 'unknownNested.list.length', 'unknownNested.list[0]', 'unknownNested.list[1]']) assert.ok(r.stdout.includes(suffix), suffix);
 assert.match(r.stdout, /101/); assert.match(r.stdout, /202/);
});
test('existing extra-record payload value type change is not hidden by its lifecycle position', () => {
 const r = runDiff(([ts]) => { ts.steps[3].frames.at(-1).event.usage = ['UNAPPROVED-TYPE']; });
 assert.equal(r.status, 1, r.stderr); assert.match(r.stdout, /\.event\.usage\.<type>/); assert.match(r.stdout, /UNAPPROVED-TYPE/);
});
test('proved nested missing observer detail and timeline expand and accept only proved leaves', () => {
 const r = runDiff(); assert.equal(r.status, 1, r.stderr);
 for (const suffix of ['event.detail.subagentId', 'event.detail.text', 'event.timeline.version']) assert.match(r.stdout, new RegExp('COMPATIBILITY-VALUE observerLifecycleWindows \\$\\.steps\\[0\\]\\.frames\\[27\\]\\.' + suffix.replaceAll('.', '\\.')));
 assert.doesNotMatch(r.stdout, /^  \$\.steps\[0\]\.frames\[27\]\.event\.(?:detail|timeline)\n/m);
});
test('nested missing non-target object preserves extra keys, array length, type and leaf values', () => {
 const r = runDiff(([ts]) => {
  const frames = ts.steps[0].frames, i = frames.findLastIndex(f => f.event.type === 'context_budget');
  const key = '\0agent03-parent\0' + frames[0].event.runId;
  for (const e of [frames[i].event, reg(ts).events[key][i].event]) { e.breakdown.unknownNested = { list: [17, 18] }; e.breakdown.messages = 'UNAPPROVED-NESTED-TYPE'; }
 });
 assert.equal(r.status, 1, r.stderr);
 for (const suffix of ['event.breakdown.unknownNested.<key order>', 'event.breakdown.unknownNested.list.length', 'event.breakdown.unknownNested.list[0]', 'event.breakdown.unknownNested.list[1]', 'event.breakdown.messages']) assert.ok(r.stdout.includes(suffix), suffix);
 assert.match(r.stdout, /UNAPPROVED-NESTED-TYPE/);
});
test('spawn plus stdout pipe drains a large proof report through its final verdict', () => {
 const r = runDiff(); assert.equal(r.error, undefined); assert.equal(r.signal, null); assert.equal(r.status, 1, r.stderr);
 assert.ok(Buffer.byteLength(r.stdout) > 128 * 1024, 'genuine pipe backpressure-sized proof report');
 assert.match(r.stdout.trimEnd(), /\nDIFF \d+$/); assert.equal(r.stderr, '');
});
