#!/usr/bin/env node
// Strict evidence checker; its contract remains provisional until TS self-runs.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import {createHash} from 'node:crypto';
const [artifactPath,scenarioPath]=process.argv.slice(2);assert.ok(artifactPath&&scenarioPath);
const bytes=fs.readFileSync(artifactPath),scenarioBytes=fs.readFileSync(scenarioPath);
const a=JSON.parse(bytes),scenario=JSON.parse(scenarioBytes);const sha=x=>createHash('sha256').update(x).digest('hex');
assert.equal(a.scenario,scenario.name);assert.ok(['ts','rust'].includes(a.side));assert.equal(a.evidence?.schema,1);assert.equal(a.evidence?.side,a.side);assert.equal(a.evidence?.scenario?.sha256,sha(scenarioBytes),'artifact bound to exact scenario bytes');assert.equal(a.evidence?.inputs?.ts?.commit,'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');assert.ok(a.evidence?.runId);assert.ok(Array.isArray(a.failures));assert.deepEqual(a.failures.filter(f=>!f.expected),[]);
const one=name=>{const rows=a.provider?.[name];assert.equal(rows?.length,1,`${name}: actual model request count`);return rows[0].body};
const text=v=>typeof v==='string'?v:JSON.stringify(v);const names=b=>(b.tools??[]).map(t=>t.function?.name??t.name);
const parent=one('parent'),child=one('child-alpha'),beta=one('child-beta'),mcp=one('child-mcp'),after=one('child-after-mcp'),observer=one('observer'),second=one('second');one('parent-after');one('parent-check');
assert.ok(names(parent).includes('agent'),'parent actual request exposed agent');assert.ok(!names(parent).includes('mcp__agent03-private__echo'),'private child MCP leaked to parent');
const user=(child.messages??[]).filter(m=>m.role==='user').map(m=>text(m.content));const init=user.findIndex(x=>x.includes('AG03-INITIAL-ONLY')),directive=user.findIndex(x=>x.includes('AG03-CHILD-DIRECTIVE'));assert.ok(init>=0&&directive>init,'separate ordered initial prompt and directive');
assert.ok(names(child).includes('read_skill'));assert.ok(names(child).includes('mcp__agent03-private__echo'));assert.ok(!names(child).includes('write_file'));
const result=(body,id)=>{const values=(body.messages??[]).filter(m=>m.role==='tool'&&m.tool_call_id===id);assert.equal(values.length,1,`${id} real tool result`);return text(values[0].content)};
assert.match(result(beta,'ag03-alpha'),/AG03-ALPHA-CONTENT/);assert.equal(result(mcp,'ag03-beta'),"Skill 'agent03-beta' not found. Available skills: agent03-alpha");assert.match(result(after,'ag03-mcp'),/AG03-MCP-RESULT:AG03-MCP-CALL/);
const observerText=text(observer.messages);assert.match(observerText,/AG03-OBSERVER-POSTAMBLE/);assert.doesNotMatch(observerText,/AG03-OBSERVER-INITIAL-MUST-NOT-LEAK/);for(const name of ['write_file','read_skill','mcp__agent03-private__echo'])assert.ok(!names(observer).includes(name),`observer must not expose ${name}`);
const secondText=text(second.messages);assert.match(secondText,/agent03-alpha/i);assert.match(secondText,/agent03-beta/i);assert.ok(!names(second).includes('mcp__agent03-private__echo'),'child MCP leaked to second session');
const log=a.tree?.['/home/.pilotdeck/agent03-mcp-log.jsonl']?.jsonl;assert.ok(Array.isArray(log),'actual child MCP log');for(const event of ['start','initialize','tools/list','tools/call','exit'])assert.equal(log.filter(x=>x.event===event).length,1,`MCP ${event} exactly once`);const call=log.find(x=>x.event==='tools/call');assert.equal(call.name,'echo');assert.equal(call.args?.text,'AG03-MCP-CALL');
const output=JSON.stringify(a.steps);assert.match(output,/AG03-PARENT-DONE/);assert.match(output,/AG03-PARENT-CHECK-DONE/);
const events=a.steps.flatMap(step=>step.frames??[]).map(frame=>frame.event).filter(event=>event?.type==='agent_status'&&event.detail?.subagentType==='reviewer');
const started=events.filter(e=>e.event==='subagent_started');assert.equal(started.length,1,'one observer start');const observerId=started[0].detail.subagentId;assert.ok(observerId);
const ordered=events.filter(e=>e.detail.subagentId===observerId);const start=ordered.findIndex(e=>e.event==='subagent_started');const report=ordered.findIndex(e=>e.event==='subagent_text_delta'&&e.detail.text==='AG03-OBSERVER-REPORT');const end=ordered.findIndex(e=>e.event==='subagent_completed'&&e.detail.success===true);assert.ok(start>=0&&report>start&&end>report,'same observer emits report and completes in order');
console.log(JSON.stringify({passed:true,side:a.side,artifactSha256:sha(bytes),scenarioSha256:sha(scenarioBytes),checks:['real-fork','ordered-initial','skill-scope','isolated-mcp-call-and-exit','observer-tool-isolation','observer-report','second-session']}));
