// C-DIAGNOSTICS-FIXED-PARSER-001: approved fixed-input presence/location/count contract.
// Only the trusted comparator raw pair enters this module; all views/differences
// and the pinned tokenizer are reconstructed internally.
import fs from 'node:fs';
import {createHash} from 'node:crypto';
import {isDeepStrictEqual as equal} from 'node:util';
import {load,compare} from './normalization.mjs';
import {loadPinnedTokenizer} from './sdk-agent-policy.mjs';
const skeleton = (line) => line
  .replace(/("(?:duration_ms|duration_seconds|durationMs)"\s*:\s*)-?[\d.eE+-]+/g, '$1D')
  .replace(/"(?:[^"\\]|\\.)*"|-?\d+(\.\d+)?([eE][+-]?\d+)?/g, (tok, frac, exp) => tok[0] === '"'
    ? `"${(tok.match(/\\(?:u[0-9a-fA-F]{4}|.)/g) ?? []).join('')}"` : exp ? '0e0' : frac ? '0.0' : '0');
const rawPrint = (text) => {
  const lines = text.split('\n'); if (text.endsWith('\n')) lines.pop();
  const counts = {};
  for (const l of lines) { const h = createHash('sha256').update(skeleton(l)).digest('hex').slice(0, 16); counts[h] = (counts[h] ?? 0) + 1; }
  return { lines: lines.length, finalNewline: text.endsWith('\n'), skeletons: Object.fromEntries(Object.entries(counts).sort()) };
};

const contractBytes=fs.readFileSync(new URL('./diagnostics-policy-contract.json',import.meta.url));
if(createHash('sha256').update(contractBytes).digest('hex')!=='94e002737b934f991c5e0e4b50e3d4c04df5de815c6d0d681cb52d48e8ccd34e')throw Error('fixed contract identity changed');
const contract=JSON.parse(contractBytes);
const insist=(ok,message)=>{if(!ok)throw Error('diagnostics fixed-parser contract: '+message);};
const clean=v=>JSON.parse(JSON.stringify(v));
const changed=['call_bbx_12','call_bbx_13','call_bbx_14'];
const values=(o)=>Object.values(o??{});
function budgets(artifact){return artifact.steps.flatMap(s=>(s.frames??[]).map(f=>f.event).filter(e=>e?.type==='context_budget'));}
function validateRaw(artifact,side){
 insist(artifact.scenario===contract.scenario,'other scene is not applicable');
 insist(artifact.evidence?.scenario?.sha256===contract.sceneSHA,'fixed input SHA changed');
 insist(equal(artifact.accepted??[],[]),'cannot add a path exemption');
 insist(equal(artifact.failures,[]),'runtime failed');
 const files=Object.fromEntries(Object.entries(artifact.tree).filter(([k])=>k.includes('/d/')));
 insist(Object.keys(files).length===15&&equal(files,contract.files[side]),'15 actual written file identities/bytes changed');
 const output=contract.outputs[side],seen=new Set();
 for(const step of artifact.steps){const events=(step.frames??[]).map(f=>f.event).filter(Boolean);
  for(const event of events.filter(e=>e.type==='tool_call_finished')){
   const id=event.toolCallId,text=output[id];
   insist(typeof text==='string'&&!seen.has(id),'unknown/duplicate write result');seen.add(id);
   insist(event.ok===true&&event.resultPreview===text,'required real diagnostic/result changed: '+id);
   insist(event.resultBytes===Buffer.byteLength(text)&&event.resultLineCount===text.split('\n').length,'result byte/line count invalid: '+id);
   const detail=events.filter(e=>e.type==='tool_result_detail_available'&&e.toolCallId===id);
   insist(detail.length===1&&detail[0].fullText===text,'full detail copy invalid: '+id);
  }
 }
 insist(seen.size===Object.keys(output).length,'write result missing');
 for(const [tag,requests]of Object.entries(artifact.provider))for(const request of requests){
  insist(request.headers['content-length']===String(Buffer.byteLength(JSON.stringify(request.body))),'raw provider request length invalid '+tag);
  for(const message of request.body.messages??[])if(message.role==='tool'){
   insist(message.content===output[message.tool_call_id],'provider tool copy unbound '+tag+'/'+message.tool_call_id);
  }
 }
 let previous;
 for(const budget of budgets(artifact)){
  const b=budget.breakdown;
  insist(b.total===budget.localEstimateTokens&&['system','tools','messages','mcp','memory'].reduce((n,k)=>n+b[k],0)===b.total,'budget breakdown invalid');
  if(budget.source==='calibrated'){
   const cap=Math.max(1,Math.floor(budget.localEstimateTokens*.5));
   const correction=budget.calibrationActualInputTokens-budget.calibrationEstimatedInputTokens;
   const used=Math.max(1,Math.round(budget.localEstimateTokens+Math.max(-cap,Math.min(cap,correction))));
   insist(budget.used===used&&budget.displayUsed===used&&budget.usageTokens===used&&budget.ratio===used/budget.effectiveTotal,'bounded calibrated token equation invalid');
   if(previous)insist(budget.calibrationEstimatedInputTokens===previous.localEstimateTokens,'calibration not bound to preceding real request budget');
  }
  previous=budget;
 }
 const transcript=Object.entries(artifact.tree).find(([k])=>k.endsWith('/chats/s-bbx-1.jsonl'));
 insist(transcript,'durable transcript missing');
 const bytes=transcript[1].jsonl.map(row=>JSON.stringify(row)+'\n').join('');
 insist(equal(rawPrint(bytes),transcript[1].raw),'durable raw skeletons not recomputed from real records');
 insist(artifact.steps.at(-1).frames[0].result.session.fileSize===Buffer.byteLength(bytes),'session file size not real UTF8 durable bytes');
 for(const [name,entry]of Object.entries(artifact.tree))if(entry.raw&&entry.json&&(name.includes('/chats/')||name.endsWith('/runs/registry.json'))){
  const forms=[JSON.stringify(entry.json),JSON.stringify(entry.json)+'\n',JSON.stringify(entry.json,null,2),JSON.stringify(entry.json,null,2)+'\n'];
  insist(forms.some(s=>equal(rawPrint(s),entry.raw)),'JSON projection/registry raw skeleton not linked '+name);
 }
}
function validatePair(view,count){
 const {a,b,raw,all}=view;
 validateRaw(raw[0],'ts');validateRaw(raw[1],'rust');
 // Every model request remains strict after only the three known tool result
 // occurrences are substituted. No arbitrary diagnostic-prefix replacement.
 for(const [tag,requests]of Object.entries(a.provider)){
  insist(b.provider[tag]?.length===requests.length,'provider request cardinality changed');
  for(let i=0;i<requests.length;i++){
   const body=structuredClone(b.provider[tag][i].body);
   for(const m of body.messages??[])if(m.role==='tool'&&changed.includes(m.tool_call_id)){
    insist(m.content===contract.outputs.rust[m.tool_call_id],'unexpected native diagnostic content');
    m.content=contract.outputs.ts[m.tool_call_id];
   }
   insist(equal(body,requests[i].body),'unrelated provider body changed '+tag);
  }
 }
 // Cumulative message-token differences are derived from the three exact
 // results, and calibration remains chained to real preceding request values.
 let delta=0;
 for(let i=0;i<raw[0].steps.length;i++){
  const ev=x=>(x.steps[i].frames??[]).map(f=>f.event).filter(Boolean);
  const x=ev(raw[0]),y=ev(raw[1]);const result=x.find(e=>e.type==='tool_call_finished');
  const xx=x.filter(e=>e.type==='context_budget'),yy=y.filter(e=>e.type==='context_budget');
  insist(xx.length===yy.length,'budget occurrence missing');
  for(let j=0;j<xx.length;j++){
   // Each write has a request before and after execution.
   if(j===1&&result&&changed.includes(result.toolCallId))delta+=count(contract.outputs.rust[result.toolCallId])-count(contract.outputs.ts[result.toolCallId]);
   insist(yy[j].localEstimateTokens-xx[j].localEstimateTokens===delta&&yy[j].breakdown.messages-xx[j].breakdown.messages===delta,'diagnostic token delta unbound');
   for(const k of ['system','tools','mcp','memory'])insist(xx[j].breakdown[k]===yy[j].breakdown[k],'unrelated budget component '+k);
  }
 }
 // Fixed input also pins the exact finite projection values. This is a narrow
 // approved fixed-fixture contract, not a general all-parser normalization rule.
 // In particular a new path, wrong value, missing copy or different run binding
 // cannot borrow permission from a path prefix.
 const allowed=new Map(contract.diffPairs.map(d=>[d.at,d]));
 for(const d of all){const expected=allowed.get(d.at);insist(expected&&equal(clean(d),expected),'unapproved difference '+d.at);}
 insist(all.length===416,'expected linked projection set changed; review required');
 return new Map(all.map(d=>[d.at,'C-DIAGNOSTICS-FIXED-PARSER-001: fixed input, exact diagnostic occurrence and validated derived value']));
}


/** The caller supplies actual parsed captures and a trusted baseline path, never
 * normalized views, a differences list, a counter, a Map or a passed flag. */
export function diagnosticsAcceptedPaths(rawLeft,rawRight,baselineRoot){
 if(rawLeft?.scenario!==contract.scenario&&rawRight?.scenario!==contract.scenario)return new Map();
 const raw=[structuredClone(rawLeft),structuredClone(rawRight)];
 const a=load(structuredClone(raw[0]),new WeakMap(),new Map());
 const b=load(structuredClone(raw[1]),new WeakMap(),new Map());
 const all=[];compare(a,b,'$',all);
 return validatePair({a,b,raw,all},loadPinnedTokenizer(baselineRoot));
}
