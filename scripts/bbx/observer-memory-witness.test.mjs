import test from'node:test';import assert from'node:assert/strict';import fs from'node:fs';import{createHash}from'node:crypto';import{memoryObserverWitness as witness,observerMemoryWitnessPaths}from'./observer-memory-witness.mjs';import{normalizer}from'./normalization.mjs';import{actualManifest,observerFiles,readActual,baselineRoot}from'./fixtures/observer-approved/actual.mjs';
const memoryObserverWitness=(a,b)=>witness(a,b,baselineRoot),existingTime=value=>normalizer()(value);
test('unrelated scene grants no memory paths',()=>assert.equal(observerMemoryWitnessPaths({scenario:'other'},{scenario:'other'}).size,0));
if(actualManifest){
const read=s=>readActual(observerFiles.memory[['ts-1','ts-2','rust'].indexOf(s)]);const db=a=>a.tree['/home/agent03-memory/workspaces/379ec3ac8a/control.sqlite'].sqlite.tables;const parent='/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl';
const state=(a,fn)=>{const row=db(a).pipeline_state[0],v=JSON.parse(row.state_json);fn(v);row.state_json=JSON.stringify(v);};
for(const s of['ts-2','rust'])test('existing '+s+' strict memory witness',()=>{const r=memoryObserverWitness(read('ts-1'),read(s));assert.equal(r.status,'proven',r.error);assert.match(r.permissions.timestamps,/existing ISO/);assert.equal(r.formalAdmission,false);assert.equal(r.paths.length,7);assert.ok(r.sides.every(x=>x.timesBounded));});
const mutants=[
 ['L0 hash',a=>db(a).l0_sessions[0].l0_index_id+='f'],
 ['L0 duplicate semantic row',a=>{db(a).l0_sessions[1]={...db(a).l0_sessions[0]};}],
 ['foreign step trace',a=>state(a,v=>v[0].retrieval.trace.steps[0].stepId='trace_foreign:step:1')],
 ['business pipeline payload',a=>state(a,v=>v[0].retrieval.contextPreview+=' FOREIGN')],
 ['foreign case session',a=>state(a,v=>v[0].sessionKey='agent03-parent')],
 ['size off one',a=>a.steps[6].frames[0].result.session.fileSize++],
 ['durable changed without witness',a=>a.tree[parent].jsonl.find(r=>r.type==='accepted_input').messages[0].content[0].text+=' FOREIGN'],
 ['durable business changed and size/hash updated',a=>{const rows=a.tree[parent].jsonl;const r=rows.find(r=>r.type==='accepted_input');r.messages[0].content[0].text+=' FOREIGN';const bytes=Buffer.from(rows.map(r=>JSON.stringify(r)).join('\n')+'\n');a.evidence.persistedFiles[parent]={byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')};a.steps[6].frames[0].result.session.fileSize=bytes.length;}],
];for(const[name,mutate]of mutants)test('reject '+name,()=>{const a=read('ts-1'),b=read('rust');mutate(b);assert.equal(memoryObserverWitness(a,b).status,'invalid');});
test('valid ISO outside run containment still reuses existing ISO grant without new range',()=>{const a=read('ts-1'),b=read('rust');state(b,v=>v[0].startedAt='2020-01-01T00:00:00.000Z');const r=memoryObserverWitness(a,b);assert.equal(r.status,'proven',r.error);assert.equal(r.sides[1].timesBounded,false);assert.match(r.permissions.timestamps,/existing ISO/);});

function refreshParent(a){const bytes=Buffer.from(a.tree[parent].jsonl.map(r=>JSON.stringify(r)+'\n').join(''));a.evidence.persistedFiles[parent]={byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')};a.steps[6].frames[0].result.session.fileSize=bytes.length;}
test('all62 original rows independently owned60typed+2strict lifecycle and exact full bytes',()=>{
 for(const side of ['ts-2','rust']){const a=read('ts-1'),b=read(side),r=memoryObserverWitness(a,b);assert.equal(r.status,'proven');assert.equal(r.formalAdmission,false);for(const[which,w]of r.fullParentWitness.entries()){const raw=which?b:a;assert.equal(w.rowCount,62);assert.equal(w.typedPayloadRows,60);assert.equal(w.lifecycleRows,2);assert.equal(new Set(w.rows.map(x=>x.index)).size,62);const bytes=Buffer.from(w.rows.map(x=>x.text).join(''));assert.deepEqual({byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')},raw.evidence.persistedFiles[parent]);for(const row of w.rows){assert.equal(row.text,JSON.stringify(raw.tree[parent].jsonl[row.index])+'\n');assert.equal(row.sha256,createHash('sha256').update(row.text).digest('hex'));}}}
});
for(const [label,fn]of [
 ['reviewer start promptTruncated',r=>r.find(x=>x.type==='subagent_started'&&x.subagentType==='reviewer').promptTruncated=true],
 ['reviewer start foreign prompt',r=>r.find(x=>x.type==='subagent_started'&&x.subagentType==='reviewer').promptPreview+=' EXTRA'],
 ['reviewer start unknown payload field',r=>r.find(x=>x.type==='subagent_started'&&x.subagentType==='reviewer').foreign='unproven'],
 ['reviewer completed turns',r=>r.find(x=>x.type==='subagent_completed'&&x.subagentType==='reviewer').turns=2],
 ['reviewer completed summaryTruncated',r=>r.find(x=>x.type==='subagent_completed'&&x.subagentType==='reviewer').summaryTruncated=true],
 ['reviewer completed duration source',r=>r.find(x=>x.type==='subagent_completed'&&x.subagentType==='reviewer').durationMs++],
])test('reject '+label+' even refreshed byte witness',()=>{const a=read('ts-1'),b=read('rust');fn(b.tree[parent].jsonl);refreshParent(b);assert.equal(memoryObserverWitness(a,b).status,'invalid');});
for(const[label,fn]of[
 ['case ID wrongschema',v=>v[0].caseId='case_trace_nothex'],
 ['case ID manytoone',v=>v[1].caseId=v[0].caseId],
 ['trace ID manytoone',v=>{v[1].retrieval.trace.traceId=v[0].retrieval.trace.traceId;v[1].retrieval.trace.steps.forEach((s,i)=>s.stepId=v[1].retrieval.trace.traceId+':step:'+(i+1));}],
 ['pipeline extra businessfield',v=>v[0].unprovenField=true],
 ['timestamp malformed',v=>v[0].startedAt='not-time'],
])test('reject '+label,()=>{const a=read('ts-1'),b=read('rust');state(b,fn);assert.equal(memoryObserverWitness(a,b).status,'invalid');});


test('existing timestamp rule keeps numeric range/fraction and embedded hash boundaries',()=>{assert.equal(existingTime(1794550000000),'<ms>');assert.equal(existingTime(1794550000000.25),'<ms.frac>');assert.notEqual(existingTime(1794550000000),existingTime(1794550000000.25));for(const x of [1600000000000,2000000000000,1599999999999])assert.equal(existingTime(x),x);assert.equal(existingTime('a1794550000000b'),'a1794550000000b');assert.equal(existingTime('1794550000000.25'),'1794550000000.25');assert.equal(existingTime('x 1794550000000 y'),'x <ms> y');});
test('all12 actual pipeline values per side use original ISO permission and preserve raw',()=>{const a=read('ts-1'),b=read('rust');const before=JSON.stringify([a,b]);const r=memoryObserverWitness(a,b);assert.equal(r.status,'proven');for(const side of r.sides){assert.equal(side.pipeline.timestampWitness.length,12);for(const t of side.pipeline.timestampWitness){assert.equal(existingTime(t.raw),t.normalized);assert.equal(t.normalized,'<iso>');assert.equal(typeof t.raw,'string');}}assert.equal(JSON.stringify([a,b]),before);assert.match(r.permissions.caseTraceIds,/User-approved/);assert.equal(r.formalAdmission,false);});

}
