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
function addFields(map, path, object, fields = DERIVED_FIELDS) {
  for (const field of fields) {
    const value = get(object, field);
    insist(typeof value === 'number' && Number.isFinite(value), `missing derived value ${path}.${field}`);
    map.set(`${path}.${field}`, value);
  }
}
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
  const i = ordered.findIndex(e=>e.event==='subagent_started'), j = ordered.findIndex(e=>e.event==='subagent_text_delta'&&e.detail.text==='AG03-OBSERVER-REPORT'), k = ordered.findIndex(e=>e.event==='subagent_completed'&&e.detail.success===true);
  insist(i>=0 && j>i && k>j, 'observer ordered report');
  return report;
}

function derivedValues(a, normalized, count) {
  insist(a?.scenario==='agent03-sdk-fork' && a.evidence?.scenario?.sha256===SCENARIO_SHA && a.evidence?.inputs?.ts?.commit==='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85' && Array.isArray(a.failures) && !a.failures.some(x=>!x.expected), 'capture identity or failures');
  const report = assertProvider(a);
  const rows = a.tree?.[PARENT_PATH]?.jsonl;
  insist(Array.isArray(rows) && rows.length > 56, 'parent transcript');
  const history = a.steps?.[6]?.frames?.[0]?.result;
  insist(history?.session?.sessionId==='agent03-parent', 'history session');
  validateCanonicalJsonlWitness(rows,a.evidence?.persistedFiles?.[PARENT_PATH],history.session.fileSize);
  const previous = canonicalBreakdown(rows[10]?.request,count);
  insist(isDeepStrictEqual(previous,rows[9]?.detail?.breakdown) && rows[9]?.detail?.localEstimateTokens===previous.total, 'initial request estimate');
  const events=[];
  for (const [i, index] of [32,51].entries()) {
    const entry=rows[index], request=rows[index+1];
    insist(entry?.type==='agent_status_message' && entry.event==='context_budget' && entry.sessionId==='agent03-parent' && request?.type==='model_request' && request.sessionId===entry.sessionId && request.turnId===entry.turnId && entry.detail?.runId===entry.turnId, 'budget request association');
    const blocks=request.request?.messages?.flatMap(m=>m.content??[]).filter(b=>b.type==='tool_result'&&b.toolCallId==='ag03-agent');
    insist(blocks?.length===1 && blocks[0].content?.every(x=>x.type==='text') && blocks[0].content.map(x=>x.text).join('\n')===report, 'canonical agent result');
    const breakdown=canonicalBreakdown(request.request,count);
    const usage=rows[i===0?15:37];
    insist(usage?.type==='model_stream_event' && usage.event?.type==='usage' && usage.event.usage?.inputTokens===12 && usage.turnId===rows[32].turnId && usage.step===i+1, 'calibration usage source');
    validateBudget(entry.detail,breakdown,{actual:usage.event.usage.inputTokens,estimated:i===0?previous.total:events[0].localEstimateTokens});
    events.push(entry.detail);
  }
  const m=new Map();
  for (const [i, event] of events.entries()) {
    const frame = a.steps?.[i===0?0:3]?.frames?.[i===0?28:6]?.event;
    const detail = history.messages?.[i===0?4:7]?.payload?.detail;
    const projections = a.tree?.[PARENT_PATH+'.projections.json']?.json?.projections;
    const projected = projections?.['web-history.agent-statuses']?.state?.[i+1]?.detail;
    sameEvent(frame,event,'frame'); sameEvent(detail,event,'history'); sameEvent(projected,event,'projection');
    for (const path of [`$.steps[${i===0?0:3}].frames[${i===0?28:6}].event`, `$.steps[6].frames[0].result.messages[${i===0?4:7}].payload.detail`, `${prefix}.jsonl[${i===0?32:51}].detail`, `${prefix}.projections.json.json.projections.web-history.agent-statuses.state[${i+1}].detail`]) addFields(m,path,event);
    const normalizedEvent=normalized?.steps?.[i===0?0:3]?.frames?.[i===0?28:6]?.event;
    const regs=normalized?.tree?.[REGISTRY_PATH]?.json?.events;
    const registry=Object.entries(regs??{}).filter(([key])=>key===`\0agent03-parent\0${normalizedEvent?.runId}`);
    insist(registry.length===1, 'normalized registry run association');
    const index=i===0?28:6;
    insist(DERIVED_FIELDS.every(field => get(registry[0][1]?.[index]?.event,field)===get(event,field)), 'normalized registry projection');
    const originalRegistry=a.tree?.[REGISTRY_PATH]?.json?.events?.[`\0agent03-parent\0${event.runId}`]?.[index]?.event;
    sameEvent(originalRegistry,event,'registry');
    addFields(m,`$.tree.${REGISTRY_PATH}.json.events.${registry[0][0]}[${index}].event`,event);
  }
  sameEvent(history.stream?.events?.[6],events[1],'history stream');
  addFields(m,'$.steps[6].frames[0].result.stream.events[6]',events[1]);
  const expectedUsage={used:events[1].used,displayUsed:events[1].displayUsed,total:65536,effectiveTotal:57344,reservedOutputTokens:8192,state:'ok',ratio:events[1].ratio,source:'history',exact:true};
  const projectionUsage=a.tree[PARENT_PATH+'.projections.json'].json.projections['web-history.token-usage'].state.latestContextBudget;
  insist(projectionUsage.index===51 && isDeepStrictEqual(history.tokenUsage,expectedUsage) && isDeepStrictEqual(projectionUsage.usage,expectedUsage), 'token usage projection');
  for (const p of ['$.steps[6].frames[0].result.tokenUsage',`${prefix}.projections.json.json.projections.web-history.token-usage.state.latestContextBudget.usage`]) addFields(m,p,expectedUsage,['used','displayUsed','ratio']);
  m.set('$.steps[6].frames[0].result.session.fileSize',history.session.fileSize);
  return m;
}

// normalized captures must be the exact output of diff.mjs's own normalizer,
// so registry UUID keys are tied to the same run rather than guessed.
export async function sdkAgentDerivedPaths(left,right,normalizedLeft,normalizedRight,declaration,baselineRoot) {
  const count = loadPinnedTokenizer(baselineRoot);
  insist(isDeepStrictEqual(declaration,{policy:'sdkAgent03DerivedValues'}), 'declaration');
  for (const index of [10,33,52]) {
    const a = comparableRequest(left.tree?.[PARENT_PATH]?.jsonl?.[index]?.request);
    const b = comparableRequest(right.tree?.[PARENT_PATH]?.jsonl?.[index]?.request);
    insist(isDeepStrictEqual(a,b), `canonical token inputs differ outside validated worker result at ${index}`);
  }
  const a=derivedValues(left,normalizedLeft,count), b=derivedValues(right,normalizedRight,count);
  const out=new Map();
  for (const [path,value] of a) if (b.has(path) && value!==b.get(path)) out.set(path,'sdkAgent03DerivedValues: validated canonical request, calibration and exact projection');
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
