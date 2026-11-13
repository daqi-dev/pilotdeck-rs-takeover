import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { discoveryPrefixContract, discoveryPrefixPair } from './ao-discovery-prefix-policy.mjs';
const ids = ['11111111-2222-4333-8444-555555555555', 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'];
const normalize = text => text.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, '<uuid>');
function fixture(run = ids[0], unicode = false) {
  const head = 'You are running an autonomous Always-On discovery for project: /work\n';
  const prefix = head + (unicode ? '😀' : '') + 'x'.repeat(1177 - head.length - (unicode ? 2 : 0)) + '> id: plan_';
  const prompt = prefix + run + `\n> sourceRunId: ${run}\nREQUIRED BUSINESS CONTENT`;
  const sessionId = `always-on/discovery:project=/work:run=${run}`, turnId = `${run}.discovery`;
  const accepted = { type: 'accepted_input', sessionId, turnId, messages: [{ role: 'user', content: [{ type: 'text', text: prompt }] }] };
  const meta = (sequence, snapshot = false) => ({ type: 'session_metadata', sessionId, turnId, sequence, entryId: `entry-${sequence}`, metadata: { firstPrompt: prompt.slice(0, 1200), lastPrompt: prompt.slice(0, 1200), ...(snapshot ? { isSnapshot: true } : {}) } });
  const rows = [accepted, meta(2), meta(3, true)];
  const file = `/home/projects/work/chats/always-on-discovery:project=-work:run=${run}.jsonl`;
  const projection = { format: 'pilotdeck.session-projections', sessionId, asOfSequence: 3, anchor: Object.fromEntries(['entryId', 'turnId', 'sequence', 'type'].map(key => [key, rows[2][key]])), projections: { 'agent-transcript.metadata': { asOfSequence: 3, state: structuredClone(rows[2].metadata) } } };
  const request = { method: 'POST', url: '/v1/chat/completions', body: { messages: [{ role: 'user', content: prompt }] } };
  return { scenario: 'always-on-fire', tree: { [file]: { jsonl: rows }, [`${file}.projections.json`]: { json: projection }, '/home/always-on/projects/work/state.json': { json: { lastRunId: run } } }, provider: { discovery: [request], 'discovery-done': [structuredClone(request)] } };
}
const parts = raw => { const [file, entry] = Object.entries(raw.tree).find(([key]) => key.endsWith('.jsonl')); return { file, rows: entry.jsonl, projection: raw.tree[`${file}.projections.json`].json }; };
function mutatePrompt(raw, transform) {
  const { rows, projection } = parts(raw);
  const prompt = transform(rows[0].messages[0].content[0].text);
  rows[0].messages[0].content[0].text = prompt;
  for (const tag of ['discovery', 'discovery-done']) raw.provider[tag][0].body.messages[0].content = prompt;
  for (const owner of [rows[1].metadata, rows[2].metadata, projection.projections['agent-transcript.metadata'].state]) for (const key of ['firstPrompt', 'lastPrompt']) owner[key] = prompt.slice(0, 1200);
}
test('six descriptors, same run aliases, raw immutability and UTF16 surrogate units', () => {
  for (const unicode of [false, true]) {
    const a = fixture(ids[0], unicode), b = fixture(ids[1], unicode), before = JSON.stringify([a, b]);
    const pair = discoveryPrefixPair(a, b, normalize);
    for (const [i, raw] of [a, b].entries()) {
      const { rows, projection } = parts(raw);
      for (const owner of [rows[1].metadata, rows[2].metadata, projection.projections['agent-transcript.metadata'].state]) {
        assert.equal(pair[i].get(owner).size, 2);
        assert.equal(pair[i].get(owner).get('firstPrompt').length, 12);
      }
    }
    assert.equal(JSON.stringify([a, b]), before);
  }
});
test('business changes before and after cap rejected even when all own slices match', () => {
  for (const change of [s => s.replace('xxx', 'xxY'), s => s.replace('REQUIRED', 'REMOVED')]) {
    const b = fixture(ids[1]); mutatePrompt(b, change);
    assert.throws(() => discoveryPrefixPair(fixture(), b, normalize), /complete discovery prompts/);
  }
});
test('wrong state run, source run and visible fragment rejected', () => {
  const a = fixture(); a.tree['/home/always-on/projects/work/state.json'].json.lastRunId = ids[1];
  assert.throws(() => discoveryPrefixContract(a));
  const b = fixture(); mutatePrompt(b, s => s.replace(`sourceRunId: ${ids[0]}`, `sourceRunId: ${ids[1]}`)); assert.throws(() => discoveryPrefixContract(b));
  const c = fixture(); parts(c).rows[1].metadata.firstPrompt += 'f'; assert.throws(() => discoveryPrefixContract(c));
});
test('1199/1201 listing lengths and codepoint instead of UTF16 cap rejected', () => {
  for (const size of [1199, 1201]) { const a = fixture(); parts(a).rows[1].metadata.firstPrompt = parts(a).rows[0].messages[0].content[0].text.slice(0, size); assert.throws(() => discoveryPrefixContract(a)); }
  const unicode = fixture(ids[0], true); parts(unicode).rows[1].metadata.firstPrompt = Array.from(parts(unicode).rows[0].messages[0].content[0].text).slice(0, 1200).join(''); assert.throws(() => discoveryPrefixContract(unicode));
});
test('missing and duplicate accepted/provider sources rejected', () => {
  for (const change of [a => parts(a).rows.shift(), a => parts(a).rows.push(parts(a).rows[0]), a => delete a.provider.discovery, a => a.provider.discovery.push(a.provider.discovery[0]), a => a.provider.discovery[0].body.messages[0].content += 'bad']) {
    const a = fixture(); change(a); assert.throws(() => discoveryPrefixContract(a));
  }
});
test('missing projection, wrong anchor and projection slice rejected', () => {
  for (const change of [a => delete a.tree[`${parts(a).file}.projections.json`], a => parts(a).projection.anchor.entryId = 'other', a => parts(a).projection.projections['agent-transcript.metadata'].state.lastPrompt += 'x']) { const a = fixture(); change(a); assert.throws(() => discoveryPrefixContract(a)); }
});
test('distinct full IDs with colliding visible prefixes never merge; noncrossing UUID rejected', () => {
  const a = fixture(); mutatePrompt(a, s => s + '\n11111111-2229-4999-8999-999999999999'); assert.throws(() => discoveryPrefixContract(a), /unambiguous prefix/);
  const b = fixture(); mutatePrompt(b, s => 'x' + s); assert.throws(() => discoveryPrefixContract(b));
});
test('real diff CLI consumes descriptors; preserves ordinary changes and rejects bad association', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ao-prefix-test-'));
  try {
    const run = (a, b) => {
      for (const [name, value] of [['a', a], ['b', b]]) fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify(value));
      return spawnSync(process.execPath, [fileURLToPath(new URL('./diff.mjs', import.meta.url)), path.join(dir, 'a.json'), path.join(dir, 'b.json'), '--self'], { encoding: 'utf8', env: { ...process.env, HOME: dir, PILOT_HOME: dir } });
    };
    const positive = run(fixture(), fixture(ids[1])); assert.equal(positive.status, 0, positive.stderr + positive.stdout); assert.match(positive.stdout, /SAME/);
    const bad = fixture(ids[1]); bad.provider.discovery[0].headers = { unexpected: 'business' };
    const diff = run(fixture(), bad); assert.equal(diff.status, 1); assert.match(diff.stdout, /DIFF/);
    const invalid = fixture(ids[1]); parts(invalid).rows[1].metadata.lastPrompt += 'bad'; assert.equal(run(fixture(), invalid).status, 2);
    const business = fixture(ids[1]); mutatePrompt(business, s => s + 'changed'); assert.equal(run(fixture(), business).status, 2);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
