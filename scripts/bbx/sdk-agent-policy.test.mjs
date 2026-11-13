import test from 'node:test';
import assert from 'node:assert/strict';
import { validateAgentReport, validateBudget, validateRawJsonlSize, sdkAgentAssociations } from './sdk-agent-policy.mjs';

// Test-only UUID aliases preserve every array's real order and index.
function normalizeRuns(raw){
  const turns=sdkAgentAssociations(raw).turns;
  let text=JSON.stringify(raw);
  for(const turn of turns)text=text.replaceAll(turn.run,`<fake-run-${turn.name}>`);
  return JSON.parse(text);
}
const id = '83bd1e48-276c-4010-af3a-05daf122b04e';
const prefix = '[worker] AG03 delegate\n\nScope: done.\nResult: AG03-CHILD-REPORT.\nKey files: none\nFiles changed: none\nIssues: none\n';
const report = {subagentType:'worker',description:'AG03 delegate',text:'Scope: done.\nResult: AG03-CHILD-REPORT.\nKey files: none\nFiles changed: none\nIssues: none',usage:{inputTokens:48,outputTokens:16,totalTokens:64},turns:4,durationMs:88,parsed:{Scope:'done.',Result:'AG03-CHILD-REPORT.','Key files':'none','Files changed':'none',Issues:'none'},subagentSessionId:`/tmp/pdrs-bbx/root/home/.pilotdeck::sub::${id}`,transcriptRelativePath:`agent03-parent/subagents/${id}.jsonl`};
const breakdown = {source:'local_estimate',total:8232,system:674,tools:7301,messages:257,mcp:0,memory:0};
const budget = {type:'context_budget',used:4116,displayUsed:4116,localEstimateTokens:8232,estimateSource:'usage',usageTokens:4116,calibrationActualInputTokens:12,calibrationEstimatedInputTokens:7984,total:65536,totalContextTokens:65536,maxContextTokens:57344,effectiveTotal:57344,effectiveContextTokens:57344,maxOutputTokens:8192,reservedOutputTokens:8192,warningRatio:0.8,blockingRatio:0.9,ratio:4116/57344,state:'ok',source:'calibrated',exact:false,breakdown};
test('worker report requires exact business data and linked child UUID', () => {
  assert.equal(validateAgentReport(prefix + JSON.stringify(report)).uuid, id);
  assert.throws(() => validateAgentReport(prefix + JSON.stringify({...report, text:'wrong result'})));
  assert.throws(() => validateAgentReport(prefix + JSON.stringify({...report, transcriptRelativePath:'agent03-parent/subagents/00000000-0000-4000-8000-000000000000.jsonl'})));
});
test('budget rejects a token, calibration or derived projection mutation', () => {
  assert.equal(validateBudget(budget, breakdown, {actual:12,estimated:7984}), 4116);
  assert.throws(() => validateBudget({...budget,breakdown:{...breakdown,messages:258}}, breakdown, {actual:12,estimated:7984}));
  assert.throws(() => validateBudget({...budget,calibrationEstimatedInputTokens:7985}, breakdown, {actual:12,estimated:7984}));
  assert.throws(() => validateBudget({...budget,ratio:0.1}, breakdown, {actual:12,estimated:7984}));
});
test('size requires actual JSONL bytes and rejects wrong length', () => {
  const raw = Buffer.from('{"a":1}\n');
  validateRawJsonlSize(raw, [{a:1}], raw.length);
  assert.throws(() => validateRawJsonlSize(raw, [{a:1}], raw.length + 1));
  assert.throws(() => validateRawJsonlSize(undefined, [{a:1}], raw.length));
});
test('canonical request counts every text, tool argument, result and schema', async () => {
  const {canonicalBreakdown} = await import('./sdk-agent-policy.mjs');
  const count = s => s.length;
  const request = {systemPrompt:'abc', tools:[{name:'tool',description:'d',inputSchema:{x:1}}],messages:[{role:'user',content:[{type:'text',text:'hello'}]},{role:'assistant',content:[{type:'tool_call',name:'tool',input:{x:1}}]},{role:'user',content:[{type:'tool_result',content:[{type:'text',text:'yes'},{type:'text',text:'no'}]}]}]};
  const b = canonicalBreakdown(request,count);
  assert.deepEqual(b, {source:'local_estimate',system:3,tools:12,messages:34,total:49,mcp:0,memory:0});
  const altered = structuredClone(request); altered.messages[0].content[0].text += '!';
  assert.equal(canonicalBreakdown(altered,count).messages, b.messages + 1);
  assert.throws(() => canonicalBreakdown({...request,systemPrompt:'<memory-context>x</memory-context>'},count));
});
test('canonical JSONL size requires matching byte length and hash', async () => {
  const {createHash} = await import('node:crypto');
  const {validateCanonicalJsonlWitness} = await import('./sdk-agent-policy.mjs');
  const rows = [{a:1},{b:'世界'}];
  const bytes = Buffer.from(rows.map(x => JSON.stringify(x)+'\n').join(''));
  const witness = {byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')};
  assert.equal(await validateCanonicalJsonlWitness(rows,witness,bytes.length),bytes.length);
  assert.throws(()=>validateCanonicalJsonlWitness(rows,undefined,bytes.length));
  assert.throws(()=>validateCanonicalJsonlWitness(rows,{...witness,sha256:'0'.repeat(64)},bytes.length));
  assert.throws(()=>validateCanonicalJsonlWitness(rows,witness,bytes.length+1));
  assert.throws(()=>validateCanonicalJsonlWitness([{a:2},rows[1]],witness,bytes.length));
});

test('captured scenario diagnostic rejects causal mutations', {skip: !process.env.AGENT03_DIAGNOSTIC_ROOT}, async () => {
  const {readFileSync} = await import('node:fs');
  const {createHash} = await import('node:crypto');
  const {sdkAgentDerivedPaths,sdkAgentContract} = await import('./sdk-agent-policy.mjs');
  const dir=process.env.AGENT03_DIAGNOSTIC_ROOT;
  const base=process.env.AGENT03_BASELINE_ROOT;
  const raws=[1,2].map(i=>JSON.parse(readFileSync(`${dir}/scenario.ts-${i}.json`)));
  const path='/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl';
  const normalize = normalizeRuns;
  const witness = a => { const bytes=Buffer.from(a.tree[path].jsonl.map(x=>JSON.stringify(x)+'\n').join('')); a.evidence.persistedFiles={[path]:{byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')}}; };
  // Old capture is used only as diagnostic data; its actual identity has not changed.
  raws.forEach(a=>{ witness(a); a.evidence.scenario.sha256='9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca'; });
  const run=async(a,b)=>sdkAgentDerivedPaths(a,b,normalize(a),normalize(b),{policy:'sdkAgent03DerivedValues'},base);
  await sdkAgentContract(raws[0],{policy:'sdkAgent03DerivedValues'},base);
  assert.throws(()=>sdkAgentContract(raws[0],{policy:'unknown'},base));
  assert.equal((await run(...raws)).size,90);
  const corruptions = [
    a=>{a.provider['parent-after'][0].body.messages.find(m=>m.tool_call_id==='ag03-agent').content+='wrong';},
    a=>{sdkAgentAssociations(a).requests[1].request.value.request.messages[0].content[0].text+='extra token';},
    a=>{sdkAgentAssociations(a).requests[1].budget.value.detail.calibrationActualInputTokens++;},
    a=>{a.steps[6].frames[0].result.session.fileSize++;},
    a=>{a.steps.find(s=>s.step?.params?.message==='AG03-PARENT-BEGIN').frames.filter(f=>f.event?.type==='context_budget')[1].event.used++;},
    a=>{a.provider.observer[0].body.tools.push({function:{name:'write_file'}});},
    a=>{delete a.evidence.persistedFiles;},
  ];
  for(const corrupt of corruptions) {const a=structuredClone(raws[0]);corrupt(a);if(a.evidence.persistedFiles)witness(a);await assert.rejects(run(a,raws[1]));}
});

test('actual e8 capture derives only validated report and HTTP lengths', {skip: !process.env.AGENT03_E8_ROOT}, async () => {
  const {readFileSync} = await import('node:fs');
  const {sdkAgentDerivedPaths,sdkAgentContract} = await import('./sdk-agent-policy.mjs');
  const dir=process.env.AGENT03_E8_ROOT, base=process.env.AGENT03_BASELINE_ROOT;
  const raws=[1,2].map(i=>JSON.parse(readFileSync(`${dir}/agent03-sdk-fork.ts-${i}.json`)));
  const normalize = normalizeRuns;
  const run=async(a,b)=>sdkAgentDerivedPaths(a,b,normalize(a),normalize(b),{policy:'sdkAgent03DerivedValues'},base);
  sdkAgentContract(raws[0],{policy:'sdkAgent03DerivedValues'},base);
  const paths=await run(...raws);
  assert.equal(paths.size,94);
  for(const p of ['$.steps[0].frames[24].event.resultBytes','$.provider.parent-after[0].headers.content-length','$.provider.parent-check[0].headers.content-length']) assert.ok(paths.has(p));
  const corruptions=[
    a=>a.steps.flatMap(s=>s.frames??[]).find(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent').event.resultBytes++,
    a=>a.provider['parent-after'][0].headers['content-length']='41236',
    a=>{const r=a.provider['parent-check'][0];r.body.messages[0].content+=' extra';r.headers['content-length']=String(Buffer.byteLength(JSON.stringify(r.body)));},
    a=>{const r=a.provider['parent-after'][0];r.body.messages.find(m=>m.tool_call_id==='ag03-agent').content+=' wrong';r.headers['content-length']=String(Buffer.byteLength(JSON.stringify(r.body)));},
    a=>{const run=sdkAgentAssociations(a).turns[0].run;a.tree['/home/.pilotdeck/runs/registry.json'].json.events[`\0agent03-parent\0${run}`].find(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent').event.resultBytes++;},
  ];
  for(const corrupt of corruptions) {const a=structuredClone(raws[0]);corrupt(a);await assert.rejects(run(a,raws[1]));}
});

test('4bf child-tree ordering follows verified worker/reviewer references', {skip: !process.env.AGENT03_4BF_ROOT}, async () => {
  const {readFileSync}=await import('node:fs');
  const {sdkAgentOrderChildTree}=await import('./sdk-agent-policy.mjs');
  const dir=process.env.AGENT03_4BF_ROOT;
  const captures=[1,2].map(i=>JSON.parse(readFileSync(`${dir}/agent03-sdk-fork.ts-${i}.json`)));
  const decl={policy:'sdkAgent03DerivedValues'};
  const parent='/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl';
  const roleOrder=a=>Object.keys(a.tree).filter(k=>k.includes('/chats/agent03-parent/subagents/') && k.endsWith('.jsonl')).map(k=>{
    const row=a.tree[parent].jsonl.find(x=>x.type==='subagent_started'&&k.endsWith('/'+x.subagentId+'.jsonl'));return row?.subagentType;
  });
  assert.deepEqual(roleOrder(captures[0]),['worker','reviewer']);
  assert.deepEqual(roleOrder(captures[1]),['reviewer','worker']);
  for(const a of captures){
    const before=JSON.stringify(a), result=sdkAgentOrderChildTree(a,decl);
    assert.equal(JSON.stringify(a),before,'original capture must not mutate');
    assert.deepEqual(roleOrder(result),['worker','reviewer']);
    assert.deepEqual(Object.entries(result.tree).filter(([p])=>!p.includes('/chats/agent03-parent/subagents/')),Object.entries(a.tree).filter(([p])=>!p.includes('/chats/agent03-parent/subagents/')));
  }
  const faults=[
    a=>{let s=a.tree[parent].jsonl.find(x=>x.type==='subagent_started'&&x.subagentType==='worker');s.subagentId='00000000-0000-4000-8000-000000000000';},
    a=>{let starts=a.tree[parent].jsonl.filter(x=>x.type==='subagent_started');[starts[0].subagentType,starts[1].subagentType]=[starts[1].subagentType,starts[0].subagentType];},
    a=>{let p=Object.keys(a.tree).find(k=>k.includes('/subagents/')&&k.endsWith('.projections.json'));delete a.tree[p];},
    a=>{a.tree[parent.replace('.jsonl','/subagents/extra.jsonl')]={jsonl:[]};},
    a=>{let p=Object.keys(a.tree).find(k=>k.includes('/subagents/')&&k.endsWith('.projections.json'));a.tree[p].json.anchor.entryId='00000000-0000-4000-8000-000000000000';},
    a=>{let e=a.steps.flatMap(x=>x.frames??[]).map(x=>x.event).find(x=>x?.type==='agent_status'&&x.event==='subagent_started'&&x.detail?.subagentType==='reviewer');e.detail.subagentId='00000000-0000-4000-8000-000000000000';},
  ];
  for(const fault of faults){const a=structuredClone(captures[0]);fault(a);assert.throws(()=>sdkAgentOrderChildTree(a,decl));}
  const changed=structuredClone(captures[0]), child=Object.keys(changed.tree).find(k=>k.includes('/subagents/')&&k.endsWith('.jsonl'));
  changed.tree[child].jsonl[1].extraMutation='must remain comparable';
  assert.equal(sdkAgentOrderChildTree(changed,decl).tree[child].jsonl[1].extraMutation,'must remain comparable');
});

test('actual TS/Rust observer interleavings bind budgets to semantic request identities', {skip: !process.env.AGENT03_CURRENT_PAIR_ROOT}, async t => {
  const {readFileSync}=await import('node:fs');const {createHash}=await import('node:crypto');
  const {sdkAgentContract,sdkAgentDerivedPaths}=await import('./sdk-agent-policy.mjs');
  const root=process.env.AGENT03_CURRENT_PAIR_ROOT,base=process.env.AGENT03_BASELINE_ROOT;
  const raw=['ts','rust'].map(side=>JSON.parse(readFileSync(`${root}/agent03-sdk-fork.${side}.json`)));
  const parent='/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl',registry='/home/.pilotdeck/runs/registry.json';
  const decl={policy:'sdkAgent03DerivedValues'};
  const refreshWitness=a=>{
    const bytes=Buffer.from(a.tree[parent].jsonl.map(row=>JSON.stringify(row)+'\n').join(''));
    a.evidence.persistedFiles[parent]={byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')};
    const step=a.steps.find(s=>s.step?.ws==='read_session_messages'&&s.step.params.sessionKey==='agent03-parent');step.frames.find(f=>f.type==='response').result.session.fileSize=bytes.length;
  };
  const assoc=a=>sdkAgentAssociations(a), budget=a=>assoc(a).requests[1].budget.value,usage=a=>assoc(a).requests[0].usage.value;
  const frames=a=>a.steps.flatMap(s=>s.frames??[]);
  const budgetFrame=a=>{const run=assoc(a).turns[0].run;return frames(a).filter(f=>f.event?.type==='context_budget'&&f.event.runId===run)[1]};
  const observer=a=>frames(a).filter(f=>f.event?.type==='agent_status'&&f.event.detail?.subagentType==='reviewer');
  await t.test('both actual partial orders pass without altering original rows',async()=>{
    const snapshots=raw.map(JSON.stringify);
    for(const a of raw)sdkAgentContract(a,decl,base);
    assert.deepEqual(assoc(raw[0]).requests.map(r=>[r.request.index,r.budget.index,r.usage.index]),[[10,9,15],[33,32,37],[52,51,56]]);
    assert.deepEqual(assoc(raw[1]).requests.map(r=>[r.request.index,r.budget.index,r.usage.index]),[[10,9,15],[34,33,38],[52,51,56]]);
    const map=await sdkAgentDerivedPaths(...raw,...raw.map(normalizeRuns),decl,base);
    assert.ok(map.size>0,'same-path derived numbers are still checked');
    assert.ok(!map.has('$.steps[0].frames[28].event.used'),'TS budget slot is not permitted when Rust slot has another semantic event');
    assert.ok(!map.has(`$.tree.${parent}.jsonl[32].detail.used`),'different durable row positions remain ordinary structural diffs');
    assert.deepEqual(raw.map(JSON.stringify),snapshots,'original artifacts stay byte-for-byte in memory');
  });
  const faults=[
    ['missing budget',a=>{const x=assoc(a).requests[1];a.tree[parent].jsonl.splice(x.budget.index,1)}],
    ['duplicate budget',a=>{const x=assoc(a).requests[1];a.tree[parent].jsonl.splice(x.budget.index,0,structuredClone(x.budget.value))}],
    ['wrong budget run',a=>budget(a).detail.runId='wrong-run'],
    ['wrong budget parent',a=>budget(a).sessionId='other-parent'],
    ['wrong request step',a=>{assoc(a).requests[1].request.value.step=7}],
    ['wrong request turn',a=>{assoc(a).requests[1].request.value.turnId=assoc(a).turns[1].run}],
    ['missing preceding usage',a=>{const x=assoc(a).requests[0];a.tree[parent].jsonl.splice(x.usage.index,1)}],
    ['duplicate preceding usage',a=>{const x=assoc(a).requests[0];a.tree[parent].jsonl.splice(x.usage.index,0,structuredClone(x.usage.value))}],
    ['usage from wrong step',a=>{usage(a).step=2}],
    ['usage from next turn',a=>{usage(a).turnId=assoc(a).turns[1].run}],
    ['wrong usage amount',a=>{usage(a).event.usage.inputTokens++}],
    ['wrong calibration source',a=>{budget(a).detail.calibrationEstimatedInputTokens++}],
    ['wrong used number',a=>{budget(a).detail.used++}],
    ['wrong breakdown number',a=>{budget(a).detail.breakdown.messages++}],
    ['wrong initial estimate',a=>{assoc(a).requests[0].budget.value.detail.used++}],
    ['canonical business text changed',a=>{assoc(a).requests[1].request.value.request.messages[0].content[0].text+='changed business'}],
    ['WS numeric projection changed',a=>{budgetFrame(a).event.used++}],
    ['registry numeric projection changed',a=>{const run=assoc(a).turns[0].run;a.tree[registry].json.events[`\0agent03-parent\0${run}`].filter(f=>f.event?.type==='context_budget')[1].event.used++}],
    ['history projection changed',a=>{const h=assoc(a);h.history.messages.find(r=>r.payload?.detail?.runId===h.turns[1].run).payload.detail.used++}],
    ['durable status projection changed',a=>{a.tree[parent+'.projections.json'].json.projections['web-history.agent-statuses'].state[1].detail.used++}],
    ['latest usage points to wrong durable row',a=>{a.tree[parent+'.projections.json'].json.projections['web-history.token-usage'].state.latestContextBudget.index--}],
    ['history stream changed',a=>{assoc(a).history.stream.events.find(e=>e.type==='context_budget').used++}],
    ['worker byte projection changed',a=>{frames(a).find(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent').event.resultBytes++}],
    ['worker durable payload changed',a=>{a.tree[parent].jsonl.find(r=>r.type==='tool_result'&&r.result?.toolCallId==='ag03-agent').result.data.turns++}],
    ['worker durable content changed',a=>{a.tree[parent].jsonl.find(r=>r.type==='tool_result'&&r.result?.toolCallId==='ag03-agent').result.content[0].text+=' wrong'}],
    ['worker message raw result changed',a=>{a.tree[parent].jsonl.find(r=>r.type==='tool_result_message').message.content[0].raw.data.turns++}],
    ['worker conversation projection changed',a=>{a.tree[parent+'.projections.json'].json.projections['agent-transcript.conversation'].state.entries.find(r=>r.entry.type==='tool_result_message').entry.message.content[0].content[0].text+=' wrong'}],
    ['worker history result changed',a=>{assoc(a).history.messages.find(r=>r.kind==='tool_result'&&r.toolCallId==='ag03-agent').text+=' wrong'}],
    ['worker WS detail changed',a=>{frames(a).find(f=>f.event?.type==='tool_result_detail_available'&&f.event.toolCallId==='ag03-agent').event.fullText+=' wrong'}],
    ['worker registry detail changed',a=>{const run=assoc(a).turns[0].run;a.tree[registry].json.events[`\0agent03-parent\0${run}`].find(f=>f.event?.type==='tool_result_detail_available'&&f.event.toolCallId==='ag03-agent').event.fullText+=' wrong'}],
    ['worker duplicate finished in another delivery',a=>{const source=frames(a).find(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='ag03-agent');a.steps.find(s=>s.step?.params?.message==='AG03-PARENT-CHECK').frames.push(structuredClone(source))}],
    ['observer durable completion before start',a=>{const rows=a.tree[parent].jsonl,from=rows.findIndex(r=>r.type==='subagent_completed'&&r.subagentType==='reviewer'),to=rows.findIndex(r=>r.type==='subagent_started'&&r.subagentType==='reviewer');const [row]=rows.splice(from,1);rows.splice(to,0,row)}],
    ['observer completion missing',a=>{for(const s of a.steps)s.frames=s.frames?.filter(f=>!(f.event?.event==='subagent_completed'&&f.event.detail?.subagentType==='reviewer'))}],
    ['observer duplicate completion',a=>{const step=a.steps.find(s=>s.frames?.some(f=>f.event?.event==='subagent_completed'&&f.event.detail?.subagentType==='reviewer'));const f=step.frames.find(f=>f.event?.event==='subagent_completed'&&f.event.detail?.subagentType==='reviewer');step.frames.push(structuredClone(f))}],
    ['observer completed before report',a=>{const e=observer(a);[e[1].event,e[2].event]=[e[2].event,e[1].event]}],
    ['observer delivered under wrong run',a=>{observer(a)[1].event.runId='wrong-run'}],
    ['observer durable wrong parent turn',a=>{a.tree[parent].jsonl.find(r=>r.type==='subagent_completed'&&r.subagentType==='reviewer').turnId=assoc(a).turns[1].run}],
    ['child projection identity changed',a=>{const p=Object.keys(a.tree).find(k=>k.includes('/subagents/')&&k.endsWith('.projections.json'));a.tree[p].json.anchor.turnId='wrong-child-turn'}],
    ['observer tool isolation changed',a=>{a.provider.observer[0].body.tools.push({function:{name:'write_file'}})}],
    ['MCP lifecycle missing',a=>{a.tree['/home/.pilotdeck/agent03-mcp-log.jsonl'].jsonl=a.tree['/home/.pilotdeck/agent03-mcp-log.jsonl'].jsonl.filter(e=>e.event!=='exit')}],
  ];
  for(const [label,mutate] of faults)await t.test(label,()=>{
    const a=structuredClone(raw[1]);mutate(a);refreshWitness(a);assert.throws(()=>sdkAgentContract(a,decl,base),label);
  });
});
