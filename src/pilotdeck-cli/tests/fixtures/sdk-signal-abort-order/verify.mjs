// Strict positive and failure-path verifier; it never normalizes TS stderr.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import path from 'node:path';
const [root,expectedBinarySha,casesRaw]=process.argv.slice(2);
assert(root&&/^[a-f0-9]{64}$/.test(expectedBinarySha??''),'output directory and expected binary SHA are required');
const cases=casesRaw?.split(',')??['final-and-abort-batch','final-and-abort-next-turn','final-and-abort-separated','wrong-run-response','malformed-response','abort-rpc-error','connection-drop','abort-timeout'];
const read=async file=>readFile(path.join(root,file),'utf8');
const identity=JSON.parse(await read('identity.json'));
assert.equal(identity.cliSha256,expectedBinarySha,'stale or incorrect binary identity');
assert(/^[a-f0-9]{64}$/.test(identity.driverSha256??''),'driver identity is required');
const summaries=JSON.parse(await read('summary.json'));
assert.deepEqual(summaries.map(s=>s.mode),cases,'exact scenario set/order required');
for(const mode of cases){
 const summary=summaries.find(s=>s.mode===mode);
 const timeline=(await read(`${mode}/timeline.jsonl`)).trimEnd().split('\n').map(JSON.parse);
 for(let index=0;index<timeline.length;index++){assert.equal(timeline[index].seq,index);if(index)assert(BigInt(timeline[index].ns)>=BigInt(timeline[index-1].ns));}
 const requests=timeline.filter(e=>e.kind==='cli_to_gateway').map(e=>({...JSON.parse(e.raw),connection:e.connection}));
 const submits=requests.filter(e=>e.method==='submit_turn'); const aborts=requests.filter(e=>e.method==='abort_turn');
 assert.equal(submits.length,1,`${mode}: no duplicate submit`);assert.equal(aborts.length,1,`${mode}: no duplicate abort`);
 assert.equal(aborts[0].connection,submits[0].connection,`${mode}: abort must use current query transport`);
 assert.equal(aborts[0].params.sessionKey,submits[0].params.sessionKey,`${mode}: exact current session`);
 assert.equal(aborts[0].params.runId,submits[0].params.runId,`${mode}: exact current run`);
 assert.equal(aborts[0].params.reason,'CLI interrupted');
 assert.equal(timeline.filter(e=>e.kind==='signal_sent'&&e.signal==='SIGINT'&&e.delivered===true).length,1,`${mode}: real signal`);
 assert(!timeline.some(e=>e.kind==='diagnostic_child_timeout'),`${mode}: natural exit required`);
 assert.equal(summary.code,1);assert.equal(summary.signal,null);
 const stdout=await read(`${mode}/stdout.txt`);const stderr=await read(`${mode}/stderr.txt`);
 const lines=stdout.trimEnd().split('\n');const terminal=lines.filter(line=>line.startsWith('{')).map(JSON.parse).find(event=>event.type==='result');
 assert.equal(terminal?.finishReason,'aborted_streaming',`${mode}: provider terminal remains unchanged`);
 const canonical=['final-and-abort-batch','final-and-abort-next-turn','final-and-abort-separated','wrong-run-response'].includes(mode);
 if(canonical){
  assert.deepEqual(JSON.parse(lines.at(-1)),{status:'aborted',reason:'CLI interrupted'},`${mode}: canonical aborted result`);
  assert.equal(stderr,'',`${mode}: no suppressed Gateway or unhandled error`);
  const delivered=timeline.find(e=>e.kind==='gateway_to_cli'&&e.label==='abort_turn_response'&&e.readyState===1);
  assert(delivered,`${mode}: successful abort reply was actually delivered on open socket`);
  assert.equal(delivered.frame.id,aborts[0].id);
  const close=timeline.find(e=>e.kind==='ws_close'&&e.connection===submits[0].connection);
  assert(close&&close.seq>delivered.seq,`${mode}: observation remains open until abort response`);
  assert.equal(timeline.filter(e=>e.kind==='ws_close').length,2,`${mode}: both owned sockets released`);
  if(mode==='final-and-abort-separated')assert(timeline.some(e=>e.kind==='diagnostic_held_response_release'&&e.childExitCode===null&&e.childSignalCode===null),`${mode}: pending abort retained during held barrier`);
  if(mode==='wrong-run-response')assert(timeline.some(e=>e.kind==='wrong_run_response_barrier'&&e.childExitCode===null&&e.childSignalCode===null),`${mode}: unrelated response cannot settle this run`);
 }else{
  assert(!stdout.includes('"status":"aborted"'),`${mode}: failure must never masquerade as successful abort`);
  const expected={'malformed-response':'Invalid Gateway JSON frame.','abort-rpc-error':'fixture abort refused','connection-drop':'Gateway WebSocket closed.','abort-timeout':'Gateway request timed out: abort_turn'}[mode];
  assert.equal(stderr,`${expected}\n`,`${mode}: preserve exact RPC/transport/deadline error`);
 }
}
console.log(JSON.stringify({status:'PASS',binary:expectedBinarySha,cases},null,2));
