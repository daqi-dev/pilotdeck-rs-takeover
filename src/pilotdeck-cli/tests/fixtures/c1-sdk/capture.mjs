// Independently authored C1 formal-entry baseline recorder. No reference source is copied.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';
import { EventEmitter, once } from 'node:events';

const REVISION = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const NODE = 'v22.22.0';
const SOURCE_HASHES = {
  'src/cli/sdkCli.ts': 'd41c08ce59380f85bb81ec8bd2d9bc876c778f9b2ebaea3fd25b1b5f08e288ec',
  'packages/sdk/src/client.ts': '7dc380d0a7e0d3ac1d97210a36b2a18855d27936de9c12ea6cedf74f75e92c2a',
  'packages/sdk/src/transport.ts': '793e38f0273836621c54a6bf9fb19645e4924343be9e6825d1fd06227136cb7f',
  'dist/src/cli/pilotdeck.js': '4489ef95805baa7ae2c278aec9bd4683ee6a8b4d838271a984142303f8bd4483',
  'dist/src/cli/sdkCli.js': '5734eed2c8e008e22ebd9f9a48b20cfe8808fc16519e32045fe05e08eb1333dc',
  'packages/sdk/dist/client.js': '1fe1552964726337893b6f294552a385c5cdf5d16a0530e32b5d17f256a2bc49',
  'packages/sdk/dist/transport.js': '065e0c379092571710f7ca3a2e02d9e417e43029fb8f4fadc1e305f7dc105d87',
};
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const save = (path, bytes) => writeFileSync(path, bytes, { flag: 'wx', mode: 0o600 });
const saveJson = (path, value) => save(path, JSON.stringify(value, null, 2) + '\n');
async function bounded(promise, milliseconds, label) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} exceeded ${milliseconds}ms`)), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}
async function observe(promise, milliseconds) {
  let timer;
  try { await Promise.race([promise, new Promise(done => { timer = setTimeout(done, milliseconds); })]); }
  finally { clearTimeout(timer); }
}
async function cleanupOwned({ child, childClosed, exit, sockets, server }, milliseconds = 3000) {
  const failures = [];
  const attempt = async action => { try { await action(); } catch (error) { failures.push(error); } };
  await attempt(() => {
    if (child && exit === null) {
      assert.ok(child.pid, 'cleanup owns a concrete PID');
      const sent = child.kill('SIGKILL');
      assert.ok(sent || child.exitCode !== null || child.signalCode !== null,
        `cleanup could not signal owned PID ${child.pid}`);
    }
  });
  if (childClosed) await attempt(() => bounded(childClosed, milliseconds, 'owned child and pipes cleanup'));
  const socketClosures = sockets.map(socket => socket.readyState === 3 ? Promise.resolve() : once(socket, 'close'));
  for (const closed of socketClosures) closed.catch(() => {});
  for (const socket of sockets) await attempt(() => socket.terminate());
  await attempt(() => bounded(Promise.all(socketClosures), milliseconds, 'owned sockets cleanup'));
  if (server) await attempt(() => bounded(new Promise((done, reject) => {
    server.close(error => error ? reject(error) : done());
  }), milliseconds, 'owned mock cleanup'));
  return failures;
}
async function testCleanup() {
  for (const mode of ['kill-false', 'child-timeout', 'already-exited']) {
    const actions = [];
    const socket = new EventEmitter();
    socket.readyState = 1;
    socket.terminate = () => { actions.push('socket'); socket.readyState = 3; socket.emit('close'); };
    const child = { pid: 123, exitCode: mode === 'already-exited' ? 0 : null, signalCode: null,
      kill: signal => { assert.equal(signal, 'SIGKILL'); actions.push('kill'); return mode === 'child-timeout'; } };
    const server = { close: done => { actions.push('server'); done(); } };
    const failures = await cleanupOwned({ child, exit: null, sockets: [socket], server,
      childClosed: mode === 'child-timeout' ? new Promise(() => {}) : Promise.resolve() }, 10);
    assert.deepEqual(actions, ['kill', 'socket', 'server'], `${mode}: all owned resources reached`);
    assert.equal(failures.length, mode === 'already-exited' ? 0 : 1);
    assert.equal(socket.readyState, 3);
  }
  console.log('3/3 cleanup fault injections passed (no product processes started)');
}
function businessStderr(raw) {
  return raw.replace(/^\(node:[0-9]+\) ExperimentalWarning: SQLite is an experimental feature and might change at any time\n\(Use `node --trace-warnings \.\.\.` to show where the warning was created\)\n/, '');
}
function comparable(result) {
  const requests = structuredClone(result.requests);
  const ids = new Map();
  for (const request of requests) {
    if (request.id !== undefined) {
      assert.match(request.id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
      assert.ok(!ids.has(request.id), 'request UUIDs must be unique');
      ids.set(request.id, ids.size);
      request.id = `request-${ids.get(request.id)}`;
    }
    if (request.params?.runId !== undefined) {
      assert.match(request.params.runId, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
      request.params.runId = 'generated-run';
    }
  }
  return {
    command: result.command, mode: result.mode, naturalExit: result.naturalExit,
    cleanupExit: result.cleanupExit, activeBeforeCleanup: result.activeBeforeCleanup,
    closesBeforeCleanup: result.closesBeforeCleanup, requests,
    stdout: result.stdout, stderr: businessStderr(result.stderr),
  };
}

if (process.argv[2] === '--self-test-cleanup') {
  assert.equal(process.argv.length, 3);
  await testCleanup();
} else if (process.argv[2] === '--compare') {
  assert.equal(process.argv.length, 5, 'usage: capture.mjs --compare FIRST/summary.json SECOND/summary.json');
  const first = JSON.parse(readFileSync(process.argv[3]));
  const second = JSON.parse(readFileSync(process.argv[4]));
  for (const [summary, path] of [[first, process.argv[3]], [second, process.argv[4]]]) {
    assert.ok(summary.suite === 'errors' || summary.suite === 'hello');
    assert.equal(summary.revision, REVISION);
    assert.equal(summary.node, NODE);
    assert.deepEqual(summary.sourceSha256, SOURCE_HASHES);
    assert.equal(summary.results.length, summary.suite === 'errors' ? 10 : 2);
    assert.ok(summary.results.every(result => result.verified && result.activeAfterCleanup === 0));
    const modes = summary.suite === 'hello' ? ['hello-timeout'] : ['missing-project', 'query-reject', 'model-ambiguous', 'stream-reject', 'success'];
    assert.deepEqual(summary.results.map(({ command, mode }) => `${command}-${mode}`),
      ['run', 'resume'].flatMap(command => modes.map(mode => `${command}-${mode}`)));
    for (const result of summary.results) {
      const directory = join(dirname(resolve(path)), `${result.command}-${result.mode}`);
      assert.deepEqual(JSON.parse(readFileSync(join(directory, 'result.json'))), result);
      for (const stream of ['stdout', 'stderr']) {
        const raw = readFileSync(join(directory, `${stream}.bin`));
        assert.equal(sha(raw), result[`${stream}Sha256`], 'raw stream identity');
        assert.equal(raw.toString(), result[stream], 'summary retains raw stream bytes');
      }
    }
  }
  assert.equal(first.suite, second.suite);
  assert.equal(first.driverSha256, second.driverSha256);
  assert.deepEqual(first.results.map(comparable), second.results.map(comparable));
  console.log(`${first.results.length}/${first.results.length} TS captures match; raw evidence remains unchanged`);
} else {
  assert.ok(process.argv.length >= 3 && process.argv.length <= 4, 'usage: capture.mjs OUTPUT [errors|hello]');
  assert.equal(process.version, NODE, 'fixed Node runtime');
  const baseline = realpathSync(process.env.PDRS_BASELINE ?? '/Users/da/ws/pilotdeck-rs/resources/PilotDeck');
  const output = resolve(process.argv[2]);
  const suite = process.argv[3] ?? 'errors';
  assert.ok(suite === 'errors' || suite === 'hello', 'known capture suite');
  assert.ok(output !== baseline && !output.startsWith(baseline + sep), 'baseline is read-only');
  const git = spawnSync('git', ['-C', baseline, 'rev-parse', 'HEAD'], { encoding: 'utf8' });
  assert.equal(git.status, 0, 'baseline identity command');
  assert.equal(git.stdout.trim(), REVISION, 'fixed baseline revision');
  const hashes = Object.fromEntries(Object.keys(SOURCE_HASHES).map(path => [path, sha(readFileSync(join(baseline, path)))]));
  assert.deepEqual(hashes, SOURCE_HASHES, 'fixed SDK source and formal-entry build identity');
  const require = createRequire(join(baseline, 'package.json'));
  const { WebSocketServer } = require('ws');
  assert.ok(!realpathSync(dirname(output)).startsWith(baseline + sep), 'output parent must not alias the baseline');
  assert.notEqual(realpathSync(dirname(output)), baseline, 'output parent must not be the baseline');
  mkdirSync(output, { mode: 0o700 });
  const identity = {
    revision: REVISION, node: process.version, platform: process.platform, arch: process.arch,
    baseline, suite, sourceSha256: hashes,
    driverSha256: sha(readFileSync(fileURLToPath(import.meta.url))),
  };
  saveJson(join(output, 'identity.json'), identity);
  const results = [];
  const modes = suite === 'hello' ? ['hello-timeout'] : ['missing-project', 'query-reject', 'model-ambiguous', 'stream-reject', 'success'];
  for (const command of ['run', 'resume']) for (const mode of modes) {
    const directory = join(output, `${command}-${mode}`);
    mkdirSync(directory, { mode: 0o700 });
    for (const name of ['home', 'pilot', 'project', 'tmp']) mkdirSync(join(directory, name), { mode: 0o700 });
    const requests = [], closes = [], sockets = [], stdout = [], stderr = [];
    let server, child, exit = null, naturalExit = null, activeBeforeCleanup = null;
    let closesBeforeCleanup = [], failure, childClosed, reached, rejectFailure;
    const failed = new Promise((_, reject) => { rejectFailure = reject; });
    failed.catch(() => {});
    const milestone = new Promise(done => { reached = done; });
    const expectedError = mode === 'missing-project' ? 'projectKey is required when options.model is set.\n'
      : mode === 'model-ambiguous' ? 'Model is not uniquely resolvable: missing\n'
      : mode === 'hello-timeout' ? 'Gateway hello timed out.\n' : 'c1 fixture rejected\n';
    try {
      server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
      server.on('error', rejectFailure);
      await bounded(once(server, 'listening'), 3000, 'mock listen');
      server.on('connection', socket => {
        const connection = sockets.push(socket);
        socket.on('error', rejectFailure);
        socket.on('close', (code, reason) => closes.push({ connection, code, reason: String(reason) }));
        socket.on('message', bytes => {
          try {
            const request = JSON.parse(bytes);
            requests.push({ connection, ...request });
            if (request.type === 'hello') {
              assert.equal(request.token, 'c1-fixture');
              if (mode === 'hello-timeout' && connection === 2) return;
              socket.send(JSON.stringify({ type: 'hello_ok', protocolVersion: '1.2', serverInfo: { capabilities: [] } }));
              return;
            }
            assert.equal(request.type, 'request');
            assert.ok(['new_session', 'resume_session', 'model_catalog_list', 'submit_turn'].includes(request.method));
            const reject = (mode === 'query-reject' && connection === 2 && request.method === 'resume_session')
              || (mode === 'stream-reject' && request.method === 'submit_turn');
            if (reject) {
              socket.send(JSON.stringify({ type: 'response', id: request.id, ok: false, error: { code: 'server_error', message: 'c1 fixture rejected' } }));
            } else if (request.method === 'submit_turn') {
              for (const [seq, event] of [{ type: 'assistant_text_delta', text: 'c1 success' }, { type: 'turn_completed', usage: {} }].entries()) {
                socket.send(JSON.stringify({ type: 'event', id: request.id, seq, final: seq === 1, event }));
              }
            } else {
              socket.send(JSON.stringify({ type: 'response', id: request.id, ok: true,
                result: request.method === 'model_catalog_list' ? { items: [] } : { sessionKey: 'c1-session' } }));
            }
          } catch (error) { rejectFailure(error); }
        });
      });
      const argv = [join(baseline, 'dist/src/cli/pilotdeck.js'), command,
        ...(command === 'resume' ? ['c1-session'] : []), 'fixture prompt'];
      if (mode === 'missing-project') argv.push('--model', 'fixture/model');
      if (mode === 'model-ambiguous') argv.push('--project', 'c1-project', '--model', 'missing');
      const env = {
        HOME: join(directory, 'home'), USERPROFILE: join(directory, 'home'), CFFIXED_USER_HOME: join(directory, 'home'),
        PILOT_HOME: join(directory, 'pilot'), TMPDIR: join(directory, 'tmp'), TMP: join(directory, 'tmp'), TEMP: join(directory, 'tmp'),
        XDG_CONFIG_HOME: join(directory, 'home'), XDG_CACHE_HOME: join(directory, 'home'), XDG_DATA_HOME: join(directory, 'home'),
        PILOTDECK_SDK_GATEWAY_URL: `ws://127.0.0.1:${server.address().port}/ws`, PILOTDECK_SDK_AUTH_TOKEN: 'c1-fixture',
        ANALYTICS_ENABLED: '0', DOCKER_CONTAINER: '1', LANG: 'C', LC_ALL: 'C', TZ: 'UTC', PATH: '',
      };
      saveJson(join(directory, 'invocation.json'), { executable: process.execPath, argv, env, cwd: join(directory, 'project') });
      child = spawn(process.execPath, argv, { cwd: join(directory, 'project'), env, stdio: ['ignore', 'pipe', 'pipe'] });
      child.on('error', rejectFailure);
      child.on('exit', (code, signal) => { exit = { code, signal }; });
      childClosed = new Promise(done => child.on('close', done));
      child.stdout.on('data', bytes => stdout.push(bytes));
      child.stderr.on('data', bytes => {
        stderr.push(bytes);
        if (businessStderr(Buffer.concat(stderr).toString()) === expectedError) reached();
      });
      await bounded(Promise.race([mode === 'success' ? childClosed : milestone, childClosed, failed]), 15000, 'formal CLI reaches result');
      if (mode !== 'success') await observe(Promise.race([childClosed, failed]), 3500);
      naturalExit = exit;
      activeBeforeCleanup = sockets.filter(socket => socket.readyState === 1).length;
      closesBeforeCleanup = [...closes];
    } catch (error) { failure = error; }
    finally {
      const cleanupFailures = await cleanupOwned({ child, childClosed, exit, sockets, server });
      if (cleanupFailures.length) failure ??= new AggregateError(cleanupFailures, 'owned resource cleanup failed');
    }
    const result = {
      command, mode, pid: child?.pid ?? null, naturalExit,
      cleanupExit: naturalExit ? null : exit, activeBeforeCleanup, closesBeforeCleanup,
      activeAfterCleanup: sockets.filter(socket => socket.readyState !== 3).length,
      stdout: Buffer.concat(stdout).toString(), stderr: Buffer.concat(stderr).toString(),
      stdoutSha256: sha(Buffer.concat(stdout)), stderrSha256: sha(Buffer.concat(stderr)), requests,
      failure: failure ? String(failure) : null, verified: false,
    };
    save(join(directory, 'stdout.bin'), Buffer.concat(stdout));
    save(join(directory, 'stderr.bin'), Buffer.concat(stderr));
    try {
      assert.ifError(failure);
      assert.equal(result.activeAfterCleanup, 0);
      const expected = [[1, 'hello'], [1, command === 'run' ? 'new_session' : 'resume_session'], [2, 'hello']];
      if (mode !== 'hello-timeout') expected.push([2, 'resume_session']);
      if (mode === 'model-ambiguous') expected.push([2, 'model_catalog_list']);
      if (mode === 'stream-reject' || mode === 'success') expected.push([2, 'submit_turn']);
      assert.deepEqual(requests.map(request => [request.connection, request.method ?? 'hello']), expected);
      if (mode === 'success') {
        assert.deepEqual(naturalExit, { code: 0, signal: null });
        assert.equal(result.stdout, 'c1 success\n');
        assert.equal(businessStderr(result.stderr), '');
        assert.equal(activeBeforeCleanup, 0);
      } else {
        assert.equal(naturalExit, null, 'baseline failed query must remain alive after its error');
        assert.deepEqual(result.cleanupExit, { code: null, signal: 'SIGKILL' });
        assert.equal(activeBeforeCleanup, 1);
        assert.deepEqual(closesBeforeCleanup, [{ connection: 1, code: 1005, reason: '' }]);
        assert.equal(result.stdout, '');
        assert.equal(businessStderr(result.stderr), expectedError);
      }
      result.verified = true;
    } catch (error) { result.failure = String(error); }
    saveJson(join(directory, 'result.json'), result);
    results.push(result);
    if (!result.verified) {
      saveJson(join(output, 'failure.json'), { ...identity, results });
      throw new Error(`capture failed: ${command}/${mode}: ${result.failure}; first failure retained in ${directory}`);
    }
  }
  saveJson(join(output, 'summary.json'), { ...identity, results });
  console.log(JSON.stringify(results.map(({ command, mode, naturalExit, activeBeforeCleanup }) => ({ command, mode, naturalExit, activeBeforeCleanup })), null, 2));
}
