#!/usr/bin/env node
// Formal `pilotdeck server` probe: no product imports, loader replacements or globals.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import { createRequire } from 'node:module';
import { createHash, randomUUID } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';

const CASES=['root-url','api-prefix','encoded-token','rejected','missing','lifecycle','cron-unsupported','lifecycle-active','null-envelope','null-post','redirect-get','redirect-post','false-url','zero-url','nonascii-token','latin1-token','bom-me','me-null'];
const BASELINE = 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const digest = data => createHash('sha256').update(data).digest('hex');
const write = (file, value) => fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n');
export function verify(record, name) {
  assert.equal(record.ready, true, 'formal Gateway health became ready');
  if(name.startsWith('null-')&&record.side!=='rust') {assert.equal(record.exitCode,1);assert.equal(record.signal,null);assert.equal(record.forceKilled,false);assert.equal(record.scriptError,undefined);assert.equal(record.rest.length,1);assert.equal(record.upgrades.length,1);assert.equal(record.stderr,'');assert.equal(record.exitedBeforeCleanup,true);return;}
  assert.equal(record.exitCode, 0, 'owned server stopped naturally after SIGTERM');
  assert.equal(record.signal, null, 'no force-kill');
  assert.equal(record.forceKilled, false);
  assert.equal(record.scriptError, undefined, 'script completed');
  if(name.startsWith('null-')) {assert.equal(record.aliveAfterNull,true);assert.equal(record.rest.length,2);assert.deepEqual(JSON.parse(record.rest[1].body),{channel_id:'fixture-channel',message:'已创建新会话。'});return;}
  if (!['cron-unsupported','lifecycle-active'].includes(name)) assert.equal(record.providerRequests.length, name === 'root-url' ? 2 : 0, 'ordinary message requests a title and a streamed answer');
  if (name === 'missing'||name==='false-url'||name==='zero-url') {
    assert.equal(record.rest.length, 0); assert.equal(record.upgrades.length, 0);
    assert.match(record.stderr, /mattermost: serverUrl and token are required/);
  } else if(name==='me-null') {assert.equal(record.rest.length,1);assert.equal(record.upgrades.length,0);assert.match(record.stderr,/token check failed: TypeError: Cannot read properties of null \(reading 'id'\)/);
  } else if(name==='nonascii-token') {assert.equal(record.rest.length,0);assert.equal(record.upgrades.length,0);assert.match(record.stderr,/ByteString/);
  } else if (name === 'rejected') {
    assert.equal(record.rest.length, 1); assert.equal(record.upgrades.length, 0);
    assert.match(record.stderr, /mattermost: token check failed: Error: HTTP 401: fixture denied/);
  } else if(name==='redirect-get'||name==='redirect-post') {assert.equal(record.stderr,'');assert.equal(record.upgrades.length,1);assert.equal(record.rest.length,name==='redirect-get'?2:3);assert.equal(record.rest.at(-1).path,name==='redirect-get'?'/users/me-final':'/posts-final');for(const request of record.rest)assert.equal(request.authorization,`Bearer ${record.token}`);if(name==='redirect-post'){assert.equal(record.rest[1].method,'POST');assert.equal(record.rest[2].method,'POST');assert.equal(record.rest[1].body,record.rest[2].body);}} else {
    assert.equal(record.rest.length, name === 'root-url' ? 3 : (['lifecycle','cron-unsupported','bom-me'].includes(name)||(name==='lifecycle-active'&&record.side!=='rust')) ? 2 : 1); assert.equal(record.upgrades.length, name === 'lifecycle' ? 3 : 1);
    if(name === 'cron-unsupported') {assert.match(record.stderr,/\[cron\] result delivery was not handled task=/);assert.ok(record.providerRequests.some(request=>JSON.parse(request.body).messages.some(message=>message.role==='user'&&String(message.content).includes('fixture-cron'))));assert.ok(!record.rpc.some(frame=>frame.value.type==='notification'&&frame.value.payload?.event?.type==='error'));assert.ok(record.rpc.some(frame=>frame.direction==='send'&&frame.value.method==='cron_run_now'));}
    else assert.equal(record.stderr, '');
    if(name === 'lifecycle-active') {assert.equal(record.aliveAfterDisable,true);assert.equal(record.providerOpenAfterDisable,record.side!=='rust');assert.ok(record.disabledObservationMs>=4400);assert.equal(record.postsAfterDisable,0);assert.equal(record.latePostsAfterRelease,record.side==='rust'?0:1);assert.equal(record.upgrades.length,1);}
    if(name === 'lifecycle') {assert.ok(record.reconnectElapsedMs >= 3900);assert.ok(record.disabledObservationMs >= 4400);assert.equal(record.disabledUpgrades,2);assert.equal(record.disabledClients,0);}
    assert.match(record.stdout, /mattermost: connected/);
    const expectedWs = new URL(`ws://fixture${name === 'api-prefix' ? '/api/v4' : ''}/api/v4/websocket?token=${encodeURIComponent(record.token)}`);
    assert.equal(record.upgrades[0].path, expectedWs.pathname + expectedWs.search);
    assert.equal(record.upgrades[0].origin, `http://127.0.0.1:${record.mockPort}`);
    if (name === 'root-url') {
      assert.deepEqual(JSON.parse(record.rest[1].body), {channel_id:'fixture-channel',message:'已创建新会话。'});
      assert.deepEqual(JSON.parse(record.rest[2].body), {channel_id:'fixture-channel',message:'fixture answer'});
      const requests = record.providerRequests.map(request => JSON.parse(request.body));
      assert.deepEqual(requests.map(request => request.stream).sort(), [false, true]);
      for (const request of requests) assert.ok(request.messages.some(message => message.role === 'user' && message.content === 'fixture-message'));
    }
  }
  if (record.rest.length) {
    assert.equal(record.rest[0].method, 'GET');
    assert.equal(record.rest[0].authorization, `Bearer ${record.token}`);
    assert.equal(record.rest[0].path, name === 'api-prefix' ? '/api/v4/users/me' : '/users/me');
  }
}
async function freePort() {
  const listener = net.createServer(); await new Promise(resolve => listener.listen(0, '127.0.0.1', resolve));
  const port = listener.address().port; await new Promise(resolve => listener.close(resolve)); return port;
}
export async function capture(baseline, out, selected=CASES, rustBinary) {
  assert.equal(process.version, 'v22.22.0');
  const sha = spawnSync('git', ['-C', baseline, 'rev-parse', 'HEAD'], { encoding: 'utf8' });
  assert.equal(sha.status, 0); assert.equal(sha.stdout.trim(), BASELINE);
  assert.ok(selected.length && new Set(selected).size===selected.length && selected.every(name=>CASES.includes(name)));
  fs.mkdirSync(out);
  const { WebSocketServer, WebSocket } = createRequire(path.join(baseline, 'package.json'))('ws');
  const entry = path.join(baseline, 'dist/src/cli/pilotdeck.js');
  write(path.join(out, 'identity.json'), {
    side:rustBinary?'rust':'ts',rustBinary:rustBinary??null,rustBinarySha256:rustBinary?digest(fs.readFileSync(rustBinary)):null,
    sourceCommit: BASELINE, entry, entrySha256: digest(fs.readFileSync(entry)), node: process.version,
    driverSha256: digest(fs.readFileSync(fileURLToPath(import.meta.url))),
    sourceSha256: digest(fs.readFileSync(path.join(baseline, 'src/adapters/channel/mattermost/MattermostChannel.ts'))),
    note: '真实server入口与随机端口mock；未修改产品源码/模块加载/全局WebSocket。',
  });
  const rows = [];
  for (const name of selected) {
    const dir = path.join(out, name), home = path.join(dir, 'home'), pilot = path.join(home, '.pilotdeck'), project = path.join(dir, 'project');
    fs.mkdirSync(pilot, { recursive: true }); fs.mkdirSync(project);
    if (name==='cron-unsupported') {const registration=path.join(pilot,'projects','fixture-project');fs.mkdirSync(registration,{recursive:true});fs.writeFileSync(path.join(registration,'.cwd'),project+'\n');}
    const token = name === 'encoded-token' ? "a b+&?!'()*" : name==='nonascii-token'?'中':name==='latin1-token'?'é':'fixture-mattermost-token';
    const record = { name, side:rustBinary?'rust':'ts', token, rest: [], upgrades: [], rpc: [], closes: [], providerRequests: [], stdout: '', stderr: '', ready: false, forceKilled: false, pid: null };
    const sockets = new Set();let heldResponse=null;
    const ws = new WebSocketServer({ noServer: true });
    ws.on('connection',client=>client.on('close',(code,reason)=>record.closes.push({at:Date.now(),code,reason:reason.toString()})));
    const mock = http.createServer(async (request, response) => {
      const chunks = []; for await (const chunk of request) chunks.push(chunk);
      if (request.url.startsWith('/v1/')) {
        record.providerRequests.push({ path: request.url, body: Buffer.concat(chunks).toString() });
        if (!JSON.parse(Buffer.concat(chunks).toString()).stream) {
          response.writeHead(200, {'content-type':'application/json'});
          response.end(JSON.stringify({id:'fixture-title',object:'chat.completion',choices:[{index:0,message:{role:'assistant',content:'{"title":"Fixture title"}'},finish_reason:'stop'}],usage:{prompt_tokens:1,completion_tokens:2,total_tokens:3}})); return;
        }
        response.writeHead(200, {'content-type':'text/event-stream'});
        response.write('data: ' + JSON.stringify({id:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta:{role:'assistant',content:'fixture answer'},finish_reason:null}]}) + '\n\n');
        if (JSON.parse(Buffer.concat(chunks).toString()).messages.some(message=>message.role==='user'&&message.content==='fixture-hang')) {heldResponse=response;response.on('close',()=>record.heldProviderClosedAt=Date.now());return;}
        response.write('data: ' + JSON.stringify({id:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta:{},finish_reason:'stop'}],usage:{prompt_tokens:1,completion_tokens:2,total_tokens:3}}) + '\n\n');
        response.end('data: [DONE]\n\n'); return;
      }
      record.rest.push({ method: request.method, path: request.url, authorization: request.headers.authorization, body: Buffer.concat(chunks).toString() });
      if(name==='redirect-get'&&request.url==='/users/me'){response.writeHead(302,{location:'/users/me-final'});response.end();return;}
      if(name==='redirect-post'&&request.url==='/posts'){response.writeHead(307,{location:'/posts-final'});response.end();return;}
      response.writeHead(name === 'rejected' ? 401 : 200, { 'content-type': 'application/json' });
      response.end(name === 'me-null'?'null':name === 'rejected' ? 'fixture denied' : (name==='bom-me'&&request.url==='/users/me'?'\uFEFF':'')+'{"id":"fixture-bot"}');
    });
    mock.on('connection', socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); });
    mock.on('upgrade', (request, socket, head) => {
      record.upgrades.push({ path: request.url, origin: request.headers.origin ?? null });
      ws.handleUpgrade(request, socket, head, client => ws.emit('connection', client, request));
    });
    await new Promise(resolve => mock.listen(0, '127.0.0.1', resolve));
    const mockPort = mock.address().port, gatewayPort = await freePort();
    record.mockPort = mockPort; record.gatewayPort = gatewayPort;
    const url = `http://127.0.0.1:${mockPort}${name === 'api-prefix' ? '/api/v4' : ''}`;
    const config = `schemaVersion: 1\nagent:\n  model: fixture/deterministic\n  maxContextTokens: 65536\n  maxOutputTokens: 8192\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:${mockPort}/v1\n      apiKey: fixture-only\n      models:\n        deterministic:\n          capabilities:\n            supportsToolUse: true\n            maxContextTokens: 65536\n            maxOutputTokens: 8192\ntelemetry:\n  enabled: false\nadapters:\n  mattermost:\n    enabled: true\n    token: ${name === 'missing' ? '\"\"' : 'fixture-mattermost-token'}\n    extra:\n      serverUrl: ${name==='false-url'?'false':name==='zero-url'?'0':url}\n      teamId: fixture-team\n`;
    fs.writeFileSync(path.join(pilot, 'pilotdeck.yaml'), config.replace('token: fixture-mattermost-token', `token: ${JSON.stringify(token)}`) + (name==='cron-unsupported'?'cron:\n  enabled: true\n':''));
    const env = { PATH: process.env.PATH, HOME: home, USERPROFILE: home, PILOT_HOME: pilot, PILOTDECK_CONFIG_PATH: path.join(pilot, 'pilotdeck.yaml'), TMPDIR: dir, LANG: 'C', LC_ALL: 'C', TZ: 'UTC', NODE_NO_WARNINGS: '1', NO_COLOR: '1' };
    const command=rustBinary??process.execPath;
    const args = [...(rustBinary?[]:[entry]), 'server', '--host', '127.0.0.1', '--port', String(gatewayPort)];
    write(path.join(dir, 'command.json'), { command, args, env, cwd: project });
    const child = spawn(command, args, { env, cwd: project, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    record.pid = child.pid;
    child.stdout.on('data', chunk => record.stdout += chunk.toString()); child.stderr.on('data', chunk => record.stderr += chunk.toString());
    const close = new Promise(resolve => child.once('close', (exitCode, signal) => { Object.assign(record, { exitCode, signal }); resolve(); }));
    child.once('error', error => record.spawnError = error.message);
    try {
      const deadline = Date.now() + 15000;
      while (Date.now() < deadline && child.exitCode === null && child.signalCode === null) {
        try { const response = await fetch(`http://127.0.0.1:${gatewayPort}/health`, { signal: AbortSignal.timeout(200) }); if (response.ok) { record.ready = true; break; } } catch {}
        await sleep(50);
      }
      const waitUntil = async predicate => { const end = Date.now() + 10000; while (!predicate() && Date.now() < end) await sleep(20); assert.ok(predicate(), 'scripted inbound message reached its observable result'); };
      if(name.startsWith('null-')) {await waitUntil(()=>ws.clients.size===1);[...ws.clients][0].send(name==='null-envelope'?'null':JSON.stringify({event:'posted',data:{post:'null'}}));if(rustBinary){await sleep(300);record.aliveAfterNull=child.exitCode===null;[...ws.clients][0].send(JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id:'fixture-user',channel_id:'fixture-channel',message:'/new'})}}));await waitUntil(()=>record.rest.length===2);}else{await waitUntil(()=>child.exitCode!==null);record.exitedBeforeCleanup=true;}}
      if(name==='bom-me') {await waitUntil(()=>ws.clients.size===1);for(const user_id of ['fixture-bot','fixture-user'])[...ws.clients][0].send(JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id,channel_id:'fixture-channel',message:'/new'})}}));await waitUntil(()=>record.rest.length===2);}
      if(name==='redirect-post') {await waitUntil(()=>ws.clients.size===1);[...ws.clients][0].send(JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id:'fixture-user',channel_id:'fixture-channel',message:'/new'})}}));await waitUntil(()=>record.rest.length===3);}
      if (name === 'root-url') {
        await waitUntil(() => ws.clients.size === 1);
        const client = [...ws.clients][0];
        const post = message => client.send(JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id:'fixture-user',channel_id:'fixture-channel',message})}}));
        post('/new'); await waitUntil(() => record.rest.length >= 2);
        post('fixture-message'); await waitUntil(() => record.rest.length >= 3);
      }
      if (['lifecycle','cron-unsupported','lifecycle-active'].includes(name)) {
        await waitUntil(()=>ws.clients.size===1);
        const gateway = new WebSocket(`ws://127.0.0.1:${gatewayPort}/ws`);
        const pending = new Map();
        gateway.on('message',raw=> {const value=JSON.parse(raw.toString());record.rpc.push({direction:'receive',value});if(value.type==='hello_ok')pending.get('hello')?.resolve(value);else if(value.type==='response'){const waiter=pending.get(value.id);value.error?waiter?.reject(new Error(JSON.stringify(value.error))):waiter?.resolve(value.result);}});
        const request=(method,params)=> {const id=randomUUID();return new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(new Error(`RPC timeout ${method}`)),10000);pending.set(id,{resolve:value=>{clearTimeout(timer);resolve(value)},reject:error=>{clearTimeout(timer);reject(error)}});const value={type:'request',id,method,params};record.rpc.push({direction:'send',value});gateway.send(JSON.stringify(value));});};
        try {
          await new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(new Error('Gateway WS open observation timeout')),10000);gateway.once('open',()=>{clearTimeout(timer);resolve()});gateway.once('error',error=>{clearTimeout(timer);reject(error)})});
          const hello=new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(new Error('Gateway hello observation timeout')),10000);pending.set('hello',{resolve:value=>{clearTimeout(timer);resolve(value)}})});
          gateway.send(JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'h1-fixture',clientVersion:'0.1.0',token:fs.readFileSync(path.join(pilot,'server-token'),'utf8').trim()}));await hello;
          if(name==='lifecycle-active') {
            [...ws.clients][0].send(JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id:'fixture-user',channel_id:'fixture-channel',message:'fixture-hang'})}}));
            await waitUntil(()=>heldResponse!==null);
            const configPath=path.join(pilot,'pilotdeck.yaml');const original=fs.readFileSync(configPath,'utf8');fs.writeFileSync(configPath,original.replace('mattermost:\n    enabled: true','mattermost:\n    enabled: false'));
            await request('reload_config',{});await waitUntil(()=>record.stdout.includes('mattermost=stopped'));
            const observed=Date.now();await sleep(4500);record.disabledObservationMs=Date.now()-observed;record.aliveAfterDisable=child.exitCode===null;record.providerOpenAfterDisable=!heldResponse.destroyed;record.postsAfterDisable=record.rest.filter(r=>r.method==='POST').length;
            if(!rustBinary) heldResponse.end('data: '+JSON.stringify({id:'fixture',object:'chat.completion.chunk',choices:[{index:0,delta:{},finish_reason:'stop'}]})+'\n\ndata: [DONE]\n\n');
            if(!rustBinary) await waitUntil(()=>record.rest.some(r=>r.method==='POST'));record.latePostsAfterRelease=record.rest.filter(r=>r.method==='POST').length;
          } else if(name==='lifecycle') {
            const began=Date.now();[...ws.clients][0].close();await waitUntil(()=>record.upgrades.length===2);record.reconnectElapsedMs=Date.now()-began;
            const configPath=path.join(pilot,'pilotdeck.yaml');const original=fs.readFileSync(configPath,'utf8');fs.writeFileSync(configPath,original.replace('mattermost:\n    enabled: true','mattermost:\n    enabled: false'));
            await request('reload_config',{});await waitUntil(()=>record.stdout.includes('mattermost=stopped'));
            const observed=Date.now();await sleep(4500);record.disabledObservationMs=Date.now()-observed;record.disabledUpgrades=record.upgrades.length;record.disabledClients=ws.clients.size;
            fs.writeFileSync(configPath,original);await request('reload_config',{});await waitUntil(()=>record.upgrades.length===3);
          } else {
            [...ws.clients][0].send(JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id:'fixture-user',channel_id:'fixture-channel',message:'fixture-message'})}}));await waitUntil(()=>record.rest.some(r=>r.method==='POST'));
            const created=await request('cron_create',{message:'fixture-cron',schedule:{type:'delay',amount:1,unit:'day'},sessionKey:'mattermost:chat=fixture-channel:general',channelKey:'mattermost',projectKey:project});
            record.created=created;
            record.runNow=await request('cron_run_now',{taskId:created.task.taskId,projectKey:project});
            await waitUntil(()=>record.stderr.includes('[cron] result delivery was not handled'));
          }
        } finally {gateway.close();}
      }
      await sleep(250);
    } catch(error) {record.scriptError=error.stack;} finally {
      if (child.exitCode === null && child.signalCode === null) process.kill(-child.pid, 'SIGTERM');
      const force = setTimeout(() => { if (child.exitCode === null && child.signalCode === null) { record.forceKilled = true; process.kill(-child.pid, 'SIGKILL'); } }, 3000);
      await close; clearTimeout(force);
      for (const client of ws.clients) client.terminate();
      for (const socket of sockets) socket.destroy();
      await new Promise(resolve => mock.close(resolve));
      ws.close();
    }
    fs.writeFileSync(path.join(dir, 'stdout.txt'), record.stdout); fs.writeFileSync(path.join(dir, 'stderr.txt'), record.stderr);
    try { verify(record, name); record.valid = true; } catch (error) { record.valid = false; record.failure = error.message; }
    write(path.join(dir, 'result.json'), record);
    rows.push(record); console.log(`${name}: ${record.valid ? 'CAPTURED' : `FAIL ${record.failure}`}`);
  }
  write(path.join(out, 'results.json'), rows);
  return rows;
}
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [baseline, out, names, rustBinary] = process.argv.slice(2);
  assert.ok(baseline && out, 'server-probe.mjs BASELINE NEW_OUTPUT_DIR');
  const rows = await capture(path.resolve(baseline), path.resolve(out),names?.split(','),rustBinary?path.resolve(rustBinary):undefined);
  process.exitCode = rows.every(row => row.valid) ? 0 : 1;
}
