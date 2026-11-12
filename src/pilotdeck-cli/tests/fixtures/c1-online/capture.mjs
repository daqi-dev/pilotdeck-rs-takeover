#!/usr/bin/env node
// Capture formal default CLI subprocesses; never imports or patches product code.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { createHash } from 'node:crypto';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { requireIsolation, startMock, TOKEN } from './mock.mjs';
const here=path.dirname(fileURLToPath(import.meta.url));
const BASELINE='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const sha=b=>createHash('sha256').update(b).digest('hex');
const json=x=>JSON.stringify(x,null,2)+'\n';
export function ownedIdentity(pid) {
  try {
    const stat=fs.readFileSync(`/proc/${pid}/stat`,'utf8'), fields=stat.slice(stat.lastIndexOf(')')+2).split(' ');
    return {pid,startTicks:fields[19],state:fields[0],command:fs.readFileSync(`/proc/${pid}/cmdline`).toString().split('\0').filter(Boolean)};
  } catch { return {pid,unavailable:true}; }
}
function descendants(pid,found=[]) {
  try { for(const child of fs.readFileSync(`/proc/${pid}/task/${pid}/children`,'utf8').trim().split(/\s+/).filter(Boolean).map(Number)){found.push(ownedIdentity(child));descendants(child,found);} }catch{}
  return found;
}
function stillAlive(identity) {
  const now=ownedIdentity(identity.pid);
  return !now.unavailable&&now.startTicks===identity.startTicks&&now.state!=='Z';
}
export function snapshot(root) {
  const rows=[];
  const visit=dir=>{for(const entry of fs.readdirSync(dir,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))){const target=path.join(dir,entry.name),relative=path.relative(root,target);if(entry.isSymbolicLink())rows.push({path:relative,symlink:fs.readlinkSync(target)});else if(entry.isDirectory())visit(target);else if(entry.isFile()){const bytes=fs.readFileSync(target);rows.push({path:relative,size:bytes.length,sha256:sha(bytes),base64:bytes.toString('base64')});}}};
  visit(root);return rows;
}
export function config(port) {return `schemaVersion: 1
agent:
  model: c1/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    c1:
      protocol: openai
      url: http://127.0.0.1:${port}/v1
      apiKey: c1-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
`;}
export async function drive(command,args,env,cwd,spec,mock,{startupMs=20000,observeMs=1500,record=()=>{}}={}) {
  const started=performance.now(),at=()=>performance.now()-started;
  const child=spawn(command,args,{cwd,env,detached:true,stdio:['pipe','pipe','pipe']});
  const stdout=[],stderr=[],signals=[],observedDescendants=new Map();
  let closed=false,problem=null,boundary=null,logicalAt=null,cleanupTimer=null,total=0,sentUserSignal=false;
  const sampleChildren=()=>{for(const ident of descendants(child.pid))observedDescendants.set(`${ident.pid}:${ident.startTicks}`,ident);return [...observedDescendants.values()];};
  const emit=(kind,details={})=>record({kind,atMs:at(),...details});
  const identity=ownedIdentity(child.pid);emit('process-start',{identity,command,args,cwd});
  const sendSignal=(signal,phase,target=identity)=>{
    if(target.pid===child.pid&&closed)return;
    if(target.startTicks&&!stillAlive(target))return;
    const entry={signal,phase,target,atMs:at()};
    try{process.kill(target.pid,signal);entry.sent=true;}catch(e){entry.sent=false;entry.errorCode=e.code;}
    signals.push(entry);emit('owned-pid-signal',entry);
  };
  const stop=phase=>{
    if(boundary)return;
    boundary={phase,atMs:at(),childAlive:!closed,openSockets:mock.state.openSockets,closeReceived:mock.state.closeReceived,descendants:sampleChildren()};
    emit('observation-boundary',boundary);sendSignal('SIGTERM','observation-cleanup');
    cleanupTimer=setTimeout(()=>{sendSignal('SIGKILL','observation-cleanup');for(const ident of observedDescendants.values())sendSignal('SIGKILL','descendant-cleanup',ident);},500);
  };
  let timer=setTimeout(()=>stop('startup-deadline'),startupMs);
  const hardLimit=setTimeout(()=>{problem='owned process pipes remained open after cleanup';stop('collector-deadline');sendSignal('SIGKILL','collector-cleanup');for(const ident of observedDescendants.values())sendSignal('SIGKILL','descendant-cleanup',ident);child.stdin.destroy();child.stdout.destroy();child.stderr.destroy();},startupMs+observeMs+1500);
  const complete=()=>{
    if(logicalAt!==null)return;
    const out=Buffer.concat(stdout).toString('utf8'),err=Buffer.concat(stderr).toString('utf8');
    const reached=out===spec.stdout&&err===(spec.stderr??'')&&mock.state.hellos.length===1&&mock.state.requests.length===spec.prompts.length;
    if(!reached)return;
    if(['hello-fallback','local-sigint'].includes(spec.id)&&mock.state.modelRequests.length===0)return;
    if(spec.id==='idle-heartbeat'&&mock.state.pongs!==1)return;
    logicalAt=at();emit('logical-output-observed');clearTimeout(timer);
    if(spec.signal){sampleChildren();sentUserSignal=true;sendSignal(spec.signal,'user-cancel');timer=setTimeout(()=>stop('signal-deadline'),observeMs);}
    else timer=setTimeout(()=>stop('after-logical-output'),observeMs);
  };
  child.stdin.on('error',e=>{if(e.code!=='EPIPE'){problem=`stdin: ${e.message}`;stop('stdin-error');}});
  for(const stream of ['stdout','stderr'])child[stream].on('data',bytes=>{(stream==='stdout'?stdout:stderr).push(bytes);total+=bytes.length;emit('output',{stream,bytes:bytes.length});if(total>4*1024*1024){problem='output exceeds 4MiB';stop('output-bound');}complete();});
  const onHello=()=>setImmediate(complete),onTurn=()=>setImmediate(complete),onModel=()=>setImmediate(complete);
  const onPong=()=>{child.stdin.end(spec.afterPong);complete();};
  mock.events.on('hello',onHello);mock.events.on('turn',onTurn);mock.events.on('model-complete',onModel);mock.events.on('model-pending',onModel);mock.events.on('pong',onPong);
  if(spec.afterPong)child.stdin.write(spec.input);else child.stdin.end(spec.input??'');
  const sampler=setInterval(sampleChildren,50);
  const status=await new Promise(resolve=>{child.once('error',e=>{problem=`spawn: ${e.message}`;emit('spawn-error',{message:e.message});});child.once('close',(exitCode,signal)=>{closed=true;resolve({exitCode,signal});});});
  clearTimeout(timer);clearTimeout(cleanupTimer);clearTimeout(hardLimit);clearInterval(sampler);
  for(const [kind,fn] of [['hello',onHello],['turn',onTurn],['model-complete',onModel],['model-pending',onModel],['pong',onPong]])mock.events.off(kind,fn);
  const leftovers=[...observedDescendants.values()].filter(stillAlive);
  for(const ident of leftovers)sendSignal('SIGTERM','descendant-cleanup',ident);
  await new Promise(r=>setTimeout(r,30));
  for(const ident of leftovers.filter(stillAlive))sendSignal('SIGKILL','descendant-cleanup',ident);
  const result={...status,identity,startupMs,observeMs,elapsedMs:at(),logicalAt,boundary,signals,sentUserSignal,problem,descendants:[...observedDescendants.values()],leftovers,stdout:Buffer.concat(stdout),stderr:Buffer.concat(stderr)};
  emit('process-close',{exitCode:status.exitCode,signal:status.signal,leftovers});return result;
}
export function verifyCase(spec,side,result,state,project) {
  assert.equal(result.problem,null,'collector problem');assert.deepEqual(state.problems,[],'mock protocol');
  assert.deepEqual(state.health,[{method:'GET',url:'/health'}],'formal fixed-address probe');assert.equal(state.hellos.length,1,'one hello');
  const hello={type:'hello',protocolVersion:'1.2',clientName:'cli',clientVersion:'0.1.0',token:TOKEN};assert.equal(state.hellos[0].raw,JSON.stringify(hello),'hello bytes');
  assert.equal(result.stdout.toString('utf8'),spec.stdout,'stdout bytes');assert.equal(result.stderr.toString('utf8'),spec.stderr??'','stderr bytes');
  assert.equal(state.requests.length,spec.prompts.length,'RPC count');assert.equal(new Set(state.requests.map(r=>r.value.id)).size,state.requests.length,'unique per-turn request IDs');
  for(const [i,request] of state.requests.entries()){
    assert.match(request.value.id,/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,'UUID request ID');
    const params={sessionKey:`cli:project=${project}:default`,channelKey:'cli',message:spec.prompts[i],canPrompt:false};
    assert.equal(request.raw,JSON.stringify({type:'request',id:request.value.id,method:'submit_turn',params}),'exact submit_turn envelope');
  }
  if(['hello-fallback','local-sigint'].includes(spec.id)){assert.equal(state.modelRequests.length,1,'fallback reaches one mock model call');const body=JSON.parse(state.modelRequests[0].raw);assert.equal(body.model,'deterministic');assert.equal(body.stream,true);}
  else assert.equal(state.modelRequests.length,0,'online route does not call local model');
  assert.equal(state.pongs,spec.id==='idle-heartbeat'?1:0,'idle heartbeat');
  if(spec.signal){assert.equal(result.boundary,null,'user signal ends without cleanup deadline');assert.equal(result.signal,spec.signal,'raw signal exit');assert.equal(result.sentUserSignal,true);assert.equal(result.signals.length,1,'only one user signal and no cleanup signal');assert.equal(result.signals[0].phase,'user-cancel');assert.equal(result.signals[0].signal,spec.signal);assert.equal(result.signals[0].sent,true,'user signal was sent');assert.equal(result.signals[0].target.pid,result.identity.pid,'signal targets recorded CLI PID');assert.deepEqual(result.leftovers,[],'no owned descendant survives cancellation');return {evidenceValid:true,productPassed:true,lifecycle:'user-signal'};}
  if(result.boundary){assert.equal(side,'ts','Rust must exit naturally');assert.equal(result.boundary.phase,'after-logical-output','startup timeout is not a baseline hang');assert.ok(result.logicalAt!==null);assert.equal(result.boundary.childAlive,true);return {evidenceValid:true,productPassed:false,lifecycle:'observed-hang'};}
  assert.equal(result.exitCode,spec.exitCode);assert.equal(result.signal,null);assert.deepEqual(result.signals,[],'natural completion uses no cleanup signal');assert.deepEqual(result.leftovers,[],'no owned descendant survives');
  if(side==='rust'&&!['disconnect','hello-fallback'].includes(spec.id))assert.equal(state.closeReceived,true,'Rust closes remote transport');
  return {evidenceValid:true,productPassed:true,lifecycle:'natural-exit'};
}
function git(root,args){const r=spawnSync('git',['-C',root,...args],{encoding:'utf8'});assert.equal(r.status,0,r.stderr);return r.stdout.trim();}
function sourceIdentity(options,side){const root=fs.realpathSync(options[`${side}-root`]),commit=git(root,['rev-parse','HEAD']);assert.equal(commit,side==='ts'?BASELINE:options['rust-sha']);assert.equal(git(root,['status','--porcelain','--untracked-files=no']),'','tracked checkout must be clean');const executable=side==='ts'?path.join(root,'dist/src/cli/pilotdeck.js'):fs.realpathSync(options['rust-bin']);const files={};if(side==='ts')for(const f of ['src/cli/pilotdeck.ts','src/adapters/channel/cli/CliChannel.ts','src/gateway/client/GatewayWsClient.ts','src/gateway/client/probeServer.ts','dist/src/adapters/channel/cli/CliChannel.js','dist/src/gateway/client/GatewayWsClient.js','dist/src/gateway/client/probeServer.js'])files[f]=sha(fs.readFileSync(path.join(root,f)));return {root,commit,executable,sha256:sha(fs.readFileSync(executable)),files};}
export async function main(argv=process.argv.slice(2)){
  requireIsolation();assert.equal(process.version,'v22.22.0','fixed baseline Node version');const options={};for(let i=0;i<argv.length;i+=2){assert.ok(argv[i].startsWith('--')&&argv[i+1]);const key=argv[i].slice(2);assert.ok(['side','ts-root','rust-root','rust-sha','rust-bin','out','startup-ms','observe-ms'].includes(key));assert.equal(options[key],undefined);options[key]=argv[i+1];}
  const side=options.side??'both';assert.ok(['ts','rust','both'].includes(side));const sides=side==='both'?['ts','rust']:[side];assert.ok(options.out,'--out NEW_DIRECTORY');for(const side of sides)assert.ok(options[`${side}-root`]);if(sides.includes('rust'))assert.match(options['rust-sha']??'',/^[0-9a-f]{40}$/);
  for(const key of ['startup-ms','observe-ms'])if(options[key]!==undefined)assert.ok(Number.isInteger(Number(options[key]))&&Number(options[key])>0&&Number(options[key])<=120000,'positive bounded timeouts');
  const out=path.resolve(options.out),specs=JSON.parse(fs.readFileSync(path.join(here,'cases.json')));assert.equal(specs.length,8);assert.deepEqual(specs.map(s=>s.id).sort(),['argv','stdin-lines','idle-heartbeat','hello-fallback','disconnect','stdin-eof','sigint','local-sigint'].sort(),'complete fixed case set');
  const identities=Object.fromEntries(sides.map(side=>[side,sourceIdentity(options,side)]));fs.mkdirSync(out);
  const identity={createdAt:new Date().toISOString(),driverPid:process.pid,platform:`${os.platform()}-${os.arch()}`,node:process.version,nodeExecutable:process.execPath,nodeSha256:sha(fs.readFileSync(process.execPath)),networkNamespace:fs.readlinkSync('/proc/self/ns/net'),isolation:process.env.PILOTDECK_C1_ONLINE_DOCKER,port:18789,publishedPorts:'declared none by E0',sources:identities,files:Object.fromEntries(['capture.mjs','mock.mjs','cases.json','../c1-cron/capture.mjs'].map(f=>[f,sha(fs.readFileSync(path.join(here,f)))]))};fs.writeFileSync(path.join(out,'identity.json'),json(identity));
  const rows=[];let failed=false;
  for(const spec of specs)for(const side of sides){
    const root=path.join(out,spec.id,side),fixture=path.join(root,'fixture'),home=path.join(fixture,'home'),pilot=path.join(home,'.pilotdeck'),project=path.join(fixture,'project');
    for(const dir of [pilot,project,path.join(fixture,'skills'),path.join(fixture,'plugins'),path.join(fixture,'tmp')])fs.mkdirSync(dir,{recursive:true});
    fs.writeFileSync(path.join(pilot,'server-token'),TOKEN+'\n',{mode:0o600});fs.writeFileSync(path.join(pilot,'.legacy-bundled-skills-migrated-v1'),'completed\n');fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),config(18789));
    const env={PATH:process.env.PATH,HOME:home,USERPROFILE:home,CFFIXED_USER_HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),TMPDIR:path.join(fixture,'tmp'),LANG:'C',LC_ALL:'C',TZ:'UTC',TERM:'dumb',NO_COLOR:'1',NODE_NO_WARNINGS:'1',DOCKER_CONTAINER:'1',COMMIT_HASH:'c1-fixed-fixture',PILOTDECK_BUNDLED_SKILLS_DIR:path.join(fixture,'skills'),PILOTDECK_BUNDLED_PLUGINS_DIR:path.join(fixture,'plugins')};
    const command=side==='ts'?process.execPath:identities[side].executable,args=[...(side==='ts'?[identities[side].executable]:[]),...spec.argv];fs.writeFileSync(path.join(root,'command.json'),json({command,args,env,cwd:project,spec}));fs.writeFileSync(path.join(root,'files-before.json'),json(snapshot(fixture)));
    const append=file=>event=>fs.appendFileSync(path.join(root,file),JSON.stringify(event)+'\n');const mock=await startMock(spec,{record:append('network.jsonl')});let result;
    try{result=await drive(command,args,env,project,spec,mock,{startupMs:Number(options['startup-ms']??20000),observeMs:Number(options['observe-ms']??1500),record:append('process.jsonl')});}finally{await mock.stop();}
    fs.writeFileSync(path.join(root,'stdout.bin'),result.stdout);fs.writeFileSync(path.join(root,'stderr.bin'),result.stderr);fs.writeFileSync(path.join(root,'files-after.json'),json(snapshot(fixture)));
    let verdict;try{verdict=verifyCase(spec,side,result,mock.state,project);}catch(e){failed=true;verdict={evidenceValid:false,productPassed:false,failure:e.message};}
    fs.writeFileSync(path.join(root,'result.json'),json({...result,stdout:result.stdout.toString('utf8'),stderr:result.stderr.toString('utf8'),stdoutSha256:sha(result.stdout),stderrSha256:sha(result.stderr),network:mock.state,verdict}));rows.push({case:spec.id,side,...verdict});console.log(`${side} ${spec.id}: ${verdict.lifecycle??`FAIL ${verdict.failure}`}`);
  }
  const summary={expectedRows:specs.length*sides.length,observedRows:rows.length,collectionPassed:!failed,rustPassed:sides.includes('rust')?rows.filter(r=>r.side==='rust').every(r=>r.productPassed):null,tsObservedHangs:rows.filter(r=>r.side==='ts'&&r.lifecycle==='observed-hang').length,rows};fs.writeFileSync(path.join(out,'summary.json'),json(summary));process.exitCode=failed?1:0;
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){try{await main();}catch(e){console.error(e.stack??e.message);process.exitCode=1;}}
