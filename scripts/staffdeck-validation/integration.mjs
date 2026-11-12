#!/usr/bin/env node
// Isolated formal HTTP entry validation when the local fixed-port bbx suite
// cannot run. It never connects to 18789 and records every actual child PID.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { createHash } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';
import assert from 'node:assert/strict';

const args = process.argv.slice(2);
const value = flag => args[args.indexOf(flag) + 1];
if (!args.includes('--out') || !args.includes('--baseline')) throw new Error('--out NEW_DIR --baseline TS_CHECKOUT [--rust BINARY] [--self] required');
const out = path.resolve(value('--out'));
if (fs.existsSync(out)) throw new Error('output directory must be new');
fs.mkdirSync(out, { recursive: true });
const root = path.join(out, 'fixture');
const baseline = path.resolve(value('--baseline'));
const rust = args.includes('--rust') ? path.resolve(value('--rust')) : null;
const scenario = JSON.parse(fs.readFileSync(new URL('../bbx/scenarios/staffdeck-identity-approval.json', import.meta.url)));
const hash = file => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const git = cwd => spawnSync('git',['rev-parse','HEAD'],{cwd,encoding:'utf8'}).stdout.trim();
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
let requests = [];
const mock = http.createServer((req, res) => {
  const rule = scenario.provider.find(rule => rule.method === req.method && rule.path === new URL(req.url,'http://localhost').pathname);
  requests.push({method:req.method,path:req.url,bearer:req.headers.authorization});
  res.writeHead(rule?.reply.status ?? 500, {'content-type':'application/json'});
  res.end(JSON.stringify(rule?.reply.body ?? {error:'unexpected mock request'}));
});
mock.listen(0,'127.0.0.1'); await once(mock,'listening');
const mockUrl = `http://127.0.0.1:${mock.address().port}`;
const home = path.join(root,'home'); const pilot = path.join(home,'.pilotdeck');
const substitutions = {ROOT:root,HOME:home,PILOT_HOME:pilot,PROJECT:path.join(root,'proj'),MOCK:mockUrl,TOKEN:'b1-gateway-test-token'};
const subst = input => typeof input === 'string' ? input.replace(/\{([A-Z_]+)\}/g,(all,key) => substitutions[key] ?? all)
  : Array.isArray(input) ? input.map(subst) : input && typeof input === 'object' ? Object.fromEntries(Object.entries(input).map(([k,v])=>[k,subst(v)])) : input;
const getPort = async () => { const server=net.createServer(); server.listen(0,'127.0.0.1'); await once(server,'listening'); const port=server.address().port; await new Promise(r=>server.close(r)); return port; };
const ids=[];
async function launch(command, arguments_, env, side, round) {
  const port=await getPort();
  if(port===18789) throw new Error('unexpected fixed port allocation');
  const child=spawn(command,[...arguments_,'server','--host','127.0.0.1','--port',String(port)],{cwd:substitutions.PROJECT,env,stdio:['ignore','pipe','pipe']});
  ids.push({side,round,pid:child.pid,port}); fs.writeFileSync(path.join(out,'processes.json'),JSON.stringify(ids,null,2));
  const log=[]; child.stdout.on('data',b=>log.push(b)); child.stderr.on('data',b=>log.push(b));
  let ended; const done=new Promise(resolve=>child.once('exit',(code,signal)=>{ended={code,signal};resolve(ended);}));
  const stop=async()=>{
    if(!ended)child.kill('SIGTERM');
    let result=await Promise.race([done,delay(10000).then(()=>null)]);
    if(!result){child.kill('SIGKILL');result=await done;}
    fs.writeFileSync(path.join(out,`${side}-${round}.log`),Buffer.concat(log));
    assert.equal(result.signal,null,`${side} was killed`); assert.equal(result.code,0,`${side} server stop`);
  };
  try {
    const deadline=Date.now()+45000;
    while(Date.now()<deadline){
      if(ended)throw new Error(`${side} startup exited ${JSON.stringify(ended)}: ${Buffer.concat(log)}`);
      try { const socket=net.connect(port,'127.0.0.1'); await Promise.race([once(socket,'connect'),once(socket,'error').then(([e])=>Promise.reject(e))]); socket.destroy(); return {url:`http://127.0.0.1:${port}`,stop}; } catch { await delay(50); }
    }
    throw new Error(`${side} startup timeout`);
  }catch(error){await stop().catch(()=>{});throw error;}
}
async function run(side, command, arguments_) {
  fs.rmSync(root,{recursive:true,force:true}); fs.mkdirSync(pilot,{recursive:true}); fs.mkdirSync(substitutions.PROJECT,{recursive:true});
  fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),subst(scenario.config));
  for(const [rel,raw] of Object.entries(scenario.files)){
    const target=path.resolve(root,rel); if(!target.startsWith(root+path.sep))throw new Error('fixture escapes root');
    fs.mkdirSync(path.dirname(target),{recursive:true}); const content=subst(raw);
    if(content && typeof content==='object' && Object.keys(content).length===1 && Array.isArray(content.$sqlite)){const db=new DatabaseSync(target);try{for(const sql of content.$sqlite)db.exec(sql);}finally{db.close();}}
    else fs.writeFileSync(target,typeof content==='string'?content:JSON.stringify(content,null,2)+'\n');
  }
  const env={PATH:process.env.PATH,HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),PILOTDECK_GATEWAY_TOKEN:substitutions.TOKEN,...subst(scenario.env)};
  const observed=[]; requests=[];
  for(const [round,serverStep] of scenario.steps.entries()){
    const server=await launch(command,arguments_,env,side,round);
    try{for(const [index,raw] of serverStep.then.entries()){
      const step=subst(raw); const [method,route]=step.http.split(' ');
      const response=await fetch(server.url+route,{method,headers:{'content-type':'application/json',...step.headers},body:JSON.stringify(step.body),signal:AbortSignal.timeout(10000)});
      observed.push({round,index,status:response.status,body:await response.json()});
    }}finally{await server.stop();}
  }
  const state=JSON.parse(fs.readFileSync(path.join(pilot,'sop/sessions',Buffer.from('s-human').toString('base64url')+'.json')));
  const result={observed,requests,state}; fs.writeFileSync(path.join(out,`${side}.json`),JSON.stringify(result,null,2)+'\n');
  assert.deepEqual(observed.map(x=>x.status),[200,200,200,403,403,409,403,200,200]);
  assert.equal(observed[0].body.status.approval.assigneeUserId,'u');
  assert.equal(observed[1].body.accepted,true);assert.equal(observed[1].body.duplicate,false);
  assert.equal(observed[2].body.sopId,'sop');assert.equal(observed[2].body.sopVersion,'1');
  assert.equal(observed[3].body.code,'SOP_AUTHORITY_ADMISSION_MISMATCH');assert.equal(observed[4].body.code,'SOP_AUTHORITY_SESSION_DENIED');
  assert.equal(observed[5].body.code,'SOP_REVISION_CONFLICT');assert.equal(observed[6].body.code,'APPROVAL_SUBJECT_MISMATCH');
  assert.equal(observed[7].body.duplicate,true);assert.equal(observed[7].body.revision,3);
  assert.equal(state.revision,3);assert.equal(state.wait,undefined);assert.equal(state.resumeRequests['human-r'].authorization.authority.subject.userId,'u');
  assert(requests.some(r=>r.bearer==='Bearer reviewer-test-token'));
  assert(requests.some(r=>r.bearer==='Bearer actor-test-token'));
  assert(requests.some(r=>r.bearer==='Bearer abcdefghijklmnopqrst-full-test-key'));
  const clean=x=>typeof x==='string'?x.replaceAll(mockUrl,'<mock>').replaceAll(root,'<root>'):Array.isArray(x)?x.map(clean):x&&typeof x==='object'?Object.fromEntries(Object.entries(x).filter(([k])=>k!=='updatedAt').map(([k,v])=>[k,clean(v)])):x;
  return clean(result);
}
let failure;
const manifest={schema:1,scenario:scenario.name,scenarioSha256:hash(new URL('../bbx/scenarios/staffdeck-identity-approval.json',import.meta.url)),tsCommit:git(baseline),rustCommit:git(path.resolve(new URL('../..',import.meta.url).pathname)),node:process.version,platform:`${process.platform}-${process.arch}`,rustBinary:rust,rustSha256:rust?hash(rust):null};
try {
  const ts=await run('ts',process.execPath,[path.join(baseline,'dist/src/cli/pilotdeck.js')]);
  if(args.includes('--self'))assert.deepEqual(await run('ts-self',process.execPath,[path.join(baseline,'dist/src/cli/pilotdeck.js')]),ts);
  if(rust)assert.deepEqual(await run('rust',rust,[]),ts);
  manifest.passed=true;
}catch(error){manifest.passed=false;failure=error;manifest.error=String(error.stack??error);}
finally{mock.closeAllConnections();await new Promise(r=>mock.close(r));fs.writeFileSync(path.join(out,'manifest.json'),JSON.stringify(manifest,null,2)+'\n');}
if(failure)throw failure;
console.log(JSON.stringify(manifest,null,2));
