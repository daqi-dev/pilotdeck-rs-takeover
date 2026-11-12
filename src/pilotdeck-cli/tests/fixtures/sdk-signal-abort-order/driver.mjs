// Diagnostic only: real fixed CLI + exact Gateway protocol mock.
// Never patches the baseline and never binds the host's fixed gateway port.
import {createRequire} from 'node:module';
import {spawn} from 'node:child_process';
import {mkdir,writeFile,readFile,readdir} from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
const baseline=process.env.H1_BASELINE;
const output=process.env.H1_DIAGNOSTIC_OUTPUT;
if(!baseline||!output||process.env.H1_ISOLATED_LOCAL!=='1'||!process.env.H1_CLI_BIN)throw Error('explicit isolated sealed Rust diagnostic environment required');
const require=createRequire(path.join(baseline,'package.json'));
let ws;
try{ws=require('ws');}catch{
 const packages=await readdir(path.join(baseline,'node_modules/.pnpm'));
 const candidate=packages.find(name=>name.startsWith('ws@'));
 if(!candidate)throw Error('baseline ws source dependency unavailable');
 ws=require(path.join(baseline,'node_modules/.pnpm',candidate,'node_modules/ws'));
}
await mkdir(output,{recursive:true});
const executable=path.resolve(process.env.H1_CLI_BIN);
const identity={node:process.version,platform:process.platform,arch:process.arch,driverSha256:createHash('sha256').update(await readFile(fileURLToPath(import.meta.url))).digest('hex'),scope:'sealed Rust formal CLI, exact controlled mock Gateway; no provider or durable gateway',cli:executable,cliSha256:createHash('sha256').update(await readFile(executable)).digest('hex'),sdkTransportSha256:createHash('sha256').update(await readFile(path.join(baseline,'packages/sdk/dist/transport.js'))).digest('hex'),sdkClientSha256:createHash('sha256').update(await readFile(path.join(baseline,'packages/sdk/dist/client.js'))).digest('hex')};
const summaries=[];
for(const mode of (process.env.H1_CASES?.split(',')??['final-and-abort-batch','final-and-abort-next-turn','final-and-abort-separated','wrong-run-response','malformed-response','abort-rpc-error','connection-drop','abort-timeout'])){
 const run=path.join(output,mode);await mkdir(path.join(run,'home'),{recursive:true});await mkdir(path.join(run,'pilot'),{recursive:true});await mkdir(path.join(run,'project'),{recursive:true});
 const began=process.hrtime.bigint();const timeline=[];let counter=0;
 const record=(kind,data={})=>timeline.push({seq:counter++,ns:String(process.hrtime.bigint()-began),kind,...data});
 const server=new ws.WebSocketServer({host:'127.0.0.1',port:0});await new Promise(resolve=>server.once('listening',resolve));
 const url=`ws://127.0.0.1:${server.address().port}/diagnostic`;record('mock_listening',{url});
 let conn=0,child,submit,abort,abortSocket,signaled=false,stdout='',stderr='',scheduled=[];const sockets=new Set();
 const send=(socket,frame,label)=>{record('gateway_to_cli',{connection:socket.connection,label,readyState:socket.readyState,frame});if(socket.readyState===ws.WebSocket.OPEN)socket.send(JSON.stringify(frame),error=>record('send_callback',{label,error:error?.message??null}));else record('delivery_prevented',{label,readyState:socket.readyState});};
 const finishSubmit=()=>send(submit.socket,{type:'response',id:submit.id,ok:true,result:{}},'submit_final_response');
 const finishAbort=()=>send(abortSocket,{type:'response',id:abort.id,ok:true,result:{aborted:true}},'abort_turn_response');
 server.on('connection',socket=>{socket.connection=++conn;sockets.add(socket);record('ws_open',{connection:conn});socket.on('close',(code,reason)=>{record('ws_close',{connection:socket.connection,code,reason:String(reason)});});socket.on('error',error=>record('ws_error',{message:error.message}));socket.on('message',bytes=>{
  const raw=String(bytes);record('cli_to_gateway',{connection:socket.connection,raw});const frame=JSON.parse(raw);
  if(frame.type==='hello'){send(socket,{type:'hello_ok',protocolVersion:frame.protocolVersion,serverVersion:'fixed-protocol-mock',serverInfo:{mode:'remote',projectKey:path.join(run,'project'),capabilities:[]}},'hello_ok');return;}
  if(frame.type!=='request')throw Error(`unexpected frame ${raw}`);
  if(frame.method==='resume_session'){send(socket,{type:'response',id:frame.id,ok:true,result:{sessionKey:'diagnostic-session'}},'resume_session_response');return;}
  if(frame.method==='submit_turn'){
   if(submit)throw Error('duplicate submit');submit={socket,id:frame.id};
   send(socket,{type:'event',id:frame.id,seq:0,final:false,event:{type:'turn_started',runId:frame.params.runId}},'turn_started');
   send(socket,{type:'event',id:frame.id,seq:1,final:false,event:{type:'assistant_text_delta',text:'C1 partial',runId:frame.params.runId}},'partial_delta');
   send(socket,{type:'event',id:frame.id,seq:2,final:false,event:{type:'assistant_block',kind:'text',streamState:'closed',text:'C1 partial',runId:frame.params.runId}},'partial_visible');return;
  }
  if(frame.method==='abort_turn'){
   abort=frame;abortSocket=socket;record('abort_gate_reached',{mode});
   if(mode==='final-and-abort-batch'){submit.socket._socket.cork();record('diagnostic_tcp_batch_begin');}
   send(submit.socket,{type:'event',id:submit.id,seq:3,final:false,event:{type:'turn_completed',usage:{},finishReason:'aborted_streaming',runId:frame.params.runId}},'turn_completed_event');
   if(mode==='wrong-run-response'){
    finishSubmit();
    send(abortSocket,{type:'response',id:'wrong-run-abort-id',ok:true,result:{aborted:true}},'wrong_run_response');
    scheduled.push(setTimeout(()=>{record('wrong_run_response_barrier',{childExitCode:child.exitCode,childSignalCode:child.signalCode});finishAbort();},100));
   }else if(mode==='malformed-response'){
    record('gateway_to_cli_raw',{connection:socket.connection,raw:'{malformed',label:'malformed_abort_response'});socket.send('{malformed');
   }else if(mode==='abort-rpc-error'){
    send(abortSocket,{type:'response',id:abort.id,ok:false,error:{code:'fixture_abort_error',message:'fixture abort refused'}},'abort_rpc_error');
   }else if(mode==='connection-drop'){
    record('mock_connection_drop',{connection:socket.connection});socket.terminate();
   }else if(mode==='abort-timeout'){
    record('abort_rpc_deliberately_unanswered');
   }else if(mode==='final-and-abort-batch'){
    finishSubmit();finishAbort();submit.socket._socket.uncork();record('diagnostic_tcp_batch_end');
   }else if(mode==='final-and-abort-next-turn'){
    finishSubmit();setImmediate(()=>{record('diagnostic_next_event_loop_turn');finishAbort();});
   }else{
    finishSubmit();
    // Diagnostic delivery gate, not a change to the formal cancellation wait.
    // Release only once the client has closed its observation transport.
    const release=()=>{record('final_first_release_after_client_close');finishAbort();};
    if(socket.readyState===ws.WebSocket.OPEN)socket.once('close',release);else release();
    scheduled.push(setTimeout(()=>{if(socket.readyState===ws.WebSocket.OPEN){record('diagnostic_held_response_release',{childExitCode:child.exitCode,childSignalCode:child.signalCode});finishAbort();}},100));
   }
   return;
  }
  send(socket,{type:'response',id:frame.id,ok:false,error:{code:'unexpected_mock_method',message:frame.method}},'unexpected_method');
 });});
 const env={...process.env,HOME:path.join(run,'home'),PILOT_HOME:path.join(run,'pilot'),PILOTDECK_TELEMETRY_DISABLED:'1',NO_PROXY:'127.0.0.1,localhost',no_proxy:'127.0.0.1,localhost'};
 for(const key of ['HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy','NODE_OPTIONS'])delete env[key];
 const argv=[executable,'resume','diagnostic-session','--gateway-url',url,'--auth-token','diagnostic-token','--project',path.join(run,'project'),'--stream-json','C1-CANCEL'];
 child=spawn(executable,argv.slice(1),{cwd:path.join(run,'project'),env,stdio:['ignore','pipe','pipe']});record('cli_spawn',{pid:child.pid,argv});
 child.stdout.on('data',chunk=>{const text=String(chunk);stdout+=text;record('cli_stdout',{text});if(!signaled&&stdout.includes('pilotdeck.assistant_block')&&stdout.includes('C1 partial')){signaled=true;record('signal_sent',{signal:'SIGINT',pid:child.pid,delivered:child.kill('SIGINT')});}});
 child.stderr.on('data',chunk=>{const text=String(chunk);stderr+=text;record('cli_stderr',{text});});
 const deadline=setTimeout(()=>{record('diagnostic_child_timeout',{pid:child.pid});child.kill('SIGKILL');},12000);
 const result=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('close',(code,signal)=>resolve({code,signal}));});
 clearTimeout(deadline);scheduled.forEach(clearTimeout);record('cli_exit',result);
 sockets.forEach(socket=>socket.terminate());await new Promise(resolve=>server.close(resolve));
 await writeFile(path.join(run,'timeline.jsonl'),timeline.map(item=>JSON.stringify(item)).join('\n')+'\n');await writeFile(path.join(run,'stdout.txt'),stdout);await writeFile(path.join(run,'stderr.txt'),stderr);
 const summary={mode,...result,signaled,abortObserved:Boolean(abort),submitObserved:Boolean(submit),unhandledClosed:stderr.includes('PilotDeckError: Gateway client closed.'),gatewayClosedError:stderr.includes('Gateway client closed.'),canonicalAbort:stdout.trimEnd().split('\n').at(-1)==='{"status":"aborted","reason":"CLI interrupted"}',stdoutLastLines:stdout.trimEnd().split('\n').slice(-3)};summaries.push(summary);await writeFile(path.join(run,'summary.json'),JSON.stringify(summary,null,2)+'\n');
}
await writeFile(path.join(output,'identity.json'),JSON.stringify(identity,null,2)+'\n');await writeFile(path.join(output,'summary.json'),JSON.stringify(summaries,null,2)+'\n');
console.log(JSON.stringify({output,summaries},null,2));
if(summaries.some(s=>!s.signaled||!s.abortObserved||!s.submitObserved||s.signal))process.exitCode=2;
