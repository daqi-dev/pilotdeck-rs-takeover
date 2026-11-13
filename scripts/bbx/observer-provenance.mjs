// Strict source-derived observer provenance; no admission Map is supplied by callers.
import assert from 'node:assert/strict';
import path from 'node:path';import{fileURLToPath}from'node:url';import{createHash}from'node:crypto';
import{load,settleTitleRecords}from'./normalization.mjs';
import{lifecyclePair}from'./observer-lifecycle-policy.mjs';
import{observerUuidViews}from'./observer-uuid-policy.mjs';
import{sdkAgentAssociations,canonicalBreakdown,loadPinnedTokenizer}from'./sdk-agent-policy.mjs';
const sha=x=>createHash('sha256').update(x).digest('hex'),one=(xs,label)=>{assert.equal(xs.length,1,label);return xs[0];};
const regpath='/home/.pilotdeck/runs/registry.json';
const baselinePath=value=>value??process.env.BBX_BASELINE??path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../../resources/PilotDeck');
const get=(x,p)=>p.split('.').reduce((v,k)=>v?.[k],x);
const derived=['used','displayUsed','localEstimateTokens','usageTokens','calibrationEstimatedInputTokens','ratio','breakdown.total','breakdown.messages'];
export function observerSingleProvenance(a,baselineRoot){
 const count=loadPinnedTokenizer(baselinePath(baselineRoot));
 const bindings=sdkAgentAssociations(a),{rows,turns,requests,history}=bindings;const proofs=[];
 const statuses=a.tree[Object.keys(a.tree).find(p=>p.endsWith('/chats/agent03-parent.jsonl'))+'.projections.json'].json.projections['web-history.agent-statuses'].state;
 let previous;
 for(const [ordinal,b]of requests.entries()){
  const request=b.request.value.request;const full=count(request.systemPrompt);const blocks=[...request.systemPrompt.matchAll(/<memory-context>[\s\S]*?<\/memory-context>/g)];assert.ok(blocks.length<=1);assert.ok(!request.systemPrompt.includes('<mcp-instructions>'));const memory=blocks.length?Math.min(full,count(blocks[0][0])):0;
  // Reuse the existing exact canonical message/tool rules. Emptying prompt
  // only isolates the independent full-system+delimiter calculation above.
  const expected=canonicalBreakdown({...request,systemPrompt:''},count);expected.system=full-memory;expected.memory=memory;expected.total+=full;
  const event=b.budget.value.detail;assert.deepEqual(event.breakdown,expected,'independent complete canonical estimate');assert.equal(event.localEstimateTokens,expected.total);
  if(ordinal===0){const initial={type:'context_budget',used:expected.total,displayUsed:expected.total,localEstimateTokens:expected.total,estimateSource:'estimator',total:65536,totalContextTokens:65536,maxContextTokens:57344,effectiveTotal:57344,effectiveContextTokens:57344,maxOutputTokens:8192,reservedOutputTokens:8192,warningRatio:0.8,blockingRatio:0.9,ratio:expected.total/57344,state:'ok',source:'local',exact:false,breakdown:expected,runId:b.run};assert.deepEqual(event,initial,'complete initial budget equation');}else{const limit=Math.floor(expected.total/2);const used=expected.total+Math.max(-limit,Math.min(limit,12-previous));assert.equal(event.used,used);assert.equal(event.displayUsed,used);assert.equal(event.usageTokens,used);assert.equal(event.calibrationActualInputTokens,12);assert.equal(event.calibrationEstimatedInputTokens,previous);assert.equal(event.ratio,used/57344);assert.equal(event.source,'calibrated');}
  previous=expected.total;
  const step=a.steps[b.stepIndex];const frame=one(step.frames.map((f,i)=>({f,i})).filter(x=>x.f.event?.type==='context_budget'&&x.f.event.runId===b.run&&x.f.event.breakdown.messages===expected.messages),'unique actual budget frame');assert.deepEqual(frame.f.event,event,'WS equals durable budget');
  const bucket=a.tree[regpath].json.events['\0agent03-parent\0'+b.run];const re=one(bucket.map((v,i)=>({v,i})).filter(x=>x.v.event.type==='context_budget'&&x.v.event.breakdown.messages===expected.messages),'unique registry budget');assert.deepEqual(re.v.event,event,'registry equals same budget');
  const hist=one(history.messages.map((v,i)=>({v,i})).filter(x=>x.v.id===b.budget.value.entryId),'history exact entry id');assert.equal(hist.v.turnId,b.run);assert.equal(hist.v.sequence,b.budget.value.sequence);assert.deepEqual(hist.v.payload.detail,event);
  assert.deepEqual(one(statuses.filter(x=>x.entryId===b.budget.value.entryId),'projection exact entry'),b.budget.value);
  proofs.push({key:b.name+':'+b.step,run:b.run,entryId:b.budget.value.entryId,step:b.stepIndex,wire:frame.i,registry:re.i,durable:b.budget.index,history:hist.i,estimate:expected,event});
 }
 // Bind each actual public-history kind to its typed durable producer.
 // Virtual display IDs are not durable entry IDs; tool rows use their
 // actual toolCallId producer/consumer instead of an array-index guess.
 const parentSourcePath=one(Object.keys(a.tree).filter(p=>a.tree[p].jsonl===rows),'history durable source path');
 const projectionPath=parentSourcePath+'.projections.json';
 const subagentState=a.tree[projectionPath].json.projections['agent-transcript.subagent-references'].state;
 assert.deepEqual(Object.keys(subagentState),['started','completed'],'complete subagent projection members');
 const subagentProjectionOwnership=[];
 for(const [kind,type]of[['started','subagent_started'],['completed','subagent_completed']]){
  const actual=rows.filter(r=>r.type===type);assert.deepEqual(subagentState[kind],actual,'entire subagent projection equals actual durable producers');
  actual.forEach((row,i)=>{const ordinal=rows.indexOf(row);assert.ok(ordinal>0);assert.equal(row.parentEntryId,rows[ordinal-1].entryId,'subagent exact predecessor');subagentProjectionOwnership.push({kind,index:i,role:row.subagentType,entryId:row.entryId,parentEntryId:row.parentEntryId,durable:ordinal,durablePath:`$.tree.${parentSourcePath}.jsonl[${ordinal}]`,projectionPath:`$.tree.${projectionPath}.json.projections.agent-transcript.subagent-references.state.${kind}[${i}]`});});
 }
 const projectionEntryOwnership=[...subagentProjectionOwnership];
 const conversation=a.tree[projectionPath].json.projections['agent-transcript.conversation'].state.entries;
 const conversationRows=rows.map((entry,index)=>({entry,index})).filter(({entry})=>['accepted_input','assistant_message','tool_result_message','turn_result'].includes(entry.type));
 assert.deepEqual(conversation,conversationRows,'complete conversation projection durable producers');
 conversation.forEach(({entry,index},i)=>projectionEntryOwnership.push({entryId:entry.entryId,parentEntryId:entry.parentEntryId,durable:index,type:entry.type,projectionPath:`$.tree.${projectionPath}.json.projections.agent-transcript.conversation.state.entries[${i}].entry`}));
 assert.deepEqual(statuses,rows.filter(r=>r.type==='agent_status_message'),'complete status projection durable producers');
 statuses.forEach((entry,i)=>projectionEntryOwnership.push({entryId:entry.entryId,parentEntryId:entry.parentEntryId,durable:rows.indexOf(entry),type:entry.type,projectionPath:`$.tree.${projectionPath}.json.projections.web-history.agent-statuses.state[${i}]`}));
 const historyOwnership=history.messages.map((h,i)=>{
  let row;
  if(h.kind==='text'){
   row=one(rows.filter(r=>r.entryId===h.entryId),'history text durable entry');
   assert.equal(row.type,h.role==='user'?'accepted_input':'assistant_message','typed text producer');
   const messages=h.role==='user'?row.messages:[row.message];
   const blocks=messages.flatMap(m=>m.content??[]).filter(b=>b.type==='text');
   assert.equal(blocks.length,1,'exact text block membership');assert.equal(h.text,blocks[0].text,'history complete text payload');
  }else if(h.kind==='status'){
   row=one(rows.filter(r=>r.entryId===h.id),'history status durable entry');assert.equal(row.type,'agent_status_message');assert.equal(h.role,'system');assert.equal(h.payload.event,row.event);assert.deepEqual(h.payload.detail,row.detail);
  }else if(h.kind==='tool_use'){
   const producers=rows.filter(r=>r.type==='assistant_message'&&r.message?.content?.some(b=>b.type==='tool_call'&&b.id===h.toolCallId));
   row=one(producers,'history tool call durable producer');const call=one(row.message.content.filter(b=>b.type==='tool_call'&&b.id===h.toolCallId),'history exact tool call');assert.equal(h.role,'tool');assert.equal(h.toolName,call.name);assert.deepEqual(h.payload,call.input);
  }else if(h.kind==='tool_result'){
   const resultRow=one(rows.filter(r=>r.type==='tool_result'&&r.result?.toolCallId===h.toolCallId),'actual tool result producer');
   row=one(rows.filter(r=>r.type==='tool_result_message'&&r.message?.content?.some(b=>b.type==='tool_result'&&b.toolCallId===h.toolCallId)),'history tool result durable message');const block=one(row.message.content.filter(b=>b.type==='tool_result'&&b.toolCallId===h.toolCallId),'exact tool result message block');assert.deepEqual(block.raw,resultRow.result,'history result message refers to actual result');assert.equal(h.role,'tool');assert.equal(h.toolName,resultRow.result.toolName);assert.equal(h.ok,resultRow.result.type==='success');
   const text=block.content.map(b=>{assert.equal(b.type,'text','bounded text-only history tool result');return b.text;}).join('\n');assert.equal(h.text,text,'history complete tool result payload');
  }else assert.fail('unproved history kind');
  assert.equal(h.sessionKey,row.sessionId);assert.equal(h.turnId,row.turnId);assert.equal(h.sequence,row.sequence);assert.equal(h.createdAt,row.createdAt);
  return{historyPath:`${bindings.historyPath}.messages[${i}]`,kind:h.kind,durablePath:`$.tree.${parentSourcePath}.jsonl[${rows.indexOf(row)}]`,durable:rows.indexOf(row),entryId:row.entryId,turnId:row.turnId,sequence:row.sequence};
 });
 const last=proofs.at(-1),lastEvent=last.event;
 const expectedUsage={used:lastEvent.used,displayUsed:lastEvent.displayUsed,total:65536,effectiveTotal:57344,reservedOutputTokens:8192,state:'ok',ratio:lastEvent.ratio,source:'history',exact:true};
 assert.deepEqual(history.tokenUsage,expectedUsage,'history final context tokenUsage projection');
 const usageProjection=a.tree[Object.keys(a.tree).find(p=>p.endsWith('/chats/agent03-parent.jsonl'))+'.projections.json'].json.projections['web-history.token-usage'].state.latestContextBudget;
 assert.equal(usageProjection.index,last.durable,'latest usage projection durable source');assert.deepEqual(usageProjection.usage,expectedUsage,'latest usage projection payload');
 const registry=a.tree[regpath].json;const counters=turns.map(t=>{const record=one(registry.records.filter(r=>r.runId===t.run&&r.sessionKey==='agent03-parent'),'actual run record');const bucket=registry.events['\0agent03-parent\0'+t.run];assert.deepEqual(bucket.map(r=>r.event),a.steps[t.stepIndex].frames.filter(f=>!f.final).map(f=>f.event));bucket.forEach((r,i)=>assert.equal(r.seq,i+1));assert.equal(record.lastSeq,bucket.length);assert.equal(record.revision,bucket.length+1);assert.deepEqual(record.result,bucket.at(-1).event);return{run:t.run,step:t.stepIndex,eventCount:bucket.length,lastSeq:record.lastSeq,revision:record.revision};});
 const historyBindings=history.messages.map((h,i)=>({h,i})).filter(x=>[4,5].includes(x.i)).map(({h,i})=>{const row=one(rows.filter(r=>r.entryId===(h.entryId??h.id)),'history message durable identity');assert.equal(h.sequence,row.sequence);assert.equal(h.turnId,row.turnId);return{history:i,durable:rows.indexOf(row),entryId:row.entryId,sequence:row.sequence,type:row.type};});
 const rf=a.tree[regpath], skeleton=line=>line.replace(/("(?:duration_ms|duration_seconds|durationMs)"\s*:\s*)-?[\d.eE+-]+/g,'$1D').replace(/"(?:[^"\\]|\\.)*"|-?\d+(\.\d+)?([eE][+-]?\d+)?/g,(tok,frac,exp)=>tok[0]==='"'?`"${(tok.match(/\\(?:u[0-9a-fA-F]{4}|.)/g)??[]).join('')}"`:exp?'0e0':frac?'0.0':'0');
 const skeletonHash=sha(skeleton(JSON.stringify(rf.json))).slice(0,16);assert.deepEqual(rf.raw,{lines:1,finalNewline:false,skeletons:{[skeletonHash]:1}},'entire actual registry raw skeleton derivation, no unknown formatting');
 const report=one(a.steps[0].frames.filter(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent'),'worker report event').event;assert.equal(report.resultBytes,Buffer.byteLength(report.resultPreview,'utf8'));return{proofs,counters,historyBindings,historyOwnership,subagentProjectionOwnership,projectionEntryOwnership,tokenUsage:{sourceEntryId:last.entryId,durable:last.durable,historyPath:`${bindings.historyPath}.tokenUsage`,projectionPath:`$.tree.${parentSourcePath}.projections.json.json.projections.web-history.token-usage.state.latestContextBudget`,value:expectedUsage},skeletonHash,reportBytes:report.resultBytes};
}
export function observerPairProvenance(a,b,baselineRoot){
 const lifecycle=lifecyclePair(a,b),sides=[observerSingleProvenance(a,baselineRoot),observerSingleProvenance(b,baselineRoot)],aliases=[new Map(),new Map()];const views=[a,b].map((r,i)=>load(structuredClone(r),undefined,aliases[i]));const stable=observerUuidViews(a,b,...views,...aliases);const maps=[new Map(),new Map()],refs=[];const raw=[a,b];
 function bind(l,r,label){assert.match(l,/^[a-f0-9-]{36}$/);assert.match(r,/^[a-f0-9-]{36}$/);for(const [side,id]of[l,r].entries()){if(maps[side].has(id))assert.equal(maps[side].get(id),label,'duplicate/many-to-one mapping');else maps[side].set(id,label);}}
 const rowsFor=(side,p)=>raw[side].tree[p].jsonl;const pars=[lifecycle.left.parentPath,lifecycle.right.parentPath];
 const parentPairs=lifecycle.nonTargetDurableComparisons.map(x=>[Number(/\[(\d+)\]$/.exec(x.left)[1]),Number(/\[(\d+)\]$/.exec(x.right)[1])]);
 function addRows(paths,pairs,scope){const rr=paths.map((p,i)=>rowsFor(i,p));for(const[i,j]of pairs){const x=rr[0][i],y=rr[1][j];assert.equal(x.type,y.type,'same unique semantic row kind');bind(x.entryId,y.entryId,`${scope}:entry:${refs.length}`);refs.push({paths,left:i,right:j,scope});}}
 addRows(pars,parentPairs,'parent');
 for(const role of['worker','reviewer']){const paths=raw.map((r,side)=>{const child=one(r.tree[pars[side]].jsonl.filter(x=>x.type==='subagent_started'&&x.subagentType===role),'child role reference');assert.equal(child.subagentId,lifecycle[side?'right':'left'].anchors[role]);return one(Object.keys(r.tree).filter(p=>p.endsWith('/'+child.transcriptRelativePath)&&r.tree[p].jsonl),'exact child file');});const rr=paths.map((p,i)=>rowsFor(i,p));assert.equal(rr[0].length,rr[1].length,'same child complete membership');addRows(paths,rr[0].map((_,i)=>[i,i]),role);}
 // Only operation producers with a unique same-run completed consumer can
 // establish an operation relation; never map arbitrary UUID-shaped values.
 for(const ref of refs){const rr=ref.paths.map((p,i)=>rowsFor(i,p)),rows=[rr[0][ref.left],rr[1][ref.right]];if(rows[0].type!=='compaction_started')continue;assert.equal(rows[1].type,'compaction_started');for(const[i,row]of rows.entries()){const consumers=rr[i].filter(x=>x.type==='compaction_completed'&&x.operationId===row.operationId&&x.turnId===row.turnId);assert.equal(consumers.length,1);assert.ok(consumers[0].sequence>row.sequence);assert.equal(rr[i].filter(x=>x.type==='compaction_started'&&x.operationId===row.operationId).length,1);}bind(rows[0].operationId,rows[1].operationId,`${ref.scope}:operation:${ref.left}`);}
 const candidateBijection=[...maps[0]].map(([left,label])=>{const right=one([...maps[1]].filter(([,v])=>v===label),'reverse unique identity')[0];const counts=raw.map((a,i)=>JSON.stringify(Object.fromEntries(Object.entries(a).filter(([k])=>!['evidence','side','serverLog','startMs','failures'].includes(k)))).split(i?right:left).length-1);return{label,left,right,referenceCounts:counts};});
 const referenceOwnership=[];
 function occurrencePaths(value,id,p='$',found=[]){if(typeof value==='string'){if(value.includes(id))found.push({path:p,exact:value===id});}else if(Array.isArray(value))value.forEach((v,i)=>occurrencePaths(v,id,`${p}[${i}]`,found));else if(value&&typeof value==='object')for(const[k,v]of Object.entries(value)){if(k.includes(id))found.push({path:`${p}.{key:${k}}`,exact:false});occurrencePaths(v,id,`${p}.${k}`,found);}return found;}
 for(const relation of candidateBijection.filter(x=>x.referenceCounts[0]!==x.referenceCounts[1]||sides.some((proof,side)=>proof.subagentProjectionOwnership.some(edge=>edge.role==='reviewer'&&edge.parentEntryId===(side?x.right:x.left))))){
  const sideProof=raw.map((artifact,side)=>{
   const id=side?relation.right:relation.left,p=pars[side],rr=artifact.tree[p].jsonl;
   const expected=[];
   rr.forEach((row,i)=>{if(row.entryId===id)expected.push({path:`$.tree.${p}.jsonl[${i}].entryId`,role:'producer'});if(row.parentEntryId===id){assert.ok(i>0);assert.equal(rr[i-1].entryId,id);expected.push({path:`$.tree.${p}.jsonl[${i}].parentEntryId`,role:'durable-successor'});}});
   for(const edge of sides[side].projectionEntryOwnership)for(const field of['entryId','parentEntryId'])if(edge[field]===id)expected.push({path:edge.projectionPath+'.'+field,role:`projection:${edge.kind??edge.type}:${edge.role??'non-reviewer'}:${field}`,ownerEntryId:edge.entryId,field});
   const compared=Object.fromEntries(Object.entries(artifact).filter(([k])=>!['evidence','side','serverLog','startMs','failures'].includes(k)));
   const actual=occurrencePaths(compared,id);const wanted=new Set(expected.map(x=>x.path));
   const unknown=actual.filter(x=>!x.exact||!wanted.has(x.path));const absent=expected.filter(x=>!actual.some(y=>y.path===x.path&&y.exact));
   return{references:expected,unknown,absent};
  });
  const parentEdges=[];
  for(const[side,proof]of sideProof.entries())for(const ref of proof.references.filter(x=>x.field==='parentEntryId')){
   const rr=raw[side].tree[pars[side]].jsonl,other=raw[1-side].tree[pars[1-side]].jsonl;
   const owner=one(rr.filter(r=>r.entryId===ref.ownerEntryId),'projected edge owner');
   const target=row=>['subagent_started','subagent_completed'].includes(row.type)&&row.subagentType==='reviewer';
   if(target(owner)){parentEdges.push({side,path:ref.path,owner:owner.entryId,reason:'actual independently window-bound reviewer producer'});continue;}
   const label=maps[side].get(owner.entryId);assert.ok(label,'projected non-target owner has typed counterpart');
   const counterpart=one(other.filter(r=>maps[1-side].get(r.entryId)===label),'projected edge counterpart');
   const previous=(rows,row)=>{let i=rows.indexOf(row)-1;while(i>=0&&target(rows[i]))i--;assert.ok(i>=0);return rows[i];};
   assert.equal(maps[side].get(previous(rr,owner).entryId),maps[1-side].get(previous(other,counterpart).entryId),'same exact predecessor when only proven reviewer rows are removed');
   parentEdges.push({side,path:ref.path,owner:owner.entryId,counterpart:counterpart.entryId,reason:'complete copied producer and exact predecessor after only proven reviewer rows removed'});
  }
  const stableRoles=sideProof.map(p=>p.references.filter(x=>x.field!=='parentEntryId').map(x=>x.role).sort());
  const closed=sideProof.every(p=>!p.unknown.length&&!p.absent.length)&&JSON.stringify(stableRoles[0])===JSON.stringify(stableRoles[1]);
  referenceOwnership.push({label:relation.label,left:relation.left,right:relation.right,closed,sideProof,parentEdges,reason:'only actual reviewer started/completed projection predecessor edges may differ; complete projection and durable predecessor are independently verified'});
 }
 const keymaps=maps.map((m,i)=>new Map([...m].map(([id,label])=>[`<uuid:${stable.aliases[i].get(id)}>`,label])));
 function canon(v,side){if(typeof v==='string')return v.replace(/<uuid:\d+>/g,x=>keymaps[side].has(x)?'<proved:'+keymaps[side].get(x)+'>':x);if(Array.isArray(v))return v.map(x=>canon(x,side));if(v&&typeof v==='object')return Object.fromEntries(Object.entries(v).map(([k,x])=>[canon(k,side),canon(x,side)]));return v;}
 const titleSlots=raw.map(artifact=>new Map(Object.entries(artifact.tree).filter(([,file])=>Array.isArray(file.jsonl)).map(([p,file])=>[p,settleTitleRecords(structuredClone(file.jsonl))])));
 const budgets=sides.map(s=>new Map(s.proofs.map(p=>[p.entryId,p])));const checked=[],unknown=[],normalizedSlotBindings=[];
 for(const ref of refs){const rr=ref.paths.map((p,i)=>rowsFor(i,p)),original=[rr[0][ref.left],rr[1][ref.right]];const values=original.map((row,i)=>{const npath=ref.paths[i].replace(/[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}/g,id=>`<uuid:${stable.aliases[i].get(id)}>`);const slot=titleSlots[i].get(ref.paths[i])?.get(row.entryId)??row;const entry=one(stable.views[i].tree[npath].jsonl.filter(x=>x.entryId===`<uuid:${stable.aliases[i].get(slot.entryId)}>`),'normalized row bound to exact existing title slot');assert.equal(entry.type,row.type,'normalized slot retains actual payload producer kind');normalizedSlotBindings.push({side:i,rawPath:`$.tree.${ref.paths[i]}.jsonl[${i?ref.right:ref.left}]`,producerEntryId:row.entryId,normalizedSlotEntryId:slot.entryId,normalizedSequence:slot.sequence,type:row.type});const copy=canon(structuredClone(entry),i);copy.entryId='<proved:'+maps[i].get(row.entryId)+'>';delete copy.sequence;delete copy.parentEntryId; // Locally validated actual chain, not a comparison exemption.
 if(budgets[i].has(row.entryId)){assert.equal(copy.event,'context_budget');for(const field of derived){const p=field.split('.');if(p.length===1)copy.detail[p[0]]='<validated:'+field+'>';else copy.detail[p[0]][p[1]]='<validated:'+field+'>';}}
 return copy;});try{assert.equal(JSON.stringify(values[0]),JSON.stringify(values[1]),'full payload including key order');checked.push({scope:ref.scope,leftPath:`$.tree.${ref.paths[0]}.jsonl[${ref.left}]`,rightPath:`$.tree.${ref.paths[1]}.jsonl[${ref.right}]`,leftEntry:original[0].entryId,rightEntry:original[1].entryId});}catch{unknown.push({scope:ref.scope,type:original[0].type,left:ref.left,right:ref.right});}}
 // No accepted-ID Map is emitted. Candidate graph requires full payload
 // closure; unresolved rows remain Unknown, including any refs into them.
 const registryCanonical=stable.views.map((view,side)=>{const copy=canon(structuredClone(view.tree[regpath].json),side);const anchor=lifecycle[side?'right':'left'].anchors;const reviewerToken=`<uuid:${stable.aliases[side].get(anchor.reviewer)}>`;for(const record of copy.records){if(sides[side].counters.some(x=>`<uuid:${stable.aliases[side].get(x.run)}>`===record.runId)){delete record.revision;delete record.lastSeq;}}
 for(const[key,bucket]of Object.entries(copy.events)){const originals=view.tree[regpath].json.events[key];copy.events[key]=bucket.filter((r,i)=>!(['subagent_text_delta','subagent_assistant_block','subagent_completed'].includes(r.event.event)&&originals[i].event.detail?.subagentId===reviewerToken)).map(r=>{delete r.seq;if(r.event.type==='tool_call_finished'&&r.event.toolCallId==='ag03-agent'){assert.equal(r.event.resultBytes,sides[side].reportBytes);r.event.resultBytes='<already-approved-exact-report-bytes>';}if(r.event.type==='context_budget'&&sides[side].proofs.some(p=>`<uuid:${stable.aliases[side].get(p.run)}>`===r.event.runId)){const known=sides[side].proofs.find(p=>`<uuid:${stable.aliases[side].get(p.run)}>`===r.event.runId&&p.estimate.messages===r.event.breakdown.messages);assert.ok(known,'semantic registry budget known');for(const field of derived){const ks=field.split('.');if(ks.length===1)r.event[ks[0]]='<validated:'+field+'>';else r.event[ks[0]][ks[1]]='<validated:'+field+'>';}}return r;});}return copy;});
 const registryEqualAfterProvenWindowsAndNumeric=JSON.stringify(registryCanonical[0])===JSON.stringify(registryCanonical[1]);
 const registryUnknown=[];function delta(a,b,p='$'){if(JSON.stringify(a)===JSON.stringify(b))return;if(a&&b&&typeof a==='object'&&typeof b==='object'&&Array.isArray(a)===Array.isArray(b)){if(JSON.stringify(Object.keys(a))!==JSON.stringify(Object.keys(b)))registryUnknown.push({path:p+'.keys',left:Object.keys(a),right:Object.keys(b)});for(const k of new Set([...Object.keys(a),...Object.keys(b)]))delta(a[k],b[k],p+'.'+k);}else registryUnknown.push({path:p,left:a,right:b});}delta(...registryCanonical);return{registryUnknown,registryEqualAfterProvenWindowsAndNumeric,historyBindings:sides.map(s=>s.historyBindings),historyOwnership:sides.map(s=>s.historyOwnership),subagentProjectionOwnership:sides.map(s=>s.subagentProjectionOwnership),tokenUsage:sides.map(s=>s.tokenUsage),registrySkeletonWitnesses:sides.map(s=>s.skeletonHash),lifecycleWindows:[lifecycle.left.completionWindow,lifecycle.right.completionWindow],uuid:{normalizedSlotBindings,closureScope:'checked durable row payloads, typed public-history producers and global occurrence counts only; not every raw reference path',wholeGraphPathOwnershipProved:false,candidateRelations:maps[0].size,candidateBijection,referenceOwnership,unresolvedReferenceOwnership:referenceOwnership.filter(r=>!r.closed),referenceCountDifferences:candidateBijection.filter(x=>x.referenceCounts[0]!==x.referenceCounts[1]),fullPayloadRows:checked,unknownRows:unknown,completeClosure:unknown.length===0&&referenceOwnership.every(x=>x.closed)},budget:sides.map(s=>s.proofs),counters:sides.map(s=>s.counters),counterDelta:sides[1].counters.map((c,i)=>c.eventCount-sides[0].counters[i].eventCount),rawSkeleton:'Each side entire raw skeleton exactly reconstructed from complete parsed registry. Cross-side semantic canonical proof result is separate; no generic hash grant.'};
}
