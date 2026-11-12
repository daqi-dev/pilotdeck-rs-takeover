import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { compareCaptured, mapCronSidePath } from './compare-common.mjs';
import { expectedOutput, resolveSpec } from './capture.mjs';
import { BASELINE, sha } from '../c1-entry/compare-common.mjs';
const here = path.dirname(fileURLToPath(import.meta.url));
const json = value => JSON.stringify(value, null, 2) + '\n';
const write = (file, value) => fs.writeFileSync(file, json(value));
const read = file => JSON.parse(fs.readFileSync(file));
const digest = '1'.repeat(64);
// Deliberately synthetic artifacts. These test the comparator, never a product.
export function fakeCapture(kind, root, { seed = 1 } = {}) {
  fs.mkdirSync(root);
  const dir = path.join(here, kind === 'cron' ? '.' : '../c1-online'), specs = read(path.join(dir, 'cases.json'));
  const executable = '/synthetic-baseline/dist/src/cli/pilotdeck.js';
  const files = kind === 'cron' ? Object.fromEntries(['src/cli/pilotdeck.ts', 'src/gateway/client/probeServer.ts', 'src/gateway/client/GatewayWsClient.ts', 'src/gateway/client/RemoteGateway.ts'].map(file => [file, { sha256: digest }]))
    : Object.fromEntries(['src/cli/pilotdeck.ts', 'src/adapters/channel/cli/CliChannel.ts', 'src/gateway/client/GatewayWsClient.ts', 'src/gateway/client/probeServer.ts', 'dist/src/adapters/channel/cli/CliChannel.js', 'dist/src/gateway/client/GatewayWsClient.js', 'dist/src/gateway/client/probeServer.js'].map(file => [file, digest]));
  const source = kind === 'cron' ? { side: 'ts', root: '/synthetic-baseline', sourceCommit: BASELINE, files: { ...files, executable: { path: executable, sha256: digest } } }
    : { root: '/synthetic-baseline', commit: BASELINE, executable, sha256: digest, files };
  write(path.join(root, 'identity.json'), { node: 'v22.22.0', platform: 'linux-test', driverPid: 100 + seed, nodeExecutable: '/synthetic-node', nodeSha256: digest, port: 18789, publishedPorts: 'declared none by E0', networkNamespace: `net:[${seed}]`,
    ...(kind === 'cron' ? { isolationDeclaration: 'private-no-published-ports', casesSha256: sha(fs.readFileSync(path.join(dir, 'cases.json'))), driverSha256: sha(fs.readFileSync(path.join(dir, 'capture.mjs'))), sources: [source] }
      : { isolation: 'private-no-published-ports', files: Object.fromEntries(['capture.mjs', 'mock.mjs', 'cases.json', '../c1-cron/capture.mjs'].map(file => [file, sha(fs.readFileSync(path.join(dir, file)))])), sources: { ts: source } }) });
  const rows = [];
  for (const [index, original] of specs.entries()) {
    const prefix = path.join(root, original.id, 'ts'); fs.mkdirSync(prefix, { recursive: true });
    const fixture = kind === 'cron' ? path.dirname(prefix) : path.join(prefix, 'fixture'), project = path.join(fixture, 'project');
    const spec = resolveSpec(original, project), pid = 1000 + seed * 100 + index;
    const env = { HOME: kind === 'cron' ? path.join(prefix, 'home') : path.join(fixture, 'home'), PILOT_HOME: kind === 'cron' ? path.join(prefix, 'home/.pilotdeck') : path.join(fixture, 'home/.pilotdeck'), TMPDIR: kind === 'cron' ? prefix : path.join(fixture, 'tmp') };
    if (kind === 'cron') { env.USERPROFILE = env.HOME; env.CFFIXED_USER_HOME = env.HOME; write(path.join(root, spec.id, 'input.json'), spec); }
    const command = { command: '/synthetic-node', args: [executable, ...(kind === 'cron' ? ['cron'] : []), ...spec.argv], env, cwd: project, ...(kind === 'online' ? { spec } : {}) };
    write(path.join(prefix, 'command.json'), command);
    const state = { health: [{ method: 'GET', url: '/health' }], hellos: [], requests: [], problems: [], closeReceived: true, openSockets: 0, ...(kind === 'online' ? { modelRequests: [], pongs: spec.id === 'idle-heartbeat' ? 1 : 0 } : {}) };
    const events = [], add = (kind, details = {}) => events.push({ kind, atMs: seed, ...details });
    const hello = { type: 'hello', protocolVersion: '1.2', clientName: 'cli', clientVersion: '0.1.0', token: `c1-${kind}-isolated-fixture-token` };
    state.hellos.push({ raw: JSON.stringify(hello), value: hello }); add('ws-open'); add('ws-receive', { raw: JSON.stringify(hello) });
    const inputs = kind === 'cron' ? (spec.method ? [spec.params] : []) : spec.prompts.map(message => ({ sessionKey: `cli:project=${project}:default`, channelKey: 'cli', message, canPrompt: false }));
    for (const [i, params] of inputs.entries()) {
      const value = { type: 'request', id: `00000000-0000-4000-8000-${String(seed * 1000 + index * 10 + i).padStart(12, '0')}`, method: kind === 'cron' ? spec.method : 'submit_turn', params };
      const raw = JSON.stringify(value); state.requests.push({ raw, value }); add('ws-receive', { raw }); add('ws-send', { raw: JSON.stringify({ type: 'response', id: value.id, ok: true }) });
    }
    if (kind === 'online' && ['hello-fallback', 'local-sigint'].includes(spec.id)) {
      const raw = JSON.stringify({ model: 'deterministic', stream: true, messages: [{ role: 'user', content: 'fixture model text' }], cwd: project });
      state.modelRequests.push({ method: 'POST', url: '/v1/chat/completions', headers: { 'content-length': String(Buffer.byteLength(raw)), authorization: 'Bearer fixture' }, raw }); add('model-request', { raw });
    }
    add('ws-client-close', { payloadBase64: '' });
    if (kind === 'cron') state.events = events;
    fs.writeFileSync(path.join(prefix, 'network.jsonl'), events.map(event => JSON.stringify(event) + '\n').join(''));
    const output = kind === 'cron' ? expectedOutput(spec) : { stdout: spec.stdout, stderr: spec.stderr ?? '', exitCode: spec.exitCode };
    const identity = { pid, startTicks: String(pid), state: 'R', command: ['/synthetic-node', executable] };
    const signals = kind === 'online' && spec.signal ? [{ signal: spec.signal, phase: 'user-cancel', target: identity, atMs: seed, sent: true }] : [];
    const verdict = { evidenceValid: true, productPassed: true, lifecycle: signals.length ? 'user-signal' : 'natural-exit', ...(kind === 'cron' ? { naturalExitCode: output.exitCode } : {}) };
    const result = { exitCode: signals.length ? null : output.exitCode, signal: spec.signal ?? null, startupMs: 20000, observeMs: 1500, elapsedMs: seed * 10, boundary: null, signals, problem: null, stdout: output.stdout, stderr: output.stderr, stdoutSha256: sha(output.stdout), stderrSha256: sha(output.stderr), network: state, verdict,
      ...(kind === 'cron' ? { pid, processGroup: pid, startedAt: 'synthetic', terminalAtMs: seed } : { identity, logicalAt: seed, sentUserSignal: signals.length > 0, descendants: [], leftovers: [] }) };
    const processEvents = [{ kind: 'process-start', atMs: 0, command: command.command, args: command.args, ...(kind === 'cron' ? { pid, processGroup: pid, startedAt: 'synthetic' } : { identity, cwd: project }) },
      ...['stdout', 'stderr'].map(stream => ({ kind: 'output', atMs: seed, stream, bytes: Buffer.byteLength(output[stream]) })),
      { kind: kind === 'cron' ? 'terminal-output-observed' : 'logical-output-observed', atMs: seed },
      ...signals.map(signal => ({ kind: 'owned-pid-signal', ...signal })),
      { kind: 'process-close', atMs: seed * 10, exitCode: result.exitCode, signal: result.signal }];
    fs.writeFileSync(path.join(prefix, 'process.jsonl'), processEvents.map(event => JSON.stringify(event) + '\n').join(''));
    for (const stream of ['stdout', 'stderr']) fs.writeFileSync(path.join(prefix, `${stream}.bin`), output[stream]);
    write(path.join(prefix, 'result.json'), result);
    if (kind === 'online') for (const phase of ['before', 'after']) {
      const bytes = Buffer.from(JSON.stringify({ project, text: phase === 'before' ? 'initial' : 'retained message' }));
      write(path.join(prefix, `files-${phase}.json`), [{ path: 'home/.pilotdeck/fixture.json', size: bytes.length, sha256: sha(bytes), base64: bytes.toString('base64') }]);
    }
    rows.push({ case: spec.id, side: 'ts', ...verdict });
  }
  write(path.join(root, 'summary.json'), { expectedRows: rows.length, observedRows: rows.length, collectionPassed: true, rustPassed: null, tsObservedHangs: 0, rows });
}

function withPair(kind, mutate, expectedStatus) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'c1-compare-synthetic-'));
  try {
    const left = path.join(root, 'left'), right = path.join(root, 'right'), out = path.join(root, 'comparison');
    fakeCapture(kind, left, { seed: 1 }); fakeCapture(kind, right, { seed: 2 }); mutate?.(right);
    const code = compareCaptured(kind, { mode: 'self', left, right, out });
    const report = read(path.join(out, 'comparison.json'));
    assert.equal(report.status, expectedStatus, JSON.stringify(report.rows.filter(row => row.status !== 'SAME')) + (report.error ?? ''));
    assert.equal(code, expectedStatus === 'SAME' ? 0 : 1); return report;
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}
test('cron complete 14-cell synthetic self comparison maps only runtime identities', () => { assert.equal(withPair('cron', null, 'SAME').observedCases, 14); });
test('cron real-driver TMPDIR side root and descendants map with exact path boundaries', () => {
  assert.equal(mapCronSidePath('/capture/list-success/ts', '/capture/list-success', 'ts'), '/capture/list-success/SIDE');
  assert.equal(mapCronSidePath('/capture/list-success/ts/home/.pilotdeck', '/capture/list-success', 'ts'), '/capture/list-success/SIDE/home/.pilotdeck');
  assert.throws(() => mapCronSidePath('/capture/list-success/ts-evil', '/capture/list-success', 'ts'), /isolated side/);
  assert.throws(() => mapCronSidePath('/capture/list-success/rust', '/capture/list-success', 'ts'), /isolated side/);
  withPair('cron', root => { const file = path.join(root, 'list-success/ts/command.json'), data = read(file); data.env.TMPDIR += '-evil'; write(file, data); }, 'INVALID');
});
test('online complete 8-cell synthetic self comparison retains model calls and snapshots', () => { assert.equal(withPair('online', null, 'SAME').observedCases, 8); });
test('missing/duplicate scenario evidence is invalid instead of reducing the denominator', () => {
  withPair('cron', root => { const file = path.join(root, 'summary.json'), data = read(file); data.rows.pop(); write(file, data); }, 'INVALID');
  withPair('cron', root => { const file = path.join(root, 'summary.json'), data = read(file); data.rows[0] = data.rows[1]; write(file, data); }, 'INVALID');
});
test('changed output/hash and wrong RPC cannot become SAME', () => {
  withPair('cron', root => fs.appendFileSync(path.join(root, 'list-success/ts/stdout.bin'), 'wrong'), 'INVALID');
  withPair('cron', root => { const file = path.join(root, 'list-success/ts/result.json'), data = read(file); data.network.requests[0].value.params.limit = 99; write(file, data); }, 'INVALID');
});
test('provider headers and complete persisted content are compared', () => {
  withPair('online', root => { const file = path.join(root, 'hello-fallback/ts/result.json'), data = read(file); data.network.modelRequests[0].headers.authorization = 'Bearer changed'; write(file, data); }, 'DIFF');
  withPair('online', root => { const file = path.join(root, 'argv/ts/files-after.json'), bytes = Buffer.from('changed message'); write(file, [{ path: 'home/.pilotdeck/fixture.json', size: bytes.length, sha256: sha(bytes), base64: bytes.toString('base64') }]); }, 'DIFF');
});
test('failed/surviving cancellation and unknown observable surfaces fail closed', () => {
  withPair('online', root => { const file = path.join(root, 'sigint/ts/result.json'), data = read(file); data.signals[0].sent = false; write(file, data); }, 'INVALID');
  withPair('online', root => { const file = path.join(root, 'argv/ts/result.json'), data = read(file); data.newObservableField = true; write(file, data); }, 'INVALID');
});
function observeHang(root) {
  const prefix = path.join(root, 'list-success/ts'), file = path.join(prefix, 'result.json'), value = read(file);
  value.exitCode = null; value.signal = 'SIGTERM'; value.boundary = { phase: 'after-terminal-output', atMs: 20, childAlive: true, openSockets: 1, closeReceived: false };
  value.signals = [{ signal: 'SIGTERM', targetProcessGroup: value.pid, atMs: 20, sent: true }];
  value.network.closeReceived = false;
  value.network.events = value.network.events.filter(event => event.kind !== 'ws-client-close');
  value.verdict = { evidenceValid: true, productPassed: false, lifecycle: 'observed-hang', naturalExitCode: null };
  write(file, value);
  fs.writeFileSync(path.join(prefix, 'network.jsonl'), value.network.events.map(event => JSON.stringify(event) + '\n').join(''));
  const journal = fs.readFileSync(path.join(prefix, 'process.jsonl'), 'utf8').trimEnd().split('\n').map(line => JSON.parse(line)); journal.pop();
  journal.push({ kind: 'observation-boundary', ...value.boundary }, { kind: 'owned-process-signal', ...value.signals[0] }, { kind: 'process-close', atMs: 21, exitCode: null, signal: 'SIGTERM' });
  fs.writeFileSync(path.join(prefix, 'process.jsonl'), journal.map(event => JSON.stringify(event) + '\n').join(''));
  const summaryFile = path.join(root, 'summary.json'), summary = read(summaryFile);
  summary.tsObservedHangs = 1; summary.rows[0] = { case: 'list-success', side: 'ts', ...value.verdict }; write(summaryFile, summary);
}
test('observed hang stays a difference, and equal hangs never become product success', () => {
  withPair('cron', observeHang, 'DIFF');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'c1-hang-comparator-'));
  try {
    const left = path.join(root, 'left'), right = path.join(root, 'right'), out = path.join(root, 'out');
    fakeCapture('cron', left); fakeCapture('cron', right); observeHang(left); observeHang(right);
    assert.equal(compareCaptured('cron', { mode: 'self', left, right, out }), 0);
    const report = read(path.join(out, 'comparison.json')); assert.equal(report.status, 'SAME'); assert.equal(report.productPassed, false);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
test('public comparator entry rejects a mutated complete capture with nonzero exit', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'c1-compare-entry-'));
  try {
    const left = path.join(root, 'left'), right = path.join(root, 'right'); fakeCapture('cron', left); fakeCapture('cron', right);
    const run = name => spawnSync(process.execPath, [path.join(here, 'compare.mjs'), '--self', left, right, '--out', path.join(root, name)], { encoding: 'utf8' });
    const first = run('good'); assert.equal(first.status, 0, first.stderr + first.stdout);
    fs.appendFileSync(path.join(right, 'list-success/ts/stdout.bin'), 'deliberate wrong output');
    const second = run('bad'); assert.equal(second.status, 1, second.stderr + second.stdout);
    assert.equal(read(path.join(root, 'bad/comparison.json')).status, 'INVALID');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
test('public comparator rejects symlink output aliases into either input without creating files', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'c1-compare-output-boundary-'));
  try {
    const left = path.join(root, 'left'), right = path.join(root, 'right'); fakeCapture('cron', left); fakeCapture('cron', right);
    for (const [index, target] of [left, right].entries()) {
      const alias = path.join(root, `alias-${index}`), before = fs.readdirSync(target).sort(); fs.symlinkSync(target, alias, 'dir');
      const child = spawnSync(process.execPath, [path.join(here, 'compare.mjs'), '--self', left, right, '--out', path.join(alias, 'new-report')], { encoding: 'utf8' });
      assert.equal(child.status, 2, child.stdout + child.stderr); assert.match(child.stderr, /must not mutate/);
      assert.equal(fs.existsSync(path.join(target, 'new-report')), false); assert.deepEqual(fs.readdirSync(target).sort(), before);
    }
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
