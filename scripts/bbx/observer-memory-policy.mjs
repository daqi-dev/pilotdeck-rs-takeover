// User-approved D1 SDK memory exceptions. Emits individually validated leaf paths;
// does not alter captures, observer lifecycle, UUID relations or formal scripts.
import assert from 'node:assert/strict';
import {isDeepStrictEqual} from 'node:util';
import {validateAgentReport} from './sdk-agent-policy.mjs';
const SHA='d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a';
const BASE='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const HEADERS=['host','connection','content-type','authorization','accept','accept-language','sec-fetch-mode','user-agent','accept-encoding','content-length'];
const one=(xs,label)=>{assert.equal(xs.length,1,label);return xs[0]};
const frames=a=>a.steps.flatMap((s,si)=>(s.frames??[]).map((f,fi)=>({si,fi,f})));
const bytes=x=>Buffer.byteLength(typeof x==='string'?x:JSON.stringify(x),'utf8');
function identity(a){assert.equal(a.scenario,'agent03-sdk-fork-lifecycle-probe');assert.equal(a.evidence?.scenario?.sha256,SHA);assert.equal(a.evidence?.inputs?.ts?.commit,BASE);assert.deepEqual(a.failures,[])}
function request(a,name,index=0){const r=a.provider?.[name]?.[index];assert.ok(r&&r.method==='POST'&&typeof r.url==='string');assert.ok(r.body&&typeof r.body==='object');assert.match(r.headers?.['content-length']??'',/^[1-9][0-9]*$/);assert.equal(Number(r.headers['content-length']),bytes(r.body));return r}
function worker(a){
 identity(a);
 const e=one(frames(a).filter(x=>x.f.event?.type==='tool_call_finished'&&x.f.event.toolCallId==='ag03-agent'),'unique finished');
 const value=e.f.event;assert.equal(value.toolName,'agent');assert.equal(value.ok,true);
 validateAgentReport(value.resultPreview);
 const data=JSON.parse(value.resultPreview.slice(value.resultPreview.indexOf('{')));assert.deepEqual(data,value.data);assert.equal(value.resultBytes,bytes(value.resultPreview));
 const bucket=a.tree?.['/home/.pilotdeck/runs/registry.json']?.json?.events?.[`\0agent03-parent\0${value.runId}`];assert.ok(Array.isArray(bucket));
 const reg=one(bucket.map((f,i)=>({f,i})).filter(x=>x.f.event?.type==='tool_call_finished'&&x.f.event.toolCallId==='ag03-agent'),'unique registry result');assert.deepEqual(reg.f.event,value);
 // Validate the exact durable worker identity/result. Do not rewrite any row.
 const rows=a.tree?.['/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl']?.jsonl;
 const durable=one(rows.filter(r=>r.type==='tool_result'&&r.result?.toolCallId==='ag03-agent'),'unique durable result');assert.equal(durable.turnId,value.runId);assert.deepEqual(durable.result.data,data);
 assert.equal(durable.sessionId,'agent03-parent');assert.equal(durable.result.type,'success');assert.equal(durable.result.toolName,'agent');
 assert.deepEqual(durable.result.content,[{type:'text',text:value.resultPreview.slice(0,value.resultPreview.indexOf('{')).trimEnd()},{type:'json',value:data}]);
 const message=one(rows.filter(r=>r.type==='tool_result_message'&&r.turnId===value.runId&&r.message?.content?.some(b=>b.type==='tool_result'&&b.toolCallId==='ag03-agent')),'unique durable worker message');
 const block=one(message.message.content.filter(b=>b.type==='tool_result'&&b.toolCallId==='ag03-agent'),'unique result block');assert.ok(block.content.every(b=>b.type==='text'));assert.equal(block.content.map(b=>b.text).join('\n'),value.resultPreview);assert.deepEqual(block.raw,durable.result);
 const step=a.steps[e.si];assert.equal(step.step?.ws,'submit_turn');assert.equal(step.step?.params?.sessionKey,'agent03-parent');assert.equal(step.step?.params?.message,'AG03-PARENT-BEGIN');
 const started=one(step.frames.filter(f=>f.event?.type==='turn_started'),'unique owning turn');assert.equal(started.event.runId,value.runId);
 const canon=structuredClone(data);canon.durationMs=0;const id=canon.subagentSessionId.split('::sub::')[1];canon.subagentSessionId=canon.subagentSessionId.replace(id,'00000000-0000-4000-8000-000000000000');canon.transcriptRelativePath=canon.transcriptRelativePath.replace(id,'00000000-0000-4000-8000-000000000000');
 return {e,reg,value,canonical:value.resultPreview.slice(0,value.resultPreview.indexOf('{'))+JSON.stringify(canon)};
}
export function memoryReportDerived(left,right,nleft=left,nright=right){
 const sides=[left,right].map(worker),canon=[];
 for(const [i,a]of[left,right].entries()){
  const bodies=[];
  for(const name of ['parent-after','parent-check']){
   assert.equal(a.provider[name].length,1);const r=request(a,name);const body=structuredClone(r.body);
   const tool=one(body.messages.filter(m=>m.role==='tool'&&m.tool_call_id==='ag03-agent'),'unique provider worker result');assert.equal(tool.content,sides[i].value.resultPreview);tool.content=sides[i].canonical;
   bodies.push(JSON.stringify(body));
  }
  canon.push({report:sides[i].canonical,bodies});
 }
 assert.deepEqual(canon[0],canon[1],'complete raw requests differ beyond validated duration/worker identity');
 const paths=new Map();
 for(const name of ['parent-after','parent-check'])if(left.provider[name][0].headers['content-length']!==right.provider[name][0].headers['content-length'])paths.set(`$.provider.${name}[0].headers.content-length`,'approved memory exact raw request byte derivation');
 if(sides[0].value.resultBytes!==sides[1].value.resultBytes){
  const wire=sides.map(s=>`$.steps[${s.e.si}].frames[${s.e.fi}].event.resultBytes`);assert.equal(wire[0],wire[1],'wire path differs; observer structure not in this policy');paths.set(wire[0],'approved memory exact report UTF8 bytes');
  const normalized=[nleft,nright],registry=sides.map((s,i)=>{const event=normalized[i].steps[s.e.si].frames[s.e.fi].event;assert.equal(event.resultBytes,s.value.resultBytes);const key=`\0agent03-parent\0${event.runId}`;assert.equal(normalized[i].tree['/home/.pilotdeck/runs/registry.json'].json.events[key][s.reg.i].event.resultBytes,s.value.resultBytes);return `$.tree./home/.pilotdeck/runs/registry.json.json.events.${key}[${s.reg.i}].event.resultBytes`});
  // No path alignment/reindexing: only matching actual normalized paths qualify.
  if(registry[0]===registry[1])paths.set(registry[0],'approved memory exact registry report UTF8 bytes');
 }
 return paths;
}
export function memoryHeaderObjectOrder(left,right){
 [left,right].forEach(identity);const paths=new Map();
 for(const name of ['memory-gate','memory-selector']){
  assert.equal(left.provider?.[name]?.length,3);assert.equal(right.provider?.[name]?.length,3);
  for(let i=0;i<3;i++){
   const a=request(left,name,i),b=request(right,name,i);
   assert.equal(a.method,b.method);assert.equal(a.url,b.url);assert.equal(JSON.stringify(a.body),JSON.stringify(b.body),'complete body bytes must match');
   for(const r of[a,b]){assert.deepEqual(Object.keys(r.headers).sort(),[...HEADERS].sort());for(const value of Object.values(r.headers))assert.ok(typeof value==='string'||(Array.isArray(value)&&value.every(x=>typeof x==='string')),'preserve captured duplicate representation')}
   assert.ok(isDeepStrictEqual(a.headers,b.headers),'all header values and string/array representations must match');
   if(JSON.stringify(Object.keys(a.headers))!==JSON.stringify(Object.keys(b.headers)))paths.set(`$.provider.${name}[${i}].headers.<key order>`,'approved exact captured header object key order; not rawHeader wire equivalence');
  }
 }
 return paths;
}

// Only the named memory case uses this supplementary contract. Fork retains
// its existing derived-value policy; unknown scenes gain no exception.
export function observerMemoryPaths(left,right,nleft,nright){
 if(![left,right].some(a=>a?.scenario==='agent03-sdk-fork-lifecycle-probe'))return new Map();
 return new Map([...memoryReportDerived(left,right,nleft,nright),...memoryHeaderObjectOrder(left,right)]);
}
