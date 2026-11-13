// All formal/wire data generated here is synthetic unit-test data, not a
// product execution. Real sealed-binary evidence is exercised separately.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs,{readFileSync} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {SDK_SIGNAL_WIRE_CASES,SDK_SIGNAL_POLICY_ID} from './sdk-signal-policy.mjs';
import {loadSdkSignalWireEvidence,validateSdkSignalAcceptedEvidence,validateArtifact,assertionResults} from './evidence.mjs';
const here=path.dirname(fileURLToPath(import.meta.url));
const scenario=JSON.parse(readFileSync(new URL('./scenarios/c1-sdk-cancel-resume.json',import.meta.url)));
const closeStack="file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/packages/sdk/dist/transport.js:160\n        const error = new PilotDeckError({ code: \"transport_error\", message: \"Gateway client closed.\" });\n                      ^\n\nPilotDeckError: Gateway client closed.\n    at GatewayTransport.close (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/packages/sdk/dist/transport.js:160:23)\n    at PilotDeckQueryImpl.close (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/packages/sdk/dist/client.js:1639:24)\n    at Object.next (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/packages/sdk/dist/client.js:2935:37)\n    at process.processTicksAndRejections (node:internal/process/task_queues:105:5)\n    at async consumeRun (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/dist/src/cli/sdkCli.js:106:22)\n    at async runTurn (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/dist/src/cli/sdkCli.js:64:42)\n    at async runSdkCli (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/dist/src/cli/sdkCli.js:19:9)\n    at async main (file:///home/clawbox-08/ws/pilotdeck-rs-cc2c/baseline/PilotDeck/dist/src/cli/pilotdeck.js:22:9) {\n  code: 'transport_error',\n  requestId: undefined,\n  details: undefined,\n  retryable: false,\n  [cause]: undefined\n}\n\nNode.js v22.22.0\n";
const session='c1-cancel:project=/fixture/project:s_fixed',project='/fixture/project',run='cancel-run';
const canonical='{"status":"aborted","reason":"CLI interrupted"}';
const sha='a'.repeat(64);
function fixture(side='rust',crash=false){
 const markers=['C1-INITIAL','C1-CANCEL','C1-RECOVER'];
 const records=markers.map((message,index)=>({sessionKey:session,projectKey:project,runId:index===1?run:`run-${index}`,state:index===1?'aborted':'completed',requestMaterial:JSON.stringify({sessionKey:session,projectKey:project,message})}));
 const partial={seq:1,event:{type:'assistant_text_delta',text:'C1 partial',runId:run}};
 const eventKey=id=>`${project}\0${session}\0${id}`;
 const events=Object.fromEntries(records.map((record,index)=>[eventKey(record.runId),[...(index===1?[partial]:[]),{seq:2,event:{type:'turn_completed',runId:record.runId,finishReason:index===1?'aborted_streaming':'completed'}}]]));
 const registry={schemaVersion:1,records,events};
 const running={...records[1],state:'running'};
 const before={schemaVersion:1,records:[running],events:{[eventKey(run)]:[partial]}};
 const raw=Buffer.from(JSON.stringify(before));
 const provider={rule:'cancel',index:0,open:true,flushedChunks:1};
 const trigger={record:running,event:partial,provider};
 const terminal={type:'result',runId:run,sessionId:session,finishReason:'aborted_streaming',status:'completed'};
 const stdout=JSON.stringify(terminal)+'\n'+(crash?'C1 partial':canonical)+'\n';
 const cli=(output)=>({exitCode:0,signal:null,stdout:output,stderr:''});
 const envelope=(output)=>'reply\n'+JSON.stringify({sessionId:session,result:{status:'completed',output}})+'\n';
 const history=markers.map(text=>({role:'user',text,sessionKey:session,projectKey:project}));history.push({role:'assistant',text:'C1 partial',sessionKey:session,projectKey:project});
 const artifact={scenario:scenario.name,side,failures:[],steps:[{step:scenario.steps[0],cli:cli(envelope('C1 initial reply.'))},{step:scenario.steps[1],serve:{reached:true,exitCode:1,signal:null,stdout,stderr:crash?closeStack:'',persistedRunEvent:trigger}},{step:scenario.steps[2],cli:cli(envelope('C1 recovered reply.'))},{step:scenario.steps[3],cli:cli(JSON.stringify(history))}],provider:{title:[{body:{messages:[]}}],initial:[{body:{messages:[{role:'user',content:'C1-INITIAL'}]}}],cancel:[{body:{messages:[{role:'user',content:'C1-CANCEL'}]}}],recovered:[{body:{messages:[{role:'assistant',content:'C1 initial reply.'},{role:'user',content:'C1-CANCEL'},{role:'assistant',content:'C1 partial'},{role:'user',content:'C1-RECOVER'}]}}]},tree:{'/home/.pilotdeck/runs/registry.json':{json:registry}},evidence:{inputs:{binaries:{pilotdeck:side==='rust'?{sha256:sha}:null}},serveTriggers:[{stepIndex:1,condition:{sessionKey:session,projectKey:project,requestText:'C1-CANCEL',event:{type:'assistant_text_delta',text:'C1 partial'},providerRule:'cancel'},projectKey:project,raw:{registryPath:'/fixture/registry.json',base64:raw.toString('base64'),sha256:createHash('sha256').update(raw).digest('hex')},providers:[provider],provider,pid:42,startedAt:'2026-11-13T00:00:00.000Z',signalSentAt:'2026-11-13T00:00:01.000Z',process:{pid:42,startedAt:'2026-11-13T00:00:00.000Z',signalSentAt:'2026-11-13T00:00:01.000Z'},signalSent:'SIGINT'}]}};
 return artifact;
}

const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
function wireDirectory(root){
 const directory=path.join(root,'wire');fs.mkdirSync(directory);
 fs.writeFileSync(path.join(directory,'identity.json'),JSON.stringify({cliSha256:sha,driverSha256:'6b29ef61cf58a1403dbfc15e55f057c65ee8b1ef0afa3272f48a7a41d2d8926a'}));
 const summaries=[];
 for(const mode of SDK_SIGNAL_WIRE_CASES){
  const dir=path.join(directory,mode);fs.mkdirSync(dir);
  const events=[{kind:'signal_sent',signal:'SIGINT',delivered:true},{kind:'cli_to_gateway',connection:2,raw:JSON.stringify({type:'request',method:'submit_turn',id:'submit',params:{sessionKey:session,runId:run}})},{kind:'cli_to_gateway',connection:2,raw:JSON.stringify({type:'request',method:'abort_turn',id:'abort',params:{sessionKey:session,runId:run,reason:'CLI interrupted'}})}];
  const positive=SDK_SIGNAL_WIRE_CASES.indexOf(mode)<4;
  if(positive)events.push({kind:'gateway_to_cli',connection:2,label:'abort_turn_response',readyState:1,frame:{type:'response',id:'abort',ok:true,result:{aborted:true}}});
  if(mode==='final-and-abort-separated')events.push({kind:'diagnostic_held_response_release',childExitCode:null,childSignalCode:null});
  if(mode==='wrong-run-response')events.push({kind:'wrong_run_response_barrier',childExitCode:null,childSignalCode:null});
  events.push({kind:'ws_close',connection:2},{kind:'ws_close',connection:1},{kind:'cli_exit',code:1,signal:null});
  fs.writeFileSync(path.join(dir,'timeline.jsonl'),events.map((event,index)=>JSON.stringify({seq:index,ns:String(index),...event})).join('\n')+'\n');
  const terminal=JSON.stringify({type:'result',finishReason:'aborted_streaming'});
  fs.writeFileSync(path.join(dir,'stdout.txt'),terminal+'\n'+(positive?canonical+'\n':''));
  const error={'malformed-response':'Invalid Gateway JSON frame.','abort-rpc-error':'fixture abort refused','connection-drop':'Gateway WebSocket closed.','abort-timeout':'Gateway request timed out: abort_turn'}[mode];
  fs.writeFileSync(path.join(dir,'stderr.txt'),positive?'':error+'\n');
  const summary={mode,code:1,signal:null};summaries.push(summary);fs.writeFileSync(path.join(dir,'summary.json'),JSON.stringify(summary));
 }
 fs.writeFileSync(path.join(directory,'summary.json'),JSON.stringify(summaries));
 const receipt=path.join(root,'receipt.json');fs.writeFileSync(receipt,JSON.stringify({status:'PASS',binary:sha,cases:SDK_SIGNAL_WIRE_CASES}));
 return {directory,receipt};
}
function withWire(run){
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'sdk-signal-public-'));
 const previous=[process.env.BBX_SDK_SIGNAL_WIRE_DIR,process.env.BBX_SDK_SIGNAL_WIRE_RECEIPT];
 try{const wire=wireDirectory(root);process.env.BBX_SDK_SIGNAL_WIRE_DIR=wire.directory;process.env.BBX_SDK_SIGNAL_WIRE_RECEIPT=wire.receipt;return run({...wire,root});}
 finally{for(const [index,key]of ['BBX_SDK_SIGNAL_WIRE_DIR','BBX_SDK_SIGNAL_WIRE_RECEIPT'].entries()){if(previous[index]===undefined)delete process.env[key];else process.env[key]=previous[index];}fs.rmSync(root,{recursive:true,force:true});}
}
const declaration=structuredClone(scenario.accepted.find(entry=>entry.id===SDK_SIGNAL_POLICY_ID));
function publicPair(root){
 const inputs={binaries:{pilotdeck:sha},sdkSignalWire:loadSdkSignalWireEvidence({binarySha256:sha}).identity};
 const artifacts=[fixture('ts',true),fixture('rust')];
 for(const artifact of artifacts)for(const rule of ['cancel','recovered'])artifact.provider[rule][0].body.model='deterministic';
 for(const [index,artifact]of artifacts.entries()){
  artifact.accepted=[declaration];artifact.evidence.inputs=inputs;artifact.evidence.assertions=assertionResults(scenario.assertions,artifact,artifact.side);
  const file=path.join(root,index?'rust.json':'ts.json');fs.writeFileSync(file,JSON.stringify(artifact));
 }
 return {inputs,artifacts,files:[path.join(root,'ts.json'),path.join(root,'rust.json')]};
}
function diff(files,args=[]){return spawnSync(process.execPath,[path.join(here,'diff.mjs'),...files,...args],{encoding:'utf8',env:process.env});}

test('wire evidence replays strict verifier and records every original raw hash',()=>withWire(({directory})=>{const checked=loadSdkSignalWireEvidence({binarySha256:sha});assert.equal(checked.receipt.status,'PASS');assert.equal(Object.keys(checked.identity.files).length,34);assert.equal(checked.identity.files['identity.json'],digest(fs.readFileSync(path.join(directory,'identity.json'))));}));
test('a handwritten PASS without complete raw proof is rejected',()=>withWire(({directory})=>{fs.unlinkSync(path.join(directory,'wrong-run-response/timeline.jsonl'));assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha}));}));
test('wire raw failure cannot be hidden by handwritten PASS receipt',()=>withWire(({directory})=>{fs.writeFileSync(path.join(directory,'final-and-abort-batch/stderr.txt'),'Gateway client closed.\n');assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha}),/strict raw verifier failed/);}));
test('wire producer must match actual trusted fixture hash',()=>withWire(({directory})=>{const file=path.join(directory,'identity.json');const identity=JSON.parse(fs.readFileSync(file));identity.driverSha256='b'.repeat(64);fs.writeFileSync(file,JSON.stringify(identity));assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha}),/producer identity/);}));
test('same-binary enforcement rejects mac/other binary receipt substitution',()=>withWire(()=>{assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:'b'.repeat(64)}),/binary\/producer identity/);}));
test('revalidation detects metadata raw change after formal input identity captured',()=>withWire(({directory})=>{const checked=loadSdkSignalWireEvidence({binarySha256:sha});const file=path.join(directory,'identity.json');const value=JSON.parse(fs.readFileSync(file));value.extra='changed';fs.writeFileSync(file,JSON.stringify(value));assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha,expectedIdentity:checked.identity}),/identity changed/);}));
test('wire raw symlinks and relative paths fail closed',()=>withWire(({directory})=>{const file=path.join(directory,'identity.json');fs.renameSync(file,file+'.real');fs.symlinkSync(file+'.real',file);assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha}),/regular file/);process.env.BBX_SDK_SIGNAL_WIRE_DIR='relative';assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha}),/absolute wire/);}));
test('public diff accepts exactly the validated SDK footer and stack',()=>withWire(({root})=>{const pair=publicPair(root);const result=diff(pair.files);assert.equal(result.status,0,result.stderr+result.stdout);assert.match(result.stdout,/ACCEPTED D1-SDK-SIGNAL-ABORT-ORDER-001/);}));
test('public self comparison remains DIFF for the original two footer shapes',()=>withWire(({root})=>{const pair=publicPair(root);pair.artifacts[1]=fixture('ts');pair.artifacts[1].accepted=[declaration];fs.writeFileSync(pair.files[1],JSON.stringify(pair.artifacts[1]));const result=diff(pair.files,['--self']);assert.equal(result.status,1,result.stderr+result.stdout);assert.doesNotMatch(result.stdout,/ACCEPTED D1-SDK-SIGNAL-ABORT-ORDER-001/);}));
test('public diff requires a receipt and reports INVALID, not ordinary DIFF',()=>withWire(({root})=>{const pair=publicPair(root);delete process.env.BBX_SDK_SIGNAL_WIRE_RECEIPT;const result=diff(pair.files);assert.equal(result.status,2);assert.match(result.stderr,/INVALID SDK signal evidence/);}));
test('public diff cannot ignore unrelated output/tree/provider changes',()=>withWire(({root})=>{const pair=publicPair(root);pair.artifacts[1].tree['/unrelated']={text:'changed'};fs.writeFileSync(pair.files[1],JSON.stringify(pair.artifacts[1]));const result=diff(pair.files);assert.equal(result.status,1);assert.match(result.stdout,/unrelated/);}));
test('public evidence validation executes the Rust signal contract and wire replay',()=>withWire(({root})=>{const pair=publicPair(root);const spec={...scenario,accepted:[declaration]};const artifact=pair.artifacts[1];const scenarioId={name:scenario.name,path:'/synthetic/scenario',sha256:'c'.repeat(64)};Object.assign(artifact.evidence,{schema:1,runId:'unit-run',side:'rust',scenario:scenarioId,assertions:assertionResults(spec.assertions,artifact,'rust')});const expected={runId:'unit-run',side:'rust',scenario:scenarioId,inputs:pair.inputs};assert.deepEqual(validateArtifact(artifact,spec,expected),[]);delete artifact.evidence.inputs.sdkSignalWire;assert(validateArtifact(artifact,spec,expected).some(error=>error.includes('wire evidence')));}));

test('public named diff rejects captures that both delete the policy declaration',()=>withWire(({root})=>{const pair=publicPair(root);for(const [index,artifact]of pair.artifacts.entries()){delete artifact.accepted;fs.writeFileSync(pair.files[index],JSON.stringify(artifact));}const result=diff(pair.files);assert.equal(result.status,2);assert.match(result.stderr,/required named scenario accepted declaration/);}));
test('original TS self still reports DIFF with no new declaration',()=>withWire(({root})=>{const pair=publicPair(root);pair.artifacts[1]=fixture('ts');for(const [index,artifact]of pair.artifacts.entries()){delete artifact.accepted;fs.writeFileSync(pair.files[index],JSON.stringify(artifact));}const result=diff(pair.files,['--self']);assert.equal(result.status,1);assert.doesNotMatch(result.stderr,/INVALID SDK signal evidence/);}));
test('raw receipt byte changes after capture are rejected even if JSON remains PASS',()=>withWire(({receipt})=>{const first=loadSdkSignalWireEvidence({binarySha256:sha});fs.writeFileSync(receipt,JSON.stringify(JSON.parse(fs.readFileSync(receipt)),null,2));assert.throws(()=>loadSdkSignalWireEvidence({binarySha256:sha,expectedIdentity:first.identity}),/identity changed/);}));
for(const filename of ['driver.mjs','verify.mjs'])test(`trusted ${filename} source tampering is rejected before execution`,()=>withWire(({root})=>{
 const dir=path.join(root,'scripts/bbx');fs.mkdirSync(dir,{recursive:true});
 for(const name of ['evidence.mjs','syntax-policy.mjs','native-identity.mjs','serve-persisted-event.mjs','channel-poll-policy.mjs','sdk-agent-policy.mjs','sdk-signal-policy.mjs'])fs.copyFileSync(new URL(name,import.meta.url),path.join(dir,name));
 const fixtureDir=path.join(root,'src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order');fs.mkdirSync(fixtureDir,{recursive:true});
 for(const name of ['driver.mjs','verify.mjs'])fs.copyFileSync(path.join(here,'../../src/pilotdeck-cli/tests/fixtures/sdk-signal-abort-order',name),path.join(fixtureDir,name));
 fs.appendFileSync(path.join(fixtureDir,filename),'\nthrow Error("untrusted verifier was executed");\n');
 const program=`const {loadSdkSignalWireEvidence}=await import(${JSON.stringify('file://'+path.join(dir,'evidence.mjs'))});loadSdkSignalWireEvidence({binarySha256:${JSON.stringify(sha)}});`;
 const result=spawnSync(process.execPath,['--input-type=module','-e',program],{encoding:'utf8',env:process.env});assert.equal(result.status,1);assert.match(result.stderr,/trusted driver\/verifier source changed/);assert.doesNotMatch(result.stderr,/Error: untrusted verifier was executed/);
}));

test('bare public diff rejects missing executed Rust positive assertions',()=>withWire(({root})=>{const pair=publicPair(root);pair.artifacts[1].evidence.assertions=[];fs.writeFileSync(pair.files[1],JSON.stringify(pair.artifacts[1]));const result=diff(pair.files);assert.equal(result.status,2);assert.match(result.stderr,/assertions were not executed/);}));
