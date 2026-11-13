// Root-approved fixed three-capture contract, never a future-watch waiver.
import fs from 'node:fs';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {brotliDecompressSync} from 'node:zlib';
import {verifyConfigWatchInput} from './fixtures/config-watch-input.mjs';
import {load,compare} from './normalization.mjs';
const digest=b=>createHash('sha256').update(b).digest('hex'), ordered=v=>JSON.stringify(v);
const one=(xs,label)=>{assert.ok(Array.isArray(xs),label);assert.equal(xs.length,1,label);return xs[0];};
const sourceHash='a41e135d307385c5fbe58ff10778207aa26a26dde90b852d7fbd24b342590e18',afterHash='e8d7815fb5641a4b2a57490f824d727f5b2667491fb48c0072a8a8199a1cf261';
const RAW_PINS=[
  {
    "file": "ts-1.json.br",
    "compressedSHA": "5d04df75537b1498cd7c1adaa95659c87ee60ec252ec3e3a4fd273814a9eeb10",
    "sha256": "470a8eda78ffb832e517152e340cc95cc735e3359c877107c7b3a0fa96daa228",
    "size": 639616
  },
  {
    "file": "ts-2.json.br",
    "compressedSHA": "e0c227d5d9eb1cdc34259ce67366e6385b2260b9018ecee3751c744b2072185b",
    "sha256": "c855e606c8b817a008ea3dea8f82460cd5f63d0e87f5bd88c21f4742d245a0ff",
    "size": 639617
  },
  {
    "file": "rust.json.br",
    "compressedSHA": "8efdf8123bb9f24edc17754b64fc13342940016a81e2228c22b39d4b2bb74f4f",
    "sha256": "6efc3c541a8cbd661b1244c73567f6cce24b7fcdb0a94b42e7053d602d734ba0",
    "size": 645967
  }
];
const CLOSURE_PIN={
  "file": "closure.json.br",
  "compressedSHA": "cf9536e122fd3adcadcf0d8ec06fe9ff45a0ffd17f88790dd842511fe8175cd2",
  "sha256": "d3a560c7c700339cacb09a650d1a370c910b2d4a890d95213bfb632451f8c0e3",
  "size": 480570
};
const LEAVES=['$.steps[25].frames[0].result.version','$.steps[27].externalConfigRewrite.observationExpired','$.steps[27].externalConfigRewrite.observed'];
function unpack(pin){
 const compressed=fs.readFileSync(new URL('./fixtures/config-watch-exact/'+pin.file,import.meta.url));assert.equal(digest(compressed),pin.compressedSHA,'sealed compressed '+pin.file);
 const bytes=brotliDecompressSync(compressed,{maxOutputLength:pin.size});assert.equal(bytes.length,pin.size);assert.equal(digest(bytes),pin.sha256,'sealed raw '+pin.file);return bytes;
}
function sealed(){
 const closure=JSON.parse(unpack(CLOSURE_PIN));assert.equal(closure.schema,1);assert.equal(closure.product,'d67705eece5a035b9f689809bf5b4088755ebcb8');assert.equal(closure.ts,'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');assert.equal(closure.harness,'644a8d80a45916bbe7a0b0ee93ff6e331a9dbf98');assert.equal(closure.sceneSHA,'627a47e10e2e94170927d04fd00539c0c9d27268954b4609d1944f114d5f50d4');assert.equal(closure.sources.length,30);
 const sources=new Map();for(const source of closure.sources){assert.ok(!sources.has(source.path),'unique sealed source');assert.equal(digest(Buffer.from(source.text)),source.sha256);sources.set(source.path,source);}
 for(const file of ['normalization.mjs','fixtures/config-watch-input.mjs'])assert.equal(digest(fs.readFileSync(new URL('./'+file,import.meta.url))),sources.get('scripts/bbx/'+file).sha256,'unchanged runtime helper '+file);
 assert.equal(sources.get('scripts/bbx/run.mjs').sha256,'24e32a6f439ab1f0f1836c2215884655f1daafc82fb17388f731c40590c02e7d');assert.equal(sources.get('scripts/bbx/scenarios/settings-update.json').sha256,closure.sceneSHA);
 assert.equal(digest(fs.readFileSync(new URL('./fixtures/config-watch-exact/original-public644.diff.txt',import.meta.url))),'fe74d45f963b661a022d9b31ad3f58c2bbd01fd2e98c657f8b9ce257a54337ca','preserved original strict DIFF2');
 const bytes=RAW_PINS.map(unpack),raw=bytes.map(b=>JSON.parse(b));return {closure,bytes,raw};
}
function snapshot(rpc,file,id){assert.deepEqual(rpc.step,{ws:'resolve_settings',params:{}});const f=one(rpc.frames,'snapshot frame');assert.equal(f.id,id);assert.equal(f.type,'response');assert.equal(f.ok,true);assert.equal(f.error,undefined);const s=one(f.result.sources.filter(s=>s.path===file),'one exact config source');return {version:f.result.version,contentHash:f.result.contentHash,sourceHash:s.contentHash};}
function following(a){
 const turn=a.steps[28];assert.deepEqual(turn.step,{ws:'submit_turn',params:{sessionKey:'s-watch-rewrite',channelKey:'test',message:'watch-rewrite: confirm current config'}});
 assert.ok(turn.frames.every(f=>f.id==='q25'),'unchanged public q25');
 const req=one(a.provider['watch-rewrite'],'one following provider request');assert.equal(req.method,'POST');assert.equal(req.url,'/v1/chat/completions');assert.equal(req.body.max_tokens,2048,'next provider max_tokens');
 const started=one(turn.frames.filter(f=>f.event?.type==='turn_started'),'following start'), terminal=one(turn.frames.filter(f=>f.final),'following terminal');assert.equal(terminal.event.type,'turn_completed');assert.equal(terminal.event.finishReason,'completed');assert.equal(terminal.event.runId,started.event.runId);
 const chat=one(Object.entries(a.tree).filter(([p])=>p.endsWith('/chats/s-watch-rewrite.jsonl')),'durable transcript')[1].jsonl;
 for(const type of ['turn_started','accepted_input','model_request','assistant_message','turn_result'])one(chat.filter(r=>r.type===type&&r.turnId===started.event.runId),'durable '+type);
}
// Deliberately exported for semantic mutants; this is NOT the accepted entry.
// It validates content before the final exact-capture remainder check.
export function verifyConfigWatchExactSemantics(raw){
 assert.equal(raw.length,3);const originals=sealed().raw;
 for(const [i,a] of raw.entries()){
  assert.equal(a.scenario,'settings-update','fixed scene');assert.equal(a.side,i===2?'rust':'ts');assert.equal(a.steps.length,29);assert.deepEqual(a.failures,[]);assert.deepEqual(a.unsolicited,[]);assert.deepEqual(a.shutdown,{code:0,signal:null});
  assert.equal(a.evidence.inputs.rust.commit,'d67705eece5a035b9f689809bf5b4088755ebcb8');assert.equal(a.evidence.inputs.ts.commit,'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
  assert.deepEqual(a.steps[26].step,{ws:'update_settings',params:{source:'localSettings',settings:{agent:{maxOutputTokens:4096}}}});assert.equal(one(a.steps[26].frames.filter(f=>f.type==='response'),'setup response').id,'q24');
  assert.deepEqual(a.steps[27].step,{externalConfigRewrite:{fromMaxOutputTokens:4096,toMaxOutputTokens:2048}},'exact action step');assert.equal(a.steps[27].timedOut,undefined);
  const p=one(a.evidence?.configWatchInput,'one rewrite receipt');assert.equal(p.schema,1);assert.equal(p.scene,'settings-update');assert.equal(p.stepIndex,27);assert.equal(p.timeoutMs,30000);assert.equal(p.written,true);
  assert.equal(p.file,'/tmp/pdrs-bbx/root/home/.pilotdeck/pilotdeck.yaml');assert.equal(p.requestedFile,p.file);
  for(const x of [p.beforeFile,p.afterFile]){assert.equal(x.sha256,digest(Buffer.from(x.text)),'file SHA from actual bytes');assert.equal(x.size,Buffer.byteLength(x.text));assert.equal(x.nlink,1);}
  assert.equal(p.beforeFile.sha256,sourceHash);assert.equal(p.afterFile.sha256,afterHash);assert.equal(p.afterFile.text,p.beforeFile.text.replace('  maxOutputTokens: 4096\n','  maxOutputTokens: 2048\n'),'exact 4096 to 2048 bytes');
  for(const k of ['dev','ino','uid','mode'])assert.equal(p.beforeFile[k],p.afterFile[k],'inplace owned '+k);
  assert.equal(a.tree['/home/.pilotdeck/pilotdeck.yaml'].text,p.afterFile.text,'final file');assert.ok(p.writeStartedMs>=0&&p.writeCompletedMs>=p.writeStartedMs&&p.durationMs>=p.writeCompletedMs,'write timing');
  assert.equal(p.frameCursor,53);assert.equal(p.observedFrameCount,p.frameCursor+p.observationFrames.length,'complete observation frame range');
  assert.deepEqual(snapshot(p.rpc[0],p.file,'bbx-config-watch:step27:snapshot1'),p.beforeSnapshot);assert.equal(p.beforeSnapshot.sourceHash,sourceHash);assert.equal(p.beforeSnapshot.version,i===2?11:10);
  if(i<2){
   assert.deepEqual(a.steps[27].externalConfigRewrite,{written:true,observed:false,fromMaxOutputTokens:4096,toMaxOutputTokens:2048,receiptIndex:0,observationExpired:true});
   assert.equal(p.observed,false);assert.equal(p.notification,null);assert.equal(p.rpc.length,1);assert.deepEqual(p.observationFrames,[]);assert.equal(p.afterSnapshot,undefined);assert.ok(p.durationMs>=30000&&p.durationMs<30001,'full exact TS window');
  }else{
   assert.equal(p.observed,true);assert.ok(p.durationMs>p.writeCompletedMs&&p.durationMs<30000,'Rust within observation window');
   assert.equal(p.rpc.length,2);assert.equal(p.notification.index,53);assert.deepEqual(p.notification.frame,{type:'notification',name:'config_changed',payload:{changedPaths:['agent.maxOutputTokens'],changeClasses:['next-request']}},'notification kind and scope');
   assert.deepEqual(p.observationFrames,[p.notification.frame,p.rpc[1].frames[0]],'notification before snapshot response, exactly once');assert.equal(p.rpc[1].afterNotificationIndex,53);
   assert.deepEqual(snapshot(p.rpc[1],p.file,'bbx-config-watch:step27:snapshot2'),p.afterSnapshot);assert.equal(p.afterSnapshot.sourceHash,afterHash);assert.equal(p.afterSnapshot.version,12);verifyConfigWatchInput(a);
  }
  following(a);assert.equal(typeof a.steps[25].frames[0].result.version,'number','step25 version type');assert.equal(a.steps[25].frames[0].result.version,[9,9,10][i],'step25 exact version');
 }
 // Every original other value, including notifications (content/attribution/order),
 // evidence snapshots/version/hash, durable rows, usage, public IDs and timings stays strict.
 // This ordered JSON check intentionally disallows any future capture or extra object.

 for(let i=0;i<3;i++)assert.equal(ordered(raw[i]),ordered(originals[i]),'all remaining ordered raw content '+i);
 return true;
}
// Public artifact validation has no raw filename, so recognize only this run.
// It grants no comparison permission; the file API below independently rereads bytes.
export function validateConfigWatchExactArtifact(artifact){
 if(artifact?.scenario!=='settings-update'||artifact.evidence?.runId!=='14af58e8-2fd7-4e71-bfc9-e2a827e5ed54')return false;
 const proof=sealed();assert.ok(proof.raw.some(r=>ordered(r)===ordered(artifact)),'fixed settings artifact content and order');verifyConfigWatchExactSemantics(proof.raw);return true;
}
export function configWatchExactAcceptedPaths(leftFile,rightFile,{self=false,expectedRaw}={}){
 const out=new Map();if(self)return out;
 const bytes=[leftFile,rightFile].map(f=>fs.readFileSync(f));const left=digest(bytes[0])===RAW_PINS[0].sha256?0:-1;
 if(left<0||digest(bytes[1])!==RAW_PINS[2].sha256)return out; // Unknown/future captures gain no permission.
 const proof=sealed();assert.ok(bytes[0].equals(proof.bytes[left])&&bytes[1].equals(proof.bytes[2]),'ordered original raw bytes');
 const raw=bytes.map(b=>JSON.parse(b));assert.equal(ordered(expectedRaw),ordered(raw),'trusted comparator raw must match same sealed bytes and key order');
 verifyConfigWatchExactSemantics(proof.raw);
 const all=[];compare(load(structuredClone(raw[0])),load(structuredClone(raw[1])),'$',all);assert.deepEqual(all.map(d=>d.at),LEAVES,'original normalized strict DIFF3 before old scene filter');
 assert.equal(all[0].a,9);assert.equal(all[0].b,10);assert.equal(all[1].a,true);assert.equal(all[1].b,undefined);assert.equal(all[2].a,false);assert.equal(all[2].b,true);
 for(const at of LEAVES)out.set(at,'A7-CONFIG-WATCH-EXACT-CAPTURE-001: root-approved sealed d677/644 originals only; exact step25 number9→10 and step27 TS expired/false versus Rust absent/true; complete source/write/notification/snapshot/provider/durable chain remains locked');
 return out;
}
