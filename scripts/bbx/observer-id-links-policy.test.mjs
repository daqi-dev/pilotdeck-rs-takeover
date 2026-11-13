import test from'node:test';import assert from'node:assert/strict';import fs from'node:fs';import{observerIdLinkPaths}from'./observer-id-links-policy.mjs';import{actualManifest,observerFiles as files,readActual,baselineRoot}from'./fixtures/observer-approved/actual.mjs';
const prove=(a,b)=>({paths:observerIdLinkPaths(a,b,baselineRoot),wholeGraphRelabeling:false});
test('unrelated scene grants no ID paths',()=>assert.equal(observerIdLinkPaths({scenario:'other'},{scenario:'other'}).size,0));
if(actualManifest){
const sets=Object.fromEntries(Object.entries(files).map(([k,ps])=>[k,ps.map(p=>readActual(p))]));
for(const[name,data]of Object.entries(sets))for(const[k,j,count]of[['self',1,name==='memory'?218:0],['pair',2,16]])test(name+' '+k+' actual bounded ID leaf count',()=>{const r=prove(data[0],data[j]);assert.equal(r.paths.size,count);assert.equal(r.wholeGraphRelabeling,false);assert.ok([...r.paths.keys()].every(x=>/\.(entryId|parentEntryId|operationId)$/.test(x)));assert.ok(!r.paths.has('$.tree.<key order>'));});
const raw=sets.memory;const parent=a=>Object.values(a.tree).find(v=>v.jsonl?.some(r=>r.sessionId==='agent03-parent')).jsonl;
const child=a=>Object.entries(a.tree).find(([p,v])=>p.includes('/subagents/')&&v.jsonl?.length>50)[0];
const controls=[
 ['child anchor wrong known entry',a=>{const f=child(a);a.tree[f+'.projections.json'].json.anchor.entryId=a.tree[f].jsonl[0].entryId;}],
 ['child projection entry payload changed',a=>{const f=child(a);a.tree[f+'.projections.json'].json.projections['agent-transcript.conversation'].state.entries[0].entry.messages[0].content[0].text='FOREIGN';}],
 ['child projection rows swapped',a=>{const f=child(a),rows=a.tree[f+'.projections.json'].json.projections['agent-transcript.conversation'].state.entries;[rows[0],rows[1]]=[rows[1],rows[0]];}],
 ['child projection extra member',a=>{const f=child(a),rows=a.tree[f+'.projections.json'].json.projections['agent-transcript.conversation'].state.entries;rows.push(structuredClone(rows[0]));}],
 ['child anchor false preceding edge',a=>{const f=child(a);a.tree[f+'.projections.json'].json.anchor.parentEntryId=a.tree[f].jsonl[0].entryId;}],
 ['reviewer projection false preceding edge',a=>{const p=Object.entries(a.tree).find(([p])=>p.endsWith('/chats/agent03-parent.jsonl.projections.json'))[1];p.json.projections['agent-transcript.subagent-references'].state.started[1].parentEntryId=parent(a)[0].entryId;}],
 ['durable duplicated identity',a=>{parent(a)[32].entryId=parent(a)[31].entryId;}],
 ['durable wrong predecessor',a=>{parent(a)[33].parentEntryId=parent(a)[0].entryId;}],
 ['wrong title payload in normalized floating window',a=>{parent(a).find(r=>r.type==='context_snapshot').snapshot={FOREIGN:true};}],
 ['new UUID-bearing unknown field',a=>{parent(a)[32].unknownReference=parent(a)[31].entryId;}],
 ['wrong budget producer history reference',a=>{a.steps[6].frames[0].result.messages[4].id=parent(a)[0].entryId;}],
 ['unknown scene',a=>{a.scenario='unknown';}],
];
for(const[name,change]of controls)test('reject '+name,()=>{const a=structuredClone(raw[2]);change(a);assert.throws(()=>prove(raw[0],a));});

}
