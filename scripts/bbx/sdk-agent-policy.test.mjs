import test from 'node:test';
import assert from 'node:assert/strict';
import { validateAgentReport, validateBudget, validateRawJsonlSize } from './sdk-agent-policy.mjs';

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
  const normalize = raw => {
    const b=structuredClone(raw), events=b.tree['/home/.pilotdeck/runs/registry.json'].json.events;
    for(const [step,index] of [[0,28],[3,6]]) { const e=b.steps[step].frames[index].event, old=`\0agent03-parent\0${e.runId}`, id=`<fake-run-${step}>`; events[`\0agent03-parent\0${id}`]=events[old]; delete events[old]; e.runId=id; }
    return b;
  };
  const witness = a => { const bytes=Buffer.from(a.tree[path].jsonl.map(x=>JSON.stringify(x)+'\n').join('')); a.evidence.persistedFiles={[path]:{byteLength:bytes.length,sha256:createHash('sha256').update(bytes).digest('hex')}}; };
  // Old capture is used only as diagnostic data; its actual identity has not changed.
  raws.forEach(a=>{ witness(a); a.evidence.scenario.sha256='9f8c13a2e92a85869dfb5ee4ed602560fe1002b78888a111068c6a511c4563ca'; });
  const run=(a,b)=>sdkAgentDerivedPaths(a,b,normalize(a),normalize(b),{policy:'sdkAgent03DerivedValues'},base);
  await sdkAgentContract(raws[0],{policy:'sdkAgent03DerivedValues'},base);
  assert.throws(()=>sdkAgentContract(raws[0],{policy:'unknown'},base));
  assert.equal((await run(...raws)).size,90);
  const corruptions = [
    a=>{a.provider['parent-after'][0].body.messages.find(m=>m.tool_call_id==='ag03-agent').content+='wrong';},
    a=>{a.tree[path].jsonl[33].request.messages[0].content[0].text+='extra token';},
    a=>{a.tree[path].jsonl[32].detail.calibrationActualInputTokens++;},
    a=>{a.steps[6].frames[0].result.session.fileSize++;},
    a=>{a.steps[0].frames[28].event.used++;},
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
  const normalize = raw => {
    const b=structuredClone(raw), events=b.tree['/home/.pilotdeck/runs/registry.json'].json.events;
    for(const [step,index] of [[0,28],[3,6]]) {const e=b.steps[step].frames[index].event, old=`\0agent03-parent\0${e.runId}`, id=`<fake-run-${step}>`;events[`\0agent03-parent\0${id}`]=events[old];delete events[old];e.runId=id;}
    return b;
  };
  const run=(a,b)=>sdkAgentDerivedPaths(a,b,normalize(a),normalize(b),{policy:'sdkAgent03DerivedValues'},base);
  sdkAgentContract(raws[0],{policy:'sdkAgent03DerivedValues'},base);
  const paths=await run(...raws);
  assert.equal(paths.size,94);
  for(const p of ['$.steps[0].frames[24].event.resultBytes','$.provider.parent-after[0].headers.content-length','$.provider.parent-check[0].headers.content-length']) assert.ok(paths.has(p));
  const corruptions=[
    a=>a.steps[0].frames[24].event.resultBytes++,
    a=>a.provider['parent-after'][0].headers['content-length']='41236',
    a=>{const r=a.provider['parent-check'][0];r.body.messages[0].content+=' extra';r.headers['content-length']=String(Buffer.byteLength(JSON.stringify(r.body)));},
    a=>{const r=a.provider['parent-after'][0];r.body.messages.find(m=>m.tool_call_id==='ag03-agent').content+=' wrong';r.headers['content-length']=String(Buffer.byteLength(JSON.stringify(r.body)));},
    a=>{const id=a.steps[0].frames[28].event.runId;a.tree['/home/.pilotdeck/runs/registry.json'].json.events[`\0agent03-parent\0${id}`][24].event.resultBytes++;},
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
