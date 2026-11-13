import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { observerUuidViews } from './observer-uuid-policy.mjs';
const fixture = JSON.parse(fs.readFileSync(new URL('./fixtures/observer-lifecycle-policy/paired-contract.json', import.meta.url))).pair;
const cases = process.env.BBX_UUID_REAL_CASES ? Object.fromEntries(Object.entries(JSON.parse(fs.readFileSync(process.env.BBX_UUID_REAL_CASES))).map(([name, files]) => [name, files.map(file => JSON.parse(fs.readFileSync(file)))])) : { synthetic: fixture };
const load = () => structuredClone(Object.values(cases)[0]);
function normalize(raw) {
 const aliases = new Map();
 const string = s => s.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, id => { id = id.toLowerCase(); if (!aliases.has(id)) aliases.set(id, aliases.size + 1); return `<uuid:${aliases.get(id)}>`; });
 const walk = v => typeof v === 'string' ? string(v) : Array.isArray(v) ? v.map(walk) : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, value]) => [string(k), walk(value)])) : v;
 const input = Object.fromEntries(Object.entries(raw).filter(([k]) => !['side', 'evidence', 'startMs', 'serverLog', 'failures'].includes(k)));
 return { view: walk(input), aliases };
}
function apply(pair, alter) {
 const normalized = pair.map(normalize); if (alter) alter(normalized);
 return observerUuidViews(...pair, ...normalized.map(v => v.view), ...normalized.map(v => v.aliases));
}
for (const [name, pair] of Object.entries(cases)) test(name + ': eight raw/view/alias-bound entities, no raw mutation/freeze', () => {
 const inputs = structuredClone(pair), before = JSON.stringify(inputs), result = apply(inputs);
 assert.equal(result.entityTokens.length, 8); assert.ok(result.bindings.every(b => b.raw.length === 64 && b.normalized.length === 64 && b.aliases.length === 64));
 assert.equal(JSON.stringify(inputs), before); assert.equal(Object.isFrozen(inputs[0].steps[0].frames[0]), false);
});
for (const [name, mutate] of [
 ['wrong run', a => a.steps[0].frames.find(f => f.event.type === 'assistant_block').event.runId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'],
 ['wrong child', a => a.steps[0].frames.find(f => f.event.event === 'subagent_assistant_block' && f.event.detail.text === 'AG03-OBSERVER-REPORT').event.detail.subagentId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'],
 ['duplicate block', a => a.steps[0].frames.push(structuredClone(a.steps[0].frames.find(f => f.event.type === 'assistant_block')))],
 ['wrong durable block', a => { const rows = Object.entries(a.tree).find(([p]) => p.endsWith('/chats/agent03-parent.jsonl'))[1].jsonl; rows.find(r => r.type === 'assistant_message' && r.message.content.some(b => b.text === 'AG03-PARENT-DONE')).message.content[0].blockId = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee:text:0'; }],
]) test('reject ' + name, () => { const pair = load(); mutate(pair[1]); assert.throws(() => apply(pair)); });
test('reject many-to-one even with all raw references consistently rewritten', () => {
 const pair = load(), r = pair[1], parent = r.steps[0].frames.find(f => f.event.type === 'assistant_block').event.blockId.split(':')[0], reviewer = r.steps[0].frames.find(f => f.event.event === 'subagent_assistant_block' && f.event.detail.text === 'AG03-OBSERVER-REPORT').event.detail.blockId.split(':')[0];
 pair[1] = JSON.parse(JSON.stringify(r).replaceAll(parent, reviewer)); assert.throws(() => apply(pair), /duplicate|many-to-one/);
});
test('unknown aliases, values and array insertion order remain exact', () => {
 const pair = load(); for (const a of pair) a.unknown = { list: ['before', 'after'], value: 'UNPROVED' };
 const normalized = pair.map(normalize), result = observerUuidViews(...pair, ...normalized.map(v => v.view), ...normalized.map(v => v.aliases));
 for (const side of [0, 1]) { assert.deepEqual(result.views[side].unknown, normalized[side].view.unknown); const parent = Object.entries(normalized[side].view.tree).find(([p]) => p.endsWith('/chats/agent03-parent.jsonl'))[0]; assert.equal(result.views[side].tree[parent].jsonl[0].entryId, normalized[side].view.tree[parent].jsonl[0].entryId); }
});
test('reject forged alias table and transplanted normalized anchor', () => {
 const pair = load(); assert.throws(() => apply(pair, n => { const entries = [...n[0].aliases]; n[0].aliases.set(entries[0][0], entries[1][1]); }), /unique/);
 assert.throws(() => apply(pair, n => { const p = Object.entries(n[0].view.tree).find(([p]) => p.endsWith('/chats/agent03-parent.jsonl'))[0]; n[0].view.tree[p].jsonl.find(r => r.type === 'accepted_input').turnId = '<uuid:999999>'; }), /binding/);
});
test('reject raw transplant after normalized views and alias tables were built', () => {
 const pair = load(), n = pair.map(normalize), r = pair[0], worker = Object.values(r.tree).find(v => v.jsonl?.some(row => row.type === 'subagent_started' && row.subagentType === 'worker')).jsonl.find(row => row.type === 'subagent_started' && row.subagentType === 'worker').subagentId;
 pair[0] = JSON.parse(JSON.stringify(r).replaceAll(worker, 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'));
 assert.throws(() => observerUuidViews(...pair, ...n.map(v => v.view), ...n.map(v => v.aliases)));
});
test('reject literal alias spoofing and reserved-token collision', () => {
 const pair = load(), n = pair.map(normalize), token = `<uuid:${n[0].aliases.get(pair[0].steps[0].frames[0].event.runId)}>`;
 pair[0].literalAlias = token; assert.throws(() => apply(pair), /reference binding/);
 const fresh = load(), normalized = fresh.map(normalize), next = Math.max(...normalized[0].aliases.values(), ...normalized[1].aliases.values()) + 1;
 fresh[0].unknownLiteral = `<uuid:${next}>`; assert.throws(() => apply(fresh), /collision/);
});
test('non-scene inputs retain exact view and alias object identities', () => {
 const a = { scenario: 'other' }, b = { scenario: 'other' }, av = { values: [1, 2] }, bv = { values: [2, 1] }, am = new Map(), bm = new Map();
 const r = observerUuidViews(a, b, av, bv, am, bm); assert.equal(r.views[0], av); assert.equal(r.views[1], bv); assert.equal(r.aliases[0], am); assert.equal(r.aliases[1], bm);
});
