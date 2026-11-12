import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { checkWorker, validateArtifact, assertionResults, loadRun, fileHash, inputIdentity, imageAttachmentsMatch, sha256 } from './evidence.mjs';
const expected = [{ name: 'a' }, { name: 'b' }];
const row = (name) => JSON.stringify({ name, status: 'SAME' }) + '\n';
for (const [name, result] of [
  ['nonzero without output', { code: 1, stdout: '' }],
  ['signal without output', { code: null, signal: 'SIGTERM', stdout: '' }],
  ['partial scenarios', { code: 0, stdout: row('a') }],
  ['duplicate scenarios', { code: 0, stdout: row('a') + row('a') }],
  ['malformed output', { code: 0, stdout: row('a') + 'not JSON\n' }],
  ['successful rows but nonzero exit', { code: 7, stdout: row('a') + row('b') }],
]) test(name, () => assert.ok(checkWorker(expected, result).errors.length));
test('complete successful worker', () => assert.deepEqual(checkWorker(expected, { code: 0, stdout: row('a') + row('b') }).errors, []));
const scenario = { steps: [{ cli: ['--version'] }], assertions: [{ id: 'version', path: '/steps/0/cli/stdout', includes: '1.0' }] };
const identity = { runId: 'run', side: 'rust', scenario: { name: 'a', sha256: 'abc' }, inputs: { binaries: { pilotdeck: 'hash' } } };
function artifact() {
  const a = { steps: [{ cli: { stdout: '1.0' } }], failures: [] };
  a.evidence = { schema: 1, ...identity, assertions: assertionResults(scenario.assertions, a) };
  return a;
}
test('complete artifact', () => assert.deepEqual(validateArtifact(artifact(), scenario, identity), []));
test('time-value policy cannot be added to a capture or silently removed', () => {
  const policy = [{ policy: 'coreChannelPollAfter' }];
  assert.ok(validateArtifact({ ...artifact(), volatilePolicies: policy }, scenario, identity)
    .includes('volatile value policy mismatch'));
  assert.ok(validateArtifact(artifact(), { ...scenario, volatilePolicies: policy }, identity)
    .includes('volatile value policy mismatch'));
});
test('declared time-value policy validates actual data even with no linked assertion', () => {
  for (const policy of [{ policy: 'unknown' }, { policy: 'coreChannelPollAfter', paths: ['.*'] }, { policy: 'coreChannelPollAfter' }]) {
    const declaration = [policy];
    assert.ok(validateArtifact({ ...artifact(), volatilePolicies: declaration },
      { ...scenario, volatilePolicies: declaration }, identity).length);
  }
});
for (const [name, mutate] of [
  ['Rust timeout while TS succeeds', (a) => { a.failures.push({ at: 'steps[0]', kind: 'timeout', expected: false }); }],
  ['missing steps', (a) => { a.steps = []; }],
  ['missing failures', (a) => { delete a.failures; }],
  ['stale run identity', (a) => { a.evidence.runId = 'old'; }],
  ['scenario hash mismatch', (a) => { a.evidence.scenario = { name: 'a', sha256: 'changed' }; }],
  ['binary hash mismatch', (a) => { a.evidence.inputs = { binaries: { pilotdeck: 'changed' } }; }],
  ['required assertion not executed', (a) => { a.evidence.assertions = []; }],
  ['required assertion failed', (a) => { a.steps[0].cli.stdout = 'wrong'; }],
]) test(name, () => { const a = artifact(); mutate(a); assert.ok(validateArtifact(a, scenario, identity).length); });
test('old SAME report cannot replace a complete manifest', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-evidence-'));
  try { fs.writeFileSync(path.join(dir, 'a.diff.txt'), 'SAME\n'); assert.throws(() => loadRun(dir)); }
  finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
test('existing output directory is rejected without touching old report', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-existing-'));
  try {
    fs.writeFileSync(path.join(dir, 'a.diff.txt'), 'SAME\n');
    const r = spawnSync(process.execPath, [new URL('./check.mjs', import.meta.url).pathname, '--out', dir], { encoding: 'utf8' });
    assert.notEqual(r.status, 0); assert.equal(fs.readFileSync(path.join(dir, 'a.diff.txt'), 'utf8'), 'SAME\n');
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
test('manifest refuses changed raw artifact even with SAME report', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-manifest-'));
  try {
    const spec = path.join(dir, 'a.json'); fs.writeFileSync(spec, JSON.stringify(scenario));
    fs.writeFileSync(path.join(dir, 'a.ts.json'), '{}');
    fs.writeFileSync(path.join(dir, 'manifest.json'), JSON.stringify({ schema: 1, complete: true, mode: 'compare', scenarios: [{ name: 'a', path: spec, sha256: fileHash(spec) }], results: [{ name: 'a', status: 'SAME', files: { 'a.ts.json': 'old-hash' } }] }));
    assert.throws(() => loadRun(dir, { verifyCurrent: false }), /artifact changed/);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
for (const [name, code] of [['actual child nonzero', 'process.exit(7)'], ['actual child signal', "process.kill(process.pid, 'SIGTERM')"]]) {
  test(name, () => {
    const r = spawnSync(process.execPath, ['-e', code], { encoding: 'utf8' });
    assert.ok(checkWorker([{ name: 'a' }], { code: r.status, signal: r.signal, error: r.error, stdout: r.stdout }).errors.length);
  });
}
test('coverage rejects parent worker errors even when all rows claim SAME', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-worker-error-'));
  try {
    fs.writeFileSync(path.join(dir, 'manifest.json'), JSON.stringify({ errors: ['worker exit=7'], complete: true }));
    assert.throws(() => loadRun(dir), /validation errors/);
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
});
test('accepted difference needs a positive assertion linked to it', () => {
  const spec = { ...scenario, accepted: [{ id: 'fixed-bug', paths: ['steps'] }] };
  assert.ok(validateArtifact(artifact(), spec, identity).some((e) => e.includes('positive assertions')));
  spec.accepted[0].assertions = ['version'];
  assert.deepEqual(validateArtifact({ ...artifact(), accepted: spec.accepted }, spec, identity), []);
});
test('side-specific positive assertions are explicit, not silently omitted', () => {
  const assertions = [{ id: 'rust-value', sides: ['rust'], path: '/value', equals: 1 }];
  assert.deepEqual(assertionResults(assertions, { value: 2 }, 'ts'), [{ id: 'rust-value', skipped: true }]);
  assert.deepEqual(assertionResults(assertions, { value: 2 }, 'rust'), [{ id: 'rust-value', passed: false }]);
});
test('component replacement or absence changes the recorded run identity', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-component-'));
  const keys = ['BBX_BASELINE', 'BBX_TARGET', 'BBX_SRC', 'BBX_PDF_TOOL'];
  const previous = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
  try {
    Object.assign(process.env, { BBX_BASELINE: dir, BBX_TARGET: dir, BBX_SRC: dir, BBX_PDF_TOOL: path.join(dir, 'pdf-component') });
    const missing = inputIdentity();
    fs.writeFileSync(process.env.BBX_PDF_TOOL, 'component-version-one');
    const first = inputIdentity();
    fs.writeFileSync(process.env.BBX_PDF_TOOL, 'component-version-two');
    const replaced = inputIdentity();
    assert.equal(missing.components.pdf.sha256, null);
    assert.notDeepEqual(first.components, missing.components);
    assert.notDeepEqual(first.components, replaced.components);
  } finally {
    for (const key of keys) if (previous[key] === undefined) delete process.env[key]; else process.env[key] = previous[key];
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
test('image assertions verify exact MIME, decoded payload bytes, hash and budget', () => {
  const bytes = Buffer.from([0, 1, 255]);
  const content = (mime, payload = bytes.toString('base64')) => [{ type: 'image_url', image_url: { url: `data:${mime};base64,${payload}` } }];
  const expected = { count: 1, mime: 'image/jpeg', maxBytes: 3, bytes: 3, sha256: sha256(bytes) };
  assert.equal(imageAttachmentsMatch(content('image/jpeg'), expected), true);
  assert.equal(imageAttachmentsMatch(content('image/png'), expected), false);
  assert.equal(imageAttachmentsMatch(content('image/jpeg', Buffer.from([0, 1, 255, 1]).toString('base64')), expected), false);
  assert.equal(imageAttachmentsMatch(content('image/jpeg', Buffer.from([0, 1, 254]).toString('base64')), expected), false);
  assert.equal(imageAttachmentsMatch(content('image/jpeg', '!bad'), expected), false);
  assert.equal(imageAttachmentsMatch([], expected), false);
  assert.equal(imageAttachmentsMatch(undefined, { count: 0 }), false);
  assert.equal(imageAttachmentsMatch([], { count: 0 }), true);
  assert.equal(imageAttachmentsMatch(content('image/jpeg'), { count: 0 }), false);
});
test('image capability cannot pass with missing execution or mutated MIME/oversize payload', () => {
  const assertion = { id: 'real-image', path: '/images', imageAttachments: { count: 1, mime: 'image/jpeg', maxBytes: 3 } };
  const spec = { ...scenario, assertions: [assertion] };
  const image = (mime, bytes) => [{ type: 'image_url', image_url: { url: `data:${mime};base64,${Buffer.alloc(bytes).toString('base64')}` } }];
  const make = () => { const a = { ...artifact(), images: image('image/jpeg', 3) }; a.evidence.assertions = assertionResults(spec.assertions, a, 'rust'); return a; };
  assert.deepEqual(validateArtifact(make(), spec, identity), []);
  for (const mutate of [(a) => { a.evidence.assertions = []; }, (a) => { a.images = image('image/png', 3); }, (a) => { a.images = image('image/jpeg', 4); }]) {
    const a = make(); mutate(a); assert.ok(validateArtifact(a, spec, identity).length);
  }
});
