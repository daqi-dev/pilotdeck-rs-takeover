// D1-SDK-SIGNAL-ABORT-ORDER-001. This policy grants two individually checked
// values only; it never edits raw captures, stream events, or TS self results.
import {isDeepStrictEqual} from 'node:util';
import {createHash} from 'node:crypto';
import {validatePersistedRunTriggers} from './serve-persisted-event.mjs';
export const SDK_SIGNAL_POLICY_ID='D1-SDK-SIGNAL-ABORT-ORDER-001';
export const SDK_SIGNAL_WIRE_CASES=Object.freeze(['final-and-abort-batch','final-and-abort-next-turn','final-and-abort-separated','wrong-run-response','malformed-response','abort-rpc-error','connection-drop','abort-timeout']);
const insist=(condition,message)=>{if(!condition)throw Error(`sdk signal policy: ${message}`);};
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
// Exact observed fixed-cc7c4e6 close stack after the already accepted, exact
// Node SQLite warning is removed. No "contains Gateway client closed" rule.
const CLOSE_STACK_SHA='7842b4a1f57b0325cf1d576f83e9110b259a8eb9e10b1062809150373a0ec855';
const canonical='{"status":"aborted","reason":"CLI interrupted"}';
const warning=/^\(node:[1-9]\d*\) ExperimentalWarning: SQLite is an experimental feature and might change at any time\n\(Use `node --trace-warnings \.\.\.` to show where the warning was created\)\n/;
const markers=['C1-INITIAL','C1-CANCEL','C1-RECOVER'];
const parse=(text,label)=>{try{return JSON.parse(text);}catch{throw Error(`sdk signal policy: invalid ${label}`);}};
const textMessage=message=>typeof message?.content==='string'?message.content:Array.isArray(message?.content)&&message.content.every(p=>p.type==='text'&&typeof p.text==='string')?message.content.map(p=>p.text).join(''):undefined;
function stdoutParts(text,side){
 insist(typeof text==='string'&&text.endsWith('\n'),`${side} stdout must end in newline`);
 const lines=text.slice(0,-1).split('\n');const footer=lines.pop();
 insist(footer===canonical||(side==='ts'&&footer==='C1 partial'),`${side} unexpected cancellation footer`);
 const events=lines.map(line=>parse(line,`${side} stream JSON`));
 insist(events.length>0&&events.every(e=>e&&typeof e==='object'&&!Array.isArray(e)),`${side} stream events missing`);
 return {footer,events,prefix:lines.join('\n')+'\n'};
}
function sessionEnvelope(step,label){
 insist(step?.cli?.exitCode===0&&step.cli.signal===null,`${label} did not naturally succeed`);
 const lines=step.cli.stdout.trimEnd().split('\n');return parse(lines.at(-1),`${label} result envelope`);
}
function scenarioContract(scenario){
 insist(scenario?.name==='c1-sdk-cancel-resume'&&scenario.steps?.length===4,'only the named four-step scenario is eligible');
 insist(isDeepStrictEqual(scenario.steps[1],{serve:['resume','{saved.session}','--project','{PROJECT}','--stream-json','C1-CANCEL'],signal:'SIGINT',timeout:12000,settleMs:100,untilPersistedRunEvent:{sessionKey:'{saved.session}',projectKey:'{PROJECT}',requestText:'C1-CANCEL',event:{type:'assistant_text_delta',text:'C1 partial'},providerRule:'cancel'}}),'only the exact first serve cancellation step is eligible');
 insist(scenario.steps.filter(step=>step.serve).length===1,'unexpected additional serve step');
}
export function validateSdkSignalArtifact(raw,scenario,side){
 scenarioContract(scenario);insist(side==='ts'||side==='rust','unsupported side');
 insist(raw?.scenario===scenario.name&&raw.side===side,'artifact scenario/side mismatch');
 insist(raw.steps?.length===4&&raw.steps.every((step,index)=>isDeepStrictEqual(step.step,scenario.steps[index])),'artifact step definitions changed');
 insist(Array.isArray(raw.failures)&&raw.failures.length===0,'artifact has runtime failures');
 const errors=validatePersistedRunTriggers(raw,scenario);insist(errors.length===0,errors.join('; '));
 const proofs=raw.evidence?.serveTriggers;insist(proofs?.length===1,'exactly one raw persisted-trigger proof required');
 const proof=proofs[0],serve=raw.steps[1].serve,trigger=serve.persistedRunEvent;
 insist(proof.stepIndex===1&&proof.signalSent==='SIGINT'&&serve.reached===true&&serve.exitCode===1&&serve.signal===null,'real SIGINT and natural interrupted exit required');
 // validatePersistedRunTriggers already replays raw SHA-256 registry bytes,
 // checks PID/time identity, session/project/request/run/partial, and verifies
 // that the provider response was open with a flushed chunk at signal time.
 const session=proof.condition.sessionKey,project=proof.condition.projectKey,run=trigger.record.runId;
 insist(Number.isSafeInteger(proof.pid)&&proof.pid>0&&proof.process.pid===proof.pid,'actual PID evidence required');
 insist(trigger.provider.rule==='cancel'&&trigger.provider.index===0&&trigger.provider.open===true&&trigger.provider.flushedChunks===1,'exact cancel provider trigger required');
 const initial=sessionEnvelope(raw.steps[0],'initial'),recovery=sessionEnvelope(raw.steps[2],'recovery');
 insist(initial.sessionId===session&&initial.result?.status==='completed'&&initial.result.output==='C1 initial reply.','initial durable session/result mismatch');
 insist(recovery.sessionId===session&&recovery.result?.status==='completed'&&recovery.result.output==='C1 recovered reply.','recovery must complete on the same durable session');
 insist(raw.steps[3]?.cli?.exitCode===0&&raw.steps[3].cli.signal===null,'history read failed');
 const history=parse(raw.steps[3].cli.stdout,'history');insist(Array.isArray(history),'history array required');
 for(const marker of markers)insist(history.filter(m=>m.role==='user'&&m.text===marker&&m.sessionKey===session&&m.projectKey===project).length===1,`history must contain exactly one ${marker} on the same session/project`);
 insist(history.filter(m=>m.role==='assistant'&&m.text==='C1 partial'&&m.sessionKey===session&&m.projectKey===project).length===1,'cancelled partial must persist exactly once');
 const registryEntries=Object.entries(raw.tree??{}).filter(([name])=>name.endsWith('/runs/registry.json'));
 insist(registryEntries.length===1,'one durable registry required');const registry=registryEntries[0][1].json;
 insist(registry?.schemaVersion===1&&Array.isArray(registry.records)&&registry.records.length===3,'exact three durable runs required');
 const runs=markers.map((marker,index)=>{
  const records=registry.records.filter(record=>{const request=parse(record.requestMaterial,'durable request');return request.message===marker&&request.sessionKey===session&&request.projectKey===project;});
  insist(records.length===1,`exact durable ${marker} run required`);const record=records[0];
  insist(record.sessionKey===session&&record.projectKey===project&&record.state===(index===1?'aborted':'completed'),'durable run state/identity mismatch');
  const events=registry.events?.[`${project}\0${session}\0${record.runId}`];insist(Array.isArray(events),'durable run events missing');
  const terminals=events.filter(frame=>frame.event?.type==='turn_completed');
  insist(terminals.length===1&&terminals[0].event.runId===record.runId&&terminals[0].event.finishReason===(index===1?'aborted_streaming':'completed'),'durable terminal mismatch');
  return {record,events};
 });
 insist(runs[1].record.runId===run&&isDeepStrictEqual(runs[1].events.find(frame=>frame.seq===trigger.event.seq),trigger.event),'raw triggered partial does not survive in final registry');
 insist(new Set(runs.map(r=>r.record.runId)).size===3,'duplicate durable run identity');
 insist(isDeepStrictEqual(Object.keys(raw.provider??{}).sort(),['cancel','initial','recovered','title']),'unexpected provider rule');
 for(const rule of ['cancel','initial','recovered','title'])insist(raw.provider[rule]?.length===1,`${rule} provider must execute exactly once`);
 const recoveryMessages=raw.provider.recovered[0].body?.messages;
 insist(Array.isArray(recoveryMessages)&&isDeepStrictEqual(recoveryMessages.slice(-4).map(m=>({role:m.role,text:textMessage(m)})),[{role:'assistant',text:'C1 initial reply.'},{role:'user',text:'C1-CANCEL'},{role:'assistant',text:'C1 partial'},{role:'user',text:'C1-RECOVER'}]),'recovery model request lost or duplicated persisted conversation');
 const parts=stdoutParts(serve.stdout,side),terminal=parts.events.filter(e=>e.type==='result');
 insist(terminal.length===1&&terminal[0].runId===run&&terminal[0].sessionId===session&&terminal[0].finishReason==='aborted_streaming'&&terminal[0].status==='completed','streamed provider terminal identity/value changed');
 if(side==='rust')insist(parts.footer===canonical&&serve.stderr==='','Rust requires exact canonical aborted footer and empty stderr');
 else{const stderr=serve.stderr.replace(warning,'');insist(parts.footer===canonical?stderr==='':hash(stderr)===CLOSE_STACK_SHA,'TS output is neither observed clean abort nor exact fixed SDK unhandled-close stack');}
 return {session,project,run,parts,proofLayers:{formal:'raw persisted partial + actual signal/PID + durable recovery',abortRpc:'not present in formal capture; independently verified wire receipt required'}};
}
// wireReceipt is a result from the independently executed strict 8-wire verifier,
// not a claim that the formal capture contains RPC frames. The caller owns
// loading its original output, identity and manifest before invoking this policy.
export function sdkSignalAcceptedPaths({left,right,rawLeft,rawRight,scenario,selfCompare=false,wireReceipt}){
 if(selfCompare)return new Map();
 insist(rawLeft?.side==='ts'&&rawRight?.side==='rust','only TS-to-Rust comparisons are eligible');
 const ts=validateSdkSignalArtifact(rawLeft,scenario,'ts');validateSdkSignalArtifact(rawRight,scenario,'rust');
 const binary=rawRight.evidence?.inputs?.binaries?.pilotdeck;
 const binarySha=typeof binary==='string'?binary:binary?.sha256;
 insist(/^[a-f0-9]{64}$/.test(binarySha??''),'Rust formal binary identity missing');
 insist(isDeepStrictEqual(Object.keys(wireReceipt??{}).sort(),['binary','cases','status']),'strict wire verifier receipt shape required');
 insist(wireReceipt?.status==='PASS'&&wireReceipt.binary===binarySha&&isDeepStrictEqual(wireReceipt.cases,SDK_SIGNAL_WIRE_CASES),'independent exact-eight-case wire PASS receipt must bind the same formal binary');
 const a=stdoutParts(left.steps[1].serve.stdout,'ts'),b=stdoutParts(right.steps[1].serve.stdout,'rust');
 insist(a.prefix===b.prefix,'normalized pre-footer stream event bytes differ');
 const accepted=new Map();
 if(left.steps[1].serve.stdout!==right.steps[1].serve.stdout)accepted.set('$.steps[1].serve.stdout',`${SDK_SIGNAL_POLICY_ID}: identical streamed events; verified current-run signal abort has canonical footer (${ts.parts.footer===canonical?'clean TS':'fixed SDK race'} baseline)`);
 if(left.steps[1].serve.stderr!==right.steps[1].serve.stderr)accepted.set('$.steps[1].serve.stderr',`${SDK_SIGNAL_POLICY_ID}: exact observed cc7c4e6 unhandled-close stack; Rust empty stderr and same-binary strict wire PASS`);
 return accepted;
}
