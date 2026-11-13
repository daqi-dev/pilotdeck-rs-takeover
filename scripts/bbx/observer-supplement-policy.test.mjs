import test from'node:test';import assert from'node:assert/strict';import fs from'node:fs';import{observerSupplementPaths}from'./observer-supplement-policy.mjs';import{actualManifest,observerFiles as files,readActual,baselineRoot}from'./fixtures/observer-approved/actual.mjs';
const proposal=(a,b)=>observerSupplementPaths(a,b,baselineRoot);
test('unrelated scene grants no supplement paths',()=>assert.equal(proposal({scenario:'other'},{scenario:'other'}).size,0));
if(actualManifest){
const raw=Object.fromEntries(Object.entries(files).map(([k,ps])=>[k,ps.map(p=>readActual(p))]));
const alter=mutate=>{const[a,b]=[structuredClone(raw.memory[0]),structuredClone(raw.memory[2])];mutate(b);return()=>proposal(a,b);};
const parent=a=>Object.values(a.tree).find(v=>v.jsonl?.some(r=>r.sessionId==='agent03-parent')).jsonl;
for(const name of['memory','fork'])test('actual '+name+' exact paired proposal has provider + numeric/counter witnesses only',()=>{const r=proposal(raw[name][0],raw[name][2]);assert.ok(r instanceof Map);assert.ok(r.has('$.provider.<key order>'));assert.ok(r.has('$.steps[6].frames[0].result.messages[4].sequence'));assert.ok(r.has('$.steps[6].frames[0].result.stream.events[3].used'));for(const at of r.keys())assert.ok(!/fileSize|control.sqlite/.test(at),'unapproved UUID/memory/size remains untouched');});
test('actual memory TSself admits only bounded existing UUID implementation candidates',()=>assert.ok([...proposal(raw.memory[0],raw.memory[1]).values()].every(v=>v.startsWith('Existing UUID'))));
const controls=[
 ['wrong scene hash',a=>a.evidence.scenario.sha256='0'.repeat(64)],
 ['wrong scene name',a=>a.scenario='other'],
 ['foreign observer child',a=>parent(a).find(r=>r.type==='subagent_started'&&r.subagentType==='reviewer').subagentId='aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa'],
 ['observer body changed',a=>a.provider.observer[0].body.messages.at(-1).content='foreign'],
 ['parent provider business changed',a=>a.provider['parent-after'][0].body.messages.at(-1).content='foreign'],
 ['missing provider branch',a=>delete a.provider.observer],
 ['duplicate provider request',a=>a.provider.observer.push(structuredClone(a.provider.observer[0]))],
 ['nonadjacent provider order',a=>{const entries=Object.entries(a.provider);[entries[0],entries[1]]=[entries[1],entries[0]];a.provider=Object.fromEntries(entries);}],
 ['foreign model request input',a=>parent(a).find(r=>r.type==='model_request').request.messages[0].content[0].text='foreign'],
 ['wrong WS numeric projection',a=>a.steps[0].frames.find(f=>f.event?.type==='context_budget').event.used++],
 ['wrong history summary',a=>a.steps[6].frames[0].result.tokenUsage.used++],
 ['history wrong input owner',a=>{const m=a.steps[6].frames[0].result.messages;[m[0].entryId,m[6].entryId]=[m[6].entryId,m[0].entryId];}],
 ['wrong history producer sequence',a=>a.steps[6].frames[0].result.messages[4].sequence++],
 ['wrong record revision',a=>a.tree['/home/.pilotdeck/runs/registry.json'].json.records[0].revision++],
 ['wrong raw skeleton',a=>a.tree['/home/.pilotdeck/runs/registry.json'].raw.skeletons={ffffffffffffffff:1}],
 ['lost observer completion',a=>a.steps[0].frames=a.steps[0].frames.filter(f=>f.event?.event!=='subagent_completed')],
];
for(const[name,change]of controls)test('reject '+name,()=>assert.throws(alter(change)));
test('missing no-new-field guarantee remains strict via lifecycle complete provider body gate',()=>assert.throws(alter(a=>a.provider['parent-check'][0].body.newBehavior=true)));

}
