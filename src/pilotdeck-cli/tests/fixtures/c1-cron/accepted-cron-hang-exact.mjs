#!/usr/bin/env node
// Revalidate only fixed original captures; never spawns any product/peer.
import assert from 'node:assert/strict';import fs from 'node:fs';import path from 'node:path';
import {createHash} from 'node:crypto';import {isDeepStrictEqual} from 'node:util';
import {fileURLToPath,pathToFileURL} from 'node:url';import {REGISTRATION as REG} from './cron-hang-registration.mjs';
export function rawContract(spec, side, result, process) {
  const expected = { stdout: spec.reply?.ok ? JSON.stringify(spec.reply.result,null,2)+'\n' : '', stderr: spec.stderr??'' };
  assert.equal(result.stdout,expected.stdout);assert.equal(result.stderr,expected.stderr);
  assert.equal(result.problem,null);assert.equal(result.startupMs,15000);assert.equal(result.observeMs,1500);
  assert.ok(Number.isFinite(result.terminalAtMs)&&result.terminalAtMs>=0);
  assert.ok(result.elapsedMs>=result.terminalAtMs);
  const starts=process.filter(x=>x.kind==='process-start'), terminals=process.filter(x=>x.kind==='terminal-output-observed'),closes=process.filter(x=>x.kind==='process-close');
  assert.equal(starts.length,1);assert.equal(terminals.length,1);assert.equal(closes.length,1);
  assert.ok(Number.isInteger(result.pid)&&result.pid>0);assert.equal(result.processGroup,result.pid);
  assert.equal(starts[0].pid,result.pid);assert.equal(starts[0].processGroup,result.pid);
  assert.ok(terminals[0].atMs>=result.terminalAtMs);assert.ok(closes[0].atMs>=terminals[0].atMs);
  assert.equal(closes[0].exitCode,result.exitCode);assert.equal(closes[0].signal,result.signal);
  if(side==='ts') {
    assert.equal(result.exitCode,null);assert.equal(result.signal,'SIGTERM');
    assert.deepEqual(Object.keys(result.boundary).sort(),['phase','atMs','childAlive','openSockets','closeReceived'].sort());
    assert.equal(result.boundary.phase,'after-terminal-output');assert.equal(result.boundary.childAlive,true);
    assert.equal(result.boundary.openSockets,1);assert.equal(result.boundary.closeReceived,false);
    assert.ok(result.boundary.atMs-result.terminalAtMs>=1500,'complete observed 1500ms after terminal');
    assert.equal(result.signals.length,1);assert.equal(result.signals[0].signal,'SIGTERM');
    assert.equal(result.signals[0].targetProcessGroup,result.pid);assert.equal(result.signals[0].sent,true);
    assert.ok(result.signals[0].atMs>=result.boundary.atMs);assert.ok(closes[0].atMs>=result.signals[0].atMs);
  } else {
    assert.equal(side,'rust');assert.equal(result.exitCode,spec.exitCode??0);assert.equal(result.signal,null);
    assert.equal(result.boundary,null);assert.deepEqual(result.signals,[]);
  }
}
export function comparePair(spec,left,right) {
  const a=structuredClone(left),b=structuredClone(right),exit=spec.exitCode??0;
  assert.deepEqual(a.lifecycle,{exitCode:null,signal:'SIGTERM',startupMs:15000,observeMs:1500,logicalOutputObserved:true,
    boundary:{phase:'after-terminal-output',childAlive:true,openSockets:1,closeReceived:false},signals:[{signal:'SIGTERM',sent:true}],problem:null,
    verdict:{evidenceValid:true,productPassed:false,lifecycle:'observed-hang',naturalExitCode:null}});
  assert.deepEqual(b.lifecycle,{exitCode:exit,signal:null,startupMs:15000,observeMs:1500,logicalOutputObserved:true,boundary:null,signals:[],problem:null,
    verdict:{evidenceValid:true,productPassed:true,lifecycle:'natural-exit',naturalExitCode:exit}});
  assert.equal(a.network.closeReceived,false);assert.equal(b.network.closeReceived,true);
  assert.equal(a.network.openSockets,0);assert.equal(b.network.openSockets,0);
  const hello=JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'cli',clientVersion:'0.1.0',token:'c1-cron-isolated-fixture-token'});
  const map=value=>value==='$PROJECT'?'<C1_FIXTURE>/project':Array.isArray(value)?value.map(map):value&&typeof value==='object'?Object.fromEntries(Object.entries(value).map(([k,v])=>[k,map(v)])):value;
  const request=spec.method?JSON.stringify({type:'request',id:'<RPC_0>',method:spec.method,params:map(spec.params)}):null;
  const business=[{kind:'http',method:'GET',url:'/health',status:200},{kind:'ws-open'},
    {kind:'ws-receive',raw:hello},{kind:'ws-send',raw:JSON.stringify({type:'hello_ok',protocolVersion:'1.2',serverInfo:{name:'c1-cron-owned-mock',version:'0.1.0'}})},
    ...(request?[{kind:'ws-receive',raw:request},{kind:'ws-send',raw:JSON.stringify({type:'response',id:'<RPC_0>',...spec.reply})}]:[])];
  for(const value of [a,b]) {
    assert.deepEqual(value.network.hellos,[hello]);assert.deepEqual(value.network.requests,request?[request]:[]);
    assert.deepEqual(value.network.events.slice(0,business.length),business,'all actual hello/RPC/response bytes remain exact');
  }
  const base=spec.method?6:4;
  assert.deepEqual(a.network.events.slice(base),[{kind:'mock-cleanup'},{kind:'ws-socket-closed'}]);
  assert.deepEqual(b.network.events.slice(base),[{kind:'ws-close-received',payloadBase64:''},{kind:'ws-socket-closed'},{kind:'mock-cleanup'}]);
  a.lifecycle=b.lifecycle='<checked fixed TS observed hang vs natural expected Rust exit>';
  a.network.closeReceived=b.network.closeReceived='<checked close difference>';
  a.network.events=a.network.events.slice(0,base);b.network.events=b.network.events.slice(0,base);
  return isDeepStrictEqual(a,b)?[]:[{left:a,right:b}];
}

const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
function pin(root,table){for(const [name,digest] of Object.entries(table)){
 assert.ok(name&&!path.isAbsolute(name)&&!name.split('/').includes('..'));
 let cursor=root;for(const part of name.split('/')){cursor=path.join(cursor,part);assert.equal(fs.lstatSync(cursor).isSymbolicLink(),false,'pinned path has no symlink');}
 assert.ok(fs.lstatSync(cursor).isFile());assert.equal(sha(fs.readFileSync(cursor)),digest,`pinned ${name}`);
}}
export function auditReadset(actual){assert.deepEqual(actual,REG.raw,'exact384 judge readkeys; no extra/missing authority');}
function allPins(source,e0,input){
 pin(source,REG.judge);pin(input,REG.raw);pin(input,REG.artifacts);pin(e0,REG.build);
 for(const [relative,expected] of Object.entries(REG.productTrees)){
  const actual={},root=path.join(input,relative);assert.equal(fs.lstatSync(root).isSymbolicLink(),false);
  function walk(p){for(const name of fs.readdirSync(p)){const f=path.join(p,name),s=fs.lstatSync(f);assert.equal(s.isSymbolicLink(),false);if(s.isDirectory())walk(f);else{assert.ok(s.isFile());actual[path.relative(root,f)]=sha(fs.readFileSync(f));}}}
  walk(root);assert.deepEqual(actual,expected,'exact postrun home/project tree');
 }
}
function owners(input){
 const post=JSON.parse(fs.readFileSync(path.join(input,'postflight.json')));assert.equal(post.binaryUnchanged,true);assert.equal(post.sourceClean,true);
 for(const stage of ['ts-a','ts-b','pair-capture']){
  const result=JSON.parse(fs.readFileSync(path.join(input,`control/cron-${stage}.result.json`)));
  assert.equal(result.exit,0);assert.equal(result.containerGone,true);assert.equal(result.cleanupUnknown,null);
  const commands=JSON.parse(fs.readFileSync(path.join(input,`control/cron-${stage}.docker.json`)));
  const inspect=commands.find(x=>x.argv[1]==='inspect'&&x.exit===0);assert.ok(inspect);
  const original=JSON.parse(inspect.stdout)[0];assert.equal(original.Id,result.containerId);
  assert.equal(original.HostConfig.NetworkMode,'none');assert.equal(original.HostConfig.ReadonlyRootfs,true);
  assert.equal(original.Config.User,'1000:1000');assert.deepEqual(original.HostConfig.PortBindings??{},{});
  const final=commands.at(-1);assert.deepEqual(final.argv,['docker','inspect',result.containerId]);assert.equal(final.exit,1);assert.equal(final.stdout.trim(),'[]');assert.equal(final.stderr.trim(),`error: no such object: ${result.containerId}`);
 }
}
export async function runExact(argv){
 assert.equal(argv.length,4,'usage: --evidence-root EXACT_CAPTURE_PARENT --out NEW_ABSOLUTE_DIRECTORY');assert.equal(argv[0],'--evidence-root');assert.equal(argv[2],'--out');assert.ok(path.isAbsolute(argv[1])&&path.isAbsolute(argv[3]));
 const source=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../../../../..'),e0=fs.realpathSync(argv[1]),input=path.join(e0,REG.capturePrefix);
 const out=path.join(fs.realpathSync(path.dirname(argv[3])),path.basename(argv[3]));assert.ok(out!==e0&&!out.startsWith(e0+path.sep),'output outside evidence');assert.ok(!fs.existsSync(out),'new output only');
 allPins(source,e0,input);owners(input);
 const record=JSON.parse(fs.readFileSync(path.join(e0,'c88650f6-sealed-release/release-record.json')));
 assert.equal(record.source.commit,REG.candidate);assert.equal(record.source.dirty,false);assert.deepEqual(record.source.status,[]);assert.equal(record.profile,'release');assert.equal(record.target,'aarch64-unknown-linux-gnu');assert.equal(record.binaries.pilotdeck.sha256,REG.binarySHA256);
 assert.deepEqual(record.evidence.map(x=>x.sha256).sort(),Object.entries(REG.build).filter(([p])=>!p.endsWith('/release-record.json')).map(([,h])=>h).sort());assert.equal(fs.readFileSync(path.join(e0,'c88650f6-release/release.exit'),'utf8').trim(),'0');
 const {compareCaptured}=await import(pathToFileURL(path.join(source,'src/pilotdeck-cli/tests/fixtures/c1-cron/compare-common.mjs')));
 fs.mkdirSync(out);const readset={},rows=[],strict=[];
 for(const mode of ['self','parity']){
  const left=mode==='self'?'ts-a':'pair',right=mode==='self'?'ts-b':'pair',target=path.join(out,'strict-'+mode);
  compareCaptured('cron',{mode,left:path.join(input,'cron',left),right:path.join(input,'cron',right),out:target,'rust-sha':REG.candidate,'rust-binary-sha256':REG.binarySHA256});
  const report=JSON.parse(fs.readFileSync(path.join(target,'comparison.json')));assert.equal(report.status,mode==='self'?'SAME':'DIFF');assert.equal(report.productPassed,false);assert.equal(report.observedCases,14);assert.ok(report.rows.every(x=>x.status!=='INVALID'));
  strict.push({mode,status:report.status,productPassed:report.productPassed,path:'strict-'+mode+'/comparison.json',sha256:sha(fs.readFileSync(path.join(target,'comparison.json')))});
  for(const [side,files] of Object.entries(report.inputHashes))for(const [p,h] of Object.entries(files)){const key=`cron/${side==='left'?left:right}/${p}`;assert.ok(Object.hasOwn(REG.raw,key),'no extra readkey');assert.equal(REG.raw[key],h);readset[key]=h;}
  for(const spec of REG.cases){
   const sides=mode==='self'?['ts','ts']:['ts','rust'];
   for(const [index,capture] of [left,right].entries()){
    const base=path.join(input,'cron',capture,spec.id,sides[index]),result=JSON.parse(fs.readFileSync(path.join(base,'result.json'))),events=fs.readFileSync(path.join(base,'process.jsonl'),'utf8').trim().split('\n').map(JSON.parse);rawContract(spec,sides[index],result,events);
   }
   const a=JSON.parse(fs.readFileSync(path.join(target,spec.id+'.left.json'))),b=JSON.parse(fs.readFileSync(path.join(target,spec.id+'.right.json')));
   if(mode==='self')assert.deepEqual(a,b);else assert.deepEqual(comparePair(spec,a,b),[]);
   rows.push({mode,id:spec.id,status:mode==='self'?'STRICT_TS_SELF_SAME':'ACCEPTED_FIXED_BASELINE_HANG_REPAIR',expectedRustExit:spec.exitCode??0});
  }
 }
 auditReadset(readset);allPins(source,e0,input);owners(input);
 const receipt={registration:REG.id,actualProductCandidate:REG.candidate,binarySHA256:REG.binarySHA256,collectorSHA256:REG.judge['src/pilotdeck-cli/tests/fixtures/c1-cron/capture.mjs'],actualTS:42,actualRust:14,RustExpectedExitCounts:{zero:6,one:8},rows,strict,fullJudgeReadset:readset,extraArtifacts:REG.artifacts,productTrees:REG.productTrees,buildReceipt:REG.build,judgeClosure:REG.judge,
 entrySHA256:sha(fs.readFileSync(fileURLToPath(import.meta.url))),registrationSHA256:sha(fs.readFileSync(new URL('./cron-hang-registration.mjs',import.meta.url))),
 limits:['only fixed three captures/14cases; no future-capture or generic-timeout authorization','TS observed at least1500ms after exact terminal with liveWS, not proof of infinite hang','TS cleanupSIGTERM remains external cleanup, never natural success','PID/PGID immediate spawn/journal + isolated exactCID; no OSbirth/PIDreuse universalproof','beforeafter hashes not a hostile concurrentwriter CAS guarantee','no product/capture/API execution by this entry']};
 fs.writeFileSync(path.join(out,'accepted.json'),JSON.stringify(receipt,null,2)+'\n');console.log('ACCEPTED_FIXED_BASELINE_HANG_REPAIR: exact14; original strict DIFF14 retained');
}
if(process.argv[1]&&fs.realpathSync(process.argv[1])===fileURLToPath(import.meta.url)){
 try{await runExact(process.argv.slice(2));}catch(error){console.error(error.message);process.exitCode=2;}
}
