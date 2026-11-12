import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { drive, expectedOutput, frame, requireIsolation, resolveSpec, startMock, verifyCase, verifyCoverage } from './capture.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const cases = JSON.parse(fs.readFileSync(path.join(here, 'cases.json'), 'utf8'));
const token = 'c1-cron-isolated-fixture-token';
const spec = cases[0];
const hello = { type: 'hello', protocolVersion: '1.2', clientName: 'cli', clientVersion: '0.1.0', token };
const request = { type: 'request', id: '123e4567-e89b-42d3-a456-426614174000', method: spec.method, params: spec.params };
const state = () => ({ problems: [], health: [{ method: 'GET', url: '/health' }], hellos: [{ raw: JSON.stringify(hello), value: hello }], requests: [{ raw: JSON.stringify(request), value: request }], closeReceived: true });
const result = () => ({ problem: null, stdout: Buffer.from(expectedOutput(spec).stdout), stderr: Buffer.alloc(0), exitCode: 0, signal: null, signals: [], boundary: null, terminalAtMs: 5 });

function isolatedRoot(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'c1-cron-driver-'));
  fs.mkdirSync(path.join(root, 'home')); fs.mkdirSync(path.join(root, 'pilot'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return root;
}
function envFor(root) {
  return { PATH: process.env.PATH, HOME: path.join(root, 'home'), PILOT_HOME: path.join(root, 'pilot'), NODE_NO_WARNINGS: '1' };
}

test('fixed-port entry refuses host before creating output or spawning products', t => {
  requireIsolation({ PILOTDECK_C1_CRON_DOCKER: 'private-no-published-ports' }, () => true);
  assert.throws(() => requireIsolation({}, () => true), /declaration required/);
  assert.throws(() => requireIsolation({ PILOTDECK_C1_CRON_DOCKER: 'private-no-published-ports' }, () => false), /outside Docker/);
  const root = isolatedRoot(t), output = path.join(root, 'must-not-exist');
  const child = spawnSync(process.execPath, [path.join(here, 'capture.mjs'), '--out', output], { env: envFor(root), encoding: 'utf8' });
  assert.notEqual(child.status, 0);
  assert.match(child.stderr, /declaration required/);
  assert.equal(fs.existsSync(output), false);
});

test('protocol, stdout/stderr, exit and socket negative controls cannot report success', () => {
  assert.equal(verifyCase(spec, 'rust', result(), state()).productPassed, true);
  for (const change of [
    { stdout: Buffer.alloc(0) }, { stderr: Buffer.from('extra\n') }, { exitCode: 1 },
    { signal: 'SIGTERM' }, { problem: 'spawn failed' }, { signals: [{ signal: 'SIGKILL' }] },
  ]) assert.throws(() => verifyCase(spec, 'rust', { ...result(), ...change }, state()));
  for (const change of [
    { problems: ['bad frame'] }, { health: [] }, { hellos: [] }, { requests: [] },
    { requests: [...state().requests, ...state().requests] }, { closeReceived: false },
    { hellos: [{ raw: JSON.stringify({ ...hello, clientName: 'sdk' }) }] },
    { requests: [{ raw: JSON.stringify({ ...request, method: 'cron_delete' }), value: request }] },
    { requests: [{ raw: JSON.stringify({ ...request, id: 'fixed' }), value: { ...request, id: 'fixed' } }] },
  ]) assert.throws(() => verifyCase(spec, 'rust', result(), { ...state(), ...change }));
});

test('observed TS hang is evidence, never product success or Rust acceptance', () => {
  const boundary = { phase: 'after-terminal-output', childAlive: true, openSockets: 1, closeReceived: false };
  const captured = { ...result(), boundary, exitCode: null, signal: 'SIGTERM', signals: [{ signal: 'SIGTERM' }] };
  assert.deepEqual(verifyCase(spec, 'ts', captured, state()), { evidenceValid: true, productPassed: false, lifecycle: 'observed-hang', naturalExitCode: null });
  assert.throws(() => verifyCase(spec, 'rust', captured, state()));
  for (const change of [{ phase: 'startup' }, { childAlive: false }, { openSockets: 0 }, { closeReceived: true }]) {
    assert.throws(() => verifyCase(spec, 'ts', { ...captured, boundary: { ...boundary, ...change } }, state()));
  }
});

test('missing or duplicate scenario-side records cannot produce a complete report', () => {
  const rows = cases.flatMap(spec => ['ts', 'rust'].map(side => ({ case: spec.id, side })));
  verifyCoverage(cases, ['ts', 'rust'], rows);
  assert.throws(() => verifyCoverage(cases, ['ts', 'rust'], rows.slice(1)));
  assert.throws(() => verifyCoverage(cases, ['ts', 'rust'], [...rows, rows[0]]));
  assert.throws(() => verifyCoverage(cases, ['ts', 'rust'], rows.map((row, index) => index === 0 ? rows[1] : row)));
});

test('scenario denominator, replacement and WS length encoding remain explicit', () => {
  assert.equal(cases.length, 14);
  assert.equal(new Set(cases.map(value => value.id)).size, 14);
  assert.deepEqual(resolveSpec({ projectKey: '$PROJECT' }, '/a/quote"/back\\slash'), { projectKey: '/a/quote"/back\\slash' });
  for (const size of [0, 125, 126, 127, 65535, 65536]) {
    const encoded = frame(1, Buffer.alloc(size));
    assert.equal(encoded[0], 0x81);
    assert.equal(encoded.length, size + (size < 126 ? 2 : size <= 65535 ? 4 : 10));
  }
});

const fakeCli = `
  const [port, mode, specJson] = process.argv.slice(1);
  const spec = JSON.parse(specJson);
  if (mode === 'no-output') { setInterval(() => {}, 1000); }
  else {
    await fetch('http://127.0.0.1:' + port + '/health');
    const ws = new WebSocket('ws://127.0.0.1:' + port + '/ws');
    ws.addEventListener('open', () => ws.send(JSON.stringify(${JSON.stringify(hello)})));
    ws.addEventListener('message', event => {
      const value = JSON.parse(event.data);
      if (value.type === 'hello_ok') ws.send(JSON.stringify({type:'request',id:'123e4567-e89b-42d3-a456-426614174000',method:spec.method,params:spec.params}));
      else if (value.type === 'response') {
        process.stdout.write(JSON.stringify(value.result, null, 2) + '\\n');
        if (mode === 'natural') ws.close();
        else setInterval(() => {}, 1000);
      }
    });
  }
`;

test('real owned child and ephemeral HTTP/WS peer prove natural close and bounded hang capture', async t => {
  const root = isolatedRoot(t);
  for (const mode of ['natural', 'hang']) {
    const mock = await startMock(spec, { port: 0 });
    try {
      const captured = await drive(process.execPath, ['--input-type=module', '-e', fakeCli, String(mock.port), mode, JSON.stringify(spec)], envFor(root), root, expectedOutput(spec), {
        startupMs: 5000, observeMs: 120,
        snapshot: () => ({ openSockets: mock.state.openSockets, closeReceived: mock.state.closeReceived }),
      });
      const verdict = verifyCase(spec, mode === 'natural' ? 'rust' : 'ts', captured, mock.state);
      assert.equal(verdict.lifecycle, mode === 'natural' ? 'natural-exit' : 'observed-hang');
      assert.equal(typeof captured.pid, 'number');
      if (mode === 'hang') {
        assert.equal(captured.signals[0].targetProcessGroup, captured.pid);
        assert.equal(captured.signals[0].signal, 'SIGTERM');
      }
    } finally { await mock.stop(); }
  }
});

test('real child without terminal output is a failure, even when timeout cleanup succeeds', async t => {
  const root = isolatedRoot(t);
  const captured = await drive(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], envFor(root), root, expectedOutput(spec), { startupMs: 80, observeMs: 30 });
  assert.equal(captured.boundary.phase, 'startup');
  assert.equal(captured.terminalAtMs, null);
  assert.throws(() => verifyCase(spec, 'ts', captured, state()));
});
