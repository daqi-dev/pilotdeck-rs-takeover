#!/usr/bin/env node
// Strict independent combination gate. Does not import product or mutate artifacts.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
const fixture=path.dirname(fileURLToPath(import.meta.url));
const defaultScenario=path.resolve(fixture,'../../scenarios/backend-core-channel-recovery.json');
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const text=content=>typeof content==='string'?content:Array.isArray(content)?content.filter(p=>p?.type==='text').map(p=>p.text??'').join(''):'';
const exact=(list,name,count)=>{assert.equal(list?.length??0,count,name+' count');return list;};
function validate(artifact,scenario,scenarioHash){
 assert.equal(artifact.scenario,scenario.name);assert.equal(scenario.name,'backend-core-channel-recovery');
 assert.equal(artifact.evidence?.scenario?.sha256,scenarioHash,'captured scenario identity');
 assert.ok(['ts','rust'].includes(artifact.side));assert.equal(artifact.evidence.side,artifact.side);
 assert.deepEqual(artifact.failures,[],'no hidden capture failures');
 const required=scenario.assertions.map(a=>a.id);const observed=artifact.evidence.assertions;
 assert.equal(new Set(required).size,required.length);assert.equal(observed?.length,required.length);
 for(const id of required){const rows=observed.filter(a=>a.id===id);assert.equal(rows.length,1,id);assert.equal(rows[0].passed,true,id);assert.ok(!rows[0].skipped,id);}
 exact(artifact.steps,'phases',2);
 const phaseResults=[];
 for(const [index,phase]of artifact.steps.entries()){
  assert.equal(phase.serve?.reached,true);assert.equal(phase.serve.exitCode,0);assert.equal(phase.serve.signal,null);
  assert.equal(phase.step.serve[0],'server');assert.ok(!phase.serve.stdout.includes('未迁移'));
  assert.equal(phase.then?.[2]?.disconnected,true,'WS closed before process stop');
  const responses=phase.then[1]?.frames?.filter(frame=>frame.type==='response');
  assert.equal(responses?.length,1,'one actual history read response per phase');
  assert.ok(!responses[0].error);phaseResults.push(responses[0].result);
  assert.ok(JSON.stringify(responses[0].result).includes('CORE-DONE'),'first result survived phase '+index);
 }
 assert.ok(JSON.stringify(phaseResults[1]).includes('CORE-RECOVERED'),'restart result read from actual Gateway');
 const provider=artifact.provider;
 const initial=exact(provider.initial,'initial model tool call',1)[0];
 const after=exact(provider['after-tool'],'after-tool model continuation',1)[0];
 const recovered=exact(provider.recovered,'restarted model turn',1)[0];
 assert.equal(provider.unexpected?.length??0,0);assert.equal(provider.default?.length??0,0);
 for(const record of[initial,after,recovered]){assert.equal(record.method,'POST');assert.equal(record.url,'/v1/chat/completions');assert.equal(record.body.model,'deterministic');}
 const marker='CORE-BEGIN-{saved.channelText}';
 assert.equal(text(initial.body.messages.at(-1).content),marker,'channel—not synthetic WS—originated input');
 assert.equal(initial.body.messages.at(-1).role,'user');
 const priorTool=after.body.messages.find(message=>message.role==='tool'&&message.tool_call_id==='call_core_write');
 assert.ok(priorTool,'real file tool response reached provider');
 const history=recovered.body.messages;
 assert.equal(history.at(-1).role,'user');assert.equal(text(history.at(-1).content),'CORE-BEGIN-CORE-RECOVER');
 assert.equal(history.filter(m=>m.role==='user'&&text(m.content)===marker).length,1,'old user input recovered once');
 assert.equal(history.filter(m=>m.role==='assistant'&&text(m.content)==='CORE-DONE').length,1,'old assistant history recovered once');
 assert.equal(history.filter(m=>m.role==='tool'&&m.tool_call_id==='call_core_write').length,1,'old real tool result recovered once');
 const calls=history.flatMap(m=>m.role==='assistant'?m.tool_calls??[]:[]).filter(call=>call.id==='call_core_write');
 exact(calls,'persisted tool call',1);assert.equal(calls[0].function.name,'write_file');
 const arguments_=JSON.parse(calls[0].function.arguments);assert.ok(arguments_.file_path.endsWith('/home/.pilotdeck/notes/core-artifact.txt'));
 assert.equal(arguments_.content,'CORE-RECOVER core-message\n');
 assert.deepEqual(history.find(m=>m.role==='tool'&&m.tool_call_id==='call_core_write'),priorTool,'recovery used original tool result');
 const polls=provider['channel-poll'];assert.ok(polls?.length>=2);
 for(const request of polls){assert.equal(request.method,'GET');const url=new URL(request.url,'http://fixture.invalid');assert.equal(url.pathname,'/api/v1/message');assert.equal(url.searchParams.get('password'),'core-channel-password');assert.equal(url.searchParams.get('limit'),'50');assert.match(url.searchParams.get('after')??'',/^\d+$/);}
 const sends=provider['channel-send'];assert.ok(sends?.length>=3);
 for(const request of sends){assert.equal(request.method,'POST');assert.equal(request.url,'/api/v1/message/text');assert.equal(request.body.chatGuid,'core-chat');assert.equal(request.body.password,'core-channel-password');assert.equal(typeof request.body.message,'string');assert.match(request.body.tempGuid,/^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$/);}
 assert.equal(sends.filter(r=>r.body.message==='CORE-DONE').length,1,'one initial final reply');
 assert.equal(sends.filter(r=>r.body.message==='CORE-RECOVERED').length,1,'one restarted final reply');
 assert.ok(sends.some(r=>r.body.message.includes('write_file')&&r.body.message.includes('core-artifact.txt')),'real channel permission prompt');
 assert.equal(artifact.tree['/home/.pilotdeck/notes/core-artifact.txt']?.text,'CORE-RECOVER core-message\n','real tool wrote expected bytes');
 const logs=Object.entries(artifact.tree).filter(([name,value])=>name.endsWith('.jsonl')&&Array.isArray(value.jsonl)).flatMap(([,value])=>value.jsonl);
 const accepted=logs.filter(entry=>entry.type==='accepted_input');
 const first=accepted.filter(entry=>JSON.stringify(entry.messages).includes(marker));
 const second=accepted.filter(entry=>JSON.stringify(entry.messages).includes('CORE-BEGIN-CORE-RECOVER'));
 exact(first,'durable initial accepted input',1);exact(second,'durable resumed accepted input',1);
 assert.equal(first[0].sessionId,second[0].sessionId,'same actual durable session after process replacement');assert.ok(first[0].sessionId);
 return {passed:true,side:artifact.side,modelRequests:{initial:1,afterTool:1,recovered:1},polls:polls.length,sends:sends.length,phases:2,sessionId:first[0].sessionId};
}
const args=process.argv.slice(2),self=args[0]==='--self-test';if(self)args.shift();
const artifactPath=args[0];assert.ok(artifactPath,'usage: verify.mjs [--self-test] artifact.json [scenario.json]');
const scenarioPath=args[1]??defaultScenario;
const scenarioBytes=fs.readFileSync(scenarioPath),bytes=fs.readFileSync(artifactPath);
const scenario=JSON.parse(scenarioBytes),artifact=JSON.parse(bytes),scenarioHash=sha(scenarioBytes);
const result=validate(artifact,scenario,scenarioHash);
if(self){
 const cases=[['missing-recovery',a=>a.provider.recovered=[]],['duplicate-side-effect',a=>a.provider.initial.push(structuredClone(a.provider.initial[0]))],['lost-original-history',a=>a.provider.recovered[0].body.messages=a.provider.recovered[0].body.messages.filter(m=>!(m.role==='assistant'&&text(m.content)==='CORE-DONE'))],['wrong-file',a=>a.tree['/home/.pilotdeck/notes/core-artifact.txt'].text='wrong'],['no-permission-prompt',a=>a.provider['channel-send']=a.provider['channel-send'].filter(r=>!r.body.message.includes('write_file'))],['wrong-channel-answer',a=>a.provider['channel-send'].find(r=>r.body.message==='CORE-RECOVERED').body.message='WRONG']];
 for(const[name,mutate]of cases){const changed=structuredClone(artifact);mutate(changed);assert.throws(()=>validate(changed,scenario,scenarioHash),undefined,name);}
 result.negativeControls=cases.map(([name])=>name);
}
assert.equal(sha(fs.readFileSync(artifactPath)),sha(bytes),'artifact stayed immutable');
console.log(JSON.stringify({...result,artifactSha256:sha(bytes),scenarioSha256:scenarioHash,verifierSha256:sha(fs.readFileSync(fileURLToPath(import.meta.url)))}));
