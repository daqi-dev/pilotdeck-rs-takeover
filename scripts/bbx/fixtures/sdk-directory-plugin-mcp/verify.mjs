#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
const scenePath=fileURLToPath(new URL('../../scenarios/sdk-directory-plugin-mcp.json',import.meta.url));
const sceneBytes=fs.readFileSync(scenePath), scene=JSON.parse(sceneBytes);
const sha=b=>createHash('sha256').update(b).digest('hex');
const text=v=>typeof v==='string'?v:Array.isArray(v)?v.filter(b=>b.type==='text').map(b=>b.text).join(''):'';
function only(rows,label){assert.equal(rows?.length,1,label);return rows[0]}
function validate(a){
 assert.equal(a.scenario,scene.name);assert.deepEqual(a.failures,[]);
 assert.equal(a.evidence?.scenario?.sha256,sha(sceneBytes),'exact candidate input');
 assert.ok(['ts','rust'].includes(a.side));assert.equal(a.evidence.side,a.side);
 assert.equal(a.evidence.assertions.length,scene.assertions.length);
 for(const expected of scene.assertions){const row=only(a.evidence.assertions.filter(r=>r.id===expected.id),expected.id);assert.equal(row.passed,true);assert.ok(!row.skipped)}
 assert.equal(a.steps.length,4);
 const records={};
 for(const name of ['a-initial','a-final','isolated','b-initial','b-final','clear-initial','clear-final']){
  const r=only(a.provider[name],name);records[name]=r;
  assert.equal(r.method,'POST');assert.equal(r.url,'/v1/chat/completions');assert.equal(r.body.model,'deterministic');assert.equal(r.body.stream,true);
 }
 assert.equal(a.provider.unexpected?.length??0,0);assert.equal(a.provider.default?.length??0,0);
 const names=r=>(r.body.tools??[]).map(t=>t.function?.name??t.name);
 for(const name of ['a-initial','a-final','b-initial','b-final'])assert.equal(names(records[name]).filter(n=>n==='mcp__sdk-owned__probe').length,1,name+' real tool advertisement');
 for(const name of ['isolated','clear-initial','clear-final'])assert.ok(!names(records[name]).some(n=>n.startsWith('mcp__sdk-owned__')),name+' scope leaked');
 const result=(name,id)=>text(only(records[name].body.messages.filter(m=>m.role==='tool'&&m.tool_call_id===id),id).content);
 assert.ok(result('a-final','call_sdk_mcp_a').includes('"marker":"SDK-MCP-CALL-A","previousAlive":null'),'real A call');
 assert.ok(result('b-final','call_sdk_mcp_b').includes('"marker":"SDK-MCP-CALL-B","previousAlive":false'),'B call observes exact old PID gone before result/model continuation');
 assert.ok(result('clear-final','call_sdk_mcp_clear').includes('"marker":"SDK-MCP-CLEAR-PROBE","aAlive":false,"bAlive":false'),'actual builtin process observes both old PIDs gone after clear');
 const eventFiles=Object.entries(a.tree).filter(([name])=>name.endsWith('/sdk-mcp-witness/events.jsonl'));
 const events=only(eventFiles,'actual server event journal')[1].jsonl;assert.ok(Array.isArray(events));
 for(const generation of ['A','B']){
  const rows=events.filter(r=>r.generation===generation);assert.equal(rows.filter(r=>r.method==='spawn').length,1,'one owned instance '+generation);
  for(const method of ['initialize','tools/list','tools/call'])assert.ok(rows.some(r=>r.method===method),generation+' actual '+method);
  assert.ok(rows.every(r=>Number.isSafeInteger(r.pid)&&r.pid>0));assert.equal(new Set(rows.map(r=>r.pid)).size,1,'one exact process identity');
  const state=only(Object.entries(a.tree).filter(([name])=>name.endsWith('/sdk-mcp-witness/'+generation+'.json')),generation+' PID state')[1].json;
  assert.equal(state.identity,'SDK-MCP-INSTANCE-'+generation);assert.ok(rows.every(r=>r.identity===state.identity));
  assert.equal(state.pid,rows[0].pid,'probe PID originates in actual MCP process');
 }
 assert.notEqual(events.find(r=>r.generation==='A').pid,events.find(r=>r.generation==='B').pid,'PID reuse is rejected rather than treated as cleanup success');
 for(const [i,phase] of ['A','ISOLATED','B','CLEAR'].entries()){
  assert.equal(a.steps[i].step.ws,'submit_turn');
  assert.ok(!a.steps[i].frames.some(f=>f.error||f.type==='__timeout'));
  assert.ok(JSON.stringify(a.steps[i].frames).includes('SDK-MCP-DONE-'+phase),'real streamed completion '+phase);
  const req=records[phase==='ISOLATED'?'isolated':phase.toLowerCase()+'-initial'];
  assert.equal(text(req.body.messages.filter(m=>m.role==='user').at(-1).content),'SDK-MCP-PHASE-'+phase);
 }
}
try{
 const artifact=JSON.parse(fs.readFileSync(process.argv[2]));validate(artifact);
 if(process.argv.includes('--self-test')){
  const faults=[a=>delete a.provider['a-final'],a=>a.provider['b-final'][0].body.messages=a.provider['b-final'][0].body.messages.filter(m=>m.role!=='tool'),a=>a.provider.isolated[0].body.tools.push({type:'function',function:{name:'mcp__sdk-owned__probe'}}),a=>a.provider['clear-final'][0].body.messages=a.provider['clear-final'][0].body.messages.filter(m=>m.role!=='tool'),a=>a.evidence.scenario.sha256='0'.repeat(64),a=>{const e=Object.entries(a.tree).find(([n])=>n.endsWith('/sdk-mcp-witness/events.jsonl'));e[1].jsonl=e[1].jsonl.filter(r=>r.method!=='initialize')}];
  for(const mutate of faults){const copy=structuredClone(artifact);mutate(copy);assert.throws(()=>validate(copy),'corruption must fail')}
  console.log('6 corruptions rejected');
 }
 console.log('SDK directory MCP strict PASS');
}catch(error){console.error(error.stack);process.exitCode=1}
