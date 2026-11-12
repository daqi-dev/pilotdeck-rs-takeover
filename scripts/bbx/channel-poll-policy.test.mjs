import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { channelPollContract, channelPollVolatilePaths } from './channel-poll-policy.mjs';
const declaration = { policy: 'coreChannelPollAfter' };
const session = 'bluebubbles:chat=core-chat:general';
const clone = (value) => structuredClone(value);
function fixture(shift = 0) {
  const initial = 1790000000 + shift, recovered = initial + 9;
  return {
    scenario: 'backend-core-channel-recovery', evidence: { scenario: { name: 'backend-core-channel-recovery', sha256: 'a'.repeat(64) } },
    steps: [8000, 5000].map((sleep) => ({ step: { serve: ['server', '--host', '127.0.0.1', '--port', '38411'], timeout: 20000, until: 'bluebubbles: polling ', then: [{ sleep }] }, serve: { reached: true, exitCode: 0, signal: null }, then: [{ step: { sleep }, slept: true }] })),
    provider: { 'channel-poll': Array.from({ length: 7 }, (_, i) => ({ method: 'GET', url: `/api/v1/message?password=core-channel-password&after=${i < 4 ? initial : recovered}&limit=50`, headers: { host: '127.0.0.1:38412', accept: '*/*' }, body: { raw: '' } })) },
    tree: { [`/home/.pilotdeck/projects/fixture/chats/${session}.jsonl`]: { mode: '600', jsonl: [
      { type: 'accepted_input', sessionId: session, createdAt: new Date((initial + 5) * 1000 + 750).toISOString(), messages: [{ role: 'user', content: [{ type: 'text', text: 'CORE-BEGIN-{saved.channelText}' }] }] },
      { type: 'accepted_input', sessionId: session, createdAt: new Date((recovered + 5) * 1000 + 500).toISOString(), messages: [{ role: 'user', content: [{ type: 'text', text: 'CORE-BEGIN-CORE-RECOVER' }] }] },
    ] } },
  };
}
const records = (raw) => raw.provider['channel-poll'];
const inputs = (raw) => Object.values(raw.tree)[0].jsonl;
// A plain structural comparator demonstrates what the exported Map can exempt.
// It deliberately does not normalize or ignore any other captured field.
function differences(a, b, path = '$') {
  if (Object.is(a, b)) return [];
  if (Array.isArray(a) && Array.isArray(b)) return a.length !== b.length ? [path] : a.flatMap((value, i) => differences(value, b[i], `${path}[${i}]`));
  if (a && b && typeof a === 'object' && typeof b === 'object') return [...new Set([...Object.keys(a), ...Object.keys(b)])].flatMap((key) => differences(a[key], b[key], `${path}.${key}`));
  return [path];
}
function survivingProviderDiffs(a, b) {
  const allowed = channelPollVolatilePaths(a, b, declaration);
  return differences(a.provider, b.provider, '$.provider').filter((path) => !allowed.has(path));
}

test('only the seven phase-bound after values receive exact URL paths; raw inputs remain immutable', () => {
  const a = fixture(), b = fixture(15), before = JSON.stringify([a, b]);
  const contract = channelPollContract(a, declaration);
  assert.deepEqual(contract.phases.map(({ phase, ageMs, maxAgeMs }) => [phase, ageMs, maxAgeMs]), [['initial', 750, 28999], ['recovered', 500, 25999]]);
  const allowed = channelPollVolatilePaths(a, b, declaration);
  assert.equal(allowed.size, 7);
  for (let i = 0; i < 7; i += 1) assert.match(allowed.get(`$.provider.channel-poll[${i}].url`), /only after decimal value varies/);
  assert.deepEqual(survivingProviderDiffs(a, b), []);
  assert.equal(JSON.stringify([a, b]), before);
  assert.equal(channelPollVolatilePaths(a, a, declaration).size, 0);
});

test('declaration and real scene identity fail closed without arbitrary tolerance settings', () => {
  for (const bad of [undefined, {}, { policy: 'other' }, { ...declaration, maxAgeMs: 999999 }, { ...declaration, config: {} }]) assert.throws(() => channelPollContract(fixture(), bad));
  for (const mutate of [raw => { raw.scenario = 'other'; }, raw => { delete raw.evidence; }, raw => { raw.evidence.scenario.name = 'other'; }, raw => { raw.evidence.scenario.sha256 = 'bad'; }]) {
    const raw = fixture(); mutate(raw); assert.throws(() => channelPollContract(raw, declaration));
  }
  const different = fixture(15); different.evidence.scenario.sha256 = 'b'.repeat(64);
  assert.throws(() => channelPollVolatilePaths(fixture(), different, declaration), /different scenario\/assertion identities/);
});

for (const [name, mutate] of [
  ['password', r => { r.url = r.url.replace('core-channel-password', 'wrong-password'); }],
  ['path', r => { r.url = r.url.replace('/api/v1/message?', '/api/v1/messages?'); }],
  ['extra query', r => { r.url += '&extra=1'; }],
  ['duplicate query', r => { r.url = r.url.replace('limit=50', 'after=1790000000'); }],
  ['limit', r => { r.url = r.url.replace('limit=50', 'limit=51'); }],
  ['encoded password spelling', r => { r.url = r.url.replace('core-channel-password', 'core%2Dchannel-password'); }],
  ['wrong method', r => { r.method = 'POST'; }],
  ['nonempty body', r => { r.body.raw = 'unexpected'; }],
  ['extra body field', r => { r.body.extra = true; }],
]) test(`${name} regression is rejected rather than exempted`, () => {
  const b = fixture(15); mutate(records(b)[0]); assert.throws(() => channelPollVolatilePaths(fixture(), b, declaration));
});

test('query ordering is preserved byte-for-byte; changing order cannot hide behind after', () => {
  const a = fixture(), b = fixture(15);
  const epoch = channelPollContract(b, declaration).phases[0].epoch;
  records(b)[0].url = `/api/v1/message?after=${epoch}&password=core-channel-password&limit=50`;
  assert.equal(channelPollVolatilePaths(a, b, declaration).size, 6);
  assert.deepEqual(survivingProviderDiffs(a, b), ['$.provider.channel-poll[0].url']);
});

test('headers, record fields and indices are still compared normally', () => {
  for (const [mutate, expected] of [
    [b => { records(b)[0].headers.accept = 'application/json'; }, '$.provider.channel-poll[0].headers.accept'],
    [b => { records(b)[2].headers.extra = 'changed'; }, '$.provider.channel-poll[2].headers.extra'],
    [b => { records(b)[1].unexpected = 1; }, '$.provider.channel-poll[1].unexpected'],
    [b => { records(b)[1].index = 99; }, '$.provider.channel-poll[1].index'],
  ]) { const a = fixture(), b = fixture(15); mutate(b); assert.deepEqual(survivingProviderDiffs(a, b), [expected]); }
});

for (const [name, mutate] of [
  ['poll count missing', raw => { records(raw).pop(); }],
  ['poll count added', raw => { records(raw).push(clone(records(raw)[0])); }],
  ['wrong 3+4 phase split', raw => { records(raw)[3].url = records(raw)[4].url; }],
  ['phase epoch reused', raw => { for (let i = 4; i < 7; i += 1) records(raw)[i].url = records(raw)[0].url; }],
  ['recovered epoch goes backward', raw => { for (const record of records(raw).slice(4)) record.url = record.url.replace(/after=\d+/, 'after=1789999999'); }],
  ['phases swapped', raw => { [records(raw)[0], records(raw)[4]] = [records(raw)[4], records(raw)[0]]; }],
  ['epoch drift within phase', raw => { records(raw)[1].url = records(raw)[1].url.replace('1790000000', '1790000001'); }],
  ['missing durable input', raw => { inputs(raw).pop(); }],
  ['duplicate durable input', raw => { inputs(raw).push(clone(inputs(raw)[0])); }],
  ['wrong durable phase', raw => { inputs(raw)[0].messages[0].content[0].text = 'CORE-BEGIN-CORE-RECOVER'; }],
  ['wrong durable session', raw => { inputs(raw)[0].sessionId = 'other'; }],
  ['backward durable time', raw => { inputs(raw)[1].createdAt = inputs(raw)[0].createdAt; }],
  ['startup budget enlarged', raw => { raw.steps[0].step.timeout = 999999; }],
  ['phase wait enlarged', raw => { raw.steps[1].step.then[0].sleep = 999999; }],
  ['server failed', raw => { raw.steps[0].serve.exitCode = 1; }],
  ['wait not actually completed', raw => { raw.steps[0].then[0].slept = false; }],
  ['actual wait budget changed', raw => { raw.steps[0].then[0].step.sleep = 1; }],
]) test(`${name} is a contract failure`, () => { const raw = fixture(); mutate(raw); assert.throws(() => channelPollContract(raw, declaration)); });

for (const epoch of ['0', '1', '0001790000000', '-1', '1.5', '1e9', '1790000000000', '9007199254740993', '999999999999999999999999999']) test(`extreme/noncanonical after ${epoch} cannot be normalized`, () => {
  const raw = fixture(); for (const record of records(raw).slice(0, 4)) record.url = record.url.replace(/after=\d+/, `after=${epoch}`);
  assert.throws(() => channelPollContract(raw, declaration));
});

test('validity ages use declared budgets plus floor remainder, not an empirical two-second window', () => {
  const raw = fixture(); const initial = 1790000000, recovered = initial + 60;
  for (const record of records(raw).slice(4)) record.url = record.url.replace(/after=\d+/, `after=${recovered}`);
  inputs(raw)[0].createdAt = new Date((initial + 5) * 1000 + 28999).toISOString();
  inputs(raw)[1].createdAt = new Date((recovered + 5) * 1000 + 25999).toISOString();
  assert.doesNotThrow(() => channelPollContract(raw, declaration));
  inputs(raw)[0].createdAt = new Date((initial + 5) * 1000 + 29000).toISOString();
  assert.throws(() => channelPollContract(raw, declaration), /startup budget/);
  inputs(raw)[0].createdAt = new Date((initial + 5) * 1000 - 1).toISOString();
  assert.throws(() => channelPollContract(raw, declaration), /startup budget/);
});


// Exercise the actual shared comparator process, not the structural helper
// above. The fake artifacts model the real capture shape; no product runs.
function runComparator(a, b) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'pdrs-channel-poll-diff-'));
  try {
    const home = path.join(root, 'home'), pilotHome = path.join(root, 'pilot');
    fs.mkdirSync(home); fs.mkdirSync(pilotHome);
    const left = path.join(root, 'left.json'), right = path.join(root, 'right.json');
    const leftBytes = JSON.stringify(a), rightBytes = JSON.stringify(b);
    fs.writeFileSync(left, leftBytes); fs.writeFileSync(right, rightBytes);
    const result = spawnSync(process.execPath, [fileURLToPath(new URL('./diff.mjs', import.meta.url)), left, right, '--self'], {
      env: { ...process.env, HOME: home, PILOT_HOME: pilotHome }, encoding: 'utf8', timeout: 10000,
    });
    assert.ifError(result.error);
    assert.equal(fs.readFileSync(left, 'utf8'), leftBytes, 'comparator must not rewrite raw left capture');
    assert.equal(fs.readFileSync(right, 'utf8'), rightBytes, 'comparator must not rewrite raw right capture');
    return result;
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}
function declaredPair() {
  const a = fixture(), b = fixture(15);
  a.volatilePolicies = [clone(declaration)]; b.volatilePolicies = [clone(declaration)];
  return [a, b];
}

test('real diff --self uses the Map: seven after changes are SAME with seven raw VOLATILE-VALUE receipts', () => {
  const result = runComparator(...declaredPair());
  assert.equal(result.status, 0, result.stderr + result.stdout);
  assert.match(result.stdout, /^SAME$/m);
  assert.equal((result.stdout.match(/VOLATILE-VALUE coreChannelPollAfter /g) ?? []).length, 7);
  assert.equal((result.stdout.match(/raw a:/g) ?? []).length, 7);
  assert.equal((result.stdout.match(/raw b:/g) ?? []).length, 7);
  for (let i = 0; i < 7; i += 1) assert.ok(result.stdout.includes(`$.provider.channel-poll[${i}].url`));
});

test('real diff --self keeps a header regression DIFF while accepting only seven after values', () => {
  const [a, b] = declaredPair(); records(b)[0].headers.accept = 'application/json';
  const result = runComparator(a, b);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /^DIFF [1-9]\d*$/m);
  assert.match(result.stdout, /\$\.provider\.channel-poll\[0\]\.headers\.accept/);
  assert.equal((result.stdout.match(/VOLATILE-VALUE coreChannelPollAfter /g) ?? []).length, 7);
});

test('real diff --self keeps reordered query bytes DIFF and exempts only the six unchanged URL shapes', () => {
  const [a, b] = declaredPair();
  records(b)[0].url = '/api/v1/message?after=1790000015&password=core-channel-password&limit=50';
  const result = runComparator(a, b);
  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /^DIFF [1-9]\d*$/m);
  assert.ok(result.stdout.includes('$.provider.channel-poll[0].url'));
  assert.equal((result.stdout.match(/VOLATILE-VALUE coreChannelPollAfter /g) ?? []).length, 6);
});

test('real diff --self rejects unknown volatile policies rather than converting them to SAME', () => {
  const [a, b] = declaredPair();
  a.volatilePolicies = [{ policy: 'unknownPolicy' }]; b.volatilePolicies = clone(a.volatilePolicies);
  const result = runComparator(a, b);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /unsupported volatile value policy/);
  assert.doesNotMatch(result.stdout, /^SAME$/m);
});

test('real diff --self rejects left/right declaration mismatches in both directions', () => {
  for (const side of [0, 1]) {
    const pair = declaredPair(); pair[side].volatilePolicies = [];
    const result = runComparator(...pair);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /volatile value policy mismatch/);
    assert.doesNotMatch(result.stdout, /^SAME$/m);
  }
});

test('real diff --self without a declaration exposes all seven raw URL differences', () => {
  const result = runComparator(fixture(), fixture(15));
  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /^DIFF 7$/m);
  assert.doesNotMatch(result.stdout, /VOLATILE-VALUE/);
});
