// agent03-sdk-fork: strict building blocks only. No diff exemption is emitted
// until canonical request tokenisation and every output projection are bound.
import { isDeepStrictEqual } from 'node:util';
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join } from 'node:path';

const SCENARIO_SHA = '9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca';
const UUID = '[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}';
const PREFIX = '[worker] AG03 delegate\n\nScope: done.\nResult: AG03-CHILD-REPORT.\nKey files: none\nFiles changed: none\nIssues: none\n';
const insist = (ok, message) => { if (!ok) throw new Error(`sdk agent policy: ${message}`); };

export function validateAgentReport(content) {
  insist(typeof content === 'string' && content.startsWith(PREFIX), 'worker report prefix');
  let value;
  try { value = JSON.parse(content.slice(PREFIX.length)); } catch { throw new Error('sdk agent policy: worker report JSON'); }
  const id = value?.subagentSessionId?.match(new RegExp(`^/tmp/pdrs-bbx/root/home/\\.pilotdeck::sub::(${UUID})$`))?.[1];
  insist(id && value.transcriptRelativePath === `agent03-parent/subagents/${id}.jsonl`, 'child UUID reference');
  insist(Number.isSafeInteger(value.durationMs) && value.durationMs >= 0, 'worker duration');
  const expected = {subagentType:'worker',description:'AG03 delegate',text:'Scope: done.\nResult: AG03-CHILD-REPORT.\nKey files: none\nFiles changed: none\nIssues: none',usage:{inputTokens:48,outputTokens:16,totalTokens:64},turns:4,durationMs:value.durationMs,parsed:{Scope:'done.',Result:'AG03-CHILD-REPORT.','Key files':'none','Files changed':'none',Issues:'none'},subagentSessionId:value.subagentSessionId,transcriptRelativePath:value.transcriptRelativePath};
  insist(isDeepStrictEqual(value, expected), 'worker business report');
  return { uuid: id, durationMs: value.durationMs };
}

export function validateBudget(event, expectedBreakdown, calibration) {
  insist(event?.type === 'context_budget' && isDeepStrictEqual(event.breakdown, expectedBreakdown), 'canonical request breakdown');
  const b = event.breakdown;
  insist(b.source === 'local_estimate' && Number.isSafeInteger(b.system) && Number.isSafeInteger(b.tools) && Number.isSafeInteger(b.messages)
    && b.system >= 0 && b.tools >= 0 && b.messages >= 0 && b.mcp === 0 && b.memory === 0
    && b.total === b.system + b.tools + b.messages && event.localEstimateTokens === b.total, 'local estimate');
  insist(event.calibrationActualInputTokens === calibration.actual && event.calibrationEstimatedInputTokens === calibration.estimated
    && Number.isSafeInteger(calibration.actual) && calibration.actual >= 0 && Number.isSafeInteger(calibration.estimated) && calibration.estimated > 0, 'calibration input');
  const limit = Math.floor(b.total / 2);
  const used = b.total + Math.max(-limit, Math.min(limit, calibration.actual - calibration.estimated));
  insist(event.used === used && event.displayUsed === used && event.usageTokens === used && event.ratio === used / 57344
    && event.total === 65536 && event.totalContextTokens === 65536 && event.maxContextTokens === 57344
    && event.effectiveTotal === 57344 && event.effectiveContextTokens === 57344 && event.maxOutputTokens === 8192
    && event.reservedOutputTokens === 8192 && event.warningRatio === 0.8 && event.blockingRatio === 0.9
    && event.state === 'ok' && event.source === 'calibrated' && event.estimateSource === 'usage' && event.exact === false, 'budget projection');
  return used;
}

export function validateRawJsonlSize(rawBytes, rows, size) {
  insist(Buffer.isBuffer(rawBytes), 'raw JSONL bytes unavailable');
  insist(rawBytes.length === size && rawBytes.length > 0 && rawBytes.at(-1) === 10, 'JSONL byte length');
  let parsed;
  try { parsed = rawBytes.toString('utf8').trimEnd().split('\n').map(JSON.parse); } catch { throw new Error('sdk agent policy: invalid JSONL bytes'); }
  insist(isDeepStrictEqual(parsed, rows), 'JSONL bytes do not match parsed rows');
}

// The tokenizer and ranks are loaded from an explicitly supplied baseline; a
// different package build must fail rather than silently change token counts.
export function loadPinnedTokenizer(baselineRoot) {
  const require = createRequire(join(baselineRoot, 'package.json'));
  const hashes = new Map([
    ['js-tiktoken/lite', 'e055861839442275fc6934696b102b3a5a09c34b5199b4e8d5ea8f4d3587e327'],
    ['js-tiktoken/ranks/o200k_base', 'd2a4d9f263b2de045dcd98f347c50e38d71104141948918b6c30a92fb2411f42'],
  ]);
  for (const [name, expected] of hashes) insist(createHash('sha256').update(readFileSync(require.resolve(name))).digest('hex') === expected, `tokenizer dependency ${name}`);
  const base64 = createRequire(require.resolve('js-tiktoken/lite')).resolve('base64-js');
  insist(createHash('sha256').update(readFileSync(base64)).digest('hex') === '829eadd8a1a441d25be0cb93b00e16a0d0c20fd294db95d8f2ed87e6954b7182', 'tokenizer dependency base64-js');
  const { Tiktoken } = require('js-tiktoken/lite');
  const rankModule = require('js-tiktoken/ranks/o200k_base');
  const encoder = new Tiktoken(rankModule.default ?? rankModule);
  return value => value.length === 0 ? 0 : encoder.encode(value, [], []).length;
}

export function canonicalBreakdown(request, count) {
  insist(typeof count === 'function' && typeof request?.systemPrompt === 'string' && Array.isArray(request.messages) && Array.isArray(request.tools), 'canonical model request');
  const prompt = request.systemPrompt;
  insist(!prompt.includes('<mcp-instructions>') && !prompt.includes('<memory-context>'), 'unsupported delimited system block');
  let messages = 0;
  for (const message of request.messages) {
    insist(Array.isArray(message.content), 'canonical message');
    messages += 4;
    for (const block of message.content) {
      let str;
      if (block.type === 'text') str = block.text;
      else if (block.type === 'tool_call') str = block.name + (block.input == null ? '' : JSON.stringify(block.input));
      else if (block.type === 'tool_result') {
        insist(Array.isArray(block.content) && block.content.every(x => x.type === 'text' && typeof x.text === 'string'), 'unsupported tool result content');
        str = block.content.map(x => x.text).join('\n');
      } else throw new Error(`sdk agent policy: unsupported canonical block ${block.type}`);
      insist(typeof str === 'string', 'canonical block text');
      messages += count(str);
    }
  }
  let tools = 0;
  for (const tool of request.tools) {
    insist(typeof tool.name === 'string' && (tool.description === undefined || typeof tool.description === 'string'), 'canonical tool schema');
    tools += count(tool.name + (tool.description ?? '') + (tool.inputSchema == null ? '' : JSON.stringify(tool.inputSchema)));
  }
  const system = count(prompt);
  return {source:'local_estimate',total:system + tools + messages,system,tools,messages,mcp:0,memory:0};
}

export function validateCanonicalJsonlWitness(rows, witness, size) {
  insist(Array.isArray(rows) && witness && Number.isSafeInteger(witness.byteLength) && /^[0-9a-f]{64}$/.test(witness.sha256), 'persisted JSONL witness unavailable');
  const bytes = Buffer.from(rows.map(x => JSON.stringify(x) + '\n').join(''), 'utf8');
  insist(bytes.length === witness.byteLength && witness.byteLength === size
    && createHash('sha256').update(bytes).digest('hex') === witness.sha256, 'persisted JSONL witness mismatch');
  return bytes.length;
}

const PARENT_PATH = '/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl';
const REGISTRY_PATH = '/home/.pilotdeck/runs/registry.json';
const DERIVED_FIELDS = ['used','displayUsed','localEstimateTokens','usageTokens','calibrationEstimatedInputTokens','ratio','breakdown.total','breakdown.messages'];
const get = (object, path) => path.split('.').reduce((x, part) => x?.[part], object);
const prefix = '$.tree.' + PARENT_PATH;
function sameEvent(actual, expected, label) {
  insist(isDeepStrictEqual(actual, expected), `${label} must equal its durable budget event`);
}
function assertProvider(a) {
  const names = ['parent','child-alpha','child-beta','child-mcp','child-after-mcp','parent-after','observer','parent-check','second'];
  for (const name of names) insist(a.provider?.[name]?.length === 1, `provider ${name} count`);
  const body = name => a.provider[name][0].body;
  const result = (name,id) => {
    const v = body(name)?.messages?.filter(m => m.role === 'tool' && m.tool_call_id === id);
    insist(v?.length === 1 && typeof v[0].content === 'string', `${name} tool result ${id}`); return v[0].content;
  };
  insist(a.provider?.title?.length===2, 'title request count');
  const user=(body('child-alpha').messages??[]).filter(m=>m.role==='user').map(m=>typeof m.content==='string'?m.content:JSON.stringify(m.content));
  const initial=user.findIndex(s=>s.includes('AG03-INITIAL-ONLY')), directive=user.findIndex(s=>s.includes('AG03-CHILD-DIRECTIVE'));
  insist(initial>=0 && directive>initial, 'separate initial prompt and child directive');
  const output=JSON.stringify(a.steps);
  insist(output.includes('AG03-PARENT-DONE') && output.includes('AG03-PARENT-CHECK-DONE'), 'parent turn outputs');
  const report = result('parent-after','ag03-agent');
  validateAgentReport(report);
  insist(result('parent-check','ag03-agent') === report, 'parent tool result must be retained exactly');
  insist(result('child-beta','ag03-alpha').includes('AG03-ALPHA-CONTENT'), 'alpha result');
  insist(result('child-mcp','ag03-beta') === "Skill 'agent03-beta' not found. Available skills: agent03-alpha", 'beta result');
  insist(result('child-after-mcp','ag03-mcp').includes('AG03-MCP-RESULT:AG03-MCP-CALL'), 'actual MCP result');
  const tools = name => (body(name).tools ?? []).map(t => t.function?.name ?? t.name);
  insist(tools('parent').includes('agent') && !tools('parent').includes('mcp__agent03-private__echo'), 'parent tools');
  insist(tools('child-alpha').includes('read_skill') && tools('child-alpha').includes('mcp__agent03-private__echo') && !tools('child-alpha').includes('write_file'), 'child tools');
  const observer = JSON.stringify(body('observer').messages);
  insist(observer.includes('AG03-OBSERVER-POSTAMBLE') && !observer.includes('AG03-OBSERVER-INITIAL-MUST-NOT-LEAK')
    && ['write_file','read_skill','mcp__agent03-private__echo'].every(x => !tools('observer').includes(x)), 'observer isolation');
  const second = JSON.stringify(body('second').messages).toLowerCase();
  insist(second.includes('agent03-alpha') && second.includes('agent03-beta') && !tools('second').includes('mcp__agent03-private__echo'), 'second session');
  const log = a.tree?.['/home/.pilotdeck/agent03-mcp-log.jsonl']?.jsonl;
  insist(Array.isArray(log) && ['start','initialize','tools/list','tools/call','exit'].every(x => log.filter(e=>e.event===x).length===1)
    && log.find(e=>e.event==='tools/call')?.name==='echo' && log.find(e=>e.event==='tools/call')?.args?.text==='AG03-MCP-CALL', 'MCP process lifecycle');
  const events = a.steps.flatMap(s=>s.frames ?? []).map(f=>f.event).filter(e=>e?.type==='agent_status' && e.detail?.subagentType==='reviewer');
  const starts = events.filter(e=>e.event==='subagent_started');
  insist(starts.length===1 && typeof starts[0].detail.subagentId === 'string', 'observer start');
  const ordered = events.filter(e=>e.detail.subagentId===starts[0].detail.subagentId);
  insist(ordered.length===3&&events.length===3,'observer exact lifecycle count');
  const i=unique(ordered,e=>e.event==='subagent_started','observer started').index;
  const j=unique(ordered,e=>e.event==='subagent_text_delta'&&e.detail.text==='AG03-OBSERVER-REPORT','observer report').index;
  const k=unique(ordered,e=>e.event==='subagent_completed'&&e.detail.success===true,'observer completed').index;
  insist(i<j&&j<k,'observer ordered report');
  for(const step of a.steps){
    const own=(step.frames??[]).filter(f=>f.event?.type==='agent_status'&&f.event.detail?.subagentType==='reviewer');
    if(!own.length)continue;
    const started=unique(step.frames,f=>f.event?.type==='turn_started','observer delivery turn');
    insist(step.step?.ws==='submit_turn'&&step.step.params?.sessionKey==='agent03-parent'&&own.every(f=>f.event.runId===started.value.event.runId),'observer delivery run');
  }
  const done=unique(a.tree?.[PARENT_PATH]?.jsonl??[],r=>r.type==='subagent_completed'&&r.subagentId===starts[0].detail.subagentId,'observer durable completion').value;
  insist(ordered[k].detail.durationMs===done.durationMs,'observer completion payload');
  return report;
}

const unique = (items, predicate, label) => {
  const found = items.map((value,index)=>({value,index})).filter(x=>predicate(x.value,x.index));
  insist(found.length===1, `unique ${label}`); return found[0];
};
const stepFrames = (a,index) => { const frames=a.steps[index]?.frames; insist(Array.isArray(frames), 'step frames'); return frames; };
const budgetRow = row => row?.type==='agent_status_message' && row.event==='context_budget';
// Bind identity before looking at token values. Observer records may interleave
// with the parent branch; neither their removal nor array reordering is needed.
export function sdkAgentAssociations(a) {
  const rows=a.tree?.[PARENT_PATH]?.jsonl;
  insist(Array.isArray(rows)&&rows.length>56&&rows.every(row=>row.sessionId==='agent03-parent'),'parent transcript');
  const begin=unique(a.steps??[],s=>s.step?.ws==='submit_turn'&&s.step.params?.sessionKey==='agent03-parent'&&s.step.params.message==='AG03-PARENT-BEGIN','parent begin step');
  const check=unique(a.steps??[],s=>s.step?.ws==='submit_turn'&&s.step.params?.sessionKey==='agent03-parent'&&s.step.params.message==='AG03-PARENT-CHECK','parent check step');
  insist(begin.index<check.index,'parent step order');
  const parentSubmits=a.steps.filter(s=>s.step?.ws==='submit_turn'&&s.step.params?.sessionKey==='agent03-parent');
  insist(parentSubmits.length===2,'parent submit count');
  const turns=[begin,check].map((step,ordinal)=>{
    const started=unique(stepFrames(a,step.index),f=>f.event?.type==='turn_started','parent turn_started');
    const run=started.value.event.runId;
    insist(typeof run==='string'&&run.length>0,'parent run');
    const input=unique(rows,r=>r.type==='accepted_input'&&r.turnId===run,'parent accepted input');
    const text=input.value.messages?.flatMap(m=>m.content??[]).filter(b=>b.type==='text').map(b=>b.text).join('');
    insist(text===step.value.step.params.message,'accepted input/step association');
    unique(rows,r=>r.type==='turn_started'&&r.turnId===run,'durable parent turn_started');
    return {name:ordinal===0?'begin':'check',stepIndex:step.index,run};
  });
  insist(turns[0].run!==turns[1].run,'distinct parent runs');
  const requests=turns.flatMap((turn,ordinal)=>(ordinal===0?[1,2]:[1]).map(step=>{
    const request=unique(rows,r=>r.type==='model_request'&&r.turnId===turn.run&&r.step===step,`${turn.name} model request step ${step}`);
    const started=unique(rows,r=>r.type==='step_started'&&r.turnId===turn.run&&r.step===step,`${turn.name} step_started ${step}`);
    insist(started.index<request.index,'request after its step');
    const budget=unique(rows,(r,index)=>budgetRow(r)&&r.turnId===turn.run&&index>started.index&&index<request.index,`${turn.name} step ${step} preceding budget`);
    insist(budget.value.detail?.runId===turn.run,'budget run association');
    const usage=unique(rows,r=>r.type==='model_stream_event'&&r.turnId===turn.run&&r.step===step&&r.event?.type==='usage',`${turn.name} step ${step} usage`);
    const complete=unique(rows,r=>r.type==='step_completed'&&r.turnId===turn.run&&r.step===step,`${turn.name} step ${step} completed`);
    insist(request.index<usage.index&&usage.index<complete.index,'usage belongs to completed request');
    insist(isDeepStrictEqual(usage.value.event.usage,{inputTokens:12,outputTokens:4,totalTokens:16}),'actual model usage');
    return {...turn,step,request,budget,usage};
  }));
  insist(rows.filter(r=>r.type==='model_request').length===3&&rows.filter(budgetRow).length===3&&rows.filter(r=>r.type==='step_started').length===3&&rows.filter(r=>r.type==='model_stream_event'&&r.event?.type==='usage').length===3,'complete parent request/budget/usage membership');
  insist(requests.every((r,i)=>i===0||requests[i-1].usage.index<r.request.index),'request/calibration causal order');
  const historyStep=unique(a.steps??[],s=>s.step?.ws==='read_session_messages'&&s.step.params?.sessionKey==='agent03-parent','parent history step');
  const historyFrame=unique(stepFrames(a,historyStep.index),f=>f.type==='response','parent history response');
  const history=historyFrame.value.result; insist(history?.session?.sessionId==='agent03-parent','history session');
  return {rows,turns,requests,history,historyPath:`$.steps[${historyStep.index}].frames[${historyFrame.index}].result`};
}
function derivedValues(a, normalized, count) {
  insist(a?.scenario==='agent03-sdk-fork' && a.evidence?.scenario?.sha256===SCENARIO_SHA && a.evidence?.inputs?.ts?.commit==='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85' && Array.isArray(a.failures) && !a.failures.some(x=>!x.expected), 'capture identity or failures');
  const report=assertProvider(a), bindings=sdkAgentAssociations(a), {rows,turns,requests,history,historyPath}=bindings;
  sdkAgentOrderChildTree(a,{policy:'sdkAgent03DerivedValues'});
  validateCanonicalJsonlWitness(rows,a.evidence?.persistedFiles?.[PARENT_PATH],history.session.fileSize);
  const estimates=requests.map(r=>canonicalBreakdown(r.request.value.request,count));
  for(const [i,binding] of requests.entries()){
    const event=binding.budget.value.detail,breakdown=estimates[i];
    if(i===0){
      const initial={type:'context_budget',used:breakdown.total,displayUsed:breakdown.total,localEstimateTokens:breakdown.total,estimateSource:'estimator',total:65536,totalContextTokens:65536,maxContextTokens:57344,effectiveTotal:57344,effectiveContextTokens:57344,maxOutputTokens:8192,reservedOutputTokens:8192,warningRatio:0.8,blockingRatio:0.9,ratio:breakdown.total/57344,state:'ok',source:'local',exact:false,breakdown,runId:binding.run};
      insist(isDeepStrictEqual(event,initial),'initial canonical request estimate');
    }else{
      const blocks=binding.request.value.request?.messages?.flatMap(m=>m.content??[]).filter(b=>b.type==='tool_result'&&b.toolCallId==='ag03-agent');
      insist(blocks?.length===1&&blocks[0].content?.every(x=>x.type==='text')&&blocks[0].content.map(x=>x.text).join('\n')===report,'canonical agent result');
      validateBudget(event,breakdown,{actual:requests[i-1].usage.value.event.usage.inputTokens,estimated:estimates[i-1].total});
    }
  }
  const m=new Map(),semantic=new Map();
  const put=(key,path,value)=>{insist(!semantic.has(key)&&!m.has(path),'unique derived binding');semantic.set(key,{path,value});m.set(path,value)};
  const fields=(key,path,object,names=DERIVED_FIELDS)=>{for(const name of names){const value=get(object,name);insist(typeof value==='number'&&Number.isFinite(value),`missing derived value ${path}.${name}`);put(`${key}:${name}`,`${path}.${name}`,value)}};
  const registry=a.tree?.[REGISTRY_PATH]?.json?.events,normalizedRegistry=normalized?.tree?.[REGISTRY_PATH]?.json?.events;
  const projections=a.tree?.[PARENT_PATH+'.projections.json']?.json?.projections;
  insist(projections&&Array.isArray(projections['web-history.agent-statuses']?.state),'status projections');
  const statuses=projections['web-history.agent-statuses'].state;
  insist(statuses.filter(budgetRow).length===3&&history.messages?.filter(r=>r.payload?.event==='context_budget').length===3,'budget projection membership');
  for(const turn of turns){
    const own=requests.filter(r=>r.run===turn.run);
    const ws=stepFrames(a,turn.stepIndex).map((value,index)=>({value,index})).filter(x=>x.value.event?.type==='context_budget');
    const bucket=registry?.[`\0agent03-parent\0${turn.run}`];insist(Array.isArray(bucket),'registry run association');
    const regs=bucket.map((value,index)=>({value,index})).filter(x=>x.value.event?.type==='context_budget');
    insist(ws.length===own.length&&regs.length===own.length,'budget WS/registry count');
    for(const [ordinal,binding] of own.entries()){
      const event=binding.budget.value.detail,frame=ws[ordinal],reg=regs[ordinal];
      sameEvent(frame.value.event,event,'frame');sameEvent(reg.value.event,event,'registry');
      const hist=unique(history.messages,r=>r.payload?.event==='context_budget'&&r.turnId===binding.run&&r.sequence===binding.budget.value.sequence,'history budget identity');
      insist(hist.value.id===binding.budget.value.entryId,'history durable entry');sameEvent(hist.value.payload.detail,event,'history');
      const projection=unique(statuses,r=>budgetRow(r)&&r.turnId===binding.run&&r.sequence===binding.budget.value.sequence,'status projection identity');
      sameEvent(projection.value,binding.budget.value,'status projection durable row');
      const nframe=normalized?.steps?.[turn.stepIndex]?.frames?.[frame.index]?.event;
      insist(nframe?.type==='context_budget'&&typeof nframe.runId==='string','normalized frame identity');
      insist(DERIVED_FIELDS.every(field=>get(nframe,field)===get(event,field)),'normalized derived values unchanged');
      const nkey=`\0agent03-parent\0${nframe.runId}`,nreg=normalizedRegistry?.[nkey]?.[reg.index]?.event;
      sameEvent(nreg,nframe,'normalized registry budget');
      if(binding===requests[0])continue;
      const semanticKey=`${turn.name}:${binding.step}`;
      fields(semanticKey+':ws',`$.steps[${turn.stepIndex}].frames[${frame.index}].event`,event);
      fields(semanticKey+':history',`${historyPath}.messages[${hist.index}].payload.detail`,event);
      fields(semanticKey+':durable',`${prefix}.jsonl[${binding.budget.index}].detail`,event);
      fields(semanticKey+':projection',`${prefix}.projections.json.json.projections.web-history.agent-statuses.state[${projection.index}].detail`,event);
      fields(semanticKey+':registry',`$.tree.${REGISTRY_PATH}.json.events.${nkey}[${reg.index}].event`,event);
    }
  }
  insist(a.steps.flatMap(s=>s.frames??[]).filter(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent').length===1,'unique worker finished across deliveries');
  const firstTurn=turns[0],finished=unique(stepFrames(a,firstTurn.stepIndex),f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent','worker tool finished');
  const completed=finished.value.event;
  insist(completed.toolName==='agent'&&completed.ok===true&&completed.resultPreview===report&&completed.resultBytes===Buffer.byteLength(report,'utf8')&&isDeepStrictEqual(completed.data,JSON.parse(report.slice(PREFIX.length)))&&completed.runId===firstTurn.run,'agent report byte count');
  const result=unique(rows,r=>r.type==='tool_result'&&r.turnId===firstTurn.run&&r.result?.toolCallId==='ag03-agent','durable worker result');
  insist(result.value.step===1&&isDeepStrictEqual(result.value.result.data,completed.data),'durable worker business data');
  insist(rows.filter(r=>r.type==='tool_result'&&r.result?.toolCallId==='ag03-agent').length===1&&result.value.result.type==='success'&&result.value.result.toolName==='agent'&&isDeepStrictEqual(result.value.result.content,[{type:'text',text:PREFIX.trimEnd()},{type:'json',value:completed.data}]),'durable worker result content');
  const message=unique(rows,r=>r.type==='tool_result_message'&&r.turnId===firstTurn.run&&r.message?.content?.some(b=>b.type==='tool_result'&&b.toolCallId==='ag03-agent'),'durable worker message');
  const block=unique(message.value.message.content,b=>b.type==='tool_result'&&b.toolCallId==='ag03-agent','worker result block').value;
  insist(block.content.every(b=>b.type==='text')&&block.content.map(b=>b.text).join('\n')===report&&isDeepStrictEqual(block.raw,result.value.result),'worker message/raw result association');
  const conversation=projections['agent-transcript.conversation']?.state?.entries;
  insist(Array.isArray(conversation),'conversation projection');
  const projectedResult=unique(conversation,r=>r.entry?.entryId===message.value.entryId,'projected worker message');
  insist(projectedResult.value.index===message.index&&isDeepStrictEqual(projectedResult.value.entry,message.value),'worker conversation projection');
  const historyResult=unique(history.messages,r=>r.kind==='tool_result'&&r.toolCallId==='ag03-agent','worker history result').value;
  insist(historyResult.turnId===firstTurn.run&&historyResult.sequence===message.value.sequence&&historyResult.ok===true&&historyResult.toolName==='agent'&&historyResult.text===report,'worker history projection');
  const detail=unique(stepFrames(a,firstTurn.stepIndex),f=>f.event?.type==='tool_result_detail_available'&&f.event.toolCallId==='ag03-agent','worker WS detail');
  insist(detail.value.event.fullText===report&&detail.value.event.runId===firstTurn.run,'worker WS report detail');

  const regs=registry[`\0agent03-parent\0${firstTurn.run}`],regFinished=unique(regs,f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent','registry worker result');sameEvent(regFinished.value.event,completed,'agent result registry');
  const registryDetail=unique(regs,f=>f.event?.type==='tool_result_detail_available'&&f.event.toolCallId==='ag03-agent','worker registry detail');sameEvent(registryDetail.value.event,detail.value.event,'worker registry report detail');
  const ncompleted=normalized?.steps?.[firstTurn.stepIndex]?.frames?.[finished.index]?.event,nkey=`\0agent03-parent\0${ncompleted?.runId}`;
  sameEvent(normalizedRegistry?.[nkey]?.[regFinished.index]?.event,ncompleted,'normalized agent result registry');
  insist(ncompleted?.resultBytes===completed.resultBytes,'normalized report byte count');
  put('worker:ws:resultBytes',`$.steps[${firstTurn.stepIndex}].frames[${finished.index}].event.resultBytes`,completed.resultBytes);
  put('worker:registry:resultBytes',`$.tree.${REGISTRY_PATH}.json.events.${nkey}[${regFinished.index}].event.resultBytes`,completed.resultBytes);
  for(const name of ['parent-after','parent-check']){
    const request=a.provider[name][0],length=request.headers?.['content-length'];
    insist(request.method==='POST'&&request.headers?.['content-type']==='application/json'&&typeof length==='string'&&/^[1-9][0-9]*$/.test(length)&&Number(length)===Buffer.byteLength(JSON.stringify(request.body),'utf8'),'model request byte count');
    put(`provider:${name}:content-length`,`$.provider.${name}[0].headers.content-length`,length);
  }
  const last=requests.at(-1),event=last.budget.value.detail;
  insist(history.stream?.sessionKey==='agent03-parent'&&history.stream.runId===last.run,'history stream run');
  const stream=unique(history.stream.events,e=>e.type==='context_budget'&&e.runId===last.run,'history stream budget');sameEvent(stream.value,event,'history stream');fields('last:history-stream',`${historyPath}.stream.events[${stream.index}]`,event);
  const expectedUsage={used:event.used,displayUsed:event.displayUsed,total:65536,effectiveTotal:57344,reservedOutputTokens:8192,state:'ok',ratio:event.ratio,source:'history',exact:true};
  const usage=projections['web-history.token-usage']?.state?.latestContextBudget;
  insist(usage?.index===last.budget.index&&isDeepStrictEqual(history.tokenUsage,expectedUsage)&&isDeepStrictEqual(usage.usage,expectedUsage),'token usage projection');
  fields('last:history-usage',`${historyPath}.tokenUsage`,expectedUsage,['used','displayUsed','ratio']);
  fields('last:projection-usage',`${prefix}.projections.json.json.projections.web-history.token-usage.state.latestContextBudget.usage`,expectedUsage,['used','displayUsed','ratio']);
  put('history:fileSize',`${historyPath}.session.fileSize`,history.session.fileSize);
  return {values:m,semantic,bindings};
}

// normalized captures must be the exact output of diff.mjs's own normalizer,
// so registry UUID keys are tied to the same run rather than guessed.
export async function sdkAgentDerivedPaths(left,right,normalizedLeft,normalizedRight,declaration,baselineRoot) {
  const count = loadPinnedTokenizer(baselineRoot);
  insist(isDeepStrictEqual(declaration,{policy:'sdkAgent03DerivedValues'}), 'declaration');
  for (const name of ['parent-after','parent-check']) {
    const comparable = raw => {
      const body=structuredClone(raw.provider?.[name]?.[0]?.body);
      const results=body?.messages?.filter(m=>m.role==='tool' && m.tool_call_id==='ag03-agent');
      insist(results?.length===1, 'model request report association');
      validateAgentReport(results[0].content);
      results[0].content='<validated-agent-result>';
      return body;
    };
    insist(isDeepStrictEqual(comparable(left),comparable(right)), `model ${name} body differs outside report`);
  }
  const a=derivedValues(left,normalizedLeft,count),b=derivedValues(right,normalizedRight,count);
  for(const [i,binding] of a.bindings.requests.entries()){
    const other=b.bindings.requests[i];
    insist(binding.name===other.name&&binding.step===other.step&&isDeepStrictEqual(comparableRequest(binding.request.value.request),comparableRequest(other.request.value.request)),`canonical token inputs differ outside validated worker result at ${binding.name}/${binding.step}`);
  }
  const out=new Map();
  for(const [key,binding] of a.semantic){
    const other=b.semantic.get(key);insist(other,'derived semantic binding missing');
    // Preserve both original array paths. An independently interleaved observer
    // remains an ordinary structural DIFF, never an excuse to hide row/index changes.
    if(binding.path===other.path&&binding.value!==other.value)out.set(binding.path,'sdkAgent03DerivedValues: validated canonical request, calibration and exact projection');
  }
  return out;
}

// Compare every input consumed by the estimator. The one removed value has
// already been validated independently on both sides by assertProvider and
// bound to each canonical request in derivedValues.
function comparableRequest(request) {
  insist(request && typeof request.systemPrompt === 'string' && Array.isArray(request.messages) && Array.isArray(request.tools), 'comparable request');
  return {systemPrompt:request.systemPrompt, tools:request.tools.map(t=>({name:t.name,description:t.description,inputSchema:t.inputSchema})), messages:request.messages.map(m=>({role:m.role,content:m.content.map(b=>{
    if (b.type==='text') return {type:b.type,text:b.text};
    if (b.type==='tool_call') return {type:b.type,name:b.name,input:b.input};
    if (b.type==='tool_result' && b.toolCallId==='ag03-agent') return {type:b.type,toolCallId:b.toolCallId,content:'<validated-agent-result>'};
    if (b.type==='tool_result') return {type:b.type,toolCallId:b.toolCallId,content:b.content};
    throw new Error(`sdk agent policy: unsupported comparable block ${b.type}`);
  })}))};
}

// Single-side preflight for evidence.mjs. Raw registry keys are valid here:
// no diff paths are emitted and every value is checked against its own run.
export function sdkAgentContract(raw, declaration, baselineRoot) {
  insist(isDeepStrictEqual(declaration,{policy:'sdkAgent03DerivedValues'}), 'declaration');
  const count = loadPinnedTokenizer(baselineRoot);
  derivedValues(raw,raw,count);
}

// Reorder only the four known child transcript/projection entries in a clone.
// UUID aliases are assigned in traversal order, so semantic identity must be
// established before the normalizer sees the filesystem's lexical UUID order.
export function sdkAgentOrderChildTree(raw, declaration) {
  insist(isDeepStrictEqual(declaration,{policy:'sdkAgent03DerivedValues'}), 'declaration');
  insist(raw?.scenario==='agent03-sdk-fork' && raw.evidence?.scenario?.sha256===SCENARIO_SHA, 'scenario identity');
  const report=raw.provider?.['parent-after']?.[0]?.body?.messages?.filter(m=>m.role==='tool'&&m.tool_call_id==='ag03-agent');
  insist(raw.provider?.['parent-after']?.length===1 && report?.length===1, 'unique worker report');
  const worker=validateAgentReport(report[0].content).uuid;
  const parent=raw.tree?.[PARENT_PATH]?.jsonl;
  insist(Array.isArray(parent), 'parent transcript missing');
  const frames=raw.steps?.flatMap(s=>s.frames??[]).map(f=>f.event) ?? [];
  const ids=[];
  for (const role of ['worker','reviewer']) {
    const starts=parent.filter(x=>x?.type==='subagent_started'&&x.subagentType===role);
    const completed=parent.filter(x=>x?.type==='subagent_completed'&&x.subagentType===role);
    const emitted=frames.filter(e=>e?.type==='agent_status'&&e.event==='subagent_started'&&e.detail?.subagentType===role);
    insist(starts.length===1 && completed.length===1 && emitted.length===1, `unique ${role} lifecycle`);
    const start=starts[0],done=completed[0],id=start.subagentId;
    insist(typeof id==='string'&&new RegExp(`^${UUID}$`).test(id)&&done.subagentId===id&&emitted[0].detail.subagentId===id
      &&start.sessionId==='agent03-parent'&&done.sessionId===start.sessionId&&start.turnId===done.turnId&&emitted[0].runId===start.turnId
      &&parent.indexOf(start)<parent.indexOf(done)&&start.sequence<done.sequence && done.errored===false && start.subagentSessionId===`/tmp/pdrs-bbx/root/home/.pilotdeck::sub::${id}`
      &&start.transcriptRelativePath===`agent03-parent/subagents/${id}.jsonl`, `${role} references`);
    if(role==='worker') insist(id===worker && emitted[0].detail.toolCallId==='ag03-agent' && start.promptPreview==='AG03-CHILD-DIRECTIVE', 'worker report identity');
    else insist(start.promptPreview?.includes('Observed agent: worker') && done.summaryPreview==='AG03-OBSERVER-REPORT', 'reviewer identity');
    ids.push(id);
  }
  insist(ids[0]!==ids[1], 'worker and reviewer must differ');
  const dir=PARENT_PATH.slice(0,-'.jsonl'.length)+'/subagents/';
  const names=Object.keys(raw.tree).filter(k=>k.startsWith(dir));
  const ordered=ids.flatMap(id=>[`${dir}${id}.jsonl`,`${dir}${id}.jsonl.projections.json`]);
  insist(names.length===4 && ordered.every(k=>names.includes(k)), 'child tree membership');
  for(const id of ids) {
    const path=`${dir}${id}.jsonl`, rows=raw.tree[path]?.jsonl, projection=raw.tree[path+'.projections.json']?.json;
    const session=`/tmp/pdrs-bbx/root/home/.pilotdeck::sub::${id}`;
    insist(Array.isArray(rows)&&rows.length>0&&rows[0]?.type==='turn_started'&&rows[0].sessionId===session
      &&rows.every(x=>x.sessionId===session) && projection?.sessionId===session && projection.asOfSequence===rows.at(-1).sequence
      &&projection.anchor?.sequence===rows.at(-1).sequence && projection.anchor.entryId===rows.at(-1).entryId
      &&projection.anchor.parentEntryId===rows.at(-1).parentEntryId && projection.anchor.turnId===rows.at(-1).turnId
      &&projection.anchor.type===rows.at(-1).type, 'child transcript/projection association');
  }
  const clone=structuredClone(raw), entries=Object.entries(clone.tree), slots=[];
  for(let i=0;i<entries.length;i++) if(names.includes(entries[i][0])) slots.push(i);
  const children=new Map(ordered.map(k=>[k,clone.tree[k]]));
  for(let i=0;i<slots.length;i++) entries[slots[i]]=[ordered[i],children.get(ordered[i])];
  clone.tree=Object.fromEntries(entries);
  return clone;
}
