import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {brotliDecompressSync} from 'node:zlib';
import {createHash} from 'node:crypto';
import {diagnosticsAcceptedPaths} from './diagnostics-policy.mjs';
const directory=new URL('./fixtures/diagnostics-fixed-parser/',import.meta.url);
const manifest=JSON.parse(fs.readFileSync(new URL('manifest.json',directory)));
const originals=['ts','rust'].map(side=>{
 const compressed=fs.readFileSync(new URL(side+'.json.br',directory));
 const bytes=brotliDecompressSync(compressed),record=manifest.files.find(f=>f.side===side);
 assert.equal(createHash('sha256').update(compressed).digest('hex'),record.compressedSHA256);
 assert.equal(createHash('sha256').update(bytes).digest('hex'),record.decompressedSHA256);
 return JSON.parse(bytes);
});
const baseline=process.env.BBX_BASELINE||fileURLToPath(new URL('../../resources/PilotDeck',import.meta.url));
const finished=(x,id)=>x.steps.flatMap(s=>s.frames??[]).find(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId===id).event;
const controls=[
 ['removed-native-ts-diagnostic',r=>{finished(r,'call_bbx_14').resultPreview='Created d/bad.ts.';}],
 ['wrong-csv-location',r=>{finished(r,'call_bbx_13').resultPreview=finished(r,'call_bbx_13').resultPreview.replace('L3:1','L4:1');}],
 ['extra-native-ts-recovery-diagnostic',r=>{finished(r,'call_bbx_14').resultPreview+='\n- L2:10 error: fabricated';}],
 ['changed-unrelated-json-diagnostic',r=>{finished(r,'call_bbx_1').resultPreview+='\nwrong';}],
 ['missing-written-file',r=>{delete r.tree['/home/.pilotdeck/d/bad.html'];}],
 ['changed-written-file',r=>{r.tree['/home/.pilotdeck/d/bad.csv'].text+='wrong';}],
 ['wrong-result-byte-count',r=>{finished(r,'call_bbx_12').resultBytes++;}],
 ['wrong-provider-copy',r=>{const q=r.provider['turn-14-after-tool'][0];q.body.messages.find(m=>m.role==='tool'&&m.tool_call_id==='call_bbx_14').content+='wrong';q.headers['content-length']=String(Buffer.byteLength(JSON.stringify(q.body)));}],
 ['wrong-durable-copy',r=>{const t=Object.entries(r.tree).find(([k])=>k.endsWith('/chats/s-bbx-1.jsonl'))[1];const rec=t.jsonl.find(v=>v.type==='tool_result'&&JSON.stringify(v).includes('bad.ts'));rec.result.content[0].text+='wrong';}],
 ['wrong-history-copy',r=>{const msgs=r.steps.at(-1).frames[0].result.messages;const m=msgs.find(m=>m.text?.includes('Created d/bad.ts.'));m.text+='wrong';}],
 ['wrong-calibrated-token',r=>{const e=r.steps[25].frames.map(f=>f.event).filter(e=>e?.type==='context_budget').at(-1);e.used++;e.displayUsed++;e.usageTokens++;e.ratio=e.used/e.effectiveTotal;}],
 ['wrong-session-file-size',r=>{r.steps.at(-1).frames[0].result.session.fileSize++;}],
 ['wrong-skeleton',r=>{const t=Object.entries(r.tree).find(([k])=>k.endsWith('/chats/s-bbx-1.jsonl'))[1];t.raw.skeletons[Object.keys(t.raw.skeletons)[0]]++;}],
 ['wrong-fixed-input-sha',r=>{r.evidence.scenario.sha256='0'.repeat(64);}],
 ['different-scene-not-exempt',r=>{r.scenario='tool-write-file-other';}],
 ['external-passed-all-map-not-authority',r=>{r.passed=true;r.all=[];r.a={};r.b={};}],
 ['views-object-is-not-a-raw-capture',r=>{for(const k of Object.keys(r))delete r[k];Object.assign(r,{a:{},b:{},raw:[],all:[],passed:true});}],
];
test('approved fixed diagnostics raw pair reconstructs all 416 exact paths internally',()=>{
 const before=originals.map(x=>JSON.stringify(x));
 const accepted=diagnosticsAcceptedPaths(...originals,baseline);
 assert.equal(accepted.size,416);
 assert.ok(accepted.has('$.steps[23].frames[6].event.resultPreview'));
 assert.deepEqual(originals.map(x=>JSON.stringify(x)),before,'comparison cannot mutate caller raw');
});
for(const[name,mutate]of controls)test('fixed diagnostics rejects actual raw corruption: '+name,()=>{
 const [left,right]=structuredClone(originals);mutate(right);
 assert.throws(()=>diagnosticsAcceptedPaths(left,right,baseline));
});
test('other scene cannot borrow a diagnostics exemption or demand a tokenizer',()=>{
 assert.equal(diagnosticsAcceptedPaths({scenario:'other'},{scenario:'other'},undefined).size,0);
});
test('external views and a forged passed flag never constitute raw evidence',()=>{
 assert.equal(diagnosticsAcceptedPaths({a:{},b:{},all:[],passed:true},{passed:true},undefined).size,0);
});
test('caller supplied counting function is not a supported source identity',()=>{
 assert.throws(()=>diagnosticsAcceptedPaths(...originals,()=>0));
});
