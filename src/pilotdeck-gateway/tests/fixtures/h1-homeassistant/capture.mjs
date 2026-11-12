#!/usr/bin/env node
// Formal CLI server, unchanged baseline modules, loopback HA and model peers.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import { createRequire } from 'node:module';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const BASELINE = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const json = value => JSON.stringify(value, null, 2) + '\n';
const hash = value => createHash('sha256').update(value).digest('hex');
async function until(test, label, ms = 20000) {
  const end = Date.now() + ms;
  while (!test()) { assert.ok(Date.now() < end, `deadline: ${label}`); await sleep(10); }
}
const lastUser = body => [...body.messages].reverse().find(message => message.role === 'user')?.content;
function state(text, { entity = 'conversation.fixture', old = 'old', event = 'state_changed' } = {}) {
  return { type: 'event', event: { event_type: event, data: { entity_id: entity, new_state: { state: text }, old_state: { state: old } } } };
}
export function verify(contract) {
  assert.deepEqual(contract.exit, { code: 0, signal: null });
  if (contract.reconnectFailure) {
    assert.deepEqual(contract.auth, [{ type: 'auth', access_token: 'fixture-ha-token' }]);
    assert.deepEqual(contract.subscriptions, [{ id: 1, type: 'subscribe_events', event_type: 'state_changed' }]);
    assert.equal(contract.connections, 1); assert.equal(contract.paths.length, 2);
    assert.equal(contract.notifications.length, 3); assert.equal(contract.modelInputs.length, 2);
    assert.deepEqual(contract.reconnectFailure.errors, ['homeassistant: ws error: [object ErrorEvent]', 'homeassistant: ws error: [object ErrorEvent]']);
    assert.equal(contract.reconnectFailure.noFurtherHandshake, true);
    return;
  }
  assert.deepEqual(contract.auth, [{ type: 'auth', access_token: 'fixture-ha-token' }, { type: 'auth', access_token: 'fixture-ha-token' }]);
  assert.deepEqual(contract.subscriptions, [{ id: 1, type: 'subscribe_events', event_type: 'state_changed' }, { id: 5, type: 'subscribe_events', event_type: 'state_changed' }]);
  assert.equal(contract.connections, 2);
  assert.equal(contract.busySkipped, true);
  assert.deepEqual(contract.paths, ['/prefix/api/websocket', '/prefix/api/websocket']);
  assert.deepEqual(contract.notifications.slice(0, 4).map(frame => frame.service_data.message), ['ha-answer 中文😀', '已创建新会话。', 'released-answer', 'ha-answer 中文😀']);
  assert.deepEqual(contract.notifications.slice(0, 4).map(frame => frame.id), [2, 3, 4, 6]);
  assert.equal(contract.notifications.length, contract.interactions ? 9 : contract.cron ? 5 : 4);
  for (const frame of contract.notifications) {
    assert.equal(frame.type, 'call_service'); assert.equal(frame.domain, 'persistent_notification'); assert.equal(frame.service, 'create');
    assert.equal(frame.service_data.title, 'Gateway · conversation.fixture');
    assert.equal(frame.service_data.notification_id, 'gw_<time>');
  }
  assert.equal(contract.modelInputs.length, contract.interactions ? 7 : (contract.stopProbe || contract.cron) ? 4 : 3);
  assert.match(JSON.stringify(contract.modelInputs[0]), /hello 中文/);
  assert.match(JSON.stringify(contract.modelInputs[1]), /hold-turn/);
  assert.match(JSON.stringify(contract.modelInputs[2]), /after-reconnect/);
  assert.equal(JSON.stringify(contract.modelHistory[1]).includes('hello 中文'), false, '/new clears the prior chat history');
  assert.equal(JSON.stringify(contract.modelHistory[2]).includes('hello 中文'), false);
  assert.match(JSON.stringify(contract.modelHistory[2]), /hold-turn/);
  assert.match(JSON.stringify(contract.modelHistory[2]), /released-answer/);
  assert.equal(JSON.stringify(contract.modelHistory).includes('must-skip'), false);
  if (contract.interactions) {
    assert.match(contract.notifications[4].service_data.message, /Colour\?/);
    assert.equal(contract.notifications[5].service_data.message, 'elicitation-done');
    assert.match(contract.notifications[6].service_data.message, /bash/);
    assert.match(contract.notifications[7].service_data.message, /拒绝/);
    assert.match(contract.notifications[8].service_data.message, /permission-done/);
    assert.equal(contract.interactions.answeredBlue, true);
    assert.equal(contract.interactions.deniedTool, true);
  }
  if (contract.cron) {
    assert.equal(contract.cron.started, true);
    assert.equal(contract.cron.completed, true);
    assert.equal(contract.notifications[4].service_data.message, 'cron-answer');
    assert.equal(contract.notifications[4].service_data.title, 'Gateway · conversation.fixture');
  }
  if (contract.stopProbe) {
    assert.equal(contract.stopProbe.gatewayHealthy, true);
    assert.equal(contract.stopProbe.haClosed, true);
    assert.equal(contract.stopProbe.providerStillOpen, contract.stopProbe.expectation === 'baseline-retains', 'owned provider lifecycle after channel disable');
    assert.equal(contract.stopProbe.oldReplyRejected, contract.stopProbe.expectation === 'baseline-retains');
  }
  assert.deepEqual(contract.problems, []);
}
export async function capture({ baseline, out, command, args, stopProbe = false, stopExpectation = 'baseline-retains', fault = null, interactions = false, cron = false, reconnectRejected = false }) {
  assert.equal(process.version, 'v22.22.0');
  assert.equal(spawnSync('git', ['-C', baseline, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).stdout.trim(), BASELINE);
  fs.mkdirSync(out); out = fs.realpathSync(out);
  fs.copyFileSync(fileURLToPath(import.meta.url), path.join(out, 'driver.mjs'));
  const home = path.join(out, 'home'), pilot = path.join(home, '.pilotdeck'), project = path.join(out, 'project'), tmp = path.join(out, 'tmp');
  for (const dir of [pilot, project, tmp]) fs.mkdirSync(dir, { recursive: true });
  const { WebSocketServer, WebSocket } = createRequire(path.join(baseline, 'package.json'))('ws');
  const clients = [], frames = [], paths = [], models = [], problems = [], sockets = new Set();
  let heldModel;
  const reply = (response, text) => {
    if (!response.headersSent) response.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const delta of [{ role: 'assistant' }, { content: text }]) response.write(`data: ${JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`);
    response.end(`data: ${JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 3, total_tokens: 13 } })}\n\ndata: [DONE]\n\n`);
  };
  const tool = (response, name, args) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.write(`data: ${JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', choices: [{ index: 0, delta: { role: 'assistant', tool_calls: [{ index: 0, id: `fixture-${name}`, type: 'function', function: { name, arguments: JSON.stringify(args) } }] }, finish_reason: null }] })}\n\n`);
    response.end(`data: ${JSON.stringify({ id: 'fixture', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] })}\n\ndata: [DONE]\n\n`);
  };
  const mock = http.createServer((request, response) => {
    const chunks = []; request.on('data', chunk => chunks.push(chunk)); request.on('end', () => {
      const raw = Buffer.concat(chunks).toString();
      if (request.method !== 'POST' || request.url !== '/v1/chat/completions') { problems.push(`${request.method} ${request.url}`); response.writeHead(404); response.end(); return; }
      const body = JSON.parse(raw); models.push(body);
      fs.appendFileSync(path.join(out, 'model.jsonl'), json(body));
      if (body.stream === false) { response.writeHead(200, { 'content-type': 'application/json' }); response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"title":"Fixture session"}' }, finish_reason: 'stop' }] })); return; }
      const user = JSON.stringify(lastUser(body));
      if (user.includes('h1-cron')) { reply(response, 'cron-answer'); return; }
      if (user.includes('h1-elicitation')) {
        if (body.messages.at(-1).role === 'tool') reply(response, 'elicitation-done');
        else tool(response, 'ask_user_question', { questions: [{ header: 'Pick', question: 'Colour?', options: [{ label: 'Red', description: 'first' }, { label: 'Blue', description: 'second' }] }] });
        return;
      }
      if (user.includes('h1-permission')) {
        if (body.messages.at(-1).role === 'tool') reply(response, 'permission-done');
        else tool(response, 'bash', { command: 'printf fixture > h1-permission.txt', description: 'Fixture permission prompt' });
        return;
      }
      if (JSON.stringify(lastUser(body)).includes('hold-turn')) {
        response.writeHead(200, { 'content-type': 'text/event-stream' }); response.write(': fixture holding model body\n\n'); heldModel = response;
      }
      else reply(response, 'ha-answer 中文😀');
    });
  });
  mock.on('connection', socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
  const ws = new WebSocketServer({ noServer: true });
  mock.on('upgrade', (request, socket, head) => { paths.push(request.url); fs.appendFileSync(path.join(out, 'ha-upgrade.jsonl'), JSON.stringify({ url: request.url, headers: request.headers, rawHeaders: request.rawHeaders }) + '\n'); if (reconnectRejected && paths.length > 1) { socket.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n'); return; } ws.handleUpgrade(request, socket, head, client => ws.emit('connection', client)); });
  ws.on('connection', client => {
    clients.push(client); client.send('{"type":"auth_required"}');
    client.on('message', raw => {
      const frame = JSON.parse(raw.toString()); frames.push(frame);
      fs.appendFileSync(path.join(out, 'ha.jsonl'), JSON.stringify({ at: Date.now(), connection: clients.indexOf(client), frame }) + '\n');
      if (frame.type === 'auth') client.send('{"type":"auth_ok"}');
      // Rejected subscription is deliberate: the baseline still starts.
      if (frame.type === 'subscribe_events') client.send(JSON.stringify({ type: 'result', id: frame.id, success: false }));
      if (frame.type === 'call_service') client.send(JSON.stringify({ type: 'result', id: frame.id, success: true }));
    });
  });
  await new Promise(resolve => mock.listen(0, '127.0.0.1', resolve));
  const port = mock.address().port; assert.notEqual(port, 18789);
  const config = `schemaVersion: 1\nagent:\n  model: fixture/deterministic\n  maxContextTokens: 65536\n  maxOutputTokens: 8192\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:${port}/v1\n      apiKey: fixture-only\n      models:\n        deterministic:\n          capabilities:\n            supportsToolUse: true\n            maxContextTokens: 65536\n            maxOutputTokens: 8192\ntelemetry:\n  enabled: false\nadapters:\n  homeassistant:\n    enabled: true\n    token: fixture-ha-token\n    extra:\n      url: http://127.0.0.1:${port}/prefix/\n      watchPrefixes: [sensor.]\n      notificationTitle: must-not-be-forwarded\n`;
  const effectiveConfig = (fault === 'auth-token' ? config.replace('token: fixture-ha-token', 'token: wrong-fixture-token') : config) + (cron ? 'cron:\n  enabled: true\n' : '');
  fs.writeFileSync(path.join(pilot, 'pilotdeck.yaml'), effectiveConfig);
  const env = { PATH: process.env.PATH, HOME: home, USERPROFILE: home, CFFIXED_USER_HOME: home, PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: path.join(pilot, 'pilotdeck.yaml'), TMPDIR: tmp, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', NO_COLOR: '1', NODE_NO_WARNINGS: '1' };
  command ??= process.execPath; args ??= [path.join(baseline, 'dist/src/cli/pilotdeck.js'), 'server', '--host', '127.0.0.1', '--port', '0'];
  fs.writeFileSync(path.join(out, 'identity.json'), json({ baseline: BASELINE, node: process.version, platform: process.platform, command, args, env, cwd: project, fault, stopExpectation, driverHash: hash(fs.readFileSync(fileURLToPath(import.meta.url))), commandHash: hash(fs.readFileSync(command)), entryHash: fs.existsSync(args[0]) ? hash(fs.readFileSync(args[0])) : null }));
  const child = spawn(command, args, { env, cwd: project, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '', exit = null, failure = null;
  const signals = [];
  child.stdout.on('data', chunk => stdout += chunk); child.stderr.on('data', chunk => stderr += chunk);
  const ended = new Promise(resolve => child.once('close', (code, signal) => { exit = { code, signal }; resolve(); }));
  child.once('error', error => problems.push(error.message));
  const send = frame => clients.at(-1).send(JSON.stringify(frame));
  const calls = () => frames.filter(frame => frame.type === 'call_service');
  const ackBarrier = () => new Promise((resolve, reject) => {
    // A ping round trip separates the result frame from the next state event;
    // otherwise TCP may batch both, correctly triggering the baseline busy guard.
    const peer = clients.at(-1); const timer = setTimeout(() => reject(Error('HA acknowledgement barrier deadline')), 2000);
    peer.once('pong', () => { clearTimeout(timer); resolve(); }); peer.ping('ack-barrier');
  });
  const signal = name => { if (!exit) signals.push({ signal: name, sent: child.kill(name) }); };
  let reconnectDelayMs, stopEvidence, cronEvidence, control;
  try {
    await until(() => stdout.includes('PilotDeck server listening:') && frames.some(frame => frame.type === 'subscribe_events'), 'formal server ready');
    clients[0].send('malformed'); clients[0].send(Buffer.from(JSON.stringify(state('binary-must-ignore')))); send(state('wrong prefix', { entity: 'sensor.fixture' })); send(state('same', { old: 'same' })); send(state(' \ufeff ')); send(state('wrong event', { event: 'not_state_changed' }));
    send(state('hello 中文')); await until(() => calls().length === 1, 'first answer'); await ackBarrier();
    send(state('/new')); await until(() => calls().length === 2, 'new session'); await ackBarrier();
    send(state('hold-turn')); await until(() => heldModel, 'held model');
    send(state('/new')); send(state('must-skip')); await until(() => (stdout + stderr).includes('already active, skipping'), 'busy skip');
    reply(heldModel, 'released-answer'); await until(() => calls().length === 3, 'held answer'); await ackBarrier();
    const closedAt = Date.now(); clients[0].close(); await until(() => reconnectRejected ? paths.length >= 2 : clients.length === 2 && frames.filter(frame => frame.type === 'subscribe_events').length === 2, 'five second reconnect'); reconnectDelayMs = Date.now() - closedAt;
    assert.ok(reconnectDelayMs >= 4900, 'reconnect must wait five seconds');
    if (reconnectRejected) {
      await sleep(5500); assert.equal(paths.length, 2, 'handshake ErrorEvent does not synthesize a close/retry');
    } else {
    send(state('after-reconnect')); await until(() => calls().length === 4, 'reconnected answer'); await ackBarrier();
    if (cron) {
      const gatewayUrl = stdout.match(/WebSocket: (ws:\/\/[^\s]+)/)?.[1];
      control = new WebSocket(gatewayUrl);
      const received = []; control.on('message', data => { const frame = JSON.parse(data.toString()); received.push(frame); fs.appendFileSync(path.join(out, 'gateway.jsonl'), JSON.stringify({ direction: 'in', frame }) + '\n'); });
      await new Promise((resolve, reject) => { control.once('open', resolve); control.once('error', reject); });
      const sendControl = frame => { fs.appendFileSync(path.join(out, 'gateway.jsonl'), JSON.stringify({ direction: 'out', frame }) + '\n'); control.send(JSON.stringify(frame)); };
      sendControl({ type: 'hello', protocolVersion: '1.2', token: fs.readFileSync(path.join(pilot, 'server-token'), 'utf8').trim(), clientName: 'cli', clientVersion: '0.1.0' });
      await until(() => received.some(frame => frame.type === 'hello_ok'), 'control hello');
      assert.equal(received.find(frame => frame.type === 'hello_ok').serverInfo.projectKey, project);
      // The General workspace is the registry's explicit PILOT_HOME project.
      // A fresh process cwd is not automatically a registered dialog project.
      const projectKey = pilot;
      let rpcId = 0;
      const rpc = async (method, params) => {
        const id = `fixture-${++rpcId}`; sendControl({ type: 'request', id, method, params });
        await until(() => received.some(frame => frame.id === id && frame.type === 'response'), method);
        const response = received.find(frame => frame.id === id && frame.type === 'response'); assert.equal(response.ok, true, JSON.stringify(response)); return response.result;
      };
      const created = await rpc('cron_create', { message: 'h1-cron', schedule: { type: 'once', runAt: '2099-01-01T00:00:00.000Z' }, sessionKey: 'homeassistant:chat=conversation.fixture:general', channelKey: 'homeassistant', projectKey });
      const run = await rpc('cron_run_now', { taskId: created.task.taskId, projectKey });
      await until(() => calls().length === 5, 'cron notification'); await ackBarrier();
      let completed = false;
      for (let i = 0; i < 100 && !completed; i++) { const list = await rpc('cron_list', { includeHistory: true, projectKey }); completed = list.recentRuns?.some(row => row.taskId === run.taskId && row.outcome === 'completed'); if (!completed) await sleep(10); }
      cronEvidence = { started: run.started, completed: Boolean(completed) }; control.close();
    }
    if (interactions) {
      await sleep(30); send(state('h1-elicitation')); await until(() => calls().length === 5, 'elicitation prompt');
      assert.match(calls()[4].service_data.message, /Colour\?/);
      await ackBarrier(); send(state('2')); await until(() => calls().length === 6, 'elicitation resumed model'); await ackBarrier();
      await sleep(30); send(state('h1-permission')); await until(() => calls().length === 7, 'permission prompt');
      assert.match(calls()[6].service_data.message, /bash/);
      await ackBarrier(); send(state('0')); await until(() => calls().length === 9, 'permission denial and resumed model'); await ackBarrier();
      assert.equal(fs.existsSync(path.join(project, 'h1-permission.txt')), false, 'denied command never executed');
    }
    if (stopProbe) {
      await sleep(30); heldModel = null; send(state('hold-turn-stop')); await until(() => heldModel, 'held turn before disable');
      const provider = heldModel; let providerClosed = false; provider.once('close', () => providerClosed = true);
      fs.writeFileSync(path.join(pilot, 'pilotdeck.yaml'), config.replace('  homeassistant:\n    enabled: true', '  homeassistant:\n    enabled: false'));
      await until(() => clients[1].readyState === 3 && stdout.includes('Adapter hot-reload complete:'), 'adapter disabled while server remains alive');
      await sleep(500);
      const gatewayUrl = stdout.match(/PilotDeck server listening: (http:\/\/[^\s]+)/)?.[1];
      const health = await fetch(`${gatewayUrl}/health`, { signal: AbortSignal.timeout(2000) });
      stopEvidence = { expectation: stopExpectation, gatewayHealthy: health.ok, haClosed: clients[1].readyState === 3, providerStillOpen: !providerClosed && !provider.destroyed && !provider.writableEnded, notificationsBeforeRelease: calls().length };
      if (stopExpectation === 'baseline-retains') {
        reply(provider, 'reply-from-disabled-channel');
        await until(() => (stdout + stderr).includes('not connected, cannot send to conversation.fixture'), 'old turn tries to reply after adapter stop');
        stopEvidence.oldReplyRejected = true;
      } else stopEvidence.oldReplyRejected = false;
      stopEvidence.notificationsAfterRelease = calls().length;
      assert.equal(stopEvidence.notificationsAfterRelease, stopEvidence.notificationsBeforeRelease);
    }
    }
    signal('SIGINT'); await Promise.race([ended, sleep(10000).then(() => { throw Error('SIGINT shutdown deadline'); })]);
  } catch (error) { failure = error.stack ?? String(error); }
  finally {
    if (!exit) { signal('SIGTERM'); await Promise.race([ended, sleep(1500)]); if (!exit) { signal('SIGKILL'); await ended; } }
    control?.terminate(); for (const client of ws.clients) client.terminate(); for (const socket of sockets) socket.destroy(); await new Promise(resolve => mock.close(resolve)); ws.close();
    fs.writeFileSync(path.join(out, 'stdout.txt'), stdout); fs.writeFileSync(path.join(out, 'stderr.txt'), stderr);
  }
  const turns = models.filter(body => body.stream === true);
  const toolResults = turns.flatMap(body => body.messages.filter(message => message.role === 'tool').map(message => JSON.stringify(message)));
  const contract = { exit, auth: frames.filter(frame => frame.type === 'auth'), subscriptions: frames.filter(frame => frame.type === 'subscribe_events'), paths, connections: clients.length, busySkipped: (stdout + stderr).includes('already active, skipping'), notifications: calls().map(frame => ({ ...frame, service_data: { ...frame.service_data, notification_id: /^gw_\d+$/.test(frame.service_data.notification_id) ? 'gw_<time>' : frame.service_data.notification_id } })), modelInputs: turns.map(lastUser), modelHistory: turns.slice(0, 3).map(body => body.messages.filter(message => message.role !== 'system')), ...(stopProbe ? { stopProbe: stopEvidence } : {}), ...(cron ? { cron: cronEvidence } : {}), ...(reconnectRejected ? { reconnectFailure: { noFurtherHandshake: paths.length === 2, errors: stderr.split('\n').filter(line => line.startsWith('homeassistant: ws error:')) } } : {}), ...(interactions ? { interactions: { answeredBlue: toolResults.some(result => result.includes('Colour?') && result.includes('Blue')), deniedTool: toolResults.some(result => /denied|rejected|拒绝/i.test(result)) } } : {}), problems };
  try { verify(contract); } catch (error) { failure ??= error.stack; }
  fs.writeFileSync(path.join(out, 'contract.json'), json(contract)); fs.writeFileSync(path.join(out, 'result.json'), json({ pid: child.pid, exit, signals, reconnectDelayMs, failure }));
  if (failure) throw Error(failure); return contract;
}
export function compare(left, right) { assert.deepEqual(left, right, 'formal HA contract differs'); }
async function main() {
  const [baseline, out, compareDir, mode] = process.argv.slice(2); assert.ok(baseline && out);
  const contract = await capture({ baseline: path.resolve(baseline), out: path.resolve(out), stopProbe: mode === '--stop-probe', interactions: mode === '--interactions', cron: mode === '--cron', reconnectRejected: mode === '--reconnect-rejected', fault: process.env.HA_FIXTURE_FAULT ?? null });
  if (compareDir) {
    compare(JSON.parse(fs.readFileSync(path.join(compareDir, 'contract.json'))), contract);
    const mutations = [row => row.auth[0].access_token = 'wrong-token', row => row.notifications[0].service = 'wrong-service', row => row.modelInputs.push('busy-leaked'), row => row.subscriptions[0].event_type = 'wrong-event', row => row.modelHistory[2] = [{ role: 'user', content: 'after-reconnect' }]];
    for (const mutate of mutations) { const wrong = structuredClone(contract); mutate(wrong); assert.throws(() => compare(contract, wrong)); }
    fs.writeFileSync(path.join(out, 'negative-control.json'), json({ mutations: ['auth-token', 'service', 'busy', 'subscription', 'session-history'], caught: 5 }));
  }
  console.log(json({ passed: true, out }));
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => { console.error(error.stack); process.exitCode = 1; });
