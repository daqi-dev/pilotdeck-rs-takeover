#!/usr/bin/env node
// Formal server startup failure paths, real loopback WebSocket, real 20s timer.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import { createRequire } from 'node:module';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
const json = value => JSON.stringify(value, null, 2) + '\n';
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const BASELINE = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
async function until(test, label) { const end = Date.now() + 25000; while (!test()) { assert.ok(Date.now() < end, `deadline: ${label}`); await sleep(10); } }
export async function captureStartup({ baseline, out, command, args }) {
  assert.equal(process.version, 'v22.22.0');
  assert.equal(spawnSync('git', ['-C', baseline, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim(), BASELINE);
  fs.mkdirSync(out); out = fs.realpathSync(out); fs.copyFileSync(fileURLToPath(import.meta.url), path.join(out, 'driver.mjs'));
  const { WebSocketServer } = createRequire(path.join(baseline, 'package.json'))('ws');
  const contracts = [];
  for (const name of ['missing-token', 'handshake-rejected', 'auth-invalid', 'auth-timeout']) {
    const directory = path.join(out, name), home = path.join(directory, 'home'), pilot = path.join(home, '.pilotdeck'), project = path.join(directory, 'project');
    fs.mkdirSync(pilot, { recursive: true }); fs.mkdirSync(project);
    const frames = [], sockets = new Set(); let connections = 0;
    const server = http.createServer((_, response) => { response.writeHead(500); response.end('unexpected HTTP request'); });
    server.on('connection', socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
    const ws = new WebSocketServer({ noServer: true });
    server.on('upgrade', (request, socket, head) => {
      if (name === 'handshake-rejected') { socket.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n'); return; }
      ws.handleUpgrade(request, socket, head, peer => ws.emit('connection', peer));
    });
    ws.on('connection', peer => {
      connections++; peer.send('{"type":"auth_required"}');
      peer.on('message', data => { const frame = JSON.parse(data.toString()); frames.push(frame); if (name === 'auth-invalid') peer.send('{"type":"auth_invalid","message":"fixture invalid"}'); });
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const port = server.address().port;
    const config = `schemaVersion: 1\nagent:\n  model: fixture/deterministic\n  maxContextTokens: 65536\n  maxOutputTokens: 8192\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:${port}/v1\n      apiKey: fixture-only\n      models:\n        deterministic:\n          capabilities:\n            supportsToolUse: true\n            maxContextTokens: 65536\n            maxOutputTokens: 8192\ntelemetry:\n  enabled: false\nadapters:\n  homeassistant:\n    enabled: true\n    token: ${name === 'missing-token' ? '\"\"' : 'fixture-ha-token'}\n    extra:\n      url: http://127.0.0.1:${port}\n`;
    fs.writeFileSync(path.join(pilot, 'pilotdeck.yaml'), config);
    const env = { PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home, PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: path.join(pilot, 'pilotdeck.yaml'), TMPDIR: directory, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', NO_COLOR: '1', NODE_NO_WARNINGS: '1' };
    const exe = command ?? process.execPath, argv = args ?? [path.join(baseline, 'dist/src/cli/pilotdeck.js'), 'server', '--host', '127.0.0.1', '--port', '0'];
    fs.writeFileSync(path.join(directory, 'identity.json'), json({ baseline: BASELINE, node: process.version, command: exe, args: argv, env, cwd: project, driverHash: hash(fs.readFileSync(fileURLToPath(import.meta.url))), commandHash: hash(fs.readFileSync(exe)), entryHash: fs.existsSync(argv[0]) ? hash(fs.readFileSync(argv[0])) : null }));
    const child = spawn(exe, argv, { env, cwd: project, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '', stderr = '', status, failure, elapsedMs; const started = Date.now(), signals = [];
    child.stdout.on('data', chunk => stdout += chunk); child.stderr.on('data', chunk => stderr += chunk);
    const ended = new Promise(resolve => child.once('close', (code, signal) => { status = { code, signal }; resolve(); }));
    child.once('error', error => failure = error.stack);
    const signal = name => { if (!status) signals.push({ signal: name, sent: child.kill(name) }); };
    try {
      const text = name === 'missing-token' ? 'homeassistant: HASS_TOKEN not set' : 'homeassistant: WebSocket auth failed or timed out';
      await until(() => stdout.includes('PilotDeck server listening:') && (stdout + stderr).includes(text), name);
      elapsedMs = Date.now() - started;
      if (name === 'auth-timeout' || name === 'handshake-rejected') assert.ok(elapsedMs >= 19500, 'authentication deadline remains twenty seconds');
      if (name === 'auth-invalid') assert.ok((stdout + stderr).includes('homeassistant: auth invalid: fixture invalid'));
      if (name === 'handshake-rejected') assert.ok((stdout + stderr).includes('homeassistant: ws error: [object ErrorEvent]'));
      assert.equal(connections, ['missing-token', 'handshake-rejected'].includes(name) ? 0 : 1);
      assert.deepEqual(frames, ['missing-token', 'handshake-rejected'].includes(name) ? [] : [{ type: 'auth', access_token: 'fixture-ha-token' }]);
      signal('SIGINT'); await Promise.race([ended, sleep(3000).then(() => { throw Error('shutdown deadline'); })]);
      assert.deepEqual(status, { code: 0, signal: null });
    } catch (error) { failure ??= error.stack; }
    finally {
      if (!status) { signal('SIGTERM'); await Promise.race([ended, sleep(1000)]); if (!status) { signal('SIGKILL'); await ended; } }
      for (const peer of ws.clients) peer.terminate(); for (const socket of sockets) socket.destroy(); await new Promise(resolve => server.close(resolve)); ws.close();
      fs.writeFileSync(path.join(directory, 'stdout.txt'), stdout); fs.writeFileSync(path.join(directory, 'stderr.txt'), stderr);
    }
    const haLogs = stderr.split('\n').filter(line => line.startsWith('homeassistant:'));
    const expectedLogs = name === 'missing-token' ? ['homeassistant: HASS_TOKEN not set'] : name === 'auth-invalid' ? ['homeassistant: auth invalid: fixture invalid', 'homeassistant: WebSocket auth failed or timed out'] : name === 'handshake-rejected' ? ['homeassistant: ws error: [object ErrorEvent]', 'homeassistant: WebSocket auth failed or timed out', 'homeassistant: ws error: [object ErrorEvent]'] : ['homeassistant: WebSocket auth failed or timed out'];
    const contract = { name, connections, frames, status, haLogs, noSuccessfulStart: !(stdout + stderr).includes('homeassistant: WebSocket ws://'), expectedError: (stdout + stderr).includes(name === 'missing-token' ? 'homeassistant: HASS_TOKEN not set' : 'homeassistant: WebSocket auth failed or timed out') };
    try { assert.deepEqual(haLogs, expectedLogs); } catch (error) { failure ??= error.stack; }
    assert.equal(contract.noSuccessfulStart, true);
    fs.writeFileSync(path.join(directory, 'result.json'), json({ pid: child.pid, failure: failure ?? null, elapsedMs, signals, contract })); contracts.push(contract);
    if (failure) throw Error(failure);
  }
  fs.writeFileSync(path.join(out, 'contract.json'), json(contracts)); return contracts;
}
async function main() {
  const [baseline, out, compareDir] = process.argv.slice(2); assert.ok(baseline && out);
  const result = await captureStartup({ baseline: path.resolve(baseline), out: path.resolve(out) });
  if (compareDir) assert.deepEqual(result, JSON.parse(fs.readFileSync(path.join(compareDir, 'contract.json'))));
  console.log(json({ passed: true, out }));
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.stack); process.exitCode = 1; });
