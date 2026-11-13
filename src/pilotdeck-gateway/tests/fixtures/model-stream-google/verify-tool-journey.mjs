// RA05 independent strict verifier. No product or network is started here.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
export const INPUT = 'RA05 synthetic source\nquantity=7\nunit_price=13\nsource_nonce=RA05-7x13-20261113\n';
export const ANSWER = 'quantity=7 unit_price=13 total=91 source_nonce=RA05-7x13-20261113';
export const PROMPT = 'Read ra05-input.txt with read_file and report quantity times unit_price, citing the source nonce.';
export const CALL = { id: 'ra05-read-001', name: 'read_file', args: { file_path: 'ra05-input.txt' } };
export const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const walk = value => value && typeof value === 'object' ? [value, ...Object.values(value).flatMap(walk)] : [];
export function verify(a) {
  assert.equal(a.schema, 'RA05-google-tool-v1');
  assert.equal(a.captureComplete, true, 'capture failed');
  assert.deepEqual(a.errors, []);
  assert.ok(['ts', 'rust'].includes(a.identity.engine));
  assert.match(a.identity.driverSha256, /^[a-f0-9]{64}$/);
  assert.match(a.identity.commandSha256, /^[a-f0-9]{64}$/);
  assert.equal(a.identity.baselineCommit, 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
  assert.ok(Object.keys(a.identity.sourceClosure).includes('src/cli/pilotdeck.ts'), 'real source closure map required');
  for (const hash of Object.values(a.identity.sourceClosure)) assert.match(hash, /^[a-f0-9]{64}$/);
  assert.match(a.identity.node.sha256, /^[a-f0-9]{64}$/);
  assert.match(a.identity.selectedEntrySha256, /^[a-f0-9]{64}$/);
  if (a.identity.engine === 'ts') {
    assert.ok(['source','built'].includes(a.identity.selectedEntryMode));
    assert.equal(a.identity.command,a.identity.node.path);
    assert.equal(a.identity.commandSha256,a.identity.node.sha256);
    const built = a.identity.selectedEntryMode === 'built';
    assert.ok(a.identity.selectedEntry.endsWith(built?'/dist/src/cli/pilotdeck.js':'/src/cli/pilotdeck.ts'));
    assert.ok(a.identity.args.includes(a.identity.selectedEntry));
    if (built) {
      assert.equal(a.identity.args[0],a.identity.selectedEntry);
      assert.match(a.identity.baselineDist.expectedSha256,/^[a-f0-9]{64}$/);
      assert.equal(a.identity.baselineDist.sha256,a.identity.baselineDist.expectedSha256);
      assert.equal(sha(JSON.stringify(a.identity.baselineDist.files)),a.identity.baselineDist.manifestSha256);
      assert.equal(a.identity.baselineDist.files.find(f=>f.path==='src/cli/pilotdeck.js')?.sha256,a.identity.selectedEntrySha256);
    } else { assert.equal(a.identity.args[0],'--import'); assert.equal(a.identity.selectedEntrySha256,a.identity.sourceClosure['src/cli/pilotdeck.ts']); }
  } else { assert.equal(a.identity.selectedEntryMode,'native'); assert.equal(a.identity.selectedEntry,a.identity.command); assert.equal(a.identity.selectedEntrySha256,a.identity.commandSha256); }
  if (a.identity.engine === 'rust') { assert.equal(a.identity.nativeExecutable, true); assert.equal(a.identity.commandSha256, a.identity.expectedBinarySha256); }
  const main = a.requests.filter(r => r.purpose === 'turn');
  assert.equal(main.length, 2, 'exact two main requests');
  for (const r of main) {
    assert.equal(r.method, 'POST');
    assert.equal(r.path, '/v1beta/models/m:streamGenerateContent?alt=sse');
    assert.equal(r.apiKey, 'fixture-only');
  }
  assert.ok(main[0].body.tools.flatMap(t => t.functionDeclarations ?? []).some(t => t.name === 'read_file' && t.parametersJsonSchema.required.includes('file_path')));
  assert.ok(main[0].body.contents.some(c => c.role === 'user' && (c.parts ?? []).some(p => p.text === PROMPT)), 'original user prompt');
  for (const r of main) {
    assert.equal(r.response.status, 200);
    assert.match(r.response.headers['content-type'], /^text\/event-stream/);
    const lines=r.response.rawHeaders.trim().split('\r\n');
    assert.match(lines.shift(),/^HTTP\/1\.1 200 /);
    const actualHeaders=Object.fromEntries(lines.map(line=>{const i=line.indexOf(':');return [line.slice(0,i).toLowerCase(),line.slice(i+1).trim()];}));
    assert.equal(actualHeaders['content-type'],r.response.headers['content-type'],'actual raw HTTP header agrees with structured record');
    assert.equal(sha(Buffer.from(r.response.bodyBase64, 'base64')), r.response.sha256);
    const chunks = Buffer.from(r.response.bodyBase64, 'base64').toString().split('\n\n').filter(Boolean).map(line => JSON.parse(line.replace(/^data: /, '')));
    assert.equal(chunks.length, 2);
    assert.equal(chunks[1].candidates[0].finishReason, 'STOP');
  }
  assert.deepEqual(JSON.parse(Buffer.from(main[0].response.bodyBase64, 'base64').toString().split('\n\n')[0].slice(6)).candidates[0].content.parts, [{functionCall: CALL}]);
  assert.equal(walk(main[0].body.contents).filter(p => p.functionResponse).length, 0);
  const calls = walk(main[1].body.contents).filter(p => p.functionCall).map(p => p.functionCall);
  assert.deepEqual(calls, [CALL]);
  const results = walk(main[1].body.contents).filter(p => p.functionResponse).map(p => p.functionResponse);
  assert.equal(results.length, 1);
  assert.equal(results[0].id, CALL.id); assert.equal(results[0].name, CALL.name);
  assert.equal(Object.hasOwn(results[0].response, 'error'), false);
  const output = results[0].response.output;
  assert.equal(typeof output, 'string');
  for (const line of INPUT.trimEnd().split('\n')) assert.ok(output.includes(line), `real read output missing ${line}`);
  assert.ok(!output.includes('RA05-WRONG-SOURCE'), 'wrong actual source');
  assert.equal(a.inputBeforeSha256, sha(INPUT));
  assert.equal(a.inputAfterSha256, sha(INPUT), 'input protected');
  const events = a.frames.filter(f => f.id === 'turn').flatMap(f => f.event ? [f.event] : []);
  const started = events.filter(e => e.type === 'tool_call_started');
  const finished = events.filter(e => e.type === 'tool_call_finished');
  assert.equal(started.length, 1); assert.equal(finished.length, 1);
  assert.equal(started[0].toolCallId, CALL.id); assert.equal(started[0].name, CALL.name);
  assert.equal(finished[0].toolCallId, CALL.id); assert.equal(finished[0].ok, true);
  const final = a.frames.filter(f => f.id === 'turn' && f.final);
  assert.equal(final.length, 1); assert.equal(final[0].event.type, 'turn_completed');
  assert.equal(final[0].event.finishReason, 'completed');
  assert.deepEqual(final[0].event.usage, { inputTokens: 28, outputTokens: 5, totalTokens: 33 });
  const history=a.history.messages;
  assert.ok(Array.isArray(history),'public history.messages');
  const uses=history.filter(m=>m.kind==='tool_use' && m.toolCallId===CALL.id);
  assert.equal(uses.length,1);assert.equal(uses[0].role,'tool');assert.equal(uses[0].toolName,CALL.name);assert.deepEqual(uses[0].payload,CALL.args);
  const durable=history.filter(m=>m.kind==='tool_result' && m.toolCallId===CALL.id);
  assert.equal(durable.length,1);assert.equal(durable[0].role,'tool');assert.equal(durable[0].toolName,CALL.name);assert.equal(durable[0].ok,true);
  assert.equal(durable[0].text,output,'public durable result equals complete functionResponse.output');
  const answers=history.filter(m=>m.role==='assistant' && m.kind==='text');
  assert.equal(answers.length,1);assert.equal(answers[0].text,ANSWER);
  const auditRows = a.audit.files.filter(f => f.path.endsWith('/invocations.jsonl')).flatMap(f => { assert.equal(sha(f.text), f.sha256); return f.text.trim().split('\n').filter(Boolean).map(line => JSON.parse(line)); });
  const invocations = auditRows.filter(r => r.sessionId === 'ra05-google-tool' && r.stream === true && r.protocol === 'google' && r.model === 'm');
  assert.equal(invocations.length, 2, 'two actual main invocation audit rows');
  for (const row of invocations) { assert.equal(row.outcome, 'success'); assert.equal(row.responseComplete, true); assert.equal(typeof row.requestBody, 'string'); assert.equal(typeof row.responseBody, 'string'); }
  assert.ok(invocations.some(row => walk(JSON.parse(row.requestBody)).some(p => p.functionResponse?.id === CALL.id && p.functionResponse.response.output === output)), 'audit retains complete continuation output');
  assert.deepEqual(a.cleanup.exit, { code: 0, signal: null });
  assert.deepEqual(a.cleanup.close,a.cleanup.exit);
  assert.equal(a.cleanup.wsClosed,true);assert.equal(a.cleanup.forced,false);
  assert.equal(a.cleanup.gracefulDeadline-a.cleanup.gracefulStartedAt,5000);
  const wsClose=a.cleanup.lifecycle.find(e=>e.kind==='ws-close'), signal=a.cleanup.lifecycle.find(e=>e.kind==='signal' && e.signal==='SIGINT'), exited=a.cleanup.lifecycle.find(e=>e.kind==='exit'), closed=a.cleanup.lifecycle.find(e=>e.kind==='close');
  assert.ok(wsClose && signal && exited && closed,'independent lifecycle receipts');
  assert.ok(wsClose.at<=signal.at && signal.at<=exited.at && exited.at<=closed.at);
  assert.ok(closed.at<=a.cleanup.gracefulDeadline,'original five-second graceful deadline');
  assert.ok(a.cleanup.groupObservations.some(o=>o.label==='final' && o.exists===false));
  assert.equal(a.cleanup.socketReleased, true); assert.equal(a.cleanup.groupGone, true);
  return { passed: true, profile: a.schema, mainRequests: 2, realReadCalls: 1 };
}
function sample() {
  const h = '0'.repeat(64), output = INPUT;
  const response = index => { const data = [{candidates:[{content:{role:'model',parts:[index === 0 ? {functionCall:CALL} : {text:ANSWER}]}}]},{candidates:[{finishReason:'STOP'}]}].map(v=>'data: '+JSON.stringify(v)+'\n\n').join(''); return {status:200,headers:{'content-type':'text/event-stream'},rawHeaders:'HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n',bodyBase64:Buffer.from(data).toString('base64'),sha256:sha(data)}; };
  const auditText = [0,1].map(index=>JSON.stringify({sessionId:'ra05-google-tool',stream:true,protocol:'google',model:'m',outcome:'success',responseComplete:true,requestBody:JSON.stringify(index===0?{}:{contents:[{parts:[{functionResponse:{id:CALL.id,name:CALL.name,response:{output}}}]}]}),responseBody:'{}'})).join('\n')+'\n';
  const body = { contents: [{role:'user',parts:[{text:PROMPT}]}], tools: [{ functionDeclarations: [{ name: 'read_file', parametersJsonSchema: { required: ['file_path'] } }] }] };
  return { schema: 'RA05-google-tool-v1', captureComplete: true, errors: [], identity: { engine: 'ts', selectedEntryMode:'source',selectedEntry:'/fixed/src/cli/pilotdeck.ts',selectedEntrySha256:h,node:{path:'/node',sha256:h},command:'/node',args:['--import','/tsx','/fixed/src/cli/pilotdeck.ts'], driverSha256: h, commandSha256: h, baselineCommit: 'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', sourceClosure: { 'src/cli/pilotdeck.ts': h } }, inputBeforeSha256: sha(INPUT), inputAfterSha256: sha(INPUT), audit:{files:[{path:'sessions/ra05-google-tool/llm/invocations.jsonl',text:auditText,sha256:sha(auditText)}]}, requests: [body, { contents: [{ role: 'model', parts: [{ functionCall: CALL }] }, { role: 'user', parts: [{ functionResponse: { id: CALL.id, name: CALL.name, response: { output } } }] }] }].map((body,index) => ({ response:response(index), purpose: 'turn', method: 'POST', path: '/v1beta/models/m:streamGenerateContent?alt=sse', apiKey: 'fixture-only', body })), frames: [{ id: 'turn', event: { type: 'tool_call_started', toolCallId: CALL.id, name: CALL.name } }, { id: 'turn', event: { type: 'tool_call_finished', toolCallId: CALL.id, ok: true } }, { id: 'turn', final: true, event: { type: 'turn_completed', finishReason: 'completed', usage: { inputTokens: 28, outputTokens: 5, totalTokens: 33 } } }], history: {messages:[{role:'tool',kind:'tool_use',toolCallId:CALL.id,toolName:CALL.name,payload:CALL.args},{role:'tool',kind:'tool_result',toolCallId:CALL.id,toolName:CALL.name,ok:true,text:output},{role:'assistant',kind:'text',text:ANSWER}]}, cleanup:{exit:{code:0,signal:null},close:{code:0,signal:null},socketReleased:true,groupGone:true,wsClosed:true,forced:false,gracefulStartedAt:1,gracefulDeadline:5001,lifecycle:[{kind:'ws-close',at:2},{kind:'signal',signal:'SIGINT',at:3},{kind:'exit',at:4},{kind:'close',at:5}],groupObservations:[{label:'final',exists:false}]}  };
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv[2] === '--self-test') {
    verify(sample());
    const built=sample(), h='0'.repeat(64);
    built.identity.selectedEntryMode='built'; built.identity.selectedEntry='/fixed/dist/src/cli/pilotdeck.js'; built.identity.args=[built.identity.selectedEntry,'server'];
    const files=[{path:'src/cli/pilotdeck.js',sha256:h,kind:'file'}];
    built.identity.baselineDist={sha256:h,expectedSha256:h,files,manifestSha256:sha(JSON.stringify(files))};
    verify(built);
    const badDist=structuredClone(built); badDist.identity.baselineDist.expectedSha256='1'.repeat(64); assert.throws(()=>verify(badDist));
    const corruptions = [a => a.requests.pop(), a => a.requests[1].body.contents[1].parts[0].functionResponse.id = 'wrong', a => a.requests[1].body.contents[1].parts[0].functionResponse.response.output = 'RA05-WRONG-SOURCE', a => a.frames[1].event.ok = false, a => a.frames[2].event.usage.inputTokens++, a => a.history.messages.splice(1,1), a => a.identity.sourceClosure = {}, a => a.cleanup.groupGone = false, a => a.requests[0].body.contents = [], a => a.history.messages[1].text += ' extra', a => a.audit.files = [], a => a.requests[0].response.bodyBase64 = Buffer.from('wrong').toString('base64'), a => a.identity.selectedEntryMode='built', a => a.identity.commandSha256='1'.repeat(64), a=>a.requests[0].response.rawHeaders='HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\r\n',a=>a.cleanup.forced=true,a=>a.cleanup.lifecycle[0].at=8,a=>a.cleanup.close.signal='SIGKILL',a=>a.history.messages[0].payload={file_path:'other'},a=>a.history.messages[1].ok=false];
    for (const corrupt of corruptions) { const a = sample(); corrupt(a); assert.throws(() => verify(a)); }
    console.log(JSON.stringify({ syntheticOnly: true, positive: 2, rejectedCorruptions: corruptions.length+1 }));
  } else console.log(JSON.stringify(verify(JSON.parse(readFileSync(process.argv[2], 'utf8')))));
}
