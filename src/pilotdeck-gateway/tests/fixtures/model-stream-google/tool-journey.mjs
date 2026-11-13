// RA05: official CLI server, real Google wire, real read_file and persisted history.
// Required: PDRS_BASELINE (clean cc7c4e6 tree), --engine ts|rust --out NEW_DIRECTORY.
// Rust additionally requires RA05_BINARY absolute native ELF/Mach-O/PE and
// RA05_BINARY_SHA256 exact sealed binary hash. No inherited credential env is used.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync, realpathSync, readdirSync, readlinkSync } from 'node:fs';
import { join, resolve, isAbsolute, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bindBaseline } from '../_shared/generator_guard.mjs';
import { sourceClosureSha256 } from '../../baseline_source_guard.mjs';
import { INPUT, ANSWER, CALL, PROMPT, sha, verify } from './verify-tool-journey.mjs';
const argv = process.argv.slice(2);
assert.ok([4,6].includes(argv.length), 'usage: tool-journey.mjs --engine ts|rust --out NEW_DIRECTORY [--ts-entry source|built]');
if (argv.length === 6) assert.equal(argv[4], '--ts-entry');
const tsEntryMode = argv[5] ?? 'source';
assert.ok(['source','built'].includes(tsEntryMode));
assert.equal(argv[0], '--engine'); assert.equal(argv[2], '--out');
const engine = argv[1]; assert.ok(['ts', 'rust'].includes(engine));
assert.ok(process.env.PDRS_BASELINE && isAbsolute(process.env.PDRS_BASELINE), 'absolute PDRS_BASELINE required');
const baseline = realpathSync(process.env.PDRS_BASELINE), out = resolve(argv[3]);
mkdirSync(out); // exclusive ownership; never reuse a capture or home
const commit = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const binding = bindBaseline({ baseline, commit });
const sourceClosure = sourceClosureSha256(baseline, commit, binding.gitExecutable, ['src/cli/pilotdeck.ts']);
assert.ok(Object.keys(sourceClosure).length > 1);
const home = join(out, 'home'), pilot = join(home, '.pilotdeck'), tmp = join(out, 'tmp');
for (const directory of [home, pilot, tmp]) mkdirSync(directory, { recursive: true });
const negative = process.env.RA05_NEGATIVE === 'wrong-output';
assert.ok(!process.env.RA05_NEGATIVE || negative, 'only wrong-output negative supported');
const actualInput = negative ? INPUT.replace('RA05-7x13-20261113', 'RA05-WRONG-SOURCE') : INPUT;
writeFileSync(join(pilot, 'ra05-input.txt'), actualInput);
const command = engine === 'ts' ? realpathSync(process.execPath) : realpathSync(process.env.RA05_BINARY || assert.fail('RA05_BINARY required'));
const commandBytes = readFileSync(command), magic = commandBytes.subarray(0, 4).toString('hex');
const nativeExecutable = ['7f454c46', 'cffaedfe', 'feedfacf', 'cefaedfe', 'feedface', 'cafebabe', 'bebafeca'].includes(magic) || commandBytes.subarray(0, 2).toString() === 'MZ';
assert.equal(nativeExecutable, true, 'native binary required; script/wrapper forbidden');
if (engine === 'rust') { assert.match(process.env.RA05_BINARY_SHA256 ?? '', /^[a-f0-9]{64}$/); assert.equal(sha(commandBytes), process.env.RA05_BINARY_SHA256); }
// Same complete-dist byte traversal as scripts/bbx/evidence.mjs treeHash:
// ordered relative names followed by raw bytes (symlinks hash their link text).
let baselineDist = null;
if (engine === 'ts' && tsEntryMode === 'built') {
  assert.match(process.env.RA05_TS_DIST_SHA256 ?? '', /^[a-f0-9]{64}$/, 'fixed complete dist hash required for built TS');
  const root = join(baseline,'dist'), digest = (await import('node:crypto')).createHash('sha256'), files = [];
  function visit(directory) {
    for (const entry of readdirSync(directory,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))) {
      const file = join(directory,entry.name);
      if (entry.isDirectory()) visit(file);
      else { const bytes = entry.isSymbolicLink() ? Buffer.from(readlinkSync(file)) : readFileSync(file); const name = relative(root,file); digest.update(name); digest.update(bytes); files.push({path:name,sha256:sha(bytes),kind:entry.isSymbolicLink()?'symlink':'file'}); }
    }
  }
  visit(root);
  baselineDist = {sha256:digest.digest('hex'),expectedSha256:process.env.RA05_TS_DIST_SHA256,files,manifestSha256:sha(JSON.stringify(files))};
  assert.equal(baselineDist.sha256,baselineDist.expectedSha256,'complete built TS dist identity mismatch');
}
const selectedEntry = engine === 'rust' ? command : join(baseline,tsEntryMode === 'built'?'dist/src/cli/pilotdeck.js':'src/cli/pilotdeck.ts');
const args = engine === 'rust' ? ['server', '--host', '127.0.0.1', '--port', '0'] : tsEntryMode === 'built' ? [selectedEntry,'server','--host','127.0.0.1','--port','0'] : ['--import', join(baseline, 'node_modules/tsx/dist/loader.mjs'), selectedEntry, 'server', '--host', '127.0.0.1', '--port', '0'];
const identity = { engine, selectedEntryMode:engine === 'rust'?'native':tsEntryMode, selectedEntry, selectedEntrySha256:sha(readFileSync(selectedEntry)), baselineDist, node:{path:realpathSync(process.execPath),sha256:sha(readFileSync(process.execPath)),version:process.version}, command, args, nativeExecutable, commandSha256: sha(commandBytes), expectedBinarySha256: engine === 'rust' ? process.env.RA05_BINARY_SHA256 : null, baselineCommit: commit, baselineBinding: binding.provenance, sourceClosure, driverSha256: sha(readFileSync(fileURLToPath(import.meta.url))), verifierSha256: sha(readFileSync(fileURLToPath(new URL('./verify-tool-journey.mjs', import.meta.url)))) };
const artifact = { schema: 'RA05-google-tool-v1', captureComplete: false, identity, negative, inputBeforeSha256: sha(actualInput), requests: [], frames: [], errors: [] };
const json = x => JSON.stringify(x, null, 2) + '\n';
const pause = ms => new Promise(done => setTimeout(done, ms));
async function until(predicate, label, limit = 20000) { const end = Date.now() + limit; while (!predicate()) { assert.ok(Date.now() < end, `deadline: ${label}`); await pause(10); } }
let child, exit = null, socket, stdout = '', stderr = '', fatal;
const mock = createServer(async (req, res) => {
  try {
    const chunks = []; let bytes = 0;
    for await (const chunk of req) { bytes += chunk.length; assert.ok(bytes < 4 * 1024 * 1024); chunks.push(chunk); }
    const body = JSON.parse(Buffer.concat(chunks).toString());
    const wire = { method: req.method, path: req.url, apiKey: req.headers['x-goog-api-key'], body, purpose: req.url.includes(':streamGenerateContent') ? 'turn' : 'auxiliary' };
    artifact.requests.push(wire);
    if (wire.purpose === 'auxiliary') {
      assert.ok(req.url.endsWith(':generateContent'));
      // Title is a background consumer; its existing grade is not re-tested.
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ candidates: [{ content: { role: 'model', parts: [{ text: '{"title":"RA05 tool journey"}' }] }, finishReason: 'STOP' }], usageMetadata: { promptTokenCount: 2, candidatesTokenCount: 1, totalTokenCount: 3 } })); return;
    }
    assert.equal(req.url, '/v1beta/models/m:streamGenerateContent?alt=sse');
    const index = artifact.requests.filter(r => r.purpose === 'turn').length;
    assert.ok(index <= 2, 'unexpected extra main model request');
    const parts = body.contents.flatMap(c => c.parts ?? []);
    let reply;
    if (index === 1) { assert.equal(parts.some(p => p.functionResponse), false); reply = { functionCall: CALL }; }
    else {
      const responses = parts.filter(p => p.functionResponse).map(p => p.functionResponse);
      assert.equal(responses.length, 1); assert.equal(responses[0].id, CALL.id); assert.equal(responses[0].name, CALL.name);
      assert.equal(typeof responses[0].response.output, 'string');
      for (const line of actualInput.trimEnd().split('\n')) assert.ok(responses[0].response.output.includes(line), `missing real output ${line}`);
      // Negative still emits a perfectly correct final answer: provenance oracle
      // must reject the real wrong file, rather than trusting model prose.
      reply = { text: ANSWER };
    }
    const usageMetadata = index === 1 ? { promptTokenCount: 11, candidatesTokenCount: 2, totalTokenCount: 13 } : { promptTokenCount: 17, candidatesTokenCount: 3, totalTokenCount: 20 };
    wire.reply = { part: reply, usageMetadata };
    const responseText = `data: ${JSON.stringify({ responseId: `ra05-response-${index}`, candidates: [{ content: { role: 'model', parts: [reply] } }] })}\n\n` + `data: ${JSON.stringify({ candidates: [{ finishReason: 'STOP' }], usageMetadata })}\n\n`;
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.flushHeaders();
    wire.response = {status:res.statusCode, headers:res.getHeaders(), rawHeaders:res._header, bodyBase64:Buffer.from(responseText).toString('base64'), sha256:sha(responseText)};
    res.end(responseText);
  } catch (error) { artifact.errors.push(String(error.stack ?? error)); res.destroy(); }
});
await new Promise((done, reject) => { mock.once('error', reject); mock.listen(0, '127.0.0.1', done); });
const config = `schemaVersion: 1\nagent:\n  model: p/m\n  maxContextTokens: 32000\n  maxOutputTokens: 4096\nmodel:\n  providers:\n    p:\n      protocol: google\n      url: http://127.0.0.1:${mock.address().port}/v1beta\n      apiKey: fixture-only\n      retry: {requestMaxRetries: 0, streamMaxRetries: 0}\n      models:\n        m:\n          capabilities: {supportsToolUse: true, supportsStreaming: true, supportsSystemPrompt: true, maxContextTokens: 32000, maxOutputTokens: 4096}\ntelemetry: {enabled: false}\n`;
writeFileSync(join(pilot, 'pilotdeck.yaml'), config);
const env = { PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home, PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: join(pilot, 'pilotdeck.yaml'), PILOTDECK_STORAGE_ROOT: join(out, 'audit'), PILOTDECK_SNAPSHOT_ROOT: join(tmp, 'snapshots'), TMPDIR: tmp, LANG: 'en_US.UTF-8', LC_ALL: 'en_US.UTF-8', TZ: 'UTC', NO_COLOR: '1', NODE_NO_WARNINGS: '1' };
identity.env = env; identity.cwd = pilot;
writeFileSync(join(out,'identity.json'),json(identity)); // identity is durable before any product spawn
let ended;
try {
  child = spawn(command, args, { cwd: pilot, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  identity.pid = child.pid; identity.pgid = child.pid;
  ended = new Promise(done => child.once('close', (code, signal) => { exit = { code, signal }; done(); }));
  child.once('error', error => { artifact.errors.push(String(error)); });
  child.stdout.on('data', data => { stdout += data; if (stdout.length > 8 * 1024 * 1024) child.kill('SIGTERM'); });
  child.stderr.on('data', data => { stderr += data; if (stderr.length > 8 * 1024 * 1024) child.kill('SIGTERM'); });
  await until(() => stdout.includes('WebSocket: ws://') || exit, 'server startup'); assert.equal(exit, null, stdout + stderr);
  const url = stdout.match(/WebSocket: (ws:\/\/[^\s]+)/)?.[1]; assert.ok(url);
  const { WebSocket } = createRequire(join(baseline, 'package.json'))('ws');
  socket = new WebSocket(url);
  socket.on('message', data => artifact.frames.push(JSON.parse(data.toString())));
  await new Promise((done, reject) => { socket.once('open', done); socket.once('error', reject); });
  const send = frame => socket.send(JSON.stringify(frame));
  send({ type: 'hello', protocolVersion: '1.2', token: readFileSync(join(pilot, 'server-token'), 'utf8').trim(), clientName: 'cli', clientVersion: '0.1.0' });
  await until(() => artifact.frames.some(f => f.type === 'hello_ok'), 'authenticated hello');
  send({ type: 'request', id: 'turn', method: 'submit_turn', params: { sessionKey: 'ra05-google-tool', channelKey: 'test', message: PROMPT } });
  await until(() => artifact.frames.some(f => f.id === 'turn' && f.final) || exit, 'tool continuation turn'); assert.equal(exit, null);
  send({ type: 'request', id: 'history', method: 'read_session_messages', params: { sessionKey: 'ra05-google-tool' } });
  await until(() => artifact.frames.some(f => f.id === 'history' && f.type === 'response'), 'durable history');
  const history = artifact.frames.find(f => f.id === 'history' && f.type === 'response'); assert.equal(history.ok, true); artifact.history = history.result;
  artifact.captureComplete = true;
} catch (error) { fatal = error; artifact.errors.push(String(error.stack ?? error)); }
finally {
  socket?.terminate();
  if (child && !exit) { child.kill('SIGINT'); await Promise.race([ended, pause(5000)]); }
  if (child && !exit) { try { process.kill(-child.pid, 'SIGKILL'); } catch (e) { if (e.code !== 'ESRCH') artifact.errors.push(String(e)); } await Promise.race([ended, pause(3000)]); }
  let groupGone = !child?.pid;
  if (child?.pid) { try { process.kill(-child.pid, 0); } catch (e) { if (e.code === 'ESRCH') groupGone = true; else artifact.errors.push(String(e)); } }
  let socketReleased = false;
  if (stdout.includes('WebSocket: ws://')) { const port = Number(new URL(stdout.match(/WebSocket: (ws:\/\/[^\s]+)/)[1]).port); const probe = createServer(); socketReleased = await new Promise(done => { probe.once('error', () => done(false)); probe.listen(port, '127.0.0.1', () => probe.close(() => done(true))); }); }
  mock.closeAllConnections(); await new Promise(done => mock.close(done));
  artifact.cleanup = { exit, groupGone, socketReleased };
  artifact.inputAfterSha256 = sha(readFileSync(join(pilot, 'ra05-input.txt')));
  artifact.workspaceFiles = readdirSync(pilot).sort();
  const files = [];
  function collectAudit(directory, relative = '') {
    for (const entry of readdirSync(directory, {withFileTypes:true})) {
      const name = relative ? relative+'/'+entry.name : entry.name;
      if (entry.isDirectory()) collectAudit(join(directory,entry.name),name);
      else if (entry.isFile() && (entry.name === 'invocations.jsonl' || entry.name.endsWith('.pending.json'))) { const text = readFileSync(join(directory,entry.name),'utf8'); files.push({path:name,text,sha256:sha(text)}); }
    }
  }
  try { collectAudit(join(out,'audit')); } catch (error) { artifact.errors.push('audit collection: '+error); }
  artifact.audit = {files};
  writeFileSync(join(out,'audit-evidence.json'),json(artifact.audit));
  writeFileSync(join(out, 'stdout.log'), stdout); writeFileSync(join(out, 'stderr.log'), stderr);
  writeFileSync(join(out, 'capture.json'), json(artifact)); writeFileSync(join(out, 'identity.json'), json(identity));
}
if (fatal) throw fatal;
const result = verify(artifact); writeFileSync(join(out, 'strict.json'), json(result)); console.log(json(result));
