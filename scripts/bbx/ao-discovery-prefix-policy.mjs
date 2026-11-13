// Only the generated AO discovery run UUID cut by the listing's 1200 UTF16 cap.
// No raw artifact is changed; descriptors reference the validated metadata owners.
import assert from 'node:assert/strict';
const SCENARIOS = new Set(['always-on-fire','always-on-fire-execfail']);
const UUID_SOURCE = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}';
const one = (xs, why) => { assert.equal(xs.length, 1, why); return xs[0]; };
export function discoveryPrefixContract(raw) {
  assert.ok(SCENARIOS.has(raw.scenario), 'explicit AO discovery scenario required');
  const [file, entry] = one(Object.entries(raw.tree ?? {}).filter(([name, value]) =>
    /\/chats\/always-on-discovery:project=.*:run=.*\.jsonl$/.test(name) && Array.isArray(value.jsonl)), 'unique discovery transcript');
  const run = file.match(new RegExp(`:run=(${UUID_SOURCE})\\.jsonl$`))?.[1];
  assert.ok(run, 'full run UUID');
  const accepted = one(entry.jsonl.filter(row => row.type === 'accepted_input'), 'unique accepted input');
  assert.equal(accepted.turnId, `${run}.discovery`);
  assert.ok(accepted.sessionId?.startsWith('always-on/discovery:project='));
  assert.ok(accepted.sessionId.endsWith(`:run=${run}`));
  assert.equal(accepted.messages?.length, 1);
  assert.equal(accepted.messages[0].role, 'user');
  assert.equal(accepted.messages[0].content?.length, 1);
  assert.equal(accepted.messages[0].content[0].type, 'text');
  const prompt = accepted.messages[0].content[0].text;
  assert.equal(typeof prompt, 'string');
  assert.ok(prompt.startsWith('You are running an autonomous Always-On discovery for project: '));
  assert.ok(prompt.length > 1200);
  const spans = [...prompt.matchAll(new RegExp(UUID_SOURCE, 'g'))];
  const cut = one(spans.filter(match => match.index < 1200 && match.index + match[0].length > 1200), 'one full UUID crosses UTF16 cap');
  assert.equal(cut[0], run);
  assert.equal(prompt.slice(cut.index - '> id: plan_'.length, cut.index), '> id: plan_');
  assert.equal(prompt.split(`> id: plan_${run}`).length, 2, 'unique generated plan reference');
  assert.equal(prompt.split(`> sourceRunId: ${run}`).length, 2, 'matching source run');
  // Another full UUID with the same visible prefix cannot supply this reference.
  const visible = prompt.slice(cut.index, 1200);
  assert.ok(spans.every(match => !match[0].startsWith(visible) || match[0] === run), 'unambiguous prefix');
  const states = Object.entries(raw.tree).filter(([name]) => /\/always-on\/projects\/[^/]+\/state\.json$/.test(name));
  assert.equal(one(states, 'unique AO state')[1].json?.lastRunId, run);
  for (const tag of ['discovery', 'discovery-done']) {
    const request = one(raw.provider?.[tag] ?? [], `unique ${tag} request`);
    assert.equal(request.method, 'POST');
    assert.equal(request.url, '/v1/chat/completions');
    const user = one((request.body?.messages ?? []).filter(message => message.role === 'user'), 'unique provider user');
    assert.equal(user.content, prompt, 'actual model prompt equals durable input');
  }
  const metadata = entry.jsonl.filter(row => row.type === 'session_metadata' && ('firstPrompt' in (row.metadata ?? {}) || 'lastPrompt' in (row.metadata ?? {})));
  assert.equal(metadata.length, 2, 'initial and final listing metadata');
  assert.equal(metadata.filter(row => row.metadata.isSnapshot === true).length, 1);
  const projection = raw.tree[`${file}.projections.json`]?.json;
  assert.equal(projection?.format, 'pilotdeck.session-projections');
  assert.equal(projection?.sessionId, accepted.sessionId);
  const last = metadata.find(row => row.metadata.isSnapshot === true);
  for (const key of ['entryId', 'turnId', 'sequence', 'type']) assert.equal(projection.anchor?.[key], last[key], `projection anchor ${key}`);
  assert.equal(projection.asOfSequence, last.sequence);
  const projected = projection.projections?.['agent-transcript.metadata'];
  assert.equal(projected?.asOfSequence, last.sequence);
  const descriptors = new WeakMap();
  for (const row of metadata) {
    assert.equal(row.sessionId, accepted.sessionId);
    assert.equal(row.turnId, accepted.turnId);
  }
  for (const owner of [...metadata.map(row => row.metadata), projected?.state]) {
    assert.ok(owner && typeof owner === 'object', 'metadata projection present');
    const fields = new Map();
    for (const key of ['firstPrompt', 'lastPrompt']) {
      assert.equal(owner[key], prompt.slice(0, 1200), `exact UTF16 slice ${key}`);
      fields.set(key, { prefix: prompt.slice(0, cut.index), run, length: visible.length });
    }
    descriptors.set(owner, fields);
  }
  return { prompt, descriptors };
}
export function discoveryPrefixPair(left, right, normalize) {
  if (!SCENARIOS.has(left.scenario) && !SCENARIOS.has(right.scenario)) return [new WeakMap(), new WeakMap()];
  const a = discoveryPrefixContract(left), b = discoveryPrefixContract(right);
  assert.equal(normalize(a.prompt), normalize(b.prompt), 'complete discovery prompts differ beyond existing UUID/time rules');
  return [a.descriptors, b.descriptors];
}
