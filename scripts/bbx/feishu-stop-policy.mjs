// Exact approved Feishu stop ownership derivatives and fixed SDK diagnostics.
import fs from 'node:fs';import path from 'node:path';import assert from 'node:assert/strict';import {createHash} from 'node:crypto';import {createRequire} from 'node:module';import {spawnSync} from 'node:child_process';import {fileURLToPath} from 'node:url';
import {SDK_INFO,SDK_WARN,PATH_GROUPS} from './feishu-stop-policy-constants.mjs';
import {load} from './normalization.mjs';
const root=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../..');
const normalize=raw=>load(raw,new WeakMap(),new Map());const hash=b=>createHash('sha256').update(b).digest('hex');
const baselineCommit='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';const fixture=path.join(root,'scripts/bbx/fixtures/feishu-stream-stop');
function once(text,part){assert.equal(text.split(part).length,2,'one exact SDK diagnostic group');return text.replace(part,'');}
function mapValues(value,p='$',map=new Map()){map.set(p,value);if(value&&typeof value==='object')for(const [k,v]of Object.entries(value))mapValues(v,p+(Array.isArray(value)?`[${k}]`:'.'+k),map);return map;}
function diffs(a,b,p='$',out=[]){if(Object.is(a,b))return out;if(a&&b&&typeof a==='object'&&typeof b==='object'&&Array.isArray(a)===Array.isArray(b)){if(Array.isArray(a)){assert.equal(a.length,b.length,'no additional/missing records');}else assert.deepEqual(Object.keys(a),Object.keys(b),'ordinary object key set/order');for(const k of Object.keys(a))diffs(a[k],b[k],p+(Array.isArray(a)?`[${k}]`:'.'+k),out);}else out.push(p);return out;}
function sealed(file,side){
 const a=JSON.parse(fs.readFileSync(file));assert.equal(a.side,side);assert.equal(a.evidence.inputs.ts.commit,baselineCommit);assert.equal(a.scenario,'backend-feishu-stream-stop');
 const rec=JSON.parse(fs.readFileSync(file+'.receipt.json'));assert.equal(rec.variant,'normal');assert.equal(rec.artifact.sha256,hash(fs.readFileSync(file)));assert.equal(rec.raw.sha256,hash(fs.readFileSync(file+'.mock-raw.jsonl')));
 assert.equal(rec.scenario.sha256,'55c8036ee20997dc37dc1f9ca0c898fc659212618ae0388cbd52613594c2bcb4');assert.equal(hash(fs.readFileSync(path.join(fixture,'verify.mjs'))),'38f9a48d631db21491fecba3ee83513d71b737c5eda7a59e714e00ee23063d87','fixed strict implementation, never external proof');
 const checked=spawnSync(process.execPath,[path.join(fixture,'verify.mjs'),file],{encoding:'utf8',timeout:5000,maxBuffer:1024*1024});assert.equal(checked.status,0,'existing source-bound strict raw socket/nonce/EOF/drain/permission/business proof: '+checked.stderr);
 return a;
}
function countMessages(messages,count){return messages.reduce((total,m)=>total+4+m.content.reduce((n,b)=>{if(b.type==='text')return n+count(b.text);if(b.type==='tool_call')return n+count(b.name+JSON.stringify(b.input));if(b.type==='tool_result'){assert(b.content.every(x=>x.type==='text'));return n+count(b.content.map(x=>x.text).join(''));}throw Error('unproven message block');},0),0);}
// Existing collector serialization fingerprint, kept byte-for-byte in its algorithm.
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

function sideProof(a,side,count,normalize){
 const frames=a.steps[0].then[0].frames;assert.deepEqual(frames.map(f=>f.event?.type),['turn_started','input_accepted','model_request_started','context_budget','model_selection_changed','tool_call_started','agent_status','permission_request','agent_status','tool_call_finished','tool_result_detail_available','agent_status','model_request_started','context_budget','assistant_text_delta','assistant_block','turn_completed','turn_completed']);
 const tool=frames[9].event,full=frames[10].event.fullText,budget=frames[13].event,initial=frames[3].event;
 assert.equal(tool.toolCallId,'call_feishu_receipt');assert.equal(tool.toolName,'bash');assert.equal(tool.data.exitCode,0);assert.equal(tool.data.stderr,'');assert.equal(tool.data.timedOut,false);
 const factsEntry=Object.entries(a.tree).find(([k])=>k.endsWith('/feishu-stop-witness/facts.json'))[1];const probe={marker:'FEISHU-STOP-PROBE',identity:'feishu-original-socket-1',alive:side==='ts',evidence:side==='ts'?'exact-nonce-pong':'socket-close'};assert.deepEqual(factsEntry.json.probe,probe);assert.equal(tool.data.stdout,JSON.stringify(probe)+'\n');const bytes=Buffer.byteLength(tool.data.stdout);assert.equal(tool.data.assertions.stdoutBytes,bytes);assert.equal(bytes,side==='ts'?112:109);
 const expectedFull=`BASH_RESULT[success][stdout_data]\nAssertions:\n- exit_code: 0\n- stdout_visible: true\n- stderr_visible: false\n- retrieved_data_available: true\n- stdout_bytes: ${bytes}\n- stderr_bytes: 0\nInterpretation: Command succeeded and stdout contains visible data; use stdout as the primary evidence for the next step.\n\nstdout:\n${JSON.stringify(probe)}`;assert.equal(full,expectedFull);assert.equal(tool.resultPreview,full);assert.equal(tool.resultBytes,Buffer.byteLength(full));
 const chat=Object.entries(a.tree).find(([k])=>k.endsWith('/feishu-stop-observer.jsonl'))[1].jsonl;assert.equal(chat.length,40);const requests=chat.filter(r=>r.type==='model_request');assert.equal(requests.length,2);const request=requests[1].request;
 const toolMessage=request.messages[2].content[0];assert.equal(toolMessage.type,'tool_result');assert.equal(toolMessage.toolCallId,'call_feishu_receipt');assert.equal(toolMessage.content[0].text,full);
 const final=a.provider['observer-final'][0];assert.equal(final.body.messages[3].content,full);assert.equal(final.headers['content-length'],String(Buffer.byteLength(JSON.stringify(final.body))));
 const system=count(request.systemPrompt);const tools=request.tools.reduce((n,t)=>n+count(t.name+(t.description??'')+JSON.stringify(t.inputSchema)),0);const messages=countMessages(request.messages,count);assert.deepEqual([system,tools,messages],[3062,7301,side==='ts'?159:156]);assert.equal(count(full),side==='ts'?110:107);assert.equal(budget.localEstimateTokens,system+tools+messages);assert.deepEqual(budget.breakdown,{source:'local_estimate',total:system+tools+messages,system,tools,messages,mcp:0,memory:0});
 assert.equal(initial.localEstimateTokens,10375);assert.equal(budget.calibrationActualInputTokens,12);assert.equal(budget.calibrationEstimatedInputTokens,initial.localEstimateTokens);const rawUsage=request.messages[1].content[0].raw.usage;assert.deepEqual(rawUsage,{prompt_tokens:12,completion_tokens:4,total_tokens:16});const correction=rawUsage.prompt_tokens-initial.localEstimateTokens;const max=Math.max(1,Math.floor(budget.localEstimateTokens*.5));const used=Math.max(1,Math.round(budget.localEstimateTokens+Math.max(-max,Math.min(max,correction))));assert.equal(budget.used,used);assert.equal(budget.displayUsed,used);assert.equal(budget.usageTokens,used);assert.equal(budget.maxContextTokens,57344);assert.equal(budget.ratio,used/57344);assert.equal(budget.source,'calibrated');assert.equal(budget.exact,false);
 assert.deepEqual(factsEntry.raw,rawPrint(JSON.stringify(factsEntry.json,null,2)+'\n'),'complete serialized witness recomputation');
 for(const [name,entry]of Object.entries(a.tree).filter(([name])=>name.endsWith('.jsonl'))){
  const rows=entry.jsonl;assert.equal(rows.length,name.endsWith('/feishu-stop-observer.jsonl')?40:20);const serialized=rows.map(r=>JSON.stringify(r)+'\n').join('');assert.deepEqual(entry.raw,rawPrint(serialized));assert.deepEqual(a.evidence.persistedFiles[name],{byteLength:Buffer.byteLength(serialized),sha256:hash(serialized)},'complete unchanged durable witness');
  for(const [i,row]of rows.entries()){assert.equal(row.sequence,i+1);if(i)assert.equal(row.parentEntryId,rows[i-1].entryId);}
  const projection=a.tree[name+'.projections.json'].json;assert.equal(projection.asOfSequence,rows.length);for(const key of ['sequence','entryId','parentEntryId','turnId','type','createdAt'])assert.equal(projection.anchor[key],rows.at(-1)[key],'same-side durable anchor');for(const view of Object.values(projection.projections))assert.equal(view.asOfSequence,rows.length);
 }
 const normalized=normalize(structuredClone(a)),values=mapValues(normalized);
 for(const [p,group]of Object.entries(PATH_GROUPS)){
  if(group==='derived-tool-receipt'){const leaf=p.split('.').at(-1);const expected=leaf==='resultBytes'?Buffer.byteLength(full):leaf==='stdoutBytes'?bytes:leaf==='stdout'?tool.data.stdout:full;assert.deepEqual(values.get(p),expected,'every real receipt copy '+p);}
  if(group==='derived-token-accounting'){const leaf=p.split('.').at(-1);assert.deepEqual(values.get(p),['total','messages'].includes(leaf)?budget.breakdown[leaf]:budget[leaf],'every calibrated budget copy '+p);}
 }
 return {normalized,stdout:normalized.steps[0].serve.stdout,stderr:normalized.steps[0].serve.stderr};
}
/** The public comparator passes original file paths, never artifact-authored proof.
 * This module binds the shared load itself and rechecks sealed raw/strict identity. */
export function feishuStopAcceptedPaths(tsFile,rustFile,baselineRoot){
 assert(path.isAbsolute(baselineRoot));const req=createRequire(path.join(baselineRoot,'package.json'));assert.equal(req('@larksuiteoapi/node-sdk/package.json').version,'1.65.0');assert.equal(hash(fs.readFileSync(req.resolve('@larksuiteoapi/node-sdk'))),'50e8421dc44eaf648078706b41cd2aea3390667d17a3db3f4e6ce88f431b60b1');
 assert.equal(hash(fs.readFileSync(req.resolve('js-tiktoken/lite'))),'e055861839442275fc6934696b102b3a5a09c34b5199b4e8d5ea8f4d3587e327');assert.equal(hash(fs.readFileSync(req.resolve('js-tiktoken/ranks/o200k_base'))),'d2a4d9f263b2de045dcd98f347c50e38d71104141948918b6c30a92fb2411f42');
 const {Tiktoken}=req('js-tiktoken/lite'),tokenizer=new Tiktoken(req('js-tiktoken/ranks/o200k_base'));const count=s=>s.length?tokenizer.encode(s,[],[]).length:0;
 const ts=sealed(tsFile,'ts'),rust=sealed(rustFile,'rust');const a=sideProof(ts,'ts',count,normalize),b=sideProof(rust,'rust',count,normalize);
 let stdout=a.stdout;for(const info of SDK_INFO){assert(!b.stdout.includes(info));stdout=once(stdout,info);}assert.equal(stdout,b.stdout,'only exact three SDK startup info groups');assert.equal(once(a.stderr,SDK_WARN),b.stderr,'only exact not.registered SDK warn');assert(a.stderr.endsWith(SDK_WARN));
 const positions=SDK_INFO.map(x=>a.stdout.indexOf(x));assert(positions[0]<positions[1]&&positions[1]<a.stdout.indexOf('feishu: stream mode connected')&&a.stdout.indexOf('[adapters] channel feishu startup task completed')<positions[2]&&positions[2]<a.stdout.indexOf('feishu: ★ im.message.receive_v1 fired'),'fixed startup-phase groups');
 const actual=diffs(a.normalized,b.normalized);assert.deepEqual(actual.slice().sort(),Object.keys(PATH_GROUPS).sort(),'entire ordinary pair differs at exact64 proven leaves only');return new Map(actual.map(p=>[p,PATH_GROUPS[p]==='SDK-log'?'H1-FEISHU-STOP-DERIVED-001: exact fixedSDK diagnostic allowance':'H1-FEISHU-STOP-DERIVED-001: exact stopownership causal derivative']));
}
