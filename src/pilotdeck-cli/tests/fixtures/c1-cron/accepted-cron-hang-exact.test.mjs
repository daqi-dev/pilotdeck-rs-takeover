import test from 'node:test';import assert from 'node:assert/strict';import fs from 'node:fs';import path from 'node:path';import {fileURLToPath} from 'node:url';import os from 'node:os';import {spawnSync} from 'node:child_process';
import {REGISTRATION as REG} from './cron-hang-registration.mjs';import {rawContract,comparePair,auditReadset} from './accepted-cron-hang-exact.mjs';
const here=path.dirname(fileURLToPath(import.meta.url)),source=path.resolve(here,'../../../../..');
const rawRoot=path.join(source,'_tmp/20261113-e0-resume');
const input=path.join(rawRoot,REG.capturePrefix,'cron/pair');
const projections=path.join(source,'_tmp/20261114-c1-historical-contract-completion/cron-public-first/strict-parity');
const read=(id,side)=>JSON.parse(fs.readFileSync(path.join(projections,`${id}.${side}.json`)));
const raw=(id,side)=>JSON.parse(fs.readFileSync(path.join(input,id,side,'result.json')));
const events=(id,side)=>fs.readFileSync(path.join(input,id,side,'process.jsonl'),'utf8').trim().split('\n').map(JSON.parse);
for(const spec of REG.cases)test(`fixed actual case: ${spec.id}`,()=>{
 rawContract(spec,'ts',raw(spec.id,'ts'),events(spec.id,'ts'));rawContract(spec,'rust',raw(spec.id,'rust'),events(spec.id,'rust'));
 assert.deepEqual(comparePair(spec,read(spec.id,'left'),read(spec.id,'right')),[]);
});
const spec=REG.cases[0];
const pairControls=[
 ['Rust exit differs',(_a,b)=>{b.lifecycle.exitCode=1;}],['Rust forced signal',(_a,b)=>{b.lifecycle.signal='SIGTERM';}],
 ['Rust artificial boundary',(_a,b)=>{b.lifecycle.boundary={phase:'startup'};}],['Rust close missing',(_a,b)=>{b.network.closeReceived=false;}],
 ['Rust close frame missing',(_a,b)=>{b.network.events.splice(6,1);}],['Rust RPC missing',(_a,b)=>{b.network.requests=[];}],
 ['Rust response missing',(_a,b)=>{b.network.events.splice(5,1);}],['Rust response wrong',(_a,b)=>{b.network.events[5].raw=b.network.events[5].raw.replace('tasks','bad');}],
 ['both response missing',(a,b)=>{a.network.events.splice(5,1);b.network.events.splice(5,1);}],
 ['RPC method',(_a,b)=>{b.network.events[4].raw=b.network.events[4].raw.replace('cron_list','cron_stop');}],
 ['RPC ID',(_a,b)=>{b.network.requests[0]=b.network.requests[0].replace('<RPC_0>','<RPC_1>');}],
 ['RPC parameters',(_a,b)=>{b.network.events[4].raw=b.network.events[4].raw.replace('12','13');}],
 ['stdout',(_a,b)=>{b.stdout+='extra';}],['stderr',(_a,b)=>{b.stderr='extra';}],['argv',(_a,b)=>{b.command.argv.push('--extra');}],
 ['extra network event',(_a,b)=>{b.network.events.push({kind:'extra'});}],['unknown lifecycle key',(_a,b)=>{b.lifecycle.newKey=true;}],
 ['TS natural success cannot be this accepted bug',(a,_b)=>{a.lifecycle.exitCode=0;a.lifecycle.signal=null;}],
 ['TS observed before terminal',(a,_b)=>{a.lifecycle.logicalOutputObserved=false;}],['TS startup timeout',(a,_b)=>{a.lifecycle.boundary.phase='startup';}],
 ['TS no live WS',(a,_b)=>{a.lifecycle.boundary.openSockets=0;}],['TS already closed WS',(a,_b)=>{a.lifecycle.boundary.closeReceived=true;}],
 ['TS cleanup failed',(a,_b)=>{a.lifecycle.signals[0].sent=false;}],
];
for(const [name,mutate] of pairControls)test(`pair content rejected: ${name}`,()=>{
 const a=read(spec.id,'left'),b=read(spec.id,'right');mutate(a,b);let bad=false,delta;try{delta=comparePair(spec,a,b);}catch{bad=true;}assert.ok(bad||delta.length);
});
const rawControls=[
 ['Rust premature exit no terminal','rust',r=>{r.terminalAtMs=null;}],['TS no terminal','ts',r=>{r.terminalAtMs=null;}],
 ['TS less than observation window','ts',r=>{r.boundary.atMs=r.terminalAtMs+1499;}],
 ['TS natural exit mistaken cleanup','ts',r=>{r.exitCode=0;r.signal=null;}],['Rust wrong success exit','rust',r=>{r.exitCode=1;}],
 ['Rust signal instead of exit','rust',r=>{r.signal='SIGTERM';}],['TS wrong owned PGID','ts',r=>{r.signals[0].targetProcessGroup++;}],
 ['TS TERM before boundary','ts',r=>{r.signals[0].atMs=r.boundary.atMs-1;}],['TS kill escalation','ts',r=>{r.signals.push({...r.signals[0],signal:'SIGKILL'});}],
 ['TS empty expected result','ts',r=>{r.stdout='';}],['collector error','rust',r=>{r.problem='spawn failed';}],
];
for(const [name,side,mutate] of rawControls)test(`raw owner/terminal rejected: ${name}`,()=>{const r=raw(spec.id,side);mutate(r);assert.throws(()=>rawContract(spec,side,r,events(spec.id,side)));});
test('usage and RPC error rows cannot become successful exit zero',()=>{
 for(const spec of REG.cases.filter(x=>x.exitCode===1)){
  const r=raw(spec.id,'rust'),e=events(spec.id,'rust');r.exitCode=0;assert.throws(()=>rawContract(spec,'rust',r,e));
  const a=read(spec.id,'left'),b=read(spec.id,'right');b.lifecycle.exitCode=0;b.lifecycle.verdict.naturalExitCode=0;assert.throws(()=>comparePair(spec,a,b));
 }
});

test('trusted raw/artifact/judge/build/case authorization is deeply immutable',()=>{
 for(const name of ['raw','artifacts','judge','build','productTrees']) {const alias=REG[name];assert.equal(Object.isFrozen(alias),true);assert.throws(()=>{alias['caller-added']='a'.repeat(64);},TypeError);}
 assert.equal(Object.isFrozen(REG.cases),true);assert.equal(Object.isFrozen(REG.cases[0]),true);assert.throws(()=>{REG.cases[0].exitCode=99;},TypeError);
});

test('public exactreadset rejects missing or extra caller authority',()=>{
 const missing={...REG.raw};delete missing[Object.keys(missing)[0]];assert.throws(()=>auditReadset(missing));assert.throws(()=>auditReadset({...REG.raw,'caller-added':'a'.repeat(64)}));
});
test('actual public CLI refuses raw/source/owner/tree/build changes and caller manifest',t=>{
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'c1-cron-exact-'));t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
 const localSource=path.join(root,'source'),e0=path.join(root,'evidence');const copy=(from,to)=>{fs.mkdirSync(path.dirname(to),{recursive:true});fs.copyFileSync(from,to);};
 for(const name of Object.keys(REG.judge))copy(path.join(source,name),path.join(localSource,name));
 for(const name of ['accepted-cron-hang-exact.mjs','cron-hang-registration.mjs']){const p='src/pilotdeck-cli/tests/fixtures/c1-cron/'+name;copy(path.join(source,p),path.join(localSource,p));}
 for(const name of Object.keys({...REG.raw,...REG.artifacts})){const p=path.join(REG.capturePrefix,name);copy(path.join(rawRoot,p),path.join(e0,p));}
 for(const name of Object.keys(REG.build))copy(path.join(rawRoot,name),path.join(e0,name));
 for(const name of Object.keys(REG.productTrees))fs.mkdirSync(path.join(e0,REG.capturePrefix,name),{recursive:true});
 const entry=path.join(localSource,'src/pilotdeck-cli/tests/fixtures/c1-cron/accepted-cron-hang-exact.mjs');let n=0;const run=extra=>spawnSync(process.execPath,[entry,'--evidence-root',e0,'--out',path.join(root,'output-'+n++),...(extra??[])],{encoding:'utf8',timeout:10000});
 const positive=run();assert.equal(positive.status,0,positive.stderr);assert.match(positive.stdout,/ACCEPTED_FIXED_BASELINE_HANG_REPAIR: exact14/);
 const edit=(file,change)=>{const before=fs.readFileSync(file);try{fs.writeFileSync(file,change(before));const q=run();assert.equal(q.status,2,q.stdout);assert.match(q.stderr,/pinned/);}finally{fs.writeFileSync(file,before);}};
 edit(path.join(e0,REG.capturePrefix,'cron/pair/list-success/rust/stdout.bin'),b=>Buffer.concat([b,Buffer.from('extra')]));
 edit(path.join(e0,REG.capturePrefix,'cron/pair/identity.json'),b=>Buffer.from(b.toString().replace(REG.candidate,'0'.repeat(40))));
 edit(path.join(localSource,'src/pilotdeck-cli/tests/fixtures/c1-cron/compare-common.mjs'),b=>Buffer.concat([b,Buffer.from('\n// changed judge\n')]));
 edit(path.join(e0,'c88650f6-sealed-release/release-record.json'),b=>Buffer.from(b.toString().replace(REG.candidate,'0'.repeat(40))));
 edit(path.join(e0,REG.capturePrefix,'control/cron-ts-a.result.json'),b=>Buffer.from(b.toString().replace('"containerGone": true','"containerGone": false')));
 const token=path.join(e0,REG.capturePrefix,'cron/pair/list-success/rust/home/.pilotdeck/server-token');edit(token,b=>Buffer.concat([b,Buffer.from('extra')]));
 const project=path.join(e0,REG.capturePrefix,'cron/pair/list-success/project/extra');fs.writeFileSync(project,'extra');try{assert.equal(run().status,2,'unexpected disk file refused');}finally{fs.unlinkSync(project);}
 assert.equal(run(['--manifest','caller-says-anything.json']).status,2);
 const restored=run();assert.equal(restored.status,0,restored.stderr);assert.match(restored.stdout,/ACCEPTED_FIXED_BASELINE_HANG_REPAIR: exact14/);
});
