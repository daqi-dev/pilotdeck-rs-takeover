import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {isDeepStrictEqual} from 'node:util';
import {cliShutdownAcceptedPaths,cliShutdownEvidence} from './cli-shutdown-policy.mjs';
const root=process.env.CLI_SHUTDOWN_ARTIFACT_ROOT;
// Reuse the comparator's actual unchanged load/normalizer in a private test closure.
// No product, no diff main(), no new normalization rules, no source rewrite.
const source=fs.readFileSync(new URL('./diff.mjs',import.meta.url),'utf8');
const code=source.slice(source.indexOf('export const IGNORED ='),source.indexOf('const show =')).replaceAll('export const ','const ');
const normalize=Function('isDeepStrictEqual',code+';return raw=>JSON.parse(JSON.stringify(load(raw,new WeakMap(),new Map())));')(isDeepStrictEqual);
const decl=[['cli-entry-default',1,9],['cli-survey-offline-nontui',1,6],['cli-default-turns',2,24]];
const read=(name,slot,side)=>JSON.parse(fs.readFileSync(path.join(root,`triplet-slot-${slot}/run/raw/${name}.${side}.json`)));
for(const [name,slot,n]of decl)test(name+' actual original TS1/TS2 and Rust exact shutdown scope',{skip:!root},()=>{
 const a=read(name,slot,'ts-1'),a2=read(name,slot,'ts-2'),b=read(name,slot,'rust'),before=JSON.stringify([a,a2,b]);
 assert.equal(cliShutdownAcceptedPaths(a,b,normalize).size,n);assert.equal(cliShutdownAcceptedPaths(a2,b,normalize).size,n);assert.equal(JSON.stringify([a,a2,b]),before,'original arrays/evidence unchanged');
});
test('fixed scope rejects early exit, missing business, wrong terminal and arbitrary observable changes',{skip:!root},()=>{
 const a=read('cli-default-turns',2,'ts-1'),b=read('cli-default-turns',2,'rust');
 const faults=[
 ['empty early stdout',x=>x.steps[0].cli.stdout=''],['Rust timeout',x=>x.steps[0].timedOut=true],['Rust bad exit',x=>x.steps[0].cli.exitCode=1],['Rust killed',x=>x.steps[0].cli.signal='SIGTERM'],
 ['changed stdin',x=>x.steps[1].step.stdin=''],['increased timeout',x=>x.steps[1].step.timeout++],['missing model tool receipt',x=>delete x.provider['read-after-tool']],['extra model request',x=>x.provider.argv.push(x.provider.argv[0])],
 ['wrong registry state',x=>x.tree['/home/.pilotdeck/runs/registry.json'].json.records[0].state='running'],['wrong registry terminal',x=>x.tree['/home/.pilotdeck/runs/registry.json'].json.records.at(-1).result.finishReason='completed'],['missing EOF probe stderr',x=>x.steps[0].cli.stderr=''],['unrelated output mutation',x=>x.steps[1].cli.stdout+='HIDDEN BUSINESS'],
 ['wrong scenario hash',x=>x.evidence.scenario.sha256='0'.repeat(64)],['unexpected scenario',x=>x.scenario='other'],
 ];
 for(const [label,mutate]of faults){const x=structuredClone(b);mutate(x);assert.throws(()=>cliShutdownAcceptedPaths(a,x,normalize),undefined,label);}
 const notHung=structuredClone(a);delete notHung.steps[0].timedOut;assert.throws(()=>cliShutdownAcceptedPaths(notHung,b,normalize));
});
test('tail checkpoint exception requires exact durable records, every view and unchanged semantics',{skip:!root},()=>{
 const a=read('cli-default-turns',2,'ts-1'),b=read('cli-default-turns',2,'rust');const chat=Object.keys(b.tree).find(k=>k.endsWith(':default.jsonl')),projection=chat+'.projections.json';
 const faults=[['wrong cache sequence',x=>x.tree[projection].json.asOfSequence=131],['wrong anchor identity',x=>x.tree[projection].json.anchor.entryId='wrong'],['wrong anchor type',x=>x.tree[projection].json.anchor.type='turn_result'],['one view stale',x=>x.tree[projection].json.projections['goal.state'].asOfSequence=129],['wrong cached metadata',x=>x.tree[projection].json.projections['agent-transcript.metadata'].state.lastPrompt='wrong'],['drop durable metadata',x=>x.tree[chat].jsonl.pop()],['change durable result',x=>x.tree[chat].jsonl[128].result.stopReason='completed'],['wrong persisted hash',x=>x.evidence.persistedFiles[chat].sha256='wrong'],['extra view',x=>x.tree[projection].json.projections['new-view']={asOfSequence:130}],['changed tool payload',x=>{const row=x.tree[chat].jsonl.find(r=>r.type==='tool_result');row.result.content[0].text='wrong';}]];
 for(const [label,mutate]of faults){const x=structuredClone(b);mutate(x);assert.throws(()=>cliShutdownAcceptedPaths(a,x,normalize),undefined,label);}
});
test('empty/error default steps cannot pass just by returning zero',{skip:!root},()=>{
 for(const [name,slot]of decl.slice(0,2)){const a=read(name,slot,'ts-1'),b=read(name,slot,'rust');const x=structuredClone(b);const i=name==='cli-entry-default'?1:4;x.steps[i].cli.stdout='';assert.throws(()=>cliShutdownAcceptedPaths(a,x,normalize));const y=structuredClone(b);y.tree['/home/.pilotdeck/runs/registry.json'].json.records=[];assert.throws(()=>cliShutdownAcceptedPaths(a,y,normalize));}
});

test('single-artifact contract requires real business, normal Rust exit and exact durable chain',{skip:!root},()=>{
 for(const [name,slot] of decl){for(const kind of ['ts-1','rust'])assert.equal(cliShutdownEvidence(read(name,slot,kind)),true);const base=read(name,slot,'rust');for(const mutate of [x=>x.steps[0].cli.exitCode=1,x=>x.steps[0].timedOut=true,x=>x.steps[0].cli.signal='SIGKILL',x=>x.tree['/home/.pilotdeck/runs/registry.json'].json.records=[]]){const x=structuredClone(base);mutate(x);assert.throws(()=>cliShutdownEvidence(x));}}
 const valid=read('cli-default-turns',2,'rust');for(const mutate of [x=>delete x.provider['read-after-tool'],x=>x.steps[0].cli.stdout='',x=>{const key=Object.keys(x.tree).find(k=>k.endsWith(':default.jsonl'));x.tree[key].jsonl.pop();}]){const x=structuredClone(valid);mutate(x);assert.throws(()=>cliShutdownEvidence(x));}
});
