import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {resolve,join} from 'node:path';

const SOURCE='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85', CALL='call_j7_read', OUTPUT='1|J7-RESPONSES-FILE\n2|';
const one=(xs,label)=>{assert.equal(xs.length,1,`${label} must occur exactly once`);return xs[0]};
const terminalRun=c=>c.terminal?.event?.runId;
const read=(dir,name)=>JSON.parse(readFileSync(join(dir,name),'utf8'));
const hash=x=>createHash('sha256').update(x).digest('hex');

export function verifyCapture(c,identity) {
 assert.equal(c.sourceCommit,SOURCE);assert.equal(identity.sourceCommit,SOURCE);
 assert.equal(c.sourceSha256,identity.sourceSha256);assert.match(c.sourceSha256,/^[a-f0-9]{64}$/);
 assert.equal(c.driverSha256,identity.driverSha256);assert.match(c.driverSha256,/^[a-f0-9]{64}$/);
 assert.match(identity.commandSha256,/^[a-f0-9]{64}$/);assert.ok(['ts','rust'].includes(c.side));
 assert.equal(c.negative,false,'negative capture cannot satisfy the positive contract');
 assert.equal(identity.negative,false);assert.equal(c.failure,null);assert.deepEqual(c.errors,[]);assert.deepEqual(c.exit,{code:0,signal:null});
 assert.equal(c.frames.filter(f=>f.type==='hello_ok').length,1,'authenticated handshake');
 const streams=c.requests.filter(r=>r.body?.stream===true);assert.equal(streams.length,2,'exactly two actual model streams');
 for(const r of c.requests){assert.equal(r.method,'POST');assert.equal(r.path,'/v1/responses');assert.equal(r.authorization,'Bearer fixture-only');assert.match(r.contentType??'',/^application\/json(?:;|$)/);assert.deepEqual(JSON.parse(r.rawBody),r.body);}
 for(const r of c.requests.filter(r=>r.body?.stream!==true))assert.ok(JSON.stringify(r.body).includes('Generate a concise title'),'unexpected background request');
 for(const r of streams)assert.equal(r.body.model,'m','configured model');
 assert.ok(JSON.stringify(streams[0].body.input).includes('J7-RESPONSES-BEGIN'));
 assert.ok(streams[0].body.tools.some(t=>t?.name==='read_file'));
 assert.equal(streams[0].body.input.filter(x=>x?.type==='function_call_output'||x?.type==='function_call').length,0);
 const calls=streams[1].body.input.filter(x=>x?.type==='function_call');
 const outputs=streams[1].body.input.filter(x=>x?.type==='function_call_output');
 const call=one(calls,'continued function call'),output=one(outputs,'continued tool output');
 assert.equal(call.call_id,CALL);assert.equal(call.name,'read_file');assert.deepEqual(JSON.parse(call.arguments),{file_path:'j7-response.txt'});
 assert.ok(streams[1].body.input.indexOf(call)<streams[1].body.input.indexOf(output),'function call must precede its output');
 assert.equal(output.call_id,CALL);assert.equal(output.output,OUTPUT);
 assert.equal(c.responses.length,2);assert.deepEqual(c.responses.map(x=>x.kind),['tool','final']);
 const decoded=c.responses.map(response=>response.chunks.map(chunk=>{
  assert.equal(typeof chunk,'string');const m=/^event: ([^\n]+)\ndata: ([^\n]+)\n\n$/.exec(chunk);assert.ok(m,'exact SSE frame');
  const data=JSON.parse(m[2]);assert.equal(data.type,m[1]);return data;
 }));
 assert.deepEqual(decoded[0].map(x=>x.type),['response.created','response.output_item.added','response.function_call_arguments.delta','response.function_call_arguments.done','response.completed']);
 assert.deepEqual(decoded[1].map(x=>x.type),['response.created','response.output_text.delta','response.completed']);
 assert.deepEqual(decoded[0][0].response,{id:'resp_j7_1',status:'in_progress'});assert.deepEqual(decoded[1][0].response,{id:'resp_j7_2',status:'in_progress'});
 assert.equal(decoded[0][1].output_index,0);assert.equal(decoded[0][2].output_index,0);assert.equal(decoded[0][3].output_index,0);assert.equal(decoded[1][1].output_index,0);assert.equal(decoded[1][1].item_id,'msg_j7');
 assert.equal(decoded[0][1].item?.type,'function_call');assert.equal(decoded[0][1].item?.id,'fc_j7');assert.equal(decoded[0][1].item?.call_id,CALL);assert.equal(decoded[0][1].item?.name,'read_file');
 assert.equal(decoded[0][2].item_id,'fc_j7');assert.deepEqual(JSON.parse(decoded[0][2].delta),{file_path:'j7-response.txt'});assert.equal(decoded[0][3].item_id,'fc_j7');assert.deepEqual(JSON.parse(decoded[0][3].arguments),{file_path:'j7-response.txt'});
 assert.equal(decoded[1][1].delta,'J7-RESPONSES-FINAL');
 for(const [i,events] of decoded.entries())assert.deepEqual(events.at(-1).response,{id:`resp_j7_${i+1}`,status:'completed',usage:{input_tokens:20,output_tokens:9,total_tokens:29}});
 const turn=c.frames.filter(f=>f.id==='turn'&&f.type==='event');assert.ok(turn.length>0);
 for(let i=1;i<turn.length;i++)assert.ok(Number.isSafeInteger(turn[i].seq)&&turn[i].seq>turn[i-1].seq,'turn sequence must increase');
 const started=one(turn.filter(f=>f.event?.type==='tool_call_started'&&f.event.toolCallId===CALL),'tool start');
 const finished=one(turn.filter(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId===CALL),'tool finish');
 const deltas=turn.filter(f=>f.event?.type==='assistant_text_delta');assert.ok(deltas.length>0,'actual streamed final text required');
 assert.ok(deltas.every(f=>f.event.runId===terminalRun(c)),'final deltas belong to terminal run');
 assert.equal(deltas.map(f=>f.event.text).join(''),'J7-RESPONSES-FINAL');
 assert.ok(deltas.every(f=>f.seq>finished.seq),'final text follows real tool result');
 assert.equal(started.event.name,'read_file');assert.deepEqual(JSON.parse(started.event.argsPreview),{file_path:'j7-response.txt'});
 assert.equal(finished.event.toolName,'read_file');assert.equal(finished.event.ok,true);assert.equal(finished.event.resultPreview,OUTPUT);
 const terminal=one(turn.filter(f=>f.final===true),'terminal');assert.deepEqual(c.terminal,terminal);
 assert.equal(terminal.event?.type,'turn_completed');assert.equal(terminal.event.finishReason,'completed');
 assert.ok(deltas.every(f=>f.seq<terminal.seq),'final text must precede terminal');
 assert.ok(started.seq<finished.seq&&finished.seq<terminal.seq,'tool must settle before final');
 const run=terminal.event.runId;assert.equal(typeof run,'string');assert.ok(run.length>0);assert.equal(started.event.runId,run);assert.equal(finished.event.runId,run);
 assert.deepEqual(terminal.event.usage,{inputTokens:40,outputTokens:18,totalTokens:58});
 assert.equal(c.history?.type,'response');assert.equal(c.history.id,'history');assert.equal(c.history.ok,true);
 assert.deepEqual(c.history,one(c.frames.filter(f=>f.id==='history'&&f.type==='response'),'history RPC')); 
 const messages=c.history.result?.messages;assert.ok(Array.isArray(messages));
 const use=one(messages.filter(m=>m.kind==='tool_use'&&m.toolCallId===CALL),'history tool call');
 const result=one(messages.filter(m=>m.kind==='tool_result'&&m.toolCallId===CALL),'history tool result');
 assert.equal(use.toolName,'read_file');assert.deepEqual(use.payload,{file_path:'j7-response.txt'});
 assert.equal(result.toolName,'read_file');assert.equal(result.ok,true);assert.equal(result.text,OUTPUT);
 assert.equal(use.sessionKey,'j7-responses');assert.equal(result.sessionKey,'j7-responses');assert.equal(use.turnId,run);assert.equal(result.turnId,run);
 assert.ok(use.sequence<result.sequence);assert.ok(Date.parse(use.createdAt)<=Date.parse(result.createdAt));
 const answer=one(messages.filter(m=>m.role==='assistant'&&m.text==='J7-RESPONSES-FINAL'),'history final answer');
 assert.equal(answer.sessionKey,'j7-responses');assert.equal(answer.turnId,run);assert.ok(answer.sequence>result.sequence);assert.ok(Date.parse(answer.createdAt)>=Date.parse(result.createdAt));
 return {endpoint:'/v1/responses',streamCount:2,callId:CALL,toolName:'read_file',arguments:{file_path:'j7-response.txt'},output:OUTPUT,final:'J7-RESPONSES-FINAL',usage:terminal.event.usage,history:['tool_use','tool_result','assistant'],exit:c.exit};
}

if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)){
 const dirs=process.argv.slice(2);assert.ok(dirs.length===1||dirs.length===2,'usage: node verify.mjs CAPTURE_DIR [CAPTURE_DIR]');
 const contracts=dirs.map(dir=>{const c=read(dir,'capture.json'),identity=read(dir,'identity.json');assert.equal(hash(readFileSync(join(dir,'provider.jsonl'))),hash(Buffer.from(c.requests.map(x=>JSON.stringify(x)+'\n').join(''))),'provider raw capture');assert.equal(hash(readFileSync(join(dir,'gateway.jsonl'))),hash(Buffer.from(c.frames.map(x=>JSON.stringify(x)+'\n').join(''))),'gateway raw capture');return verifyCapture(c,identity)});
 if(contracts.length===2)assert.deepEqual(contracts[0],contracts[1],'stable business contract differs');
 console.log(JSON.stringify({passed:true,captures:dirs.length,contract:contracts[0]}));
}
