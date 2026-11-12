// Full CLI server -> authenticated WS -> native model consumers -> real HTTP.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync, appendFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bindBaseline } from '../_shared/generator_guard.mjs';
import { sourceClosureSha256 } from '../../baseline_source_guard.mjs';

const baseline = resolve(process.env.PDRS_BASELINE);
const out = resolve(process.argv[2]);
mkdirSync(out, { recursive: true });
const sourceCommit = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const { gitExecutable } = bindBaseline({ baseline, commit: sourceCommit });
const sourceSha256 = sourceClosureSha256(baseline, sourceCommit, gitExecutable, ['src/cli/pilotdeck.ts']);
const home = join(out, 'home'), pilot = join(home, '.pilotdeck'), project = join(out, 'project'), tmp = join(out, 'tmp');
for (const dir of [pilot, project, tmp]) mkdirSync(dir, { recursive: true });
const hash = value => createHash('sha256').update(value).digest('hex');
const json = value => JSON.stringify(value, null, 2) + '\n';
const pause = ms => new Promise(done => setTimeout(done, ms));
async function until(test, label, ms = 20000) {
  const end = Date.now() + ms;
  while (!test()) { assert.ok(Date.now() < end, `deadline: ${label}`); await pause(10); }
}
const requests = [], frames = [], errors = [];
const server = createServer(async (req, res) => {
  try {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString());
    const wire = { method: req.method, path: req.url, apiKey: req.headers['x-goog-api-key'], body };
    requests.push(wire); appendFileSync(join(out, 'provider.jsonl'), JSON.stringify(wire) + '\n');
    const input = JSON.stringify(body);
    let purpose, text;
    if (req.url.endsWith(':generateContent')) {
      if (input.includes('Generate a concise title')) { purpose = 'title'; text = JSON.stringify({ title: process.env.M1_NEGATIVE_RESPONSE ? 'WRONG' : 'Google 正式标题' }); }
      else if (input.includes('Suggest the single most useful')) { purpose = 'suggestion'; text = '补充 Google 回归测试'; }
      else if (input.includes('model-tier classifier')) { purpose = 'judge'; text = '<tier>simple</tier>'; }
      else throw Error(`unrecognized complete request ${req.url}`);
      wire.purpose = purpose;
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ candidates: [{ content: { role: 'model', parts: [{ text }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 10, candidatesTokenCount: 3, totalTokenCount: 13 } }));
    } else {
      assert.match(req.url, /:streamGenerateContent\?alt=sse$/);
      wire.purpose = 'turn';
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write(`data: ${JSON.stringify({ candidates: [{ content: { role: 'model', parts: [{ text: 'Google 正式答复' }] } }] })}\n\n`);
      res.end(`data: ${JSON.stringify({ candidates: [{ finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 10, candidatesTokenCount: 3, totalTokenCount: 13 } })}\n\n`);
    }
  } catch (error) { errors.push(error.message); res.destroy(); }
});
await new Promise(done => server.listen(0, '127.0.0.1', done));
const origin = `http://127.0.0.1:${server.address().port}`;
const config = `schemaVersion: 1
agent:
  model: p/m
  maxContextTokens: 16000
  maxOutputTokens: 4096
model:
  providers:
    p:
      protocol: google
      url: ${origin}/v1beta
      apiKey: fixture-only
      retry:
        requestMaxRetries: 0
        streamMaxRetries: 0
      models:
${['m', 'judge', 'cheap'].map(model => `        ${model}:
          capabilities:
            supportsToolUse: true
            supportsStreaming: true
            supportsSystemPrompt: true
            maxContextTokens: 16000
            maxOutputTokens: 4096`).join('\n')}
telemetry:
  enabled: false
router:
  enabled: true
  tokenSaver:
    enabled: true
    judge: p/judge
    defaultTier: medium
    judgeTimeoutMs: 5000
    cacheAwareSwitching:
      enabled: false
    tiers:
      simple:
        model: p/cheap
      medium:
        model: p/m
`;
writeFileSync(join(pilot, 'pilotdeck.yaml'), config);
const env = { PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home, PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: join(pilot, 'pilotdeck.yaml'), TMPDIR: tmp, LANG: 'en_US.UTF-8', LC_ALL: 'en_US.UTF-8', TZ: 'UTC', NO_COLOR: '1', NODE_NO_WARNINGS: '1' };
const command = process.env.M1_BINARY || process.execPath;
const args = process.env.M1_BINARY ? ['server', '--host', '127.0.0.1', '--port', '0'] : ['--import', join(baseline, 'node_modules/tsx/dist/loader.mjs'), join(baseline, 'src/cli/pilotdeck.ts'), 'server', '--host', '127.0.0.1', '--port', '0'];
writeFileSync(join(out, 'identity.json'), json({ sourceCommit, sourceSha256, command, args, cwd: project, env, driverSha256: hash(readFileSync(fileURLToPath(import.meta.url))), commandSha256: hash(readFileSync(command)) }));
writeFileSync(join(out, 'driver.mjs'), readFileSync(fileURLToPath(import.meta.url)));
const child = spawn(command, args, { cwd: project, env, stdio: ['ignore', 'pipe', 'pipe'] });
let stdout = '', stderr = '', exit = null, failure, socket;
child.stdout.on('data', data => { stdout += data; }); child.stderr.on('data', data => { stderr += data; });
const ended = new Promise(done => child.once('close', (code, signal) => { exit = { code, signal }; done(); }));
let contract;
try {
  await until(() => stdout.includes('WebSocket: ws://') || exit, 'CLI server startup');
  assert.equal(exit, null, stdout + stderr);
  const { WebSocket } = createRequire(join(baseline, 'package.json'))('ws');
  socket = new WebSocket(stdout.match(/WebSocket: (ws:\/\/[^\s]+)/)[1]);
  socket.on('message', data => { const frame = JSON.parse(data.toString()); frames.push(frame); appendFileSync(join(out, 'gateway.jsonl'), JSON.stringify(frame) + '\n'); });
  await new Promise((done, reject) => { socket.once('open', done); socket.once('error', reject); });
  const send = frame => socket.send(JSON.stringify(frame));
  send({ type: 'hello', protocolVersion: '1.2', token: readFileSync(join(pilot, 'server-token'), 'utf8').trim(), clientName: 'cli', clientVersion: '0.1.0' });
  await until(() => frames.some(frame => frame.type === 'hello_ok'), 'authenticated handshake');
  send({ type: 'request', id: 'turn', method: 'submit_turn', params: { sessionKey: 'google-complete', channelKey: 'test', message: '请修复 Google 模型请求', sdkSessionConfig: { promptSuggestions: true } } });
  await until(() => frames.some(frame => frame.id === 'turn' && frame.final), 'formal turn completion');
  const terminal = frames.find(frame => frame.id === 'turn' && frame.final);
  assert.equal(terminal.event.type, 'turn_completed', JSON.stringify(terminal));
  let rpcId = 0;
  async function rpc(method, params = {}) {
    const id = `read-${++rpcId}`; send({ type: 'request', id, method, params });
    await until(() => frames.some(frame => frame.id === id && frame.type === 'response'), method);
    const reply = frames.find(frame => frame.id === id && frame.type === 'response');
    assert.equal(reply.ok, true, JSON.stringify(reply)); return reply.result;
  }
  let sessions;
  for (let attempt = 0; attempt < 100; attempt++) {
    sessions = await rpc('list_sessions');
    if (JSON.stringify(sessions).includes('Google 正式标题')) break;
    if (process.env.M1_NEGATIVE_RESPONSE && JSON.stringify(sessions).includes('WRONG')) break;
    await pause(20);
  }
  const history = await rpc('read_session_messages', { sessionKey: 'google-complete' });
  contract = {
    handshake: true, terminal: terminal.event.type,
    titleSaved: JSON.stringify(sessions).includes('Google 正式标题'),
    suggestions: frames.filter(frame => frame.id === 'turn' && frame.event?.type === 'prompt_suggestion').map(frame => frame.event.suggestion),
    historyHasAnswer: JSON.stringify(history.messages).includes('Google 正式答复'),
    historyHasSuggestion: JSON.stringify(history.messages).includes('补充 Google 回归测试'),
    completePurposes: requests.filter(request => request.purpose !== 'turn').map(request => request.purpose).sort(),
    turnPaths: requests.filter(request => request.purpose === 'turn').map(request => request.path),
    errors,
  };
  assert.equal(contract.titleSaved, true, 'title must be generated and persisted through formal server');
  assert.deepEqual(contract.suggestions, ['补充 Google 回归测试']);
  assert.equal(contract.historyHasAnswer, true); assert.equal(contract.historyHasSuggestion, false);
  assert.deepEqual(contract.completePurposes, ['judge', 'suggestion', 'title']);
  assert.deepEqual(contract.turnPaths, ['/v1beta/models/cheap:streamGenerateContent?alt=sse']);
  assert.deepEqual(errors, []);
} catch (error) { failure = error; }
finally {
  socket?.close();
  if (!exit) child.kill('SIGINT');
  await Promise.race([ended, pause(10000)]);
  if (!exit) { child.kill('SIGKILL'); await ended; }
  server.closeAllConnections(); await new Promise(done => server.close(done));
  writeFileSync(join(out, 'stdout.log'), stdout); writeFileSync(join(out, 'stderr.log'), stderr);
  writeFileSync(join(out, 'contract.json'), json({ ...contract, exit }));
}
if (failure) throw failure;
assert.deepEqual(exit, { code: 0, signal: null });
console.log(json({ out, contract }));
