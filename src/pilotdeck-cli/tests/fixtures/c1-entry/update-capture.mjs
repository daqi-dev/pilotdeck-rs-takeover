#!/usr/bin/env node
// Observe the fixed baseline's real update entry against a local Web double.
// Every database, credential and endpoint belongs to this invocation.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import { createHash, createHmac } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
import { spawn, spawnSync } from 'node:child_process';

const [baselineArg, outputArg] = process.argv.slice(2);
assert.ok(baselineArg && outputArg, 'usage: update-capture.mjs BASELINE NEW_OUTPUT_DIR');
const baseline = fs.realpathSync(baselineArg);
const output = path.resolve(outputArg);
assert.equal(spawnSync('git', ['-C', baseline, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim(),
  'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', 'fixed TS source');
fs.mkdirSync(output);
const sources = ['dist/src/cli/pilotdeck.js', 'dist/src/runtime/updateCommand.js',
  'scripts/update-web.mjs', 'ui/server/services/webUpdateCommand.js'];
fs.writeFileSync(path.join(output, 'identity.json'), JSON.stringify({
  sourceCommit: 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', node: process.version,
  sourceSha256: Object.fromEntries(sources.map(name => [name,
    createHash('sha256').update(fs.readFileSync(path.join(baseline, name))).digest('hex')])),
}, null, 2) + '\n');

const cases = [
  { id: 'check', args: ['--check'], text: 'Release available: c1-fixture (01234567)', exit: 0 },
  { id: 'up-to-date', args: [], text: 'Already at the latest Release.', exit: 0 },
  { id: 'apply', args: [], text: 'Update prepared. Restart PilotDeck manually to load the new version.', exit: 0 },
  { id: 'restart', args: ['--restart'], text: 'Update prepared; the running Web service accepted the restart request.', exit: 0 },
  { id: 'apply-error', args: [], text: 'C1 mock apply denied', exit: 1 },
  ...['SIGINT', 'SIGTERM'].flatMap(signal => ['pid', 'group'].map(target => ({
    id: `cancel-${target}-${signal.toLowerCase()}`, args: [], signal, target,
  }))),
];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const activeGroups = new Set();
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => {
  for (const pid of activeGroups) { try { process.kill(-pid, 'SIGKILL'); } catch {} }
  process.exit(signal === 'SIGINT' ? 130 : 143);
});
let failed = false;
for (const spec of cases) {
  const root = path.join(output, spec.id);
  const home = path.join(root, 'home'); const pilot = path.join(home, '.pilotdeck');
  const project = path.join(root, 'project');
  fs.mkdirSync(pilot, { recursive: true }); fs.mkdirSync(project);
  const config = path.join(pilot, 'pilotdeck.yaml');
  fs.writeFileSync(config, 'schemaVersion: 1\ntelemetry:\n  enabled: false\n');
  const database = path.join(pilot, 'auth.db');
  const db = new DatabaseSync(database);
  db.exec("CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT); INSERT INTO users VALUES (1, 'c1-mock-user'); CREATE TABLE app_config (key TEXT PRIMARY KEY, value TEXT); INSERT INTO app_config VALUES ('jwt_secret', 'c1-local-mock-signing-key');");
  db.close();
  const requests = []; let requestFailure; let applyActive = false; let releaseApply;
  const applyStarted = new Promise(resolve => { releaseApply = resolve; });
  const server = http.createServer(async (req, res) => {
    try {
      const chunks = []; for await (const chunk of req) chunks.push(chunk);
      const text = Buffer.concat(chunks).toString();
      const body = text ? JSON.parse(text) : undefined;
      const record = { method: req.method, path: req.url, ...(body ? { body } : {}) };
      if (req.url !== '/health') {
        const raw = String(req.headers.authorization ?? '');
        const token = raw.replace(/^Bearer /, '').split('.');
        assert.equal(token.length, 3, 'mock Web request has JWT');
        assert.equal(createHmac('sha256', 'c1-local-mock-signing-key').update(`${token[0]}.${token[1]}`).digest('base64url'), token[2], 'JWT uses isolated mock secret');
        const claims = JSON.parse(Buffer.from(token[1], 'base64url').toString());
        assert.equal(claims.username, 'c1-mock-user'); assert.equal(claims.userId, 1);
        record.authenticatedMockUser = true;
      }
      requests.push(record);
      res.setHeader('content-type', 'application/json');
      if (req.url === '/health') return res.end('{"ok":true}');
      if (req.url === '/api/update/context') return res.end(JSON.stringify({ projectRoot: baseline }));
      if (req.url === '/api/update/check') return res.end(JSON.stringify(spec.id === 'up-to-date'
        ? { canUpdate: false, reason: 'upToDate' }
        : { canUpdate: true, latest: { tagName: 'c1-fixture', sourceSha: '0123456789abcdef0123456789abcdef01234567' } }));
      if (req.url === '/api/update/apply') {
        if (spec.id === 'apply-error') { res.statusCode = 409; return res.end('{"reason":"fixtureDenied","message":"C1 mock apply denied"}'); }
        res.setHeader('content-type', 'application/x-ndjson');
        res.write('{"status":"progress","message":"C1 mock apply started"}\n');
        if (spec.signal) {
          applyActive = true; res.once('close', () => { applyActive = false; });
          releaseApply(); return;
        }
        return res.end('{"stage":"complete","status":"success"}\n');
      }
      if (req.url === '/api/update/restart') return res.end('{"status":"accepted"}');
      throw Error(`unexpected mock endpoint ${req.method} ${req.url}`);
    } catch (error) {
      requestFailure = error.message; res.statusCode = 500; res.end(JSON.stringify({ error: error.message }));
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const child = spawn(process.execPath, [path.join(baseline, 'dist/src/cli/pilotdeck.js'), 'update', ...spec.args], {
    cwd: project, detached: true, stdio: ['ignore', 'pipe', 'pipe'], env: {
      PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home,
      PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: config, DATABASE_PATH: database,
      SERVER_PORT: String(port), TMPDIR: root, LANG: 'C', LC_ALL: 'C', TZ: 'UTC',
      TERM: 'dumb', NO_COLOR: '1', COMMIT_HASH: 'c1-fixture',
    },
  });
  if (child.pid) activeGroups.add(child.pid);
  child.stdout.setEncoding('utf8'); child.stderr.setEncoding('utf8');
  let stdout = '', stderr = '', deadline = false, spawnFailure;
  child.stdout.on('data', text => { stdout += text; }); child.stderr.on('data', text => { stderr += text; });
  child.once('error', error => { spawnFailure = error.message; });
  const exited = new Promise(resolve => {
    child.once('exit', (exitCode, signal) => resolve({ exitCode, signal }));
    child.once('error', () => resolve({ exitCode: null, signal: null }));
  });
  const closed = new Promise(resolve => child.once('close', resolve));
  const killOwnedGroup = () => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} };
  const timer = setTimeout(() => { deadline = true; killOwnedGroup(); }, 10000);
  let afterCancel;
  if (spec.signal) {
    const ready = await Promise.race([applyStarted.then(() => true), exited.then(() => false)]);
    if (ready) {
      await sleep(50);
      process.kill(spec.target === 'group' ? -child.pid : child.pid, spec.signal);
    }
  }
  const result = await exited;
  if (spec.signal) {
    await sleep(200);
    let ownedGroupExists = false;
    try { process.kill(-child.pid, 0); ownedGroupExists = true; } catch {}
    afterCancel = { ownedGroupExists, mockApplyConnectionOpen: applyActive };
  }
  killOwnedGroup();
  await closed; clearTimeout(timer);
  activeGroups.delete(child.pid);
  server.closeAllConnections(); await new Promise(resolve => server.close(resolve));
  let assertionFailure;
  try {
    assert.equal(deadline, false, 'bounded process termination');
    assert.equal(spawnFailure, undefined); assert.equal(requestFailure, undefined);
    assert.ok(requests.some(r => r.path === '/api/update/check'), 'formal updater reached authenticated local check');
    if (spec.signal) assert.ok(requests.some(r => r.path === '/api/update/apply'), 'cancel during real update child request');
    else { assert.equal(result.exitCode, spec.exit); assert.equal(result.signal, null); assert.ok(stdout.includes(spec.text), spec.text); }
  } catch (error) { assertionFailure = error.message; failed = true; }
  const artifact = { id: spec.id, argv: ['update', ...spec.args], pid: child.pid, ...result,
    stdout, stderr, requests, ...(afterCancel ? { afterCancel } : {}),
    ...(assertionFailure ? { assertionFailure } : {}) };
  fs.writeFileSync(path.join(root, 'result.json'), JSON.stringify(artifact, null, 2) + '\n');
  console.log(`${spec.id}: ${assertionFailure ? `FAIL ${assertionFailure}` : 'PASS'}${afterCancel ? ` ${JSON.stringify(afterCancel)}` : ''}`);
}
process.exitCode = failed ? 1 : 0;
