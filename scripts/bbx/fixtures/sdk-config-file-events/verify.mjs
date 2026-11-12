#!/usr/bin/env node
// Independent formal-entry contract. Imports neither product nor shared BBX policy.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
const here=path.dirname(fileURLToPath(import.meta.url));
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const turnCases=[['a-create','s-sdk-event-a','SDK-EVENT-A-CREATE','sdk-a.txt','SDK-A-CREATED\n'],['b-create','s-sdk-event-b','SDK-EVENT-B-CREATE','sdk-b.txt','SDK-B-CREATED\n'],['a-reopen','s-sdk-event-a','SDK-EVENT-A-REOPEN','sdk-reopened.txt','SDK-A-REOPENED\n'],['b-nonmatch','s-sdk-event-b','SDK-EVENT-B-NONMATCH','sdk-unmatched.txt','SDK-B-UNMATCHED\n']];
const exact=(rows,count,name)=>{assert.ok(Array.isArray(rows),name+' is captured');assert.equal(rows.length,count,name+' exact count');return rows;};
function response(step){const rows=exact(step.frames?.filter(f=>f.type==='response'),1,'RPC response');assert.equal(rows[0].ok,true,'RPC succeeds');assert.equal(rows[0].error,undefined);return rows[0].result;}
function validate(a,s,scenarioHash){
 assert.equal(s.name,'sdk-config-file-events');assert.equal(a.scenario,s.name);assert.ok(['ts','rust'].includes(a.side));assert.equal(a.evidence?.side,a.side);assert.equal(a.evidence?.scenario?.sha256,scenarioHash);assert.equal(a.evidence?.schema,1);assert.ok(a.evidence?.runId);
 assert.deepEqual(a.failures,[],'capture failures never count as success');assert.equal(a.startFailure,undefined);assert.equal(a.shutdown?.timeout,undefined);assert.equal(a.shutdown?.code,0,'normal server shutdown');assert.equal(a.shutdown?.signal,null);
 const required=s.assertions.map(row=>row.id);assert.equal(new Set(required).size,required.length);exact(a.evidence.assertions,required.length,'required assertions');
 for(const id of required){const rows=exact(a.evidence.assertions.filter(row=>row.id===id),1,id);assert.equal(rows[0].passed,true,id);assert.ok(!rows[0].skipped,id);}
 exact(a.steps,s.steps.length,'all steps executed');
 const submits=exact(a.steps.filter(row=>row.step?.ws==='submit_turn'),4,'real submit turns');
 const settings=exact(a.steps.filter(row=>row.step?.ws==='update_settings'),3,'actual settings updates');
 const closes=exact(a.steps.filter(row=>row.step?.ws==='close_session'),1,'actual session close');response(closes[0]);
 assert.equal(closes[0].step.params.sessionKey,'s-sdk-event-a');
 assert.ok(a.steps.indexOf(settings[0])<a.steps.indexOf(closes[0]));assert.ok(a.steps.indexOf(closes[0])<a.steps.indexOf(settings[1]));assert.ok(a.steps.indexOf(settings[1])<a.steps.indexOf(submits[2]));assert.ok(a.steps.indexOf(submits[3])<a.steps.indexOf(settings[2]));
 for(const row of settings){const result=response(row);assert.deepEqual(result.changedPaths,['agent.maxOutputTokens'],'settings really changed live snapshot');}
 for(const row of a.steps.filter(row=>row.step?.ws==='set_permission_mode'))response(row);
 const provider=a.provider;assert.equal(provider.default?.length??0,0,'no unrecognized HTTP/model route');
 for(const [index,[label,session,marker,file,content]] of turnCases.entries()){
  const submit=submits[index];assert.equal(submit.step.params.sessionKey,session);assert.equal(submit.step.params.message,marker);
  assert.ok(submit.frames?.some(frame=>frame.type==='event'&&frame.final===true),'turn terminal frame');
  assert.ok(!submit.frames.some(frame=>frame.error||frame.event?.type==='error'),'turn has no error');
  assert.ok(submit.frames.some(frame=>frame.event?.type==='assistant_text_delta'&&frame.event.text.includes(marker+'-DONE')),'actual assistant continuation');
  const initial=exact(provider[label],1,label+' actual model request')[0];const after=exact(provider[label+'-after'],1,label+' actual continuation')[0];
  for(const request of [initial,after]){assert.equal(request.method,'POST');assert.equal(new URL(request.url,'http://fixture').pathname,'/v1/chat/completions');assert.equal(request.body.model,'deterministic');}
  assert.ok(initial.body.messages.some(message=>message.role==='user'&&JSON.stringify(message.content).includes(marker)));
  assert.ok(after.body.messages.some(message=>message.role==='tool'&&message.tool_call_id==='call_'+label.replaceAll('-','_')),'tool actually ran before continuation');
  assert.equal(a.tree['/home/.pilotdeck/'+file]?.text,content,'real tool persisted exact content');
 }
 const fileA=exact(provider['hook-a'],2,'A file+one config callback')[0];const configA=provider['hook-a'][1];const fileB=exact(provider['hook-b'],1,'B own file callback')[0];
 const cwd=fileA.body.cwd;assert.equal(typeof cwd,'string');assert.match(cwd,/\/pdrs-bb(?:x|[1-8])\/root\/home\/\.pilotdeck$/,'isolated formal gateway project');
 for(const [request,owner,event,session] of [[fileA,'a','FileChanged','s-sdk-event-a'],[configA,'a','ConfigChange','s-sdk-event-a'],[fileB,'b','FileChanged','s-sdk-event-b']]){
  assert.equal(request.method,'POST');assert.equal(request.url,`/sdk-hooks/${owner}?retained=1&event=${event}&matcher=0`,'exact selector replaces stale values');assert.equal(request.headers['x-sdk-owner'],owner);assert.equal(request.headers['content-type'],'application/json');
  assert.equal(request.body.sessionId,session);assert.equal(request.body.hookEventName,event);assert.equal(request.body.cwd,cwd);assert.equal(request.body.transcriptPath,'');
 }
 const expectedFile=(session,file)=>({sessionId:session,transcriptPath:'',cwd,filePath:file,absolutePath:path.join(cwd,file),root:cwd,changeType:'created',hookEventName:'FileChanged'});
 assert.deepEqual(fileA.body,expectedFile('s-sdk-event-a','sdk-a.txt'));assert.deepEqual(fileB.body,expectedFile('s-sdk-event-b','sdk-b.txt'));
 assert.deepEqual(configA.body,{sessionId:'s-sdk-event-a',transcriptPath:'',cwd,changedPaths:['agent.maxOutputTokens'],changeClasses:['next-request'],hookEventName:'ConfigChange'});
 // A is closed before settings update #2 and reopened without SDK hooks before #3.
 // Exactly these three requests proves no stale A callback, no B inheritance,
 // and no callback for B's later non-matching actual write.
 const hookRecords=Object.values(provider).flat().filter(request=>request.body?.hookEventName);
 exact(hookRecords,3,'no leaked/duplicate/unrecognized SDK callback');
 const history=exact(a.steps.filter(row=>row.step?.ws==='read_session_messages'),2,'formal history reads');
 for(const [index,row]of history.entries()) {const result=response(row);const markers=index===0?['SDK-EVENT-A-CREATE','SDK-EVENT-A-REOPEN']:['SDK-EVENT-B-CREATE','SDK-EVENT-B-NONMATCH'];for(const marker of markers)assert.ok(JSON.stringify(result).includes(marker),'same session retains real history '+marker);}
 const logs=Object.entries(a.tree).filter(([name,value])=>name.endsWith('.jsonl')&&Array.isArray(value.jsonl)).flatMap(([,value])=>value.jsonl);
 const accepted=logs.filter(entry=>entry.type==='accepted_input');
 for(const[, ,marker]of turnCases)exact(accepted.filter(entry=>JSON.stringify(entry.messages).includes(marker)),1,'durable accepted input '+marker);
 return {passed:true,side:a.side,turns:4,actualToolFiles:4,fileCallbacks:2,configCallbacks:1,settingsUpdates:3,closedSession:'s-sdk-event-a',sessions:2};
}
const args=process.argv.slice(2);const self=args[0]==='--self-test';if(self)args.shift();assert.ok(args[0],'usage: verify.mjs [--self-test] ARTIFACT [SCENARIO]');
const scenarioFile=args[1]??path.resolve(here,'../../scenarios/sdk-config-file-events.json');const scenarioBytes=fs.readFileSync(scenarioFile),artifactBytes=fs.readFileSync(args[0]);const scenario=JSON.parse(scenarioBytes),artifact=JSON.parse(artifactBytes);const scenarioHash=hash(scenarioBytes);const result=validate(artifact,scenario,scenarioHash);
if(self){
 const cases=[['missing-file-callback',a=>a.provider['hook-a'].shift()],['wrong-created-type',a=>a.provider['hook-b'][0].body.changeType='updated'],['wrong-exact-matcher',a=>a.provider['hook-a'][0].body.filePath='sdk-other.txt'],['stale-selector',a=>a.provider['hook-a'][0].url='/sdk-hooks/a?retained=1&event=stale&matcher=old'],['wrong-owner-header',a=>a.provider['hook-b'][0].headers['x-sdk-owner']='a'],['cross-session-leak',a=>a.provider['hook-b'][0].body.sessionId='s-sdk-event-a'],['closed-session-callback',a=>a.provider['hook-a'].push(structuredClone(a.provider['hook-a'][1]))],['missing-config-dispatch',a=>a.provider['hook-a'].pop()],['duplicate-tool-effect',a=>a.provider['a-create'].push(structuredClone(a.provider['a-create'][0]))],['wrong-real-file',a=>a.tree['/home/.pilotdeck/sdk-reopened.txt'].text='wrong'],['hidden-step-failure',a=>a.failures.push({kind:'timeout',expected:false})],['missing-formal-step',a=>a.steps.pop()],['unexecuted-assertion',a=>a.evidence.assertions[0].passed=false]];
 for(const [name,mutate]of cases){const changed=structuredClone(artifact);mutate(changed);assert.throws(()=>validate(changed,scenario,scenarioHash),undefined,name);}
 result.negativeControls=cases.map(([name])=>name);
}
assert.equal(hash(fs.readFileSync(args[0])),hash(artifactBytes),'input artifact unchanged');console.log(JSON.stringify({...result,scenarioSha256:scenarioHash,artifactSha256:hash(artifactBytes),verifierSha256:hash(fs.readFileSync(fileURLToPath(import.meta.url)))}));
