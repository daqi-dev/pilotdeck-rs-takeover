import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {fileURLToPath,pathToFileURL} from 'node:url';
import {createHash} from 'node:crypto';
import {brotliDecompressSync,brotliCompressSync} from 'node:zlib';
import {spawnSync} from 'node:child_process';
import {configWatchExactAcceptedPaths,verifyConfigWatchExactSemantics,validateConfigWatchExactArtifact} from './config-watch-exact-policy.mjs';
import {validateArtifact} from './evidence.mjs';
const here=path.dirname(fileURLToPath(import.meta.url)),fixture=path.join(here,'fixtures/config-watch-exact'),temp=fs.mkdtempSync(path.join(os.tmpdir(),'bbx-config-exact-'));
const names=['ts-1','ts-2','rust'],bytes=names.map(n=>brotliDecompressSync(fs.readFileSync(path.join(fixture,n+'.json.br')))),files=bytes.map((b,i)=>{const p=path.join(temp,names[i]+'.json');fs.writeFileSync(p,b);return p;});
const original=bytes.map(b=>JSON.parse(b)),clone=()=>structuredClone(original),copy=v=>structuredClone(v),proof=a=>a.evidence.configWatchInput[0],snapshot=(a,i)=>a.steps[i].frames[0].result;
const pair=(left=0)=>[files[left],files[2]],rawPair=(raw=clone(),left=0)=>[raw[left],raw[2]];
const paths=['$.steps[25].frames[0].result.version','$.steps[27].externalConfigRewrite.observationExpired','$.steps[27].externalConfigRewrite.observed'];
process.on('exit',()=>fs.rmSync(temp,{recursive:true,force:true}));test.after(()=>fs.rmSync(temp,{recursive:true,force:true}));
const sha=b=>createHash('sha256').update(b).digest('hex');
test('positive original semantic contract and public evidence assertions',()=>{assert.equal(verifyConfigWatchExactSemantics(clone()),true);const closure=JSON.parse(brotliDecompressSync(fs.readFileSync(path.join(fixture,'closure.json.br')))),scene=JSON.parse(closure.sources.find(s=>s.path==='scripts/bbx/scenarios/settings-update.json').text);for(const a of clone()){assert.equal(validateConfigWatchExactArtifact(a),true);assert.deepEqual(validateArtifact(a,scene,{runId:a.evidence.runId,side:a.side,scenario:a.evidence.scenario,inputs:a.evidence.inputs}),[]);}});
test('positive exact entry reads original bytes and emits precisely three licensed leaves',()=>assert.deepEqual([...configWatchExactAcceptedPaths(...pair(),{expectedRaw:rawPair()}).keys()],paths));
const mutants=[
 ['wrong write value',a=>{proof(a[2]).afterFile.text=proof(a[2]).afterFile.text.replace('maxOutputTokens: 2048','maxOutputTokens: 1024');},/file SHA/],
 ['missing write',a=>{proof(a[2]).written=false;},/false !== true/],
 ['wrong owned file',a=>{proof(a[2]).file='/tmp/other.yaml';},/other.yaml/],
 ['different inode',a=>{proof(a[2]).afterFile.ino='9999';},/inplace owned ino/],
 ['wrong next model request',a=>{a[2].provider['watch-rewrite'][0].body.max_tokens=4096;},/4096 !== 2048/],
 ['duplicate next model request',a=>{a[2].provider['watch-rewrite'].push(structuredClone(a[2].provider['watch-rewrite'][0]));},/following provider request/],
 ['wrong snapshot source',a=>{proof(a[2]).rpc[1].frames[0].result.sources[1].contentHash='0'.repeat(64);},/./],
 ['wrong snapshot version',a=>{proof(a[2]).rpc[1].frames[0].result.version=11;},/./],
 ['wrong notification kind',a=>{proof(a[2]).notification.frame.name='session_changed';},/./],
 ['wrong notification scope',a=>{proof(a[2]).notification.frame.payload.changedPaths=['agent.model'];},/./],
 ['extra notification scope',a=>{proof(a[2]).notification.frame.payload.changedPaths.push('agent.model');},/notification kind and scope/],
 ['wrong notification change class',a=>{proof(a[2]).notification.frame.payload.changeClasses=['immediate'];},/notification kind and scope/],
 ['missing receipt',a=>{delete a[2].evidence.configWatchInput;},/one rewrite receipt/],
 ['multiple receipts',a=>{a[2].evidence.configWatchInput.push(structuredClone(proof(a[2])));},/one rewrite receipt/],
 ['multiple observation notifications',a=>{proof(a[2]).observationFrames.unshift(structuredClone(proof(a[2]).notification.frame));proof(a[2]).observedFrameCount++;},/notification before snapshot response/],
 ['wrong receipt step',a=>{proof(a[2]).stepIndex=25;},/25 !== 27/],
 ['wrong scene',a=>{a[2].scenario='sdk-config-file-events';},/fixed scene/],
 ['wrong receipt scene',a=>{proof(a[2]).scene='sdk-config-file-events';},/settings-update/],
 ['wrong action step',a=>{a[2].steps[27].step.externalConfigRewrite.toMaxOutputTokens=1024;},/exact action step/],
 ['TS incomplete window',a=>{proof(a[0]).durationMs=29999;},/full exact TS window/],
 ['TS unexpected observation',a=>{proof(a[0]).observationFrames.push({type:'notification',name:'config_changed',payload:{changedPaths:['agent.maxOutputTokens'],changeClasses:['next-request']}});proof(a[0]).observedFrameCount++;},/./],
 ['TS expired flag removed',a=>{delete a[0].steps[27].externalConfigRewrite.observationExpired;},/./],
 ['Rust missing notification',a=>{proof(a[2]).notification=null;},/./],
 ['Rust exceeds window',a=>{proof(a[2]).durationMs=30001;},/./],
 ['old notification cursor',a=>{proof(a[2]).notification.index=52;},/./],
 ['wrong snapshot notification attribution',a=>{proof(a[2]).rpc[1].afterNotificationIndex=54;},/./],
 ['observation order changed',a=>{proof(a[2]).observationFrames.reverse();},/notification before snapshot response/],
 ['other object difference',a=>{a[2].extraObject={observed:true};},/all remaining ordered raw content/],
 ['different unrelated step',a=>{a[2].steps[25].extra=true;},/all remaining ordered raw content/],
 ['public request id changed',a=>{a[2].steps[28].frames[0].id='q26';},/unchanged public q25/],
 ['usage changed',a=>{a[2].steps[28].frames.find(f=>f.event?.type==='turn_completed').event.usage={changed:1};},/all remaining ordered raw content/],
 ['extra failure',a=>{a[2].failures.push({kind:'fixture'});},/./],
 ['durable row missing',a=>{Object.entries(a[2].tree).find(([p])=>p.endsWith('/chats/s-watch-rewrite.jsonl'))[1].jsonl.pop();},/./],
 ['top notification full content changed',a=>{a[2].notifications[0].payload.unexpected=true;},/all remaining ordered raw content/],
 ['top notification order changed',a=>{a[2].notifications.reverse();},/all remaining ordered raw content/],
 ['config source wrong kind',a=>{proof(a[2]).rpc[0].frames[0].result.sources[1].kind='projectSettings';},/all remaining ordered raw content/],
 ['unexpected valid timing',a=>{proof(a[2]).durationMs+=1;},/all remaining ordered raw content/],
];
const versionMutants=[
 ['TS1 number 9 to 8',a=>snapshot(a[0],25).version=8],['TS2 number 9 to 10',a=>snapshot(a[1],25).version=10],['Rust number 10 to 9',a=>snapshot(a[2],25).version=9],['Rust number 10 to 11',a=>snapshot(a[2],25).version=11],
 ['TS1 number 9 to string 9',a=>snapshot(a[0],25).version='9'],['TS2 number 9 to string 9',a=>snapshot(a[1],25).version='9'],['Rust number 10 to string 10',a=>snapshot(a[2],25).version='10'],['Rust number 10 to null',a=>snapshot(a[2],25).version=null],['Rust version absent',a=>delete snapshot(a[2],25).version],['fractional version',a=>snapshot(a[2],25).version=10.5],
 ['other step0 version',a=>snapshot(a[2],0).version=2],['other step9 version',a=>snapshot(a[2],9).version=4],['other step24 injected version',a=>snapshot(a[2],24).version=10],['wrong step25 public id',a=>a[2].steps[25].frames[0].id='q24'],['step25 sourceHash changed',a=>snapshot(a[2],25).sources[1].contentHash='0'.repeat(64)],['step25 config changed',a=>snapshot(a[2],25).config.agent.subagents.timeoutMs=60001],['step25 contentHash changed',a=>snapshot(a[2],25).contentHash='0'.repeat(64)],['step25 duplicate source',a=>snapshot(a[2],25).sources.push(copy(snapshot(a[2],25).sources[1]))],['step25 snapshot moved to other step',a=>{a[2].steps[24].frames[0].result=copy(snapshot(a[2],25));}],['both sides equalized version',a=>{for(const r of a)snapshot(r,25).version=10;}],
];
for(const [label,mutate,expected] of mutants)test('semantic37: '+label,()=>{const a=clone();mutate(a);assert.throws(()=>verifyConfigWatchExactSemantics(a),expected);});
for(const [label,mutate] of mutants)test('identity37: '+label,()=>{const a=clone();mutate(a);const changed=a.flatMap((v,i)=>JSON.stringify(v)!==JSON.stringify(original[i])?[i]:[]);assert.ok(changed.length);const p=files.slice();for(const i of changed){p[i]=path.join(temp,'mutant-'+i+'.json');fs.writeFileSync(p[i],JSON.stringify(a[i]));}const left=changed.includes(1)?1:0;assert.equal(configWatchExactAcceptedPaths(p[left],p[2],{expectedRaw:rawPair(a,left)}).size,0);});
test('identity38: reordered original keys',()=>{const a=clone();a[0]=Object.fromEntries(Object.entries(a[0]).reverse());const p=path.join(temp,'reordered.json');fs.writeFileSync(p,JSON.stringify(a[0]));assert.equal(configWatchExactAcceptedPaths(p,files[2],{expectedRaw:rawPair(a)}).size,0);});
test('identity39: TS2 substituted into original TS1 pair',()=>assert.equal(configWatchExactAcceptedPaths(...pair(1),{expectedRaw:rawPair(clone(),1)}).size,0));
for(const [label,mutate] of versionMutants)test('version20: '+label,()=>{const a=clone();mutate(a);assert.throws(()=>verifyConfigWatchExactSemantics(a),/step25 exact version|step25 version type|all remaining ordered raw content/);});
function isolated(){const dir=fs.mkdtempSync(path.join(temp,'module-'));for(const f of ['config-watch-exact-policy.mjs','normalization.mjs','sdk-agent-policy.mjs','fixtures/config-watch-input.mjs']){fs.mkdirSync(path.dirname(path.join(dir,f)),{recursive:true});fs.copyFileSync(path.join(here,f),path.join(dir,f));}fs.cpSync(fixture,path.join(dir,'fixtures/config-watch-exact'),{recursive:true});return dir;}
test('closure77: changed helper plus editable manifest cannot self-authorize',async()=>{const dir=isolated();fs.appendFileSync(path.join(dir,'fixtures/config-watch-input.mjs'),'\n// substituted helper\n');fs.writeFileSync(path.join(dir,'fixtures/config-watch-exact/manifest.json'),JSON.stringify({approved:true,sha256:'caller-supplied'}));const api=await import(pathToFileURL(path.join(dir,'config-watch-exact-policy.mjs')));assert.throws(()=>api.configWatchExactAcceptedPaths(...pair(),{expectedRaw:rawPair()}),/unchanged runtime helper/);});
for(const [label,change] of [['compressed bytes',b=>{const x=Buffer.from(b);x[0]^=1;return x;}],['self-consistent closure replacement',b=>{const c=JSON.parse(brotliDecompressSync(b));c.sources[0].sha256='0'.repeat(64);return brotliCompressSync(Buffer.from(JSON.stringify(c)));}]])test('extra sealed closure: '+label,async()=>{const dir=isolated(),f=path.join(dir,'fixtures/config-watch-exact/closure.json.br');fs.writeFileSync(f,change(fs.readFileSync(f)));const api=await import(pathToFileURL(path.join(dir,'config-watch-exact-policy.mjs')));assert.throws(()=>api.configWatchExactAcceptedPaths(...pair(),{expectedRaw:rawPair()}),/sealed compressed closure/);});
test('extra sealed TS2 witness cannot be replaced with caller manifest',async()=>{const dir=isolated(),f=path.join(dir,'fixtures/config-watch-exact/ts-2.json.br');const raw=JSON.parse(brotliDecompressSync(fs.readFileSync(f)));raw.steps[25].frames[0].result.version=10;fs.writeFileSync(f,brotliCompressSync(Buffer.from(JSON.stringify(raw))));fs.writeFileSync(path.join(dir,'fixtures/config-watch-exact/manifest.json'),JSON.stringify({approved:true,sha256:sha(fs.readFileSync(f))}));const api=await import(pathToFileURL(path.join(dir,'config-watch-exact-policy.mjs')));assert.throws(()=>api.configWatchExactAcceptedPaths(...pair(),{expectedRaw:rawPair()}),/sealed compressed ts-2/);});
test('extra comparator raw cannot differ from same sealed files',()=>{const a=rawPair();a[1].steps[25].frames[0].result.version=11;assert.throws(()=>configWatchExactAcceptedPaths(...pair(),{expectedRaw:a}),/trusted comparator raw/);});
test('extra comparator raw key order cannot differ',()=>{const a=rawPair();a[1]=Object.fromEntries(Object.entries(a[1]).reverse());assert.throws(()=>configWatchExactAcceptedPaths(...pair(),{expectedRaw:a}),/trusted comparator raw/);});
test('extra self and reversed pair obtain no permission',()=>{assert.equal(configWatchExactAcceptedPaths(files[0],files[1],{self:true}).size,0);assert.equal(configWatchExactAcceptedPaths(files[2],files[0]).size,0);});
test('extra public evidence rejects changed fixed artifact even with caller identity',()=>{const a=clone()[2];a.steps[25].frames[0].result.version=11;assert.throws(()=>validateConfigWatchExactArtifact(a),/fixed settings artifact content/);});
function cli(args,env={}){return spawnSync(process.execPath,[path.join(here,'diff.mjs'),...args,'--max','1000'],{encoding:'utf8',maxBuffer:8*1024*1024,timeout:30000,env:{...process.env,HOME:path.join(temp,'home'),PILOT_HOME:path.join(temp,'pilot'),...env}});}
test('actual public CLI old raw self SAME without watch license',()=>{const r=cli([files[0],files[1],'--self']);assert.equal(r.signal,null);assert.equal(r.status,0,r.stderr);assert.equal(r.stdout.trim().split('\n').at(-1),'SAME');assert.ok(!r.stdout.includes('fixedConfigWatchExactApproved'));});
test('actual public CLI old raw pair SAME prints all exact three leaves',()=>{const r=cli(pair());assert.equal(r.signal,null);assert.equal(r.status,0,r.stderr+r.stdout);assert.equal(r.stdout.trim().split('\n').at(-1),'SAME');for(const p of paths)assert.ok(r.stdout.includes('COMPATIBILITY-VALUE fixedConfigWatchExactApproved '+p));assert.ok(r.stdout.includes('略过 0 处'));});
test('actual public CLI altered version is denied, old strict reports preserved',()=>{const a=clone()[2];a.steps[25].frames[0].result.version=11;const p=path.join(temp,'altered-version.json');fs.writeFileSync(p,JSON.stringify(a));const r=cli([files[0],p]);assert.equal(r.signal,null);assert.equal(r.status,1,r.stderr);assert.equal(r.stdout.trim().split('\n').at(-1),'DIFF 2');assert.ok(!r.stdout.includes('fixedConfigWatchExactApproved'));assert.ok(fs.readFileSync(path.join(fixture,'original-public644.diff.txt'),'utf8').endsWith('DIFF 2\n'));assert.deepEqual(JSON.parse(fs.readFileSync(path.join(fixture,'original-normalized-diff3.json'))).map(d=>d.at),paths);});
test('actual public runtime has no Git or private _tmp dependency',()=>{const r=cli(pair(),{PATH:path.join(temp,'no-tools'),BBX_BASELINE:path.join(temp,'absent-baseline')});assert.equal(r.signal,null);assert.equal(r.status,0,r.stderr);assert.equal(r.stdout.trim().split('\n').at(-1),'SAME');});
