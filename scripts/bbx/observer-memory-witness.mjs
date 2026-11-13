// Exact memory schema and byte witness; the path wrapper enforces the approved scope.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {observerPairProvenance as pair} from './observer-provenance.mjs';
import {lifecyclePair} from './observer-lifecycle-policy.mjs';
import {normalizer} from './normalization.mjs';
const normalizeTimestamp=value=>normalizer()(value);
const DB='/home/agent03-memory/workspaces/379ec3ac8a/control.sqlite';
const PARENT='/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl';
const EXPECT=[['agent03-parent','AG03-PARENT-BEGIN'],['agent03-parent','AG03-PARENT-CHECK'],['agent03-second','AG03-SECOND-SESSION']];
const hash=(x,algo='sha256')=>createHash(algo).update(x).digest('hex');
const one=(xs,label)=>{assert.equal(xs.length,1,label);return xs[0];};
const iso=x=>{assert.match(x,/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);return Date.parse(x);};
function turn(a,session,query){
 const candidates=Object.entries(a.tree).filter(([p,v])=>p.endsWith('/'+session+'.jsonl')&&v.jsonl);
 const rows=one(candidates,'unique actual session durable')[1].jsonl;
 const accepted=one(rows.filter(r=>r.type==='accepted_input'&&JSON.stringify(r.messages)==JSON.stringify([{role:'user',content:[{type:'text',text:query}]}])),'actual query input');
 const started=one(rows.filter(r=>r.type==='turn_started'&&r.turnId===accepted.turnId),'same run start');
 const final=one(rows.filter(r=>r.type==='turn_result'&&r.turnId===accepted.turnId),'same run final');
 assert.equal(final.result.stopReason,'completed');assert.equal(started.sessionId,session);assert.equal(accepted.sessionId,session);assert.equal(final.sessionId,session);
 assert.ok(started.sequence<accepted.sequence&&accepted.sequence<final.sequence);
 return{run:accepted.turnId,start:iso(started.createdAt),end:iso(final.createdAt)};
}
function within(value,bounds){const n=iso(value);return n>=bounds.start&&n<=bounds.end;}
function side(a){
 assert.equal(a.scenario,'agent03-sdk-fork-lifecycle-probe');
 const tables=a.tree[DB]?.sqlite?.tables;assert.ok(tables);assert.deepEqual(Object.keys(tables),['l0_sessions','pipeline_state']);assert.equal(tables.l0_sessions.length,3);
 const raw=tables.l0_sessions.map(r=>JSON.stringify(r));assert.deepEqual(raw,[...raw].sort(),'original collector row ordering');
 const l0=EXPECT.map(([session,query])=>{
  const row=one(tables.l0_sessions.filter(r=>r.session_key===session&&r.messages_json===JSON.stringify([{role:'user',content:query}])),'exact unique semantic L0 row');
  assert.equal(row.source,'pilotdeck');assert.equal(row.indexed,0);assert.deepEqual(Object.keys(row),['l0_index_id','session_key','timestamp','messages_json','source','indexed','created_at']);
  assert.equal(row.l0_index_id,session+'_'+hash(row.timestamp+':'+row.messages_json,'sha1').slice(0,10)+'_raw');
  const bounds=turn(a,session,query);const timesBounded=within(row.timestamp,bounds)&&within(row.created_at,bounds);
  return{rowIndex:tables.l0_sessions.indexOf(row),session,query,id:row.l0_index_id,timesBounded,run:bounds.run};
 });
 assert.deepEqual(tables.pipeline_state.map(r=>r.state_key),['recentCaseTraces','workspaceDir']);const workspace=one(tables.pipeline_state.filter(r=>r.state_key==='workspaceDir'),'exact workspace row');assert.deepEqual(Object.keys(workspace),['state_key','state_json','updated_at']);
 const state=one(tables.pipeline_state.filter(r=>r.state_key==='recentCaseTraces'),'exact pipeline row');const cases=JSON.parse(state.state_json);assert.equal(JSON.stringify(cases),state.state_json,'actual serialized state bytes');assert.equal(cases.length,3);
 assert.deepEqual(Object.keys(state),['state_key','state_json','updated_at']);
 const ids=new Set(),traceIds=new Set();let allTimes=true;const timestampWitness=[];
 const canonical=cases.map((c,i)=>{
  assert.deepEqual(Object.keys(c),['caseId','sessionKey','query','startedAt','finishedAt','status','retrieval','toolEvents','assistantReply']);
  const [session,query]=EXPECT[2-i];assert.equal(c.sessionKey,session);assert.equal(c.query,query);assert.equal(c.status,'completed');assert.match(c.caseId,/^case_trace_[a-f0-9]{10}$/);assert.ok(!ids.has(c.caseId));ids.add(c.caseId);
  assert.deepEqual(Object.keys(c.retrieval),['intent','injected','contextPreview','trace']);
  const tr=c.retrieval.trace;assert.deepEqual(Object.keys(tr),['traceId','query','mode','startedAt','finishedAt','steps']);assert.equal(tr.query,query);assert.match(tr.traceId,/^trace_[a-f0-9]{10}$/);assert.ok(!traceIds.has(tr.traceId));traceIds.add(tr.traceId);assert.equal(tr.steps.length,7);
  tr.steps.forEach((s,j)=>assert.equal(s.stepId,tr.traceId+':step:'+(j+1)));
  const bounds=turn(a,session,query);const times=[c.startedAt,c.finishedAt,tr.startedAt,tr.finishedAt];allTimes&&=times.every(t=>within(t,bounds));for(const[field,raw]of [['startedAt',c.startedAt],['finishedAt',c.finishedAt],['retrieval.trace.startedAt',tr.startedAt],['retrieval.trace.finishedAt',tr.finishedAt]]){iso(raw);const normalized=normalizeTimestamp(raw);assert.equal(normalized,'<iso>','exact whole-field existing ISO eligibility');timestampWitness.push({caseIndex:i,session,query,field,raw,normalized,existingPermission:'diff.mjs ISO timestamp rule; no new producer/range requirement'});}assert.ok(iso(c.startedAt)<=iso(c.finishedAt));assert.ok(iso(tr.startedAt)<=iso(tr.finishedAt));
  const copy=structuredClone(c);delete copy.caseId;delete copy.startedAt;delete copy.finishedAt;delete copy.retrieval.trace.traceId;delete copy.retrieval.trace.startedAt;delete copy.retrieval.trace.finishedAt;copy.retrieval.trace.steps.forEach(s=>delete s.stepId);return copy;
 });
 const parent=a.tree[PARENT];assert.equal(parent.jsonl.length,62);assert.equal(parent.raw.lines,62);assert.equal(parent.raw.finalNewline,true);
 const bytes=Buffer.from(parent.jsonl.map(r=>JSON.stringify(r)).join('\n')+'\n');const persisted=a.evidence.persistedFiles[PARENT];assert.equal(bytes.length,persisted.byteLength);assert.equal(hash(bytes),persisted.sha256);assert.equal(a.steps[6].frames[0].result.session.fileSize,bytes.length);
 return{workspaceState:workspace.state_json,l0,pipeline:{canonical,timesBounded:allTimes,timestampWitness,timestampPermission:'existing frozen normalizer ISO rule reused; same-run bounds are diagnostic only, not new eligibility range',entropyHashRecomputable:false,caseIds:[...ids],traceIds:[...traceIds]},file:{bytes:bytes.length,sha256:hash(bytes)},timesBounded:allTimes&&l0.every(r=>r.timesBounded)};
}
export function memoryObserverWitness(a,b,baselineRoot){
 try{
  assert.equal(a.side,'ts');assert.ok(['ts','rust'].includes(b.side));assert.equal(a.evidence.scenario.sha256,'d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a');assert.equal(a.evidence.scenario.sha256,b.evidence.scenario.sha256);
  const sides=[side(a),side(b)];assert.equal(sides[0].workspaceState,sides[1].workspaceState,'non-target pipeline workspace unchanged');assert.deepEqual(sides[0].pipeline.canonical,sides[1].pipeline.canonical,'all non-target memory business payload exact');
  // Existing independent lifecycle/context proof checks complete durable body
  // producers. It is executed internally, never supplied as a caller boolean.
  const provenance=pair(a,b,baselineRoot);assert.equal(provenance.registryEqualAfterProvenWindowsAndNumeric,true);
  // 60 non-target rows come from Inventory's full typed payload proof;
  // the remaining two are the strict lifecycle-bound reviewer producers.
  // Union must be bijective over every original row; never weaken 62 to 60.
  const lifecycle=lifecyclePair(a,b),parentRows=provenance.uuid.fullPayloadRows.filter(r=>r.scope==='parent');assert.equal(parentRows.length,60,'exact non-target parent proof');
  const parse=(path,expected)=>{assert.ok(path.startsWith('$.tree.'+expected+'.jsonl['));const m=/\.jsonl\[(\d+)\]$/.exec(path);assert.ok(m);return Number(m[1]);};
  const fullRows=[a,b].map((raw,side)=>{
   const proof=side?lifecycle.right:lifecycle.left;assert.equal(proof.parentPath,PARENT);const rows=raw.tree[PARENT].jsonl;
   const ordinary=new Map(parentRows.map(r=>[parse(side?r.rightPath:r.leftPath,PARENT),r]));assert.equal(ordinary.size,60,'non-target bijection');
   const targets=[['started',proof.durableStart],['completed',proof.durableCompleted]].map(([kind,path])=>({kind,path,index:parse(path,PARENT)}));assert.equal(new Set(targets.map(x=>x.index)).size,2);
   for(const target of targets){const row=rows[target.index];assert.ok(!ordinary.has(target.index),'no target/non-target overlap');assert.equal(row.subagentId,proof.anchors.reviewer);assert.equal(row.subagentType,'reviewer');assert.equal(row.sessionId,'agent03-parent');assert.equal(row.turnId,proof.anchors.parentRun0);iso(row.createdAt);
    if(target.kind==='started'){assert.deepEqual(Object.keys(row),['type','subagentId','subagentType','promptPreview','promptTruncated','transcriptRelativePath','subagentSessionId','sessionId','turnId','sequence','createdAt','entryId','parentEntryId']);assert.equal(row.type,'subagent_started');assert.equal(row.promptPreview,proof.digest);assert.equal(row.promptTruncated,false);assert.equal(row.transcriptRelativePath,'agent03-parent/subagents/'+proof.anchors.reviewer+'.jsonl');assert.equal(row.subagentSessionId,'/tmp/pdrs-bbx/root/home/.pilotdeck::sub::'+proof.anchors.reviewer);}
    else {assert.deepEqual(Object.keys(row),['type','subagentId','subagentType','summaryPreview','summaryTruncated','usage','turns','durationMs','errored','sessionId','turnId','sequence','createdAt','entryId','parentEntryId']);assert.equal(row.type,'subagent_completed');assert.equal(row.summaryPreview,'AG03-OBSERVER-REPORT');assert.equal(row.summaryTruncated,false);assert.deepEqual(row.usage,{inputTokens:12,outputTokens:4,totalTokens:16});assert.equal(row.turns,1);assert.equal(row.errored,false);const wire=one(proof.reviewerEvents.filter(e=>e.original.event.event==='subagent_completed'),'same reviewer completion wire').original.event;assert.equal(row.durationMs,wire.detail.durationMs,'durable duration comes from exact same completion producer');assert.ok(Number.isFinite(row.durationMs)&&row.durationMs>=0);}
   }
   const coverage=rows.map((row,index)=>{const target=targets.find(t=>t.index===index);assert.ok(target||ordinary.has(index),'all62 rows accounted');const text=JSON.stringify(row)+'\n';return{index,path:'$.tree.'+PARENT+'.jsonl['+index+']',entryId:row.entryId,type:row.type,proof:target?'strict-reviewer-lifecycle:'+target.kind:'inventory-full-typed-payload',text,byteLength:Buffer.byteLength(text),sha256:hash(text)};});
   assert.equal(coverage.length,62);assert.equal(coverage.filter(r=>r.proof==='inventory-full-typed-payload').length,60);assert.equal(coverage.filter(r=>r.proof.startsWith('strict-reviewer')).length,2);
   const bytes=Buffer.from(coverage.map(r=>r.text).join(''));assert.deepEqual(raw.evidence.persistedFiles[PARENT],{byteLength:bytes.length,sha256:hash(bytes)},'complete62 exact bytes+SHA witness');return{rowCount:62,typedPayloadRows:60,lifecycleRows:2,byteLength:bytes.length,sha256:hash(bytes),rows:coverage};
  });
  assert.deepEqual(sides[0].pipeline.timestampWitness.map(x=>x.normalized),sides[1].pipeline.timestampWitness.map(x=>x.normalized),'all12 pipeline fields use existing identical timestamp normalization');
  const unknown=[];
  if(provenance.uuid.unknownRows.length)unknown.push('full durable payload rows not completely proven');
  return{status:unknown.length?'unknown':'proven',privateOnly:false,formalAdmission:false,requiresUserApproval:false,unknown,permissions:{timestamps:'existing ISO rule reused, no new time grant',caseTraceIds:'User-approved exact schema bimap only; not accepted or hash entropy recomputation'},sides,fullParentWitness:fullRows,entropy:'hash inputs absent; case/trace graph only, never entropy recomputation',paths:[...[0,1,2].map(i=>`$.tree.${DB}.sqlite.tables.l0_sessions[${i}].l0_index_id`),...[0,1].map(i=>`$.tree.${DB}.sqlite.tables.l0_sessions[${i}].messages_json`),`$.tree.${DB}.sqlite.tables.pipeline_state[0].state_json`,'$.steps[6].frames[0].result.session.fileSize']};
 }catch(error){return{status:'invalid',privateOnly:false,formalAdmission:false,error:error.message};}
}

export function observerMemoryWitnessPaths(a,b,baselineRoot){
 if(![a,b].some(x=>x.scenario==='agent03-sdk-fork-lifecycle-probe'))return new Map();
 const proof=memoryObserverWitness(a,b,baselineRoot);assert.equal(proof.status,'proven',proof.error??JSON.stringify(proof.unknown));
 return new Map(proof.paths.map(p=>[p,'User-approved exact memory schema/bimap/collector rows/fileSize; complete source and byte witnesses']));
}
