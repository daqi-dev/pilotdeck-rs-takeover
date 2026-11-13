#!/usr/bin/env node
// Formal CLI subprocess evidence. Product code is never imported or patched.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import http from 'node:http';
import { createHash } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const BASELINE = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const TOKEN = 'c1-cron-isolated-fixture-token';
const PORT = 18789;
const MAX_BYTES = 1024 * 1024;
let activeEvidenceDirectory;
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const json = value => JSON.stringify(value, null, 2) + '\n';

export function requireIsolation(env = process.env, exists = fs.existsSync) {
  assert.equal(env.PILOTDECK_C1_CRON_DOCKER, 'private-no-published-ports', 'explicit private Docker isolation declaration required');
  assert.ok(exists('/.dockerenv'), 'refusing fixed port 18789 outside Docker');
}

export function expectedOutput(spec) {
  return {
    stdout: spec.reply?.ok ? JSON.stringify(spec.reply.result, null, 2) + '\n' : '',
    stderr: spec.stderr ?? '',
    exitCode: spec.exitCode ?? 0,
  };
}

export function resolveSpec(spec, project) {
  if (spec === '$PROJECT') return project;
  if (Array.isArray(spec)) return spec.map(value => resolveSpec(value, project));
  if (spec && typeof spec === 'object') return Object.fromEntries(Object.entries(spec).map(([key, value]) => [key, resolveSpec(value, project)]));
  return spec;
}

// Server frames are unmasked. This fixture needs text, ping/pong and close;
// the parser below accepts fragmented text and checks client masking/size.
export function frame(opcode, bytes, masked = false) {
  const payload = Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes);
  const sizeCode = payload.length < 126 ? payload.length : payload.length <= 65535 ? 126 : 127;
  const header = Buffer.alloc(sizeCode === 127 ? 10 : sizeCode === 126 ? 4 : 2);
  header[0] = 0x80 | opcode;
  header[1] = (masked ? 0x80 : 0) | sizeCode;
  if (sizeCode === 126) header.writeUInt16BE(payload.length, 2);
  if (sizeCode === 127) header.writeBigUInt64BE(BigInt(payload.length), 2);
  if (!masked) return Buffer.concat([header, payload]);
  const mask = Buffer.from([17, 34, 51, 68]);
  const encoded = payload.map((byte, index) => byte ^ mask[index % 4]);
  return Buffer.concat([header, mask, encoded]);
}

export async function startMock(spec, { port = PORT, record = () => {} } = {}) {
  if (port === PORT) requireIsolation();
  const started = performance.now();
  const state = { events: [], problems: [], health: [], hellos: [], requests: [], closeReceived: false, openSockets: 0 };
  const sockets = new Set();
  const emit = (kind, details = {}) => {
    const event = { kind, atMs: performance.now() - started, ...details };
    state.events.push(event); record(event);
  };
  const server = http.createServer((request, response) => {
    const accepted = request.method === 'GET' && request.url === '/health';
    state.health.push({ method: request.method, url: request.url });
    emit('http', { method: request.method, url: request.url, status: accepted ? 200 : 404 });
    response.writeHead(accepted ? 200 : 404, { 'content-type': 'application/json', connection: 'close' });
    response.end(accepted ? '{"ok":true}' : '{}');
  });
  server.on('connection', socket => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });
  server.on('upgrade', (request, socket, head) => {
    if (request.url !== '/ws' || typeof request.headers['sec-websocket-key'] !== 'string') {
      state.problems.push('unexpected WebSocket upgrade'); socket.destroy(); return;
    }
    const accept = createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    state.openSockets++; emit('ws-open');
    socket.on('close', () => { state.openSockets--; emit('ws-socket-closed'); });
    socket.on('error', error => emit('ws-socket-error', { message: error.message }));
    let buffered = Buffer.alloc(0), fragments = [], fragmentBytes = 0;
    const send = value => {
      const raw = JSON.stringify(value); emit('ws-send', { raw }); socket.write(frame(1, raw));
    };
    const text = payload => {
      const raw = payload.toString('utf8'); emit('ws-receive', { raw });
      let value;
      try { value = JSON.parse(raw); } catch { throw new Error('invalid client JSON'); }
      if (value.type === 'hello') {
        state.hellos.push({ raw, value });
        send({ type: 'hello_ok', protocolVersion: '1.2', serverInfo: { name: 'c1-cron-owned-mock', version: '0.1.0' } });
      } else if (value.type === 'request') {
        state.requests.push({ raw, value });
        if (!spec.reply) throw new Error('usage case unexpectedly sent RPC');
        send({ type: 'response', id: value.id, ...spec.reply });
      } else throw new Error(`unexpected client frame type: ${value.type}`);
    };
    const consume = chunk => {
      buffered = Buffer.concat([buffered, chunk]);
      try {
        while (buffered.length >= 2) {
          const opcode = buffered[0] & 15, final = (buffered[0] & 128) !== 0;
          assert.equal(buffered[0] & 0x70, 0, 'unsupported reserved WebSocket bits');
          assert.ok(buffered[1] & 128, 'client frame must be masked');
          let size = buffered[1] & 127, offset = 2;
          if (size === 126) { if (buffered.length < 4) return; size = buffered.readUInt16BE(2); offset = 4; }
          else if (size === 127) { if (buffered.length < 10) return; const big = buffered.readBigUInt64BE(2); assert.ok(big <= BigInt(MAX_BYTES)); size = Number(big); offset = 10; }
          assert.ok(size <= MAX_BYTES && fragmentBytes + size <= MAX_BYTES, 'client frame exceeds fixture bound');
          if (buffered.length < offset + 4 + size) return;
          const mask = buffered.subarray(offset, offset + 4); offset += 4;
          const payload = Buffer.from(buffered.subarray(offset, offset + size));
          for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
          buffered = buffered.subarray(offset + size);
          if (opcode === 8) { state.closeReceived = true; emit('ws-close-received', { payloadBase64: payload.toString('base64') }); socket.end(frame(8, payload)); continue; }
          if (opcode === 9) { emit('ws-ping'); socket.write(frame(10, payload)); continue; }
          if (opcode === 10) continue;
          assert.ok(opcode === 1 || (opcode === 0 && fragments.length > 0), 'unsupported client data opcode');
          if (opcode === 1) assert.equal(fragments.length, 0, 'new text inside fragmented frame');
          fragments.push(payload); fragmentBytes += payload.length;
          if (final) { text(Buffer.concat(fragments)); fragments = []; fragmentBytes = 0; }
        }
      } catch (error) { state.problems.push(error.message); emit('protocol-error', { message: error.message }); socket.destroy(); }
    };
    socket.on('data', consume);
    if (head.length) consume(head);
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(port, '127.0.0.1', resolve); });
  return {
    state, port: server.address().port,
    async stop() {
      emit('mock-cleanup');
      const closed = [...sockets].map(socket => new Promise(resolve => {
        socket.once('close', resolve);
        socket.destroy();
      }));
      await Promise.all([
        new Promise(resolve => server.close(resolve)),
        ...closed,
      ]);
    },
  };
}

export async function drive(command, args, env, cwd, expected, { startupMs = 15000, observeMs = 1500, record = () => {}, snapshot = () => ({}) } = {}) {
  const started = performance.now(), startedAt = new Date().toISOString();
  const child = spawn(command, args, { env, cwd, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  const output = { stdout: [], stderr: [] }, signals = [];
  let terminalAtMs = null, boundary = null, problem = null, closed = false, bytes = 0, killTimer;
  const elapsed = () => performance.now() - started;
  const emit = (kind, details = {}) => record({ kind, atMs: elapsed(), ...details });
  emit('process-start', { pid: child.pid, processGroup: child.pid, command, args, startedAt });
  const kill = signal => {
    if (closed || !child.pid) return;
    const item = { signal, targetProcessGroup: child.pid, atMs: elapsed() };
    try { process.kill(-child.pid, signal); item.sent = true; } catch (error) { item.sent = false; item.errorCode = error.code; }
    signals.push(item); emit('owned-process-signal', item);
  };
  const stop = phase => {
    if (boundary) return;
    boundary = { phase, atMs: elapsed(), childAlive: child.exitCode === null && child.signalCode === null, ...snapshot() };
    emit('observation-boundary', boundary); kill('SIGTERM');
    killTimer = setTimeout(() => kill('SIGKILL'), 500);
  };
  let timer = setTimeout(() => stop('startup'), startupMs);
  for (const stream of ['stdout', 'stderr']) child[stream].on('data', chunk => {
    output[stream].push(chunk); bytes += chunk.length;
    emit('output', { stream, bytes: chunk.length });
    if (bytes > MAX_BYTES) { problem = 'combined output exceeds 1MiB'; stop('output-bound'); return; }
    if (terminalAtMs === null) {
      const stdout = Buffer.concat(output.stdout).toString('utf8'), stderr = Buffer.concat(output.stderr).toString('utf8');
      const marker = expected.stdout ? stdout.includes(expected.stdout) : stderr.includes(expected.stderr);
      if (marker) { terminalAtMs = elapsed(); emit('terminal-output-observed'); clearTimeout(timer); timer = setTimeout(() => stop('after-terminal-output'), observeMs); }
    }
  });
  const status = await new Promise(resolve => {
    child.once('error', error => { problem = `spawn: ${error.message}`; emit('spawn-error', { message: error.message }); });
    child.once('close', (exitCode, signal) => { closed = true; resolve({ exitCode, signal }); });
  });
  clearTimeout(timer); clearTimeout(killTimer);
  const result = {
    ...status, pid: child.pid, processGroup: child.pid, startedAt, elapsedMs: elapsed(),
    startupMs, observeMs, terminalAtMs, boundary, signals, problem,
    stdout: Buffer.concat(output.stdout), stderr: Buffer.concat(output.stderr),
  };
  emit('process-close', { exitCode: status.exitCode, signal: status.signal, elapsedMs: result.elapsedMs });
  return result;
}

export function verifyCase(spec, side, result, state) {
  const expected = expectedOutput(spec);
  assert.equal(result.problem, null, `${spec.id}: collector problem`);
  assert.deepEqual(state.problems, [], `${spec.id}: mock protocol errors`);
  assert.deepEqual(state.health, [{ method: 'GET', url: '/health' }], `${spec.id}: real HTTP probe`);
  assert.equal(state.hellos.length, 1, `${spec.id}: hello count`);
  const hello = { type: 'hello', protocolVersion: '1.2', clientName: 'cli', clientVersion: '0.1.0', token: TOKEN };
  assert.equal(state.hellos[0].raw, JSON.stringify(hello), `${spec.id}: exact CLI hello bytes`);
  assert.equal(result.stdout.toString('utf8'), expected.stdout, `${spec.id}: stdout bytes`);
  assert.equal(result.stderr.toString('utf8'), expected.stderr, `${spec.id}: stderr bytes`);
  assert.equal(state.requests.length, spec.method ? 1 : 0, `${spec.id}: RPC count`);
  if (spec.method) {
    const request = state.requests[0];
    assert.equal(typeof request.value.id, 'string', `${spec.id}: request identity`);
    assert.match(request.value.id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/, `${spec.id}: UUIDv4 request identity`);
    assert.equal(request.raw, JSON.stringify({ type: 'request', id: request.value.id, method: spec.method, params: spec.params }), `${spec.id}: exact RPC bytes`);
  }
  if (result.boundary) {
    assert.equal(side, 'ts', `${spec.id}: Rust must exit naturally`);
    assert.equal(result.boundary.phase, 'after-terminal-output', `${spec.id}: timeout before expected output is failure`);
    assert.equal(result.boundary.childAlive, true, `${spec.id}: observed live process`);
    assert.ok(result.boundary.openSockets > 0, `${spec.id}: live owned WS at boundary`);
    assert.equal(result.boundary.closeReceived, false, `${spec.id}: no client close before cleanup`);
    assert.ok(result.terminalAtMs !== null);
    return { evidenceValid: true, productPassed: false, lifecycle: 'observed-hang', naturalExitCode: null };
  }
  assert.equal(result.exitCode, expected.exitCode, `${spec.id}: natural exit code`);
  assert.equal(result.signal, null, `${spec.id}: no signal exit`);
  assert.deepEqual(result.signals, [], `${spec.id}: no cleanup kill for natural exit`);
  if (side === 'rust') assert.equal(state.closeReceived, true, `${spec.id}: Rust closes owned WS`);
  return { evidenceValid: true, productPassed: true, lifecycle: 'natural-exit', naturalExitCode: result.exitCode };
}

export function verifyCoverage(specs, sides, rows) {
  const expected = specs.flatMap(spec => sides.map(side => `${side}:${spec.id}`)).sort();
  const observed = rows.map(row => `${row.side}:${row.case}`).sort();
  assert.deepEqual(observed, expected, 'every requested scenario/side must occur exactly once');
}

function git(root, args) {
  const result = spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' });
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
}

function identity(options, side) {
  const root = fs.realpathSync(options[`${side}-root`]);
  const commit = git(root, ['rev-parse', 'HEAD']);
  assert.equal(commit, side === 'ts' ? BASELINE : options['rust-sha'], `${side}: fixed source SHA`);
  assert.equal(git(root, ['status', '--porcelain', '--untracked-files=no']), '', `${side}: tracked source must be clean`);
  const executable = side === 'ts' ? path.join(root, 'dist/src/cli/pilotdeck.js') : fs.realpathSync(options['rust-bin']);
  const files = { executable: { path: executable, sha256: sha(fs.readFileSync(executable)) } };
  if (side === 'ts') for (const name of ['src/cli/pilotdeck.ts', 'src/gateway/client/probeServer.ts', 'src/gateway/client/GatewayWsClient.ts', 'src/gateway/client/RemoteGateway.ts']) files[name] = { sha256: sha(fs.readFileSync(path.join(root, name))) };
  return { side, root, sourceCommit: commit, files };
}

export async function main(argv = process.argv.slice(2)) {
  // This check runs before any file creation, port binding or product spawn.
  requireIsolation();
  assert.equal(process.version, 'v22.22.0', 'fixed Node 22.22.0 runtime required');
  const options = {};
  for (let index = 0; index < argv.length; index += 2) {
    assert.ok(argv[index].startsWith('--') && argv[index + 1], 'arguments must be --key value pairs');
    const key = argv[index].slice(2);
    assert.ok(['side', 'ts-root', 'rust-root', 'rust-bin', 'rust-sha', 'out'].includes(key), `unknown option ${key}`);
    assert.equal(options[key], undefined, `duplicate option ${key}`); options[key] = argv[index + 1];
  }
  const side = options.side ?? 'both';
  assert.ok(['ts', 'rust', 'both'].includes(side));
  const sides = side === 'both' ? ['ts', 'rust'] : [side];
  assert.ok(options.out, '--out NEW_DIRECTORY required');
  for (const side of sides) assert.ok(options[`${side}-root`], `--${side}-root required`);
  if (sides.includes('rust')) assert.ok(options['rust-bin'] && /^[a-f0-9]{40}$/.test(options['rust-sha'] ?? ''), '--rust-bin and full --rust-sha required');
  const out = path.resolve(options.out); fs.mkdirSync(out); activeEvidenceDirectory = out;
  const specsPath = path.join(here, 'cases.json');
  const specs = JSON.parse(fs.readFileSync(specsPath, 'utf8'));
  assert.equal(specs.length, 14, 'complete scenario denominator');
  assert.equal(new Set(specs.map(spec => spec.id)).size, specs.length, 'unique scenarios');
  const manifest = {
    startedAt: new Date().toISOString(), driverPid: process.pid, platform: `${os.platform()}-${os.arch()}`, node: process.version,
    nodeExecutable: process.execPath, nodeSha256: sha(fs.readFileSync(process.execPath)),
    isolationDeclaration: process.env.PILOTDECK_C1_CRON_DOCKER,
    networkNamespace: fs.readlinkSync('/proc/self/ns/net'), port: PORT, publishedPorts: 'declared none by E0',
    casesSha256: sha(fs.readFileSync(specsPath)), driverSha256: sha(fs.readFileSync(fileURLToPath(import.meta.url))),
    sources: sides.map(side => identity(options, side)),
    note: 'Driver exit 0 means evidence complete and Rust checks passed where selected. TS observed-hang is explicitly not product success.',
  };
  fs.writeFileSync(path.join(out, 'identity.json'), json(manifest));
  const results = []; let failed = false;
  for (const original of specs) {
    const caseRoot = path.join(out, original.id), project = path.join(caseRoot, 'project');
    fs.mkdirSync(project, { recursive: true });
    const spec = resolveSpec(original, project);
    fs.writeFileSync(path.join(caseRoot, 'input.json'), json(spec));
    for (const side of sides) {
      const root = path.join(caseRoot, side), home = path.join(root, 'home'), pilot = path.join(home, '.pilotdeck');
      fs.mkdirSync(pilot, { recursive: true });
      fs.writeFileSync(path.join(pilot, 'server-token'), `${TOKEN}\n`, { mode: 0o600 });
      const env = { PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home, PILOT_HOME: pilot, TMPDIR: root, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', TERM: 'dumb', NO_COLOR: '1', NODE_NO_WARNINGS: '1' };
      const source = manifest.sources.find(value => value.side === side);
      const command = side === 'ts' ? process.execPath : source.files.executable.path;
      const args = [...(side === 'ts' ? [source.files.executable.path] : []), 'cron', ...spec.argv];
      fs.writeFileSync(path.join(root, 'command.json'), json({ command, args, env, cwd: project }));
      const append = file => event => fs.appendFileSync(path.join(root, file), JSON.stringify(event) + '\n');
      const mock = await startMock(spec, { record: append('network.jsonl') });
      let result;
      try {
        result = await drive(command, args, env, project, expectedOutput(spec), {
          record: append('process.jsonl'), snapshot: () => ({ openSockets: mock.state.openSockets, closeReceived: mock.state.closeReceived }),
        });
      } finally { await mock.stop(); }
      fs.writeFileSync(path.join(root, 'stdout.bin'), result.stdout);
      fs.writeFileSync(path.join(root, 'stderr.bin'), result.stderr);
      let verdict;
      try { verdict = verifyCase(spec, side, result, mock.state); }
      catch (error) { failed = true; verdict = { evidenceValid: false, productPassed: false, failure: error.message }; }
      const evidence = { ...result, stdout: result.stdout.toString('utf8'), stderr: result.stderr.toString('utf8'), stdoutSha256: sha(result.stdout), stderrSha256: sha(result.stderr), network: mock.state, verdict };
      fs.writeFileSync(path.join(root, 'result.json'), json(evidence));
      results.push({ case: spec.id, side, ...verdict });
      console.log(`${side} ${spec.id}: ${verdict.evidenceValid ? verdict.lifecycle : `FAIL ${verdict.failure}`}`);
    }
  }
  verifyCoverage(specs, sides, results);
  const summary = {
    expectedRows: specs.length * sides.length, observedRows: results.length, collectionPassed: !failed,
    rustPassed: sides.includes('rust') ? results.filter(row => row.side === 'rust').every(row => row.productPassed) : null,
    tsObservedHangs: results.filter(row => row.side === 'ts' && row.lifecycle === 'observed-hang').length,
    rows: results,
  };
  fs.writeFileSync(path.join(out, 'summary.json'), json(summary));
  process.exitCode = failed ? 1 : 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { await main(); } catch (error) {
    if (activeEvidenceDirectory) fs.writeFileSync(path.join(activeEvidenceDirectory, 'fatal.json'), json({ at: new Date().toISOString(), error: error.stack ?? error.message }));
    console.error(error.stack ?? error.message); process.exitCode = 1;
  }
}
