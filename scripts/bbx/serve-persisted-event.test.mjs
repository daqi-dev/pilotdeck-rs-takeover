// Real TCP provider and real CLI child signals through the production runner.
// Only its fixed port constants and product entrypoint are replaced in a temp
// copy; no real PilotDeck, HOME, Gateway subscription, or port 18789 is used.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { validateArtifact } from './evidence.mjs';
import { matchPersistedRunEvent, validateTrigger } from './serve-persisted-event.mjs';

const hash = value => createHash('sha256').update(value).digest('hex');
const evidenceDirectory = process.env.BBX_TRIGGER_TEST_EVIDENCE;
let evidenceSequence = 0;
if (evidenceDirectory) {
  assert(path.isAbsolute(evidenceDirectory), 'absolute test evidence directory required');
  fs.mkdirSync(evidenceDirectory); // Never overwrite an earlier evidence run.
}
const boundPort = async () => {
  const socket = net.createServer(); await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port; await new Promise(resolve => socket.close(resolve)); return port;
};
const fixtureCli = String.raw`
import fs from 'node:fs'; import path from 'node:path'; import http from 'node:http';
const mode=process.argv[2], pilot=process.env.PILOT_HOME, project=process.argv[3];
const session='fixture-session', run='11111111-1111-4111-8111-111111111111';
const write=()=>{
 if(mode==='missing')return;
 const record={sessionKey:mode==='wrong-session'?'other':session,projectKey:mode==='wrong-project'?project+'/other':project,runId:run,state:'running',requestMaterial:JSON.stringify({sessionKey:session,projectKey:project,message:mode==='wrong-request'?'OTHER':'C1-CANCEL'})};
 const event={seq:6,event:{type:'assistant_text_delta',text:mode==='wrong-text'?'OTHER':'C1 partial',runId:mode==='wrong-run-id'?'22222222-2222-4222-8222-222222222222':run}};
 const records=mode==='duplicate-record'?[record,record]:[record];
 fs.mkdirSync(path.join(pilot,'runs'),{recursive:true});fs.writeFileSync(path.join(pilot,'runs/registry.json'),JSON.stringify({schemaVersion:1,records,events:{[project+'\0'+session+'\0'+run]:[event]}}));
};
process.on('SIGINT',()=>{fs.writeFileSync(path.join(pilot,'signal.json'),JSON.stringify({signal:'SIGINT',pid:process.pid}));console.log(JSON.stringify({status:'aborted',reason:'CLI interrupted'}));process.exit(1);});
const timer=setInterval(()=>{},1000);console.log('fixture alive, no partial stdout');
const url=/url: (http:\/\/[^\n]+)/.exec(fs.readFileSync(path.join(pilot,'pilotdeck.yaml'),'utf8'))[1];
if(mode==='missing-provider')write();
const body=JSON.stringify({model:'deterministic',stream:true,messages:[{role:'user',content:mode==='wrong-provider-request'?'OTHER':'C1-CANCEL'}]});
if(mode==='unflushed')write();
const request=http.request(url+'/chat/completions',{method:'POST',headers:{'content-type':'application/json','content-length':Buffer.byteLength(body)}},response=>{response.once('data',write);response.on('error',()=>{});response.resume();});
request.on('error',()=>{});if(mode==='missing-provider')request.destroy();else request.end(body);
if(mode==='duplicate-provider'){const second=http.request(url+'/chat/completions',{method:'POST',headers:{'content-type':'application/json','content-length':Buffer.byteLength(body)}},response=>response.resume());second.on('error',()=>{});second.end(body);}
`;
async function capture(mode) {
  const root=fs.mkdtempSync(path.join(os.tmpdir(),'bbx-persisted-trigger-'));
  const scripts=path.join(root,'scripts/bbx'),base=path.join(root,'baseline'),bin=path.join(root,'bin');
  for(const dir of [scripts,path.join(base,'dist/src/cli'),bin])fs.mkdirSync(dir,{recursive:true});
  for(const name of ['run.mjs','check.mjs','diff.mjs','evidence.mjs','mock-support.mjs','syntax-policy.mjs','native-identity.mjs','serve-persisted-event.mjs', 'channel-poll-policy.mjs'])fs.copyFileSync(new URL(name,import.meta.url),path.join(scripts,name));
  const ports=[await boundPort(),await boundPort()];assert(!ports.includes(18789));
  const runner=path.join(scripts,'run.mjs');let source=fs.readFileSync(runner,'utf8');
  source=source.replace('const GATEWAY_PORT = 38411 + 10 * SLOT;',`const GATEWAY_PORT = ${ports[0]};`).replace('const MOCK_PORT = 38412 + 10 * SLOT;',`const MOCK_PORT = ${ports[1]};`);fs.writeFileSync(runner,source);
  fs.symlinkSync(process.execPath,path.join(bin,'node'));
  // Read-only identity probes and the real-instance guard are inert in this
  // fake product fixture, so they cannot inspect a developer's real process.
  for(const name of ['git','rustc','pnpm','lsof'])fs.writeFileSync(path.join(bin,name),'#!/bin/sh\nexit 1\n',{mode:0o755});
  fs.writeFileSync(path.join(base,'dist/src/cli/pilotdeck.js'),fixtureCli);
  const scenario={name:'trigger-fixture',server:false,provider:[{name:'cancel',reply:mode==='closed'?{text:'C1 partial'}:{chunks:['C1 partial'],hangAfter:mode==='unflushed'?0:1,hangMs:5000}}],steps:[{serve:[mode,'{PROJECT}'],untilPersistedRunEvent:{sessionKey:'fixture-session',projectKey:'{PROJECT}',requestText:'C1-CANCEL',providerRule:'cancel',event:{type:'assistant_text_delta',text:'C1 partial'}},signal:'SIGINT',timeout:1200,settleMs:100}],assertions:[{id:'reached',path:'/steps/0/serve/reached',equals:true},{id:'self-exit',path:'/steps/0/serve/exitCode',equals:1},{id:'handled',path:'/steps/0/serve/signal',equals:null}]};
  const spec=path.join(root,'scenario.json'),out=path.join(root,'capture.json');fs.writeFileSync(spec,JSON.stringify(scenario));
  const child=spawn(process.execPath,[runner,'ts',spec,out],{env:{PATH:bin,HOME:path.join(root,'home'),TMPDIR:path.join(root,'tmp'),BBX_BASELINE:base,BBX_TMP_ROOT:path.join(root,'fixture'),BBX_SRC:path.join(root,'src'),BBX_TARGET:path.join(root,'target')},stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='';child.stdout.on('data',b=>stdout+=b);child.stderr.on('data',b=>stderr+=b);
  const timer=setTimeout(()=>child.kill('SIGKILL'),10000);
  const status=await new Promise(resolve=>child.once('close',(code,signal)=>resolve({code,signal})));clearTimeout(timer);
  assert.deepEqual(status,{code:0,signal:null},`${stdout}\n${stderr}`);
  const artifact=JSON.parse(fs.readFileSync(out)),receipt=path.join(root,'fixture/pdrs-bbx/root/home/.pilotdeck/signal.json');
  if (evidenceDirectory) {
    const archive=path.join(evidenceDirectory,`${++evidenceSequence}-${mode}`);fs.mkdirSync(archive);
    fs.copyFileSync(out,path.join(archive,'capture.json'));fs.copyFileSync(spec,path.join(archive,'scenario.json'));
    fs.writeFileSync(path.join(archive,'runner.stdout.bin'),stdout);fs.writeFileSync(path.join(archive,'runner.stderr.bin'),stderr);
    if(fs.existsSync(receipt))fs.copyFileSync(receipt,path.join(archive,'signal.json'));
    fs.writeFileSync(path.join(archive,'identity.json'),JSON.stringify({apparatusOnly:true,realProductExecuted:false,mode,ports,status,signalReceived:fs.existsSync(receipt),node:process.version,
      productionRunnerSha256:hash(fs.readFileSync(new URL('./run.mjs',import.meta.url))),temporaryRunnerSha256:hash(fs.readFileSync(runner)),
      temporarySubstitutions:'GATEWAY_PORT and MOCK_PORT only; fake product entrypoint; inert lsof/git/rustc/pnpm',
      captureSha256:hash(fs.readFileSync(out)),scenarioSha256:hash(fs.readFileSync(spec)),fixtureCliSha256:hash(fixtureCli)},null,2)+'\n');
  }
  return {root,scenario,artifact,signalReceived:fs.existsSync(receipt),cleanup:()=>fs.rmSync(root,{recursive:true,force:true})};
}
test('production serve sends SIGINT only after real flushed/open provider and persisted partial',async()=>{
 const f=await capture('valid');try{
  const serve=f.artifact.steps[0].serve;assert.equal(serve.reached,true,JSON.stringify(f.artifact));assert.equal(serve.exitCode,1);assert.equal(serve.signal,null);assert.equal(f.signalReceived,true);assert(!serve.stdout.includes('C1 partial'));
  assert.equal(serve.persistedRunEvent.provider.flushedChunks,1);assert.deepEqual(f.artifact.failures,[]);
  assert.deepEqual(validateArtifact(f.artifact,f.scenario,f.artifact.evidence),[]);
  const proof=f.artifact.evidence.serveTriggers[0];assert.equal(hash(Buffer.from(proof.raw.base64,'base64')),proof.raw.sha256);
  // Reproduce the real scenario's escaped capture regex, not a simplified
  // substitution: serve uses the session saved from the initial CLI output.
  const original=JSON.parse(fs.readFileSync(new URL('./scenarios/c1-sdk-cancel-resume.json',import.meta.url)));
  const a=structuredClone(f.artifact), spec=structuredClone(f.scenario);
  const initial={cli:{stdout:'{"sessionId":"fixture-session"}\n'}};
  a.steps.unshift(initial);spec.steps.unshift({cli:['fixture-initial'],save:original.steps[0].save});
  spec.steps[1].untilPersistedRunEvent.sessionKey='{saved.session}';a.evidence.serveTriggers[0].stepIndex=1;
  spec.assertions=[];a.evidence.assertions=[];
  assert.deepEqual(validateArtifact(a,spec,a.evidence),[]);
  a.steps[0].cli.stdout='{"sessionId":"other-session"}\n';
  assert(validateArtifact(a,spec,a.evidence).some(error=>error.includes('trigger condition mismatch')));
 }finally{f.cleanup();}
});
for(const mode of ['missing','missing-provider','wrong-session','wrong-project','wrong-request','wrong-text','wrong-run-id','duplicate-record','duplicate-provider','wrong-provider-request','closed','unflushed'])test(`production serve refuses ${mode} and never signals SIGINT`,async()=>{
 const f=await capture(mode);try{assert.equal(f.artifact.steps[0].serve.reached,false);assert.equal(f.artifact.steps[0].serve.signal,'SIGKILL');assert.equal(f.signalReceived,false);assert(f.artifact.failures.length);assert(validateArtifact(f.artifact,f.scenario,f.artifact.evidence).length);}finally{f.cleanup();}
});
test('raw hash and fully rebound wrong event cannot forge a reached artifact',async()=>{
 const f=await capture('valid');try{
  for(const mutate of [
   a=>{a.evidence.serveTriggers[0].raw.sha256='0'.repeat(64);},
   a=>{const raw=a.evidence.serveTriggers[0].raw;const registry=JSON.parse(Buffer.from(raw.base64,'base64'));Object.values(registry.events)[0][0].event.text='different';const bytes=Buffer.from(JSON.stringify(registry));raw.base64=bytes.toString('base64');raw.sha256=hash(bytes);},
   a=>{a.steps[0].serve.persistedRunEvent.event.event.runId='other';},
   a=>{a.evidence.serveTriggers[0].provider.open=false;},
   a=>{a.evidence.serveTriggers[0].signalSent=null;},
   a=>{a.evidence.serveTriggers[0].pid++;},
   a=>{a.evidence.serveTriggers[0].signalSentAt='invalid';},
   a=>{a.evidence.serveTriggers[0].providers=[];},
   a=>{const proof=a.evidence.serveTriggers[0];const p=structuredClone(proof.providers[0]);p.index=1;proof.providers.push(p);a.provider.cancel.push(structuredClone(a.provider.cancel[0]));},
   a=>{a.provider.cancel[0].body.messages[0].content='other';},
  ]){const a=structuredClone(f.artifact);mutate(a);assert(validateArtifact(a,f.scenario,a.evidence).some(e=>e.includes('persisted trigger')));}
 }finally{f.cleanup();}
});
test('strict trigger rejects missing fields and unsupported event kinds',()=>{
 for(const condition of [null,{}, {sessionKey:'s',projectKey:'p',requestText:'r',providerRule:'x',event:{type:'turn_started',text:'t'}}])assert.throws(()=>validateTrigger(condition));
 assert.equal(matchPersistedRunEvent({}, {sessionKey:'s',projectKey:'p',requestText:'r',providerRule:'x',event:{type:'assistant_text_delta',text:'t'}},[]),null);
});
