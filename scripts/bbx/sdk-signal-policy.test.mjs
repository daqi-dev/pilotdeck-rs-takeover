import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {sdkSignalAcceptedPaths,validateSdkSignalArtifact,SDK_SIGNAL_WIRE_CASES} from './sdk-signal-policy.mjs';
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
const receipt=()=>({status:'PASS',binary:sha,cases:[...SDK_SIGNAL_WIRE_CASES]});
function pair(crash=true){const rawLeft=fixture('ts',crash),rawRight=fixture();return {left:structuredClone(rawLeft),right:structuredClone(rawRight),rawLeft,rawRight,scenario:structuredClone(scenario),wireReceipt:receipt()};}

test('crash case maps exactly two individually validated fields',()=>{const input=pair();assert.deepEqual([...sdkSignalAcceptedPaths(input).keys()],['$.steps[1].serve.stdout','$.steps[1].serve.stderr']);});
test('clean baseline adds no accepted paths when values already match',()=>{assert.equal(sdkSignalAcceptedPaths(pair(false)).size,0);});
test('TS self always leaves the original differences unaccepted',()=>{const input=pair();input.rawRight=fixture('ts');input.selfCompare=true;assert.equal(sdkSignalAcceptedPaths(input).size,0);});
const mutations={
 'wrong-scenario':p=>p.scenario.name='another',
 'wrong-signal':p=>p.scenario.steps[1].signal='SIGTERM',
 'wrong-serve-index':p=>p.rawRight.evidence.serveTriggers[0].stepIndex=2,
 'missing-trigger':p=>p.rawRight.evidence.serveTriggers=[],
 'missing-real-signal':p=>p.rawRight.evidence.serveTriggers[0].signalSent=null,
 'wrong-pid':p=>p.rawRight.evidence.serveTriggers[0].process.pid=43,
 'reversed-signal-time':p=>p.rawRight.evidence.serveTriggers[0].signalSentAt='2025-01-01T00:00:00Z',
 'forged-raw-hash':p=>p.rawRight.evidence.serveTriggers[0].raw.sha256='b'.repeat(64),
 'wrong-project':p=>p.rawRight.evidence.serveTriggers[0].projectKey='/another',
 'wrong-request':p=>p.rawRight.evidence.serveTriggers[0].condition.requestText='another',
 'wrong-run':p=>p.rawRight.steps[1].serve.persistedRunEvent.record.runId='another',
 'wrong-partial':p=>p.rawRight.steps[1].serve.persistedRunEvent.event.event.text='another',
 'closed-provider':p=>p.rawRight.evidence.serveTriggers[0].providers[0].open=false,
 'unflushed-provider':p=>p.rawRight.evidence.serveTriggers[0].providers[0].flushedChunks=0,
 'duplicate-provider':p=>p.rawRight.provider.cancel.push(structuredClone(p.rawRight.provider.cancel[0])),
 'duplicate-recovery-provider':p=>p.rawRight.provider.recovered.push(structuredClone(p.rawRight.provider.recovered[0])),
 'wrong-recovery-session':p=>{const lines=p.rawRight.steps[2].cli.stdout.split('\n');const envelope=JSON.parse(lines[1]);envelope.sessionId='another';lines[1]=JSON.stringify(envelope);p.rawRight.steps[2].cli.stdout=lines.join('\n');},
 'missing-history-partial':p=>{const h=JSON.parse(p.rawRight.steps[3].cli.stdout);p.rawRight.steps[3].cli.stdout=JSON.stringify(h.filter(m=>m.text!=='C1 partial'));},
 'missing-model-recovery-history':p=>p.rawRight.provider.recovered[0].body.messages.splice(2,1),
 'wrong-durable-state':p=>p.rawRight.tree['/home/.pilotdeck/runs/registry.json'].json.records[1].state='completed',
 'duplicate-durable-run':p=>p.rawRight.tree['/home/.pilotdeck/runs/registry.json'].json.records.push(structuredClone(p.rawRight.tree['/home/.pilotdeck/runs/registry.json'].json.records[1])),
 'wrong-stream-event':p=>p.right.steps[1].serve.stdout=p.right.steps[1].serve.stdout.replace('aborted_streaming','completed'),
 'rust-error-footer':p=>p.rawRight.steps[1].serve.stdout=p.rawRight.steps[1].serve.stdout.replace(canonical,'C1 partial'),
 'rust-hidden-stderr':p=>p.rawRight.steps[1].serve.stderr='Gateway client closed.\n',
 'arbitrary-ts-close-line':p=>p.rawLeft.steps[1].serve.stderr='Gateway client closed.\n',
 'altered-ts-stack':p=>p.rawLeft.steps[1].serve.stderr=p.rawLeft.steps[1].serve.stderr.replace('Object.next','Object.other'),
 'extra-ts-stack':p=>p.rawLeft.steps[1].serve.stderr+='extra error\n',
 'missing-wire':p=>delete p.wireReceipt,
 'stale-binary-wire':p=>p.wireReceipt.binary='b'.repeat(64),
 'missing-wire-control':p=>p.wireReceipt.cases.pop(),
 'failed-wire':p=>p.wireReceipt.status='FAIL',
 'extra-wire-receipt-field':p=>p.wireReceipt.unchecked=true,
 'changed-stream-format':p=>p.right.steps[1].serve.stdout=p.right.steps[1].serve.stdout.replace('{\"type\":','{ \"type\":'),
 'wrong-exit':p=>p.rawRight.steps[1].serve.exitCode=0,
 'killed-instead-of-exit':p=>p.rawRight.steps[1].serve.signal='SIGKILL',
};
for(const [name,mutate] of Object.entries(mutations))test(`reject ${name}`,()=>{const input=pair();mutate(input);assert.throws(()=>sdkSignalAcceptedPaths(input));});
test('policy does not grant paths for mutations outside the two fields',()=>{const input=pair();input.right.tree.extra={text:'corrupt'};input.right.provider.extra=[];input.right.steps[2].cli.stdout='corrupt';const accepted=sdkSignalAcceptedPaths(input);assert.deepEqual([...accepted.keys()],['$.steps[1].serve.stdout','$.steps[1].serve.stderr']);});
test('formal validation declares its RPC proof limitation',()=>{assert.match(validateSdkSignalArtifact(fixture(),scenario,'rust').proofLayers.abortRpc,/not present in formal capture/);});

// Approved D1-SDK-SIGNAL-TERMINAL-OBSERVATION-001: synthetic proof data only.
function omissionPair(){
 const input=pair(false);
 const types=['turn.started','user.accepted','pilotdeck.model_request_started','context.usage','pilotdeck.model_selection_changed','pilotdeck.assistant_block'];
 const sequences=[0,1,2,3,4,6];
 for(const raw of [input.rawLeft,input.rawRight]){
  const prefix=types.map((type,i)=>({type,sequence:sequences[i],runId:run,sessionId:session}));
  const terminal={type:'result',usage:{},finishReason:'aborted_streaming',runId:run,sequence:7,sessionId:session,status:'completed'};
  raw.steps[1].serve.stdout=[...prefix,...(raw.side==='rust'?[terminal]:[])].map(e=>JSON.stringify(e)).join('\n')+'\n'+canonical+'\n';
  raw.tree['/home/.pilotdeck/runs/registry.json'].json.events[`${project}\0${session}\0${run}`][1]={seq:8,event:{type:'turn_completed',usage:{},finishReason:'aborted_streaming',runId:run}};
 }
 input.left=structuredClone(input.rawLeft);input.right=structuredClone(input.rawRight);input.policy='sdkSignalTerminalObservation';return input;
}
test('approved TS-only terminal omission maps stdout solely under its own policy',()=>{
 const input=omissionPair();assert.deepEqual([...sdkSignalAcceptedPaths(input).keys()],['$.steps[1].serve.stdout']);
 input.policy='sdkSignalAbortOrder';assert.equal(sdkSignalAcceptedPaths(input).size,0);
});
for(const [name,mutate]of [
 ['wrong run',p=>p.rawLeft.steps[1].serve.stdout=p.rawLeft.steps[1].serve.stdout.replace('"runId":"cancel-run"','"runId":"wrong-run"')],
 ['missing other event',p=>p.rawLeft.steps[1].serve.stdout=p.rawLeft.steps[1].serve.stdout.split('\n').filter(line=>!line.includes('"sequence":3')).join('\n')],
 ['missing durable terminal',p=>p.rawLeft.tree['/home/.pilotdeck/runs/registry.json'].json.events[`${project}\0${session}\0${run}`].pop()],
 ['Rust missing terminal',p=>p.rawRight.steps[1].serve.stdout=p.rawRight.steps[1].serve.stdout.split('\n').filter(line=>!line.includes('"type":"result"')).join('\n')],
])test(`approved terminal omission rejects ${name}`,()=>{const input=omissionPair();mutate(input);assert.throws(()=>sdkSignalAcceptedPaths(input));});
test('approved terminal omission never accepts TS self',()=>{const input=omissionPair();input.selfCompare=true;assert.equal(sdkSignalAcceptedPaths(input).size,0);});
