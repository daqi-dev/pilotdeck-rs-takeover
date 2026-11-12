// Exercise the public CLI in a temporary copy with intentionally faulty collectors/workers.
// No PilotDeck binary, real provider, real user home, or fixed port is touched.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { spawnSync } from 'node:child_process';
import { loadRun } from './evidence.mjs';

const fixture = () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-cli-test-'));
  const dir = path.join(root, 'scripts/bbx'); fs.mkdirSync(dir, { recursive: true });
  for (const name of ['check.mjs', 'evidence.mjs', 'diff.mjs', 'mock-support.mjs', 'syntax-policy.mjs']) fs.copyFileSync(new URL(name, import.meta.url), path.join(dir, name));
  // The supervisor is the production CLI; only its workers are faulted in this isolated copy.
  const check = path.join(dir, 'check.mjs');
  fs.writeFileSync(check, fs.readFileSync(check, 'utf8').replace('#!/usr/bin/env node', `#!/usr/bin/env node
if (process.env.BBX_CHECK_ROWS && process.env.BBX_TEST_WORKER_FAULT) {
  const name = (await import('node:path')).basename(process.argv.at(-1), '.json');
  const row = JSON.stringify({name, status:'SAME'}) + '\\n';
  const fault = process.env.BBX_TEST_WORKER_FAULT;
  if (fault === 'signal') process.kill(process.pid, 'SIGTERM');
  else { process.stdout.write(fault === 'duplicate' ? row + row : fault === 'bad-exit' ? row : ''); process.exit(fault === 'bad-exit' ? 7 : 0); }
}`));
  fs.writeFileSync(path.join(dir, 'run.mjs'), `
import fs from 'node:fs';
import {inputIdentity, scenarioIdentity, assertionResults} from './evidence.mjs';
const [side, file, out] = process.argv.slice(2);
const spec = JSON.parse(fs.readFileSync(file, 'utf8'));
if (process.env.BBX_TEST_COLLECTOR_FAULT === 'no-output') process.exit(7);
if (process.env.BBX_TEST_COLLECTOR_FAULT === 'signal') process.kill(process.pid, 'SIGTERM');
const artifact = {side, steps:spec.steps.map(step=>({step, cli:{exitCode:0,stdout:'ok'}})), failures:[]};
if (process.env.BBX_TEST_COLLECTOR_FAULT === 'rust-timeout' && side === 'rust') artifact.failures.push({at:'steps[0]',kind:'timeout',expected:false});
artifact.evidence = {schema:1,runId:process.env.BBX_RUN_ID,side,scenario:scenarioIdentity(file),inputs:inputIdentity(),assertions:assertionResults(spec.assertions,artifact,side)};
if (process.env.BBX_TEST_COLLECTOR_FAULT === 'missing-assertions') artifact.evidence.assertions=[];
fs.writeFileSync(out,JSON.stringify(artifact));
`);
  const files = ['a', 'b'].map((name) => {
    const file = path.join(root, `${name}.json`);
    fs.writeFileSync(file, JSON.stringify({ name, server: false, steps: [{ cli: ['fixture'] }], assertions: [{ id: 'ran', path: '/steps/0/cli/stdout', equals: 'ok' }] }));
    return file;
  });
  return { root, check, files };
};
function execute(extra = {}, { parallel = false } = {}) {
  const f = fixture();
  const out = path.join(f.root, 'results');
  const result = spawnSync(process.execPath, [f.check, '--out', out, ...(parallel ? ['--slots', '1,2'] : []), ...f.files], {
    encoding: 'utf8', timeout: 30000,
    env: { ...process.env, BBX_BASELINE: path.join(f.root, 'baseline'), BBX_SRC: path.join(f.root, 'src'), BBX_TARGET: path.join(f.root, 'target'), ...extra },
  });
  return { ...f, out, result, manifest: JSON.parse(fs.readFileSync(path.join(out, 'manifest.json'), 'utf8')) };
}
for (const fault of ['no-output', 'signal', 'rust-timeout', 'missing-assertions']) {
  test(`public CLI rejects collector ${fault}`, () => {
    const f = execute({ BBX_TEST_COLLECTOR_FAULT: fault });
    try { assert.equal(f.result.status, 1, f.result.stderr); assert.equal(f.manifest.counts.passed, 0); assert.equal(f.manifest.counts.failed, 2); }
    finally { fs.rmSync(f.root, { recursive: true, force: true }); }
  });
}
for (const fault of ['missing', 'duplicate', 'signal', 'bad-exit']) {
  test(`public parallel CLI rejects worker ${fault}`, () => {
    const f = execute({ BBX_TEST_WORKER_FAULT: fault }, { parallel: true });
    try { assert.equal(f.result.status, 1, f.result.stderr); assert.ok(f.manifest.errors.length); assert.equal(f.manifest.counts.passed, 0); assert.throws(() => loadRun(f.out, { verifyCurrent: false })); }
    finally { fs.rmSync(f.root, { recursive: true, force: true }); }
  });
}
test('public parallel CLI accepts complete evidence and coverage can read it', () => {
  const f = execute({}, { parallel: true });
  try { assert.equal(f.result.status, 0, f.result.stderr); assert.equal(f.manifest.counts.passed, 2); assert.equal(loadRun(f.out, { verifyCurrent: false }).results.length, 2); }
  finally { fs.rmSync(f.root, { recursive: true, force: true }); }
});
test('complete failed parallel batch remains reportable by coverage', () => {
  const f = execute({ BBX_TEST_COLLECTOR_FAULT: 'rust-timeout' }, { parallel: true });
  try {
    assert.equal(f.result.status, 1, f.result.stderr); assert.deepEqual(f.manifest.errors, []);
    assert.equal(loadRun(f.out, { verifyCurrent: false }).counts.failed, 2);
  } finally { fs.rmSync(f.root, { recursive: true, force: true }); }
});
