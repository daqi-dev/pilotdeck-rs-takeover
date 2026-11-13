import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {acceptedPositiveContract,recoveredDialog} from './accepted-positive-policy.mjs';
const config={scene:'baseline-bug-rename-reload',id:'rename-then-reload-bricks-session'};
const minimal=()=>({scenario:config.scene,side:'rust',failures:[],steps:[],tree:{},provider:{}});
test('unknown accepted scene cannot obtain a receipt',()=>assert.throws(()=>acceptedPositiveContract(minimal(),{scene:'unknown',id:'unknown'})));
test('wrong accepted ID rejected',()=>assert.throws(()=>acceptedPositiveContract(minimal(),{...config,id:'rename-record-chain'})));
test('unknown config fields rejected',()=>assert.throws(()=>acceptedPositiveContract(minimal(),{...config,allowMissing:true})));
test('TS does not obtain Rust benefit receipt',()=>assert.throws(()=>acceptedPositiveContract({...minimal(),side:'ts'},config)));
test('capture failures never turn into positive proof',()=>assert.throws(()=>acceptedPositiveContract({...minimal(),failures:[{kind:'timeout'}]},config)));
test('absence of all causal inputs cannot pass',()=>assert.throws(()=>acceptedPositiveContract(minimal(),config)));
test('HTTP remains ordinary scalar assertions, not implicit new policy',()=>assert.throws(()=>acceptedPositiveContract({scenario:'server-local-token-origin',side:'rust',failures:[]},{scene:'server-local-token-origin',id:'local-token-dns-rebinding'})));
test('watcher without independent rewrite never obtains a receipt',()=>assert.throws(()=>acceptedPositiveContract({scenario:'settings-update',side:'rust',failures:[]},{scene:'settings-update',id:'config-watch-survives-rewrite'})));
// Explicit private raw-control mode; absent env means only portable rejection
// tests above run. No synthetic object is advertised as an actual product pass.
const root=process.env.BBX_ACCEPTED_POSITIVE_RAW_ROOT;
if(root){
 const definitions=[['baseline-bug-rename-reload','rename-then-reload-bricks-session',1],['hook-cancelled','aborted-hook-does-not-run-tool',1],['robust-restart-registry-lock-leftover','registry-dead-owner-lock-blocks-startup',1],['project-keys','rename-record-chain',2],['read-messages-after-rename','rename-record-chain',2],['robust-restart-crash-leftovers','dialog-recovery-keeps-history',2]];
 const loaded=new Map(definitions.map(([scene,id,slot])=>{const file=path.join(root,`triplet-slot-${slot}/run/raw/${scene}.rust.json`),bytes=fs.readFileSync(file);return [scene,{artifact:JSON.parse(bytes),config:{scene,id},file,sha256:createHash('sha256').update(bytes).digest('hex')}];}));
 const copy=name=>structuredClone(loaded.get(name).artifact);
 const verify=(name,a)=>acceptedPositiveContract(a??loaded.get(name).artifact,loaded.get(name).config);
 const transcript=(a,session)=>Object.values(a.tree).find(v=>v.jsonl?.some(r=>r.sessionId===session))?.jsonl;
 for(const [scene]of definitions.slice(0,5))test('actual raw positive '+scene,()=>{assert.equal(verify(scene).passed,true);});
 const mutations=[
 ['hook-cancelled','abort response explicitly unsuccessful',a=>{a.steps[1].answers[0].frames[0].ok=false;}],
 ['baseline-bug-rename-reload','postreload submit foreign session',a=>{a.steps[4].step.params.sessionKey='foreign-session';}],
 ['baseline-bug-rename-reload','foreign durable turn IDs',a=>{for(const r of transcript(a,'s-meta'))r.turnId='foreign-run';}],
 ['hook-cancelled','negative final sequence',a=>{a.steps[1].frames.find(f=>f.final).seq=-1;}],
 ['hook-cancelled','foreign durable accepted input',a=>{for(const r of transcript(a,'s-cancel'))if(r.type==='accepted_input')r.messages=[{role:'user',content:[{type:'text',text:'FOREIGN INPUT'}]}];}],
 ['hook-cancelled','requested tool after cancelled hook',a=>{a.steps[1].frames.find(f=>f.event?.type==='tool_call_started').seq=999;}],
 ['robust-restart-registry-lock-leftover','foreign durable model request',a=>{transcript(a,'s-bbx-1').find(r=>r.type==='model_request').turnId='foreign-run';}],
 ['project-keys','foreign assistant turn',a=>{transcript(a,'s-alpha').find(r=>r.type==='assistant_message').turnId='foreign-run';}],
 ['read-messages-after-rename','foreign durable result turn',a=>{transcript(a,'s-cold').find(r=>r.type==='turn_result').result.turnId='foreign-run';}],
 ['baseline-bug-rename-reload','broken rename sequence',a=>{transcript(a,'s-meta').find(r=>r.turnId==='sdk-rename').sequence=1;}],
 ['baseline-bug-rename-reload','lost previous provider history',a=>{a.provider.main[1].body.messages=a.provider.main[1].body.messages.filter(m=>!(m.role==='user'&&m.content==='hello'));}],
 ['baseline-bug-rename-reload','failed real postreload turn',a=>{a.steps[4].frames.find(f=>f.final).event.finishReason='error';}],
 ['hook-cancelled','tool sideeffect after cancel',a=>{a.tree['/home/.pilotdeck/ran.txt']={text:'ran'};}],
 ['hook-cancelled','missing matched hook start',a=>{a.steps[1].frames=a.steps[1].frames.filter(f=>f.event?.event!=='hook_execution_started');}],
 ['hook-cancelled','cancelled hook falsely reported success',a=>{a.steps[1].frames.find(f=>f.event?.event==='hook_execution_completed').event.detail.outcome='success';}],
 ['hook-cancelled','wrong run hook completion',a=>{a.steps[1].frames.find(f=>f.event?.event==='hook_execution_completed').event.runId='foreign';}],
 ['robust-restart-registry-lock-leftover','old record dropped',a=>{a.tree['/home/.pilotdeck/runs/registry.json'].json.records.shift();}],
 ['robust-restart-registry-lock-leftover','false completed query',a=>{a.steps[2].frames[0].result.state='running';}],
 ['robust-restart-registry-lock-leftover','stale lock remains',a=>{a.tree['/home/.pilotdeck/runs/registry.json.lock']={text:'{}'};}],
 ['project-keys','wrong project renamed',a=>{transcript(a,'s-beta').find(r=>r.type==='session_metadata').metadata.title='Renamed';}],
 ['project-keys','broken parent chain',a=>{const r=transcript(a,'s-alpha');r.at(-1).parentEntryId=null;}],
 ['project-keys','wrong project mutation accepted',a=>{a.steps[30].frames[0]={type:'response',ok:true,result:{updated:true}};}],
 ['read-messages-after-rename','cold metadata lost',a=>{const r=transcript(a,'s-cold');for(const row of r)if(row.type==='session_metadata'&&row.metadata?.title==='Never talked')row.metadata.title='lost';}],
 ['read-messages-after-rename','warm input lost',a=>{a.steps[12].frames[0].result.messages=a.steps[12].frames[0].result.messages.filter(m=>m.text!=='hello');}],
 ['read-messages-after-rename','retag parent reset',a=>{transcript(a,'s-meta').find(r=>r.turnId==='sdk-tag').parentEntryId=null;}],
 ];
 for(const [scene,label,mutate]of mutations)test('actual raw mutation rejects '+label,()=>{const a=copy(scene);mutate(a);assert.throws(()=>verify(scene,a));});
 test('actual crash raw with unloaded seed is rejected, never a positive',()=>assert.throws(()=>verify('robust-restart-crash-leftovers'),/first question/));
 test('claiming delivered without loaded history still rejected',()=>{const a=copy('robust-restart-crash-leftovers');a.steps[7].frames[0].result.delivered=true;assert.throws(()=>verify('robust-restart-crash-leftovers',a),/first question/);});
 test('all actual raw source bytes preserved',()=>{for(const x of loaded.values())assert.equal(createHash('sha256').update(fs.readFileSync(x.file)).digest('hex'),x.sha256);});
}

// Public integration uses the existing assertion/recording interface. This
// tests declarations and execution; no old capture is rewritten in place.
const {assertionResults,validateArtifact}=await import('./evidence.mjs');
const relationScenes=['baseline-bug-rename-reload','hook-cancelled','project-keys','read-messages-after-rename','robust-restart-registry-lock-leftover','robust-restart-crash-leftovers'];
const spec=name=>JSON.parse(fs.readFileSync(new URL(`./scenarios/${name}.json`,import.meta.url)));
test('six relation declarations bind real contract IDs and Rust side',()=>{
 for(const name of relationScenes){const s=spec(name),a=s.assertions;assert.equal(a.length,1);assert.deepEqual(s.accepted[0].assertions,[a[0].id]);assert.deepEqual(a[0].sides,['rust']);assert.equal(a[0].path,'/steps');assert.deepEqual(a[0].acceptedPositive,{scene:name,id:s.accepted[0].id});}
});
test('HTTP declarations use exact scalar route outcomes and all linked IDs',()=>{const s=spec('server-local-token-origin');assert.equal(s.assertions.length,14);assert.deepEqual(s.accepted[0].assertions,s.assertions.map(a=>a.id));assert.ok(s.assertions.every(a=>a.sides.length===1&&a.sides[0]==='rust'&&'equals'in a&&!('acceptedPositive'in a)));});
test('public branch cannot pass a marker just because steps exist',()=>{const assertions=[{id:'p',path:'/steps',exists:true,acceptedPositive:config}];assert.equal(assertionResults(assertions,minimal(),'rust')[0].passed,false);});
test('public branch skips intended Rust benefit on TS explicitly',()=>{assert.deepEqual(assertionResults(spec(config.scene).assertions,{side:'ts'},'ts'),[{id:spec(config.scene).assertions[0].id,skipped:true}]);});
if(root){
 const names=[...relationScenes.filter(n=>n!=='robust-restart-crash-leftovers'),'server-local-token-origin'];
 const slot=name=>['baseline-bug-rename-reload','hook-cancelled','robust-restart-registry-lock-leftover','server-local-token-origin'].includes(name)?1:2;
 const read=name=>JSON.parse(fs.readFileSync(path.join(root,`triplet-slot-${slot(name)}/run/raw/${name}.rust.json`)));
 for(const name of names)test('actual raw executes public assertion branch '+name,()=>{const a=read(name),rows=assertionResults(spec(name).assertions,a,'rust');assert.ok(rows.length>0&&rows.every(r=>r.passed===true&&!r.skipped));});
 test('old actual raw is not falsely claimed to have executed new assertions',()=>{const a=read(config.scene),s=spec(config.scene);assert.deepEqual(a.evidence.assertions,[]);assert.ok(validateArtifact(a,s,{runId:a.evidence.runId,side:'rust',scenario:a.evidence.scenario,inputs:a.evidence.inputs}).includes('required assertions were not executed'));});
 test('recomputed genuine assertion receipts satisfy existing positive guard in apparatus copy',()=>{const a=read(config.scene),s=spec(config.scene);a.accepted=structuredClone(s.accepted);a.evidence.assertions=assertionResults(s.assertions,a,'rust');assert.deepEqual(validateArtifact(a,s,{runId:a.evidence.runId,side:'rust',scenario:a.evidence.scenario,inputs:a.evidence.inputs}),[]);});
 test('forged passed marker does not hide actual foreign input',()=>{const a=read('hook-cancelled'),s=spec('hook-cancelled');a.accepted=structuredClone(s.accepted);a.evidence.assertions=[{id:s.assertions[0].id,passed:true}];const t=Object.values(a.tree).find(v=>v.jsonl?.some(r=>r.sessionId==='s-cancel')).jsonl;t.find(r=>r.type==='accepted_input').messages=[{role:'user',content:'FOREIGN'}];const errors=validateArtifact(a,s,{runId:a.evidence.runId,side:'rust',scenario:a.evidence.scenario,inputs:a.evidence.inputs});assert.ok(errors.includes('required assertion failed'));assert.ok(errors.includes('required assertions were not executed'));assert.ok(errors.some(e=>e.includes('lacks executed positive assertions')));});
 test('HTTP scalar rejects token leak on disallowed origin',()=>{const a=read('server-local-token-origin');a.steps[2].http.body={token:'bbx-token'};assert.ok(assertionResults(spec('server-local-token-origin').assertions,a,'rust').some(r=>!r.passed));});
 test('public crash strong oracle rejects unchanged actual unloaded seed',()=>{const a=read('robust-restart-crash-leftovers');assert.ok(assertionResults(spec('robust-restart-crash-leftovers').assertions,a,'rust').every(r=>r.passed===false));});
}

test('CLI shutdown delegate rejects false marker and missing real contract',()=>{for(const a of [{id:'p',path:'/steps',cliShutdownCompleted:false},{id:'p',path:'/wrong',cliShutdownCompleted:true},{id:'p',path:'/steps',cliShutdownCompleted:true}])assert.equal(assertionResults([a],minimal(),'rust')[0].passed,false);});

// Optional exact actual capture: call the inner relation oracle without altering
// its recorded failures or manufacturing a new capture/assertion receipt.
const crashRaw=process.env.BBX_CRASH_POSITIVE_RAW;
if(crashRaw){
 const bytes=fs.readFileSync(crashRaw),original=JSON.parse(bytes),digest=createHash('sha256').update(bytes).digest('hex');
 const dialogs=(a,i)=>a.steps[i].frames.find(f=>f.type==='response').result.dialogs;
 test('actual loaded crash seed satisfies inner relation without clearing failures',()=>{assert.equal(recoveredDialog(original).length,3);assert.ok(original.failures.length>0);});
 const controls=[
 ['missing nested dialog',a=>{dialogs(a,5).splice(0);}],
 ['missing nested request',a=>{delete dialogs(a,5)[0].request;}],
 ['foreign nested request ID',a=>{dialogs(a,5)[0].request.requestId='foreign';}],
 ['foreign nested tool ID',a=>{dialogs(a,5)[0].request.toolCallId='foreign';}],
 ['wrong wrapper',a=>{dialogs(a,5)[0].type='user_dialog_request';}],
 ['wrong restart reason',a=>{dialogs(a,5)[0].reason='foreign';}],
 ['wrong continuation',a=>{dialogs(a,5)[0].recovery='foreign';}],
 ['duplicate exact nested request',a=>{dialogs(a,5).push(structuredClone(dialogs(a,5)[0]));}],
 ['answered request remains immediately',a=>{dialogs(a,8).push(structuredClone(dialogs(a,5)[0]));}],
 ['answered request remains after turn',a=>{dialogs(a,11).push(structuredClone(dialogs(a,5)[0]));}],
 ['old provider history missing',a=>{a.provider['turn-3'][0].body.messages=a.provider['turn-3'][0].body.messages.filter(m=>m.content!=='first question');}],
 ];
 for(const [label,mutate]of controls)test('actual crash nested control rejects '+label,()=>{const a=structuredClone(original);mutate(a);assert.throws(()=>recoveredDialog(a));});
 test('actual crash capture failure and bytes remain unchanged',()=>{assert.equal(createHash('sha256').update(fs.readFileSync(crashRaw)).digest('hex'),digest);assert.throws(()=>acceptedPositiveContract(original,{scene:'robust-restart-crash-leftovers',id:'dialog-recovery-keeps-history'}),/no capture failures/);});
}
