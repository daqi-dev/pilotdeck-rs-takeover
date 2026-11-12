#!/usr/bin/env node
// Formal server + public WebSocket submit_turn. No product module injection.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import {spawn,spawnSync} from 'node:child_process';
import {sourceIdentity,hash} from './source-identity.mjs';
const options={};for(let i=2;i<process.argv.length;i+=2)options[process.argv[i].replace(/^--/,'')]=process.argv[i+1];assert.equal(process.version,'v22.22.0');assert.ok(options.baseline&&options.out);
let candidate=null;if(options['rust-bin']){assert.ok(options['rust-root']);assert.match(options['rust-sha']??'',/^[0-9a-f]{40}$/);const root=fs.realpathSync(options['rust-root']);assert.equal(spawnSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'}).stdout.trim(),options['rust-sha']);candidate={root,sha:options['rust-sha']};}
const out=path.resolve(options.out);fs.mkdirSync(out);const json=v=>JSON.stringify(v,null,2)+'\n',save=(name,v)=>fs.writeFileSync(path.join(out,name),json(v));
const root=path.join(out,'fixture'),home=path.join(root,'home'),pilot=path.join(home,'.pilotdeck'),project=path.join(root,'project');for(const p of [pilot,project,path.join(root,'tmp'),path.join(root,'skills'),path.join(root,'plugins')])fs.mkdirSync(p,{recursive:true});fs.writeFileSync(path.join(pilot,'server-token'),'o1-fake-server-token\n',{mode:0o600});fs.writeFileSync(path.join(pilot,'.legacy-bundled-skills-migrated-v1'),'completed\n');save('source-identity.json',await sourceIdentity(options.baseline));
const wait=ms=>new Promise(r=>setTimeout(r,ms));async function until(fn,label,ms=20000){const end=Date.now()+ms;while(!fn()){assert.ok(Date.now()<end,'deadline: '+label);await wait(5);}}
const requests=[],telemetry=[],models=[],connections=[],sockets=new Set(),frames=[],sentFrames=[];let status=null,ws=null,failure=null;
const record=(kind,details)=>fs.appendFileSync(path.join(out,'network.jsonl'),JSON.stringify({kind,at:new Date().toISOString(),...details})+'\n');
const mock=http.createServer((req,res)=>{const chunks=[];req.on('data',b=>chunks.push(b));req.on('end',()=>{const raw=Buffer.concat(chunks).toString();const row={method:req.method,url:req.url,headers:req.headers,raw};requests.push(row);record('http',row);const body=JSON.parse(raw);if(req.url==='/collect'){telemetry.push(body);res.writeHead(200);res.end('accepted');return;}assert.equal(req.url,'/v1/chat/completions');models.push(body);if(!body.stream){res.setHeader('content-type','application/json');res.end(JSON.stringify({choices:[{message:{role:'assistant',content:'fixture-title'},finish_reason:'stop'}]}));return;}res.writeHead(200,{'content-type':'text/event-stream'});for(const delta of [{role:'assistant'},{content:options.mutate==='model-output'?'mutated-output':'o1-answer'}])res.write('data: '+JSON.stringify({id:'o1-model',object:'chat.completion.chunk',model:'deterministic',choices:[{index:0,delta,finish_reason:null}]})+'\n\n');res.write('data: '+JSON.stringify({id:'o1-model',object:'chat.completion.chunk',model:'deterministic',choices:[{index:0,delta:{},finish_reason:'stop'}],usage:{prompt_tokens:10,completion_tokens:3,total_tokens:13}})+'\n\n');res.end('data: [DONE]\n\n');});});
mock.on('connection',s=>{sockets.add(s);const row={index:connections.length,closed:false};connections.push(row);s.on('close',()=>{sockets.delete(s);row.closed=true;record('socket-closed',row);});});await new Promise(r=>mock.listen(0,'127.0.0.1',r));const port=mock.address().port;assert.notEqual(port,18789);
fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),`schemaVersion: 1
agent:
  model: o1/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    o1:
      protocol: openai
      url: http://127.0.0.1:${port}/v1
      apiKey: o1-fake-local-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: ${options.mutate==='disabled-telemetry'?'false':'true'}
`);
const env={PATH:process.env.PATH,HOME:home,USERPROFILE:home,CFFIXED_USER_HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),TMPDIR:path.join(root,'tmp'),LANG:'C',LC_ALL:'C',TZ:'UTC',TERM:'dumb',NO_COLOR:'1',NODE_NO_WARNINGS:'1',DOCKER_CONTAINER:'1',COMMIT_HASH:'o1-fixed-commit',PILOTDECK_VERSION:'o1-fixed-version',PILOTDECK_BUNDLED_SKILLS_DIR:path.join(root,'skills'),PILOTDECK_BUNDLED_PLUGINS_DIR:path.join(root,'plugins'),ANALYTICS_ENABLED:'false',ANALYTICS_BASE_URL:`http://127.0.0.1:${port}`,ANALYTICS_FLUSH_INTERVAL_MS:'600000',ANALYTICS_BATCH_SIZE:'100',ANALYTICS_TIMEOUT_MS:'1000',ANALYTICS_MAX_RETRIES:'2'};
const command=options['rust-bin']?fs.realpathSync(options['rust-bin']):process.execPath,args=options['rust-bin']?['server','--port','0','--host','127.0.0.1']:[path.join(options.baseline,'dist/src/cli/pilotdeck.js'),'server','--port','0','--host','127.0.0.1'];save('identity.json',{candidate,command,args,commandHash:hash(fs.readFileSync(command)),entryHash:fs.existsSync(args[0])?hash(fs.readFileSync(args[0])):null,driverHash:hash(fs.readFileSync(new URL(import.meta.url))),node:process.version,env,cwd:project});
const child=spawn(command,args,{cwd:project,env,stdio:['ignore','pipe','pipe']}),stdout=[],stderr=[],signals=[];child.stdout.on('data',b=>stdout.push(b));child.stderr.on('data',b=>stderr.push(b));const exit=new Promise(r=>child.on('close',(code,signal)=>{status={code,signal};r();}));
function signal(name,reason){if(status)return;signals.push({pid:child.pid,signal:name,reason,sent:child.kill(name)});}
let contract=null;
try{
  await until(()=>Buffer.concat(stdout).includes('PilotDeck server listening:'),'server ready');const url=Buffer.concat(stdout).toString().match(/PilotDeck server listening: (http:\/\/[^\s]+)/)[1];assert.notEqual(new URL(url).port,'18789');ws=new WebSocket(url.replace(/^http/,'ws')+'/ws');ws.addEventListener('message',e=>{const frame=JSON.parse(String(e.data));frames.push(frame);record('ws-receive',{frame});});await new Promise((r,j)=>{ws.addEventListener('open',r,{once:true});ws.addEventListener('error',j,{once:true});});const send=frame=>{sentFrames.push(frame);record('ws-send',{frame});ws.send(JSON.stringify(frame));};send({type:'hello',protocolVersion:'1.2',clientName:'o1-fixture',clientVersion:'0.1.0',token:'o1-fake-server-token'});await until(()=>frames.some(f=>f.type==='hello_ok'),'hello');send({type:'request',id:'o1-formal-turn',method:'submit_turn',params:{sessionKey:'o1:formal',channelKey:'web',message:'o1 formal input'}});await until(()=>frames.some(f=>f.type==='event'&&f.id==='o1-formal-turn'&&f.final),'turn complete');assert.ok(frames.some(f=>f.event?.type==='assistant_text_delta'&&f.event.text==='o1-answer'),'real model text reaches public event');assert.equal(telemetry.length,0,'batch remains queued before shutdown');ws.close();await until(()=>ws.readyState===WebSocket.CLOSED,'owned WS closed');signal('SIGINT','user-stop');await Promise.race([exit,wait(10000).then(()=>{throw Error('shutdown deadline');})]);assert.deepEqual(status,{code:0,signal:null});assert.equal(signals.length,1);assert.equal(signals[0].sent,true);assert.equal(telemetry.length,1);const events=telemetry.flat();assert.deepEqual(events.map(e=>e.properties.loopStage),['loop_start','model_request','model_response','loop_end']);assert.equal(new Set(events.map(e=>e.eventId)).size,4);
  const instance=hash(`${pilot}|${command}|${options['rust-bin']?command:args[0]}|docker`).slice(0,24);const runIds=new Set();for(const event of events){assert.match(event.eventId,/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);assert.match(event.occurredAt,/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);assert.equal(event.instanceId,instance);assert.equal(event.installationId,hash('o1-fake-server-token').slice(0,24));assert.equal(event.sessionId,hash('o1:formal').slice(0,24));assert.equal(event.commitHash,'o1-fixed-commit');assert.equal(event.appVersion,'o1-fixed-version');assert.equal(event.deploymentMode,'docker');assert.equal(event.eventName,'feature_used');assert.match(event.properties.runId,/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);runIds.add(event.properties.runId);}assert.equal(runIds.size,1);
  const match=Buffer.concat(stdout).toString().match(/\[telemetry\] shutdown snapshot (\{[^\n]+\})/);assert.ok(match);const snapshot=JSON.parse(match[1]);assert.deepEqual(snapshot,{queued:4,sent:0,sendFailures:0,retries:0,dropped:0,queueDepth:4});assert.equal(fs.existsSync(path.join(pilot,'telemetry/queue.jsonl')),false);contract={events:events.map(e=>({...e,eventId:'<uuid-v4>',occurredAt:'<iso-time>',instanceId:'<verified-instance-id>',properties:{...e.properties,runId:'<run-id>',...(e.properties.providerBaseUrl?{providerBaseUrl:e.properties.providerBaseUrl.replace(`:${port}`,':<port>')}:{})}})),snapshot,status,signal:signals.map(({signal,reason,sent})=>({signal,reason,sent})),modelMainCount:models.filter(m=>m.stream).length,queueFileExists:false};save('contract.json',contract);if(options.compare)assert.deepEqual(contract,JSON.parse(fs.readFileSync(path.join(options.compare,'contract.json'))));
}catch(error){failure=error.stack;}finally{
  if(ws&&ws.readyState!==WebSocket.CLOSED)ws.close();if(!status){signal('SIGTERM','failure-cleanup');await Promise.race([exit,wait(1000)]);if(!status){signal('SIGKILL','failure-cleanup');await exit;}}
  save('result.json',{pid:child.pid,status,signals,failure,connectionsBeforeCollectorCleanup:connections});fs.writeFileSync(path.join(out,'stdout.bin'),Buffer.concat(stdout));fs.writeFileSync(path.join(out,'stderr.bin'),Buffer.concat(stderr));save('requests.json',requests);save('frames.json',{sent:sentFrames,received:frames});const queue=path.join(pilot,'telemetry/queue.jsonl');save('queue-after.json',{exists:fs.existsSync(queue),raw:fs.existsSync(queue)?fs.readFileSync(queue,'utf8'):null});for(const s of sockets)s.destroy();await new Promise(r=>mock.close(r));
}
if(failure)throw Error(failure);console.log(json({passed:true,out}));
