import test from'node:test';import assert from'node:assert/strict';import fs from'node:fs';import{observerPairProvenance as provePair,observerSingleProvenance as proveSingle}from'./observer-provenance.mjs';
import{actualManifest,observerFiles as files,readActual,baselineRoot}from'./fixtures/observer-approved/actual.mjs';
const pair=(a,b)=>provePair(a,b,baselineRoot),single=a=>proveSingle(a,baselineRoot);
test('unknown source cannot supply observer provenance',()=>assert.throws(()=>pair({scenario:'unknown'},{scenario:'unknown'})));
if(actualManifest){
const original=files.memory.map(p=>readActual(p));const run=mutate=>{const a=structuredClone(original);mutate(a[1]);return()=>pair(a[0],a[1]);};
const parent=a=>Object.values(a.tree).find(v=>v.jsonl?.some(r=>r.sessionId==='agent03-parent'))?.jsonl;
const registry=a=>a.tree['/home/.pilotdeck/runs/registry.json'].json;
const controls=[
 ['foreign parent run',a=>{a.steps[0].frames[0].event.runId='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';}],
 ['wrong child producer',a=>{parent(a).find(r=>r.type==='subagent_started'&&r.subagentType==='reviewer').subagentId='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';}],
 ['wrong durable session',a=>{parent(a)[5].sessionId='foreign';}],
 ['duplicate durable entry',a=>{parent(a)[5].entryId=parent(a)[4].entryId;}],
 ['broken previous-entry edge',a=>{parent(a)[5].parentEntryId=parent(a)[3].entryId;}],
 ['foreign operation completed',a=>{parent(a).find(r=>r.type==='compaction_completed').operationId='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';}],
 ['incorrect budget number',a=>{parent(a).find(r=>r.type==='agent_status_message'&&r.event==='context_budget').detail.used+=1;}],
 ['incorrect budget WS projection',a=>{a.steps[0].frames.find(f=>f.event?.type==='context_budget').event.used+=1;}],
 ['wrong budget history identity',a=>{a.steps[6].frames[0].result.messages.find(r=>r.payload?.event==='context_budget').id='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';}],
 ['incorrect revision',a=>{registry(a).records[0].revision+=1;}],
 ['incorrect lastSeq',a=>{registry(a).records[0].lastSeq+=1;}],
 ['duplicate registry event',a=>{const k=Object.keys(registry(a).events)[0];registry(a).events[k].push(structuredClone(registry(a).events[k].at(-1)));}],
 ['lost observer completion',a=>{a.steps[0].frames=a.steps[0].frames.filter(f=>f.event?.event!=='subagent_completed');}],
 ['wrong canonical tool input',a=>{parent(a).find(r=>r.type==='model_request').request.messages[0].content[0].text='FOREIGN INPUT';}],
];
for(const[label,mutation]of controls)test('reject '+label,()=>assert.throws(run(mutation)));
test('actual memory self full payload graph and exact counter equations',()=>{const p=pair(original[0],original[1]);assert.equal(p.uuid.unknownRows.length,0);assert.deepEqual(p.counterDelta,[0,0]);assert.equal(p.uuid.fullPayloadRows.length,151);});
test('actual memory pair exact delta +/-3 and all budget links',()=>{const p=pair(original[0],original[2]);assert.deepEqual(p.counterDelta,[3,-3]);assert.equal(p.budget[0].length,3);assert.equal(p.budget[1].length,3);assert.equal(p.uuid.unknownRows.length,0);});
test('reject forged report byte derivative',()=>assert.throws(run(a=>{a.steps[0].frames.find(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent').event.resultBytes+=1;})));
test('reject raw skeleton marker spoof',()=>assert.throws(run(a=>{a.tree['/home/.pilotdeck/runs/registry.json'].raw.skeletons={ffffffffffffffff:1};})));
test('reject history assistant foreign entry identity',()=>assert.throws(run(a=>{a.steps[6].frames[0].result.messages[5].entryId='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';})));
test('unknown extra child field prevents complete UUID graph closure',()=>{const a=structuredClone(original);const child=Object.entries(a[1].tree).find(([p,v])=>p.includes('/subagents/')&&v.jsonl?.length>50)[1];child.jsonl[8].unprovedNewField=true;assert.equal(pair(a[0],a[1]).uuid.completeClosure,false);});

// Independently discovered real blind spots: preserve the rest of each
// actual capture, including reference counts, rather than inventing a matrix.
test('reject consistently corrupted initial displayUsed in all actual projections',()=>assert.throws(run(a=>{
 const row=parent(a).find(r=>r.type==='agent_status_message'&&r.event==='context_budget');let copies=0;
 const visit=value=>{if(!value||typeof value!=='object')return;if(value.type==='context_budget'&&value.runId===row.detail.runId&&value.breakdown?.messages===row.detail.breakdown.messages){value.displayUsed++;copies++;}for(const child of Object.values(value))visit(child);};
 visit(a);assert.equal(copies,5);
})));
test('reject swapped user history entry ownership despite unchanged UUID counts',()=>assert.throws(run(a=>{
 const messages=a.steps[6].frames[0].result.messages;[messages[0].entryId,messages[6].entryId]=[messages[6].entryId,messages[0].entryId];
})));
test('reject corrupted last history tokenUsage projection',()=>assert.throws(run(a=>{a.steps[6].frames[0].result.tokenUsage.used++;})));
test('all six original raw remain valid while pair and title Unknown stay explicit',()=>{
 const fork=files.fork.map(p=>readActual(p));
 const results=[pair(original[0],original[1]),pair(original[0],original[2]),pair(fork[0],fork[1]),pair(fork[0],fork[2])];
 assert.deepEqual(results.map(p=>p.uuid.unknownRows.length),[0,0,0,0]);
 assert.deepEqual(results.map(p=>p.uuid.referenceCountDifferences.length),[0,3,0,2]);
 for(const p of results){assert.equal(p.uuid.wholeGraphPathOwnershipProved,false);assert.deepEqual(p.historyOwnership.map(rows=>rows.length),[9,9]);assert.equal(p.tokenUsage.length,2);}
});

test('reject swapped reviewer projection predecessor references with counts unchanged',()=>assert.throws(run(a=>{
 const state=Object.entries(a.tree).find(([p])=>p.endsWith('/chats/agent03-parent.jsonl.projections.json'))[1].json.projections['agent-transcript.subagent-references'].state;
 [state.started[1].parentEntryId,state.completed[1].parentEntryId]=[state.completed[1].parentEntryId,state.started[1].parentEntryId];
})));
test('unknown added reviewer-predecessor reference cannot be hidden by balancing global counts',()=>{
 const a=structuredClone(original);const before=pair(a[0],a[2]);const relation=before.uuid.referenceCountDifferences.find(r=>r.referenceCounts[0]===r.referenceCounts[1]+1);assert.ok(relation);
 a[2].unprovedReference=relation.right;const p=pair(a[0],a[2]);assert.ok(p.uuid.unresolvedReferenceOwnership.some(r=>r.right===relation.right));assert.equal(p.uuid.completeClosure,false);
});
test('existing title slot projection does not forgive a changed context payload',()=>{
 const fork=files.fork.map(p=>readActual(p));const row=Object.values(fork[1].tree).find(v=>v.jsonl?.some(r=>r.sessionId==='agent03-parent')).jsonl.find(r=>r.type==='context_snapshot');row.promptGeneration++;
 const p=pair(fork[0],fork[1]);assert.ok(p.uuid.unknownRows.some(r=>r.type==='context_snapshot'));assert.equal(p.uuid.completeClosure,false);
});
test('title settlement proof never mutates caller raw arrays or slot fields',()=>{
 const fork=files.fork.map(p=>readActual(p));const before=fork.map(x=>JSON.stringify(x));pair(fork[0],fork[1]);assert.deepEqual(fork.map(x=>JSON.stringify(x)),before);
});

}
