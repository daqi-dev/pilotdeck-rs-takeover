// Isolated real CLI server -> authenticated WS -> real tool -> Responses HTTP.
// This harness never calls a real provider; its source and captures are retained.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync, appendFileSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bindBaseline } from '../_shared/generator_guard.mjs';
import { sourceClosureSha256 } from '../../baseline_source_guard.mjs';

assert.ok(process.env.PDRS_BASELINE, 'PDRS_BASELINE is required');
assert.ok(process.argv[2], 'a new output directory is required');
const baseline=resolve(process.env.PDRS_BASELINE), out=resolve(process.argv[2]);
assert.ok(!existsSync(out), 'refuse to overwrite prior evidence');
mkdirSync(out,{recursive:true});
const sourceCommit='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const {gitExecutable}=bindBaseline({baseline,commit:sourceCommit});
const sourceSha256=sourceClosureSha256(baseline,sourceCommit,gitExecutable,['src/cli/pilotdeck.ts']);
const hash=x=>createHash('sha256').update(x).digest('hex');
const json=x=>JSON.stringify(x,null,2)+'\n';
const pause=ms=>new Promise(done=>setTimeout(done,ms));
const home=join(out,'home'),pilot=join(home,'.pilotdeck'),project=join(out,'project'),tmp=join(out,'tmp');
for(const dir of [pilot,project,tmp])mkdirSync(dir,{recursive:true});
const marker='J7-RESPONSES-FILE', expectedOutput=`1|${marker}\n2|`;
// Negative control changes the real file consumed by read_file, not capture data.
writeFileSync(join(project,'j7-response.txt'),(process.env.J7_WRONG_TOOL_OUTPUT?'J7-WRONG-FILE':marker)+'\n');
const requests=[],frames=[],errors=[],responses=[];
const event=(type,extra={})=>`event: ${type}\ndata: ${JSON.stringify({type,...extra})}\n\n`;
const usage={input_tokens:20,output_tokens:9,total_tokens:29};
const mock=createServer(async(req,res)=>{
 try {
  const parts=[];for await(const part of req)parts.push(part);
  const raw=Buffer.concat(parts).toString('utf8');let body;
  try{body=JSON.parse(raw);}catch{throw Error('provider request body is not JSON')}
  const record={method:req.method,path:req.url,authorization:req.headers.authorization??null,contentType:req.headers['content-type']??null,rawBody:raw,body};
  requests.push(record);appendFileSync(join(out,'provider.jsonl'),JSON.stringify(record)+'\n');
  assert.equal(req.method,'POST');assert.equal(req.url,'/v1/responses');assert.equal(req.headers.authorization,'Bearer fixture-only');
  if(body.stream!==true){
   assert.ok(JSON.stringify(body).includes('Generate a concise title'),'unexpected non-stream request');
   res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({id:'resp_title',status:'completed',output_text:'{"title":"Responses formal"}',output:[],usage}));return;
  }
  assert.equal(body.model,'m','configured model');assert.ok(Array.isArray(body.input),'Responses input must be an array');
  const toolOutputs=body.input.filter(item=>item?.type==='function_call_output');
  let chunks;
  if(toolOutputs.length===0){
   assert.equal(requests.filter(r=>r.body?.stream===true).length,1,'duplicate initial stream');
   assert.ok(JSON.stringify(body.input).includes('J7-RESPONSES-BEGIN'),'initial user prompt');
   assert.equal(body.input.filter(item=>item?.type==='function_call').length,0,'no prior function call');
   assert.ok(Array.isArray(body.tools)&&body.tools.some(x=>x?.name==='read_file'),'real read_file tool schema');
   const args=JSON.stringify({file_path:'j7-response.txt'});
   chunks=[event('response.created',{response:{id:'resp_j7_1',status:'in_progress'}}),event('response.output_item.added',{output_index:0,item:{type:'function_call',id:'fc_j7',call_id:'call_j7_read',name:'read_file',arguments:''}}),event('response.function_call_arguments.delta',{item_id:'fc_j7',output_index:0,delta:args}),event('response.function_call_arguments.done',{item_id:'fc_j7',output_index:0,arguments:args}),event('response.completed',{response:{id:'resp_j7_1',status:'completed',usage}})];
  } else {
   assert.equal(requests.filter(r=>r.body?.stream===true).length,2,'unexpected stream count');
   assert.equal(toolOutputs.length,1,'one tool output');assert.equal(toolOutputs[0].call_id,'call_j7_read');
   assert.equal(toolOutputs[0].output,expectedOutput,'actual read_file output mismatch');
   const calls=body.input.filter(item=>item?.type==='function_call');
   assert.equal(calls.length,1);assert.equal(calls[0].call_id,'call_j7_read');assert.ok(body.input.indexOf(calls[0])<body.input.indexOf(toolOutputs[0]),'tool call precedes result');assert.equal(calls[0].name,'read_file');assert.deepEqual(JSON.parse(calls[0].arguments),{file_path:'j7-response.txt'});
   chunks=[event('response.created',{response:{id:'resp_j7_2',status:'in_progress'}}),event('response.output_text.delta',{item_id:'msg_j7',output_index:0,delta:'J7-RESPONSES-FINAL'}),event('response.completed',{response:{id:'resp_j7_2',status:'completed',usage}})];
  }
  responses.push({kind:toolOutputs.length?'final':'tool',chunks});
  appendFileSync(join(out,'mock-responses.jsonl'),JSON.stringify(responses.at(-1))+'\n');
  res.writeHead(200,{'content-type':'text/event-stream'});for(const chunk of chunks)res.write(chunk);res.end();
 } catch(error){errors.push(String(error));appendFileSync(join(out,'mock-errors.jsonl'),JSON.stringify({error:String(error)})+'\n');if(!res.headersSent)res.writeHead(422,{'content-type':'application/json'});res.end(JSON.stringify({error:{message:'J7 mock rejected request'}}));}
});
await new Promise(resolve=>mock.listen(0,'127.0.0.1',resolve));
const origin=`http://127.0.0.1:${mock.address().port}`;
const config=`schemaVersion: 1\nagent:\n  model: p/m\n  maxContextTokens: 16000\n  maxOutputTokens: 4096\nmodel:\n  providers:\n    p:\n      protocol: openai-responses\n      url: ${origin}/v1\n      apiKey: fixture-only\n      retry:\n        requestMaxRetries: 0\n        streamMaxRetries: 0\n      models:\n        m:\n          capabilities:\n            supportsToolUse: true\n            supportsStreaming: true\n            supportsSystemPrompt: true\n            maxContextTokens: 16000\n            maxOutputTokens: 4096\ntelemetry:\n  enabled: false\n`;
writeFileSync(join(pilot,'pilotdeck.yaml'),config);
const env={PATH:process.env.PATH,HOME:home,USERPROFILE:home,CFFIXED_USER_HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:join(pilot,'pilotdeck.yaml'),TMPDIR:tmp,LANG:'en_US.UTF-8',LC_ALL:'en_US.UTF-8',TZ:'UTC',NO_COLOR:'1',NODE_NO_WARNINGS:'1'};
const command=process.env.M1_BINARY?resolve(process.env.M1_BINARY):process.execPath;
const args=process.env.M1_BINARY?['server','--host','127.0.0.1','--port','0']:['--import',join(baseline,'node_modules/tsx/dist/loader.mjs'),join(baseline,'src/cli/pilotdeck.ts'),'server','--host','127.0.0.1','--port','0'];
writeFileSync(join(out,'identity.json'),json({sourceCommit,sourceSha256,command,args,cwd:project,env,driverSha256:hash(readFileSync(fileURLToPath(import.meta.url))),commandSha256:hash(readFileSync(command)),negative:!!process.env.J7_WRONG_TOOL_OUTPUT}));
const child=spawn(command,args,{cwd:project,env,stdio:['ignore','pipe','pipe']});
let stdout='',stderr='',exit=null,spawnError=null,socket,history=null,terminal=null,failure=null;
child.once('error',error=>{spawnError=String(error)});
child.stdout.on('data',x=>stdout+=x);child.stderr.on('data',x=>stderr+=x);
const ended=new Promise(done=>child.once('close',(code,signal)=>{exit={code,signal};done()}));
async function until(fn,label,ms=30000){const end=Date.now()+ms;while(!fn()){if(spawnError)throw Error(`${label}: spawn failed ${spawnError}`);if(exit)throw Error(`${label}: server exited ${JSON.stringify(exit)}`);if(Date.now()>=end)throw Error(`deadline: ${label}`);await pause(10)}}
try{
 await until(()=>stdout.includes('WebSocket: ws://'),'server startup');
 const {WebSocket}=createRequire(join(baseline,'package.json'))('ws');socket=new WebSocket(stdout.match(/WebSocket: (ws:\/\/[^\s]+)/)[1]);
 socket.on('message',data=>{try{const frame=JSON.parse(data.toString());frames.push(frame);appendFileSync(join(out,'gateway.jsonl'),JSON.stringify(frame)+'\n');if(frame.id==='turn'&&frame.event?.type==='permission_request'){socket.send(JSON.stringify({type:'request',id:'permission',method:'permission_decide',params:{sessionKey:'j7-responses',requestId:frame.event.requestId,decision:'allow'}}));}}catch(e){errors.push(String(e))}});
 await new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('deadline: WebSocket open')),10000);socket.once('open',()=>{clearTimeout(timer);resolve()});socket.once('error',error=>{clearTimeout(timer);reject(error)});});
 socket.send(JSON.stringify({type:'hello',protocolVersion:'1.2',token:readFileSync(join(pilot,'server-token'),'utf8').trim(),clientName:'cli',clientVersion:'0.1.0'}));
 await until(()=>frames.some(f=>f.type==='hello_ok'),'authenticated handshake');
 socket.send(JSON.stringify({type:'request',id:'turn',method:'submit_turn',params:{sessionKey:'j7-responses',channelKey:'test',message:'J7-RESPONSES-BEGIN'}}));
 await until(()=>frames.some(f=>f.id==='turn'&&f.final),'turn terminal');terminal=frames.find(f=>f.id==='turn'&&f.final);
 socket.send(JSON.stringify({type:'request',id:'history',method:'read_session_messages',params:{sessionKey:'j7-responses'}}));
 await until(()=>frames.some(f=>f.id==='history'&&f.type==='response'),'read history');history=frames.find(f=>f.id==='history'&&f.type==='response');
}catch(error){failure=String(error)}finally{
 socket?.close();if(!exit&&!spawnError&&child.pid)child.kill('SIGINT');await Promise.race([ended,pause(10000)]);if(!exit&&!spawnError&&child.pid){child.kill('SIGKILL');await Promise.race([ended,pause(10000)])}mock.closeAllConnections();await new Promise(done=>mock.close(done));
 writeFileSync(join(out,'stdout.log'),stdout);writeFileSync(join(out,'stderr.log'),stderr);
 writeFileSync(join(out,'capture.json'),json({sourceCommit,sourceSha256,side:process.env.M1_BINARY?'rust':'ts',negative:!!process.env.J7_WRONG_TOOL_OUTPUT,requests,responses,frames,history,terminal,errors,failure,spawnError,exit,driverSha256:hash(readFileSync(fileURLToPath(import.meta.url)))}));
}
if(failure)throw Error(failure);
