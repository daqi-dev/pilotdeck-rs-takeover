// Private, unapproved proposal. Never imported by the formal harness.
import assert from'node:assert/strict';import{load,compare}from'./normalization.mjs';import{observerUuidViews}from'./observer-uuid-policy.mjs';import{observerPairProvenance as pair,observerSingleProvenance as single}from'./observer-provenance.mjs';import{lifecyclePair}from'./observer-lifecycle-policy.mjs';
const one=(xs,label)=>{assert.equal(xs.length,1,label);return xs[0];};
const reg='/home/.pilotdeck/runs/registry.json';
const FIELDS=['used','displayUsed','localEstimateTokens','usageTokens','calibrationEstimatedInputTokens','ratio','breakdown.total','breakdown.messages'];
const hashes=new Set(['9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca','d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a']);
const get=(v,p)=>p.split('.').reduce((x,k)=>x?.[k],v);
export function observerSupplementPaths(a,b,baselineRoot){
 if(![a,b].some(x=>['agent03-sdk-fork','agent03-sdk-fork-lifecycle-probe'].includes(x.scenario)||hashes.has(x.evidence?.scenario?.sha256)))return new Map();
 // Inputs and all proof are derived inside this call. No caller-authored
 // receipt, passed flag, or mapping can supply admission evidence.
 assert.ok(hashes.has(a.evidence?.scenario?.sha256));assert.equal(a.evidence.scenario.sha256,b.evidence?.scenario?.sha256);assert.equal(a.side,'ts');assert.ok(['ts','rust'].includes(b.side));const tables=[new Map(),new Map()];const loaded=[a,b].map((r,i)=>load(structuredClone(r),undefined,tables[i]));const bound=observerUuidViews(a,b,...loaded,...tables);const views=bound.views,aliases=bound.aliases;
 const life=lifecyclePair(a,b),p=pair(a,b,baselineRoot);const raw=[a,b];
 const proofSides=[life.left,life.right];const out=new Map(),witness=[];
 const put=(at,reason,proof)=>{assert.ok(!out.has(at)||out.get(at)===reason,'conflicting proposed leaf');out.set(at,reason);if(proof)witness.push({at,proof});};
 const token=(id,i)=>{assert.ok(aliases[i] instanceof Map&&aliases[i].has(id),'actual alias required');return`<uuid:${aliases[i].get(id)}>`;};
 // Bind normalized views at every consumed node to the same raw budgets and
 // anchors. Caller must supply diff's own views, not arbitrary tree clones.
 const typed=[];compare(...views,'$',typed);const inverse=aliases.map(m=>new Map([...m].map(([id,n])=>[`<uuid:${n}>`,id])));const singles=raw.map(x=>single(x,baselineRoot));
 const rawpath=(s,i)=>s.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g,id=>token(id,i));
 const ownerBinding=(path,side)=>{const direct=/^\$\.tree\.(.*)\.jsonl\[(\d+)\]$/.exec(path);if(direct){const v=views[side].tree[direct[1]]?.jsonl?.[Number(direct[2])];if(!v)return null;const slot=p.uuid.normalizedSlotBindings.filter(x=>x.side===side&&token(x.normalizedSlotEntryId,side)===v.entryId&&x.type===v.type);return slot.length===1?slot[0]:null;}const owned=singles[side].projectionEntryOwnership.filter(x=>rawpath(x.projectionPath,side)===path);if(owned.length!==1)return null;return one(p.uuid.normalizedSlotBindings.filter(x=>x.side===side&&x.producerEntryId===owned[0].entryId),'projection producer slot');};
 for(const d of typed){if(!/\.(entryId|parentEntryId|operationId)$/.test(d.at)||!inverse[0].has(d.a)||!inverse[1].has(d.b))continue;const ids=[inverse[0].get(d.a),inverse[1].get(d.b)];const relation=p.uuid.candidateBijection.find(x=>x.left===ids[0]&&x.right===ids[1]);if(!relation)continue;const extra=p.uuid.referenceOwnership.find(x=>x.label===relation.label);if(extra&&!extra.closed)continue;const owners=[0,1].map(i=>ownerBinding(d.at.replace(/\.[^.]+$/,''),i));if(owners.some(x=>!x))continue;const payload=p.uuid.fullPayloadRows.find(x=>x.leftEntry===owners[0].producerEntryId&&x.rightEntry===owners[1].producerEntryId);if(!payload)continue;put(d.at,'Existing UUID same-entity implementation repair: exact same leaf + typed producer/slot/ownership only',{relation,owners,payload});}
 const keys=raw.map(r=>Object.keys(r.provider));
 if(JSON.stringify(keys[0])!==JSON.stringify(keys[1])){
  assert.equal(b.side,'rust','TS reverse order not admitted by this proposal');const index=keys[0].indexOf('parent-after');assert.equal(keys[0][index+1],'observer','exact adjacent left pair');const expected=[...keys[0]];[expected[index],expected[index+1]]=[expected[index+1],expected[index]];assert.deepEqual(keys[1],expected,'only exact two-provider adjacent transposition');
  assert.deepEqual(Object.keys(views[0].provider),keys[0]);assert.deepEqual(Object.keys(views[1].provider),keys[1]);
  // lifecyclePair already compares every complete provider request body,
  // tag/count and anchored observer producer/digest. Headers not exempted.
  put('$.provider.<key order>','User-approved: two exact adjacent requests only',{keys});
 }
 for(const [ordinal,left]of p.budget[0].entries()){
  const right=one(p.budget[1].filter(x=>x.key===left.key),'unique cross-side canonical request');assert.equal(left.step,right.step);if(ordinal===0)continue;
  const bindings=[left,right];const surfaces=bindings.map((q,i)=>{
   const run=token(q.run,i),parent=proofSides[i].parentPath,rows=views[i].tree[parent].jsonl,entry=token(q.entryId,i);const di=one(rows.map((r,j)=>({r,j})).filter(x=>x.r.entryId===entry),'normalized durable budget');assert.equal(di.r.event,'context_budget');assert.equal(di.r.turnId,run);
   const statuses=views[i].tree[parent+'.projections.json'].json.projections['web-history.agent-statuses'].state;const si=one(statuses.map((r,j)=>({r,j})).filter(x=>x.r.entryId===entry),'normalized projection budget');
   const ws=views[i].steps[q.step].frames[q.wire].event,registry=views[i].tree[reg].json.events['\0agent03-parent\0'+run][q.registry].event,hist=views[i].steps[6].frames[0].result.messages[q.history];assert.equal(hist.id,entry);assert.equal(hist.turnId,run);for(const v of[ws,registry,hist.payload.detail,di.r.detail,si.r.detail]){assert.equal(v.type,'context_budget');assert.equal(v.runId,run);for(const f of FIELDS)assert.equal(get(v,f),get(q.event,f),'normalized exact derived value bound');}
   const stream=q.step===3?{stream:`$.steps[6].frames[0].result.stream.events[${q.registry}]`}:{};if(q.step===3)assert.deepEqual(views[i].steps[6].frames[0].result.stream.events[q.registry],ws,'full same-run history replay');return{...stream,wire:`$.steps[${q.step}].frames[${q.wire}].event`,registry:`$.tree.${reg}.json.events.\0agent03-parent\0${run}[${q.registry}].event`,history:`$.steps[6].frames[0].result.messages[${q.history}].payload.detail`,durable:`$.tree.${parent}.jsonl[${di.j}].detail`,projection:`$.tree.${parent}.projections.json.json.projections.web-history.agent-statuses.state[${si.j}].detail`};
  });
  for(const kind of Object.keys(surfaces[0]))for(const f of FIELDS){assert.equal(typeof get(left.event,f),'number');assert.equal(typeof get(right.event,f),'number');if(get(left.event,f)===get(right.event,f)&&surfaces[0][kind]===surfaces[1][kind])continue;for(const i of[0,1])put(surfaces[i][kind]+'.'+f,'User-approved context: independently recomputed semantic budget at two approved lifecycle counterpart paths',{semantic:left.key,surface:kind,field:f,left:surfaces[0][kind],right:surfaces[1][kind],values:[get(left.event,f),get(right.event,f)]});}
 }
 assert.ok(p.tokenUsage,'strengthened summary usage proof');for(const field of['used','displayUsed','ratio']){const values=p.tokenUsage.map(t=>t.value[field]);for(const i of[0,1]){const u=p.tokenUsage[i],actual=views[i].steps[6].frames[0].result.tokenUsage;assert.deepEqual(actual,u.value);const project=views[i].tree[proofSides[i].parentPath+'.projections.json'].json.projections['web-history.token-usage'].state.latestContextBudget;assert.deepEqual(project.usage,u.value);if(values[0]!==values[1])for(const at of[u.historyPath+'.'+field,u.projectionPath+'.usage.'+field])put(at,'User-approved context summary: exact last budget producer and complete usage projection',{field,values,sourceEntryIds:p.tokenUsage.map(t=>t.sourceEntryId)});}}
 for(const ordinal of[0,1]){
  const counters=p.counters.map(s=>s[ordinal]);for(const i of[0,1]){const run=token(counters[i].run,i);const record=one(views[i].tree[reg].json.records.map((r,j)=>({r,j})).filter(x=>x.r.runId===run&&x.r.sessionKey==='agent03-parent'),'normalized registry counter');for(const f of['revision','lastSeq']){assert.equal(record.r[f],counters[i][f]);if(counters[0][f]!==counters[1][f])put(`$.tree.${reg}.json.records[${record.j}].${f}`,'Exact lifecycle counter supplement: full WS/registry membership and terminal proof',{runOrdinal:ordinal,field:f,values:counters.map(c=>c[f])});}}
 }
 // Only the two current residual history messages, with independently
 // checked full producer ownership. Never a blanket history sequence grant.
 assert.ok(p.historyOwnership,'strengthened history ownership required');
 for(const index of[4,5]){const owners=p.historyOwnership.map((rows,i)=>one(rows.filter(x=>x.historyPath===`$.steps[6].frames[0].result.messages[${index}]`),'exact history producer'));
  const paired=p.uuid.fullPayloadRows.find(r=>r.leftEntry===owners[0].entryId&&r.rightEntry===owners[1].entryId);assert.ok(paired,'complete counterpart durable payload proof');for(const i of[0,1]){const h=views[i].steps[6].frames[0].result.messages[index];assert.equal(h.sequence,owners[i].sequence);assert.equal(h.entryId??h.id,token(owners[i].entryId,i));assert.equal(h.turnId,token(owners[i].turnId,i));}if(owners[0].sequence!==owners[1].sequence)put(`$.steps[6].frames[0].result.messages[${index}].sequence`,'Exact history producer sequence supplement',{owners});
 }
 assert.equal(p.registryEqualAfterProvenWindowsAndNumeric,true,'entire non-target registry strict after only validated semantics');
 for(const i of[0,1]){assert.deepEqual(views[i].tree[reg].raw,raw[i].tree[reg].raw,'normalized raw skeleton unchanged');}
 const skeletons=raw.map(r=>r.tree[reg].raw.skeletons);for(const k of new Set([...Object.keys(skeletons[0]),...Object.keys(skeletons[1])]))if(skeletons[0][k]!==skeletons[1][k])put(`$.tree.${reg}.raw.skeletons.${k}`,'User-approved bounded registry skeleton: complete same-side raw reconstruction + all non-target registry exact',{rawHashes:p.registrySkeletonWitnesses});
 return out;
}
