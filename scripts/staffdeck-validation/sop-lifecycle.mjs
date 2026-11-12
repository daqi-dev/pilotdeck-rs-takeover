#!/usr/bin/env node
// Formal SOP cancellation/failure and same-session recovery, using only local
// deterministic services and a fresh isolated HOME for each executable.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import assert from 'node:assert/strict';
import { validationIdentity } from './identity.mjs';
const args=process.argv.slice(2),arg=key=>args[args.indexOf(key)+1];
if(!args.includes('--baseline')||!args.includes('--out'))throw new Error('--baseline CHECKOUT --out NEW_DIR [--case cancel_prepare|cancel_model|owner_error] [--mode new_session|inherit] [--self] [--rust BINARY]');
const scenario=args.includes('--case')?arg('--case'):'cancel_model';
const mode=args.includes('--mode')?arg('--mode'):'new_session';
assert(['cancel_prepare','cancel_model','owner_error'].includes(scenario));assert(['new_session','inherit'].includes(mode));
const baseline=path.resolve(arg('--baseline')),out=path.resolve(arg('--out')),rust=args.includes('--rust')?path.resolve(arg('--rust')):null;
assert(!fs.existsSync(out),'output directory must be new');fs.mkdirSync(out,{recursive:true});
const root=path.join(out,'fixture'),home=path.join(root,'home'),pilot=path.join(home,'.pilotdeck'),project=path.join(root,'project');
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
let phase='first',requests=[],held=[];
const server=http.createServer(async(req,res)=>{
 let raw='';for await(const part of req)raw+=part;
 let body;try{body=JSON.parse(raw);}catch{body=null;}
 const route=new URL(req.url,'http://localhost').pathname;
 const record={phase,method:req.method,path:route,body};requests.push(record);
 const send=value=>{res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(value));};
 const hold=()=>{const pending={phase,path:route,closed:false};held.push(pending);res.on('close',()=>{pending.closed=true;});};
 if(route==='/healthz')return send({status:'ok',protocolVersion:'2.0',moduleId:'sop.runtime',contract:'sop.lifecycle/v2',operations:['prepare','submit']});
 if(route==='/v1/sop/prepare'){
  if(phase==='first'&&scenario==='cancel_prepare'){hold();return;}
  if(phase==='first'&&scenario==='owner_error')return send({protocolVersion:'2.0',requestId:body.requestId,ok:false,outcome:'failed',error:{code:'SOP_TEST_OWNER_REJECTED',message:'Fixture owner rejected preparation.',retryability:'unsafe',details:{fixture:true}}});
  const state={...body.payload.state,active_skill_id:'sop',active_step_id:'n1',status:'active',successful_tool_names:[]};
  return send({protocolVersion:'2.0',requestId:body.requestId,ok:true,outcome:'completed',payload:{state,step:{skillId:'sop',skillName:'One step',version:'1',nodeId:'n1',node:{type:'response'},instruction:'Complete only after a successful tool receipt.',expectedUserInfo:[],knownSlots:state.slots_json??{},allowedNextStepIds:[],requiredToolNames:[],allowedActions:[],isTerminal:true,declaresHandoff:false,contextMode:mode}}});
 }
 if(route==='/v1/sop/submit')return send({protocolVersion:'2.0',requestId:body.requestId,ok:true,outcome:'completed',payload:{state:{...body.payload.state,status:'completed'},result:{...body.payload.proposal,events:[]}}});
 if(route==='/v1/chat/completions'){
  const title=JSON.stringify(body.messages).includes('Generate a concise title');
  if(phase==='first'&&scenario==='cancel_model'&&!title){
   res.writeHead(200,{'content-type':'text/event-stream'});res.write(`data: ${JSON.stringify({choices:[{index:0,delta:{role:'assistant',content:'Uncommitted draft.'},finish_reason:null}]})}\n\n`);hold();return;
  }
  const calls=title?[]:[{id:'call-final',type:'function',function:{name:'submit_step_result',arguments:JSON.stringify({status:'completed',replyFragment:'Recovered SOP final answer.',slotUpdates:{}})}}];
  const content=title?'{"title":"Lifecycle"}':null,usage={prompt_tokens:10,completion_tokens:5,total_tokens:15};
  if(!body.stream)return send({id:'fixture',model:body.model,choices:[{index:0,message:{role:'assistant',content,...(calls.length?{tool_calls:calls}:{})},finish_reason:calls.length?'tool_calls':'stop'}],usage});
  res.writeHead(200,{'content-type':'text/event-stream'});
  res.write(`data: ${JSON.stringify({choices:[{index:0,delta:{role:'assistant',...(content?{content}:{}),...(calls.length?{tool_calls:calls.map((call,index)=>({index,...call}))}:{})},finish_reason:null}]})}\n\n`);
  res.end(`data: ${JSON.stringify({choices:[{index:0,delta:{},finish_reason:calls.length?'tool_calls':'stop'}],usage})}\n\ndata: [DONE]\n\n`);return;
 }
 res.writeHead(500);res.end('{"error":"Unexpected fixture route"}');
});
server.listen(0,'127.0.0.1');await once(server,'listening');const endpoint=`http://127.0.0.1:${server.address().port}`;
const processes=[];
function state(){const file=path.join(pilot,'sop','sessions',Buffer.from('lifecycle-session').toString('base64url')+'.json');return fs.existsSync(file)?JSON.parse(fs.readFileSync(file)):null;}
function transcripts(){const result={};const walk=dir=>{if(!fs.existsSync(dir))return;for(const entry of fs.readdirSync(dir,{withFileTypes:true})){const file=path.join(dir,entry.name);if(entry.isDirectory())walk(file);else if(entry.name.endsWith('.jsonl'))result[path.relative(pilot,file)]=fs.readFileSync(file,'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);}};walk(path.join(pilot,'projects'));return result;}
function snapshotState(value){return value?{schemaVersion:value.schemaVersion,state:value.state,revision:value.revision,replyDelivery:value.replyDelivery??null}:null;}
function terminal(frames,id){return frames.find(f=>f.id===id&&(f.type==='response'||f.final));}
async function run(side,command,argv){
 fs.rmSync(root,{recursive:true,force:true});fs.mkdirSync(pilot,{recursive:true});fs.mkdirSync(project,{recursive:true});
 phase='first';requests=[];held=[];
 fs.writeFileSync(path.join(pilot,'definitions.json'),JSON.stringify({sops:[{id:'sop',version:'1',name:'One step',content:{nodes:[{node_id:'n1',type:'response',contextMode:mode}]}}]}));
 fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),`schemaVersion: 1\nagent:\n  model: mock/deterministic\n  maxTurns: 6\nmodel:\n  providers:\n    mock:\n      protocol: openai\n      url: ${endpoint}/v1\n      apiKey: mock-only\n      models:\n        deterministic: {}\nmodules:\n  sop:\n    enabled: true\n    provider: staffdeck\n    endpoint: ${endpoint}\n    timeoutMs: 12000\n    definitionsPath: '${path.join(pilot,'definitions.json')}'\n    defaultSopId: sop\n    contextMode: ${mode}\ntelemetry:\n  enabled: false\n`);
 const reserve=net.createServer();reserve.listen(0,'127.0.0.1');await once(reserve,'listening');const port=reserve.address().port;await new Promise(resolve=>reserve.close(resolve));assert.notEqual(port,18789);
 const child=spawn(command,[...argv,'server','--host','127.0.0.1','--port',String(port)],{cwd:project,env:{PATH:process.env.PATH,HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),PILOTDECK_GATEWAY_TOKEN:'lifecycle-test',DOCKER_CONTAINER:'1',COMMIT_HASH:'lifecycle-test'},stdio:['ignore','pipe','pipe']});
 processes.push({side,pid:child.pid,port});fs.writeFileSync(path.join(out,'processes.json'),JSON.stringify(processes,null,2));
 const logs=[];child.stdout.on('data',v=>logs.push(v));child.stderr.on('data',v=>logs.push(v));let ended;const done=new Promise(resolve=>child.once('exit',(code,signal)=>{ended={code,signal};resolve(ended);}));
 let ws;const frames=[];
 const wait=async(predicate,label)=>{const end=Date.now()+30000;for(;;){const value=predicate();if(value)return value;if(ended)throw new Error(`${side} exited ${JSON.stringify(ended)}`);if(Date.now()>end)throw new Error(`${side} timeout ${label}`);await sleep(15);}};
 const request=(id,method,params)=>ws.send(JSON.stringify({type:'request',id,method,params}));
 try{
  const deadline=Date.now()+45000;for(;;){if(ended)throw new Error(`startup exited ${JSON.stringify(ended)}`);try{const socket=net.connect(port,'127.0.0.1');await Promise.race([once(socket,'connect'),once(socket,'error').then(([e])=>Promise.reject(e))]);socket.destroy();break;}catch{if(Date.now()>deadline)throw new Error('startup timeout');await sleep(50);}}
  ws=new WebSocket(`ws://127.0.0.1:${port}/ws`);ws.onmessage=e=>frames.push(JSON.parse(e.data));await once(ws,'open');ws.send(JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'test',clientVersion:'0.0.0',token:'lifecycle-test'}));await wait(()=>frames.find(f=>f.type==='hello_ok'),'hello');
  request('first','submit_turn',{sessionKey:'lifecycle-session',channelKey:'test',runId:'lifecycle-first',message:'First attempt.'});
  if(scenario!=='owner_error'){
   await wait(()=>held.length>0,'held owner/model request');
   request('wrong-abort','abort_turn',{sessionKey:'lifecycle-session',runId:'different-run'});await wait(()=>terminal(frames,'wrong-abort'),'wrong abort response');
   assert.equal(held[0].closed,false,'wrong run must not close the request');assert(!terminal(frames,'first'),'wrong run must not complete the turn');
   request('abort','abort_turn',{sessionKey:'lifecycle-session',runId:'lifecycle-first'});await wait(()=>terminal(frames,'abort'),'abort response');
  }
  await wait(()=>terminal(frames,'first'),'first terminal');
  if(scenario!=='owner_error')await wait(()=>held[0].closed,'cancelled HTTP request closes');
  const firstState=state(),firstTranscripts=transcripts(),firstRequests=requests.slice();
  fs.writeFileSync(path.join(out,`${side}-first.json`),JSON.stringify({frames:frames.slice(),state:firstState,transcripts:firstTranscripts,requests:firstRequests,held},null,2));
  assert.equal(firstRequests.filter(r=>r.path==='/v1/sop/submit').length,0,'failed/cancelled first turn must not commit a SOP submission');
  assert.notEqual(firstState?.state.status,'completed');assert.equal(firstState?.replyDelivery,undefined);
  if(scenario==='owner_error')assert(JSON.stringify(frames).includes('Fixture owner rejected preparation.'));
  phase='second';request('second','submit_turn',{sessionKey:'lifecycle-session',channelKey:'test',runId:'lifecycle-second',message:'Recover and finish.'});await wait(()=>terminal(frames,'second'),'recovery terminal');
  const finalState=state(),allTranscripts=transcripts();
  fs.writeFileSync(path.join(out,`${side}.json`),JSON.stringify({frames,requests,state:finalState,transcripts:allTranscripts,held},null,2));
  assert.equal(terminal(frames,'second').event?.finishReason,'completed');assert.equal(finalState.state.status,'completed');assert.equal(finalState.replyDelivery,undefined);
  assert.equal(requests.filter(r=>r.path==='/v1/sop/submit').length,1);
  assert.equal(frames.filter(f=>f.id==='second'&&f.event?.type==='assistant_text_delta'&&f.event.text==='Recovered SOP final answer.').length,1);
  const events=id=>frames.filter(f=>f.id===id).map(f=>({type:f.type,final:f.final,event:f.event?.type,finishReason:f.event?.finishReason,error:f.error,code:f.event?.code,message:f.event?.message}));
  const counts=Object.entries(allTranscripts).map(([name,entries])=>({name,accepted:entries.filter(e=>e.type==='accepted_input').length,modelRequests:entries.filter(e=>e.type==='model_request').length,assistantMessages:entries.filter(e=>e.type==='assistant_message').length}));
  return {firstState:snapshotState(firstState),finalState:snapshotState(finalState),first:events('first'),second:events('second'),wrongAbort:terminal(frames,'wrong-abort')?.result,abort:terminal(frames,'abort')?.result,counts,submissions:requests.filter(r=>r.path==='/v1/sop/submit').map(r=>r.body.payload.proposal),held:held.map(({path,closed})=>({path,closed}))};
 }finally{
  fs.writeFileSync(path.join(out,`${side}-partial.json`),JSON.stringify({frames,requests,state:state(),held},null,2));
  ws?.close();if(!ended)child.kill('SIGTERM');let exit=await Promise.race([done,sleep(10000).then(()=>null)]);if(!exit){child.kill('SIGKILL');exit=await done;}
  fs.writeFileSync(path.join(out,`${side}.log`),Buffer.concat(logs));assert.equal(exit.signal,null,`${side} killed`);assert.equal(exit.code,0,`${side} exit`);
 }
}
const identity=validationIdentity({baseline,rust,scenario:new URL(import.meta.url)});
const manifest={schema:2,scenario:`sop-lifecycle-${scenario}-${mode}`,...identity.fields,node:process.version,platform:`${process.platform}-${process.arch}`};let failure;
try{const expected=await run('ts',process.execPath,[path.join(baseline,'dist/src/cli/pilotdeck.js')]);if(args.includes('--self'))assert.deepEqual(await run('ts-self',process.execPath,[path.join(baseline,'dist/src/cli/pilotdeck.js')]),expected);if(rust)assert.deepEqual(await run('rust',rust,[]),expected);manifest.passed=true;}
catch(error){manifest.passed=false;manifest.error=String(error.stack??error);failure=error;}
finally{try{Object.assign(manifest,identity.finish());}catch(error){manifest.passed=false;manifest.identityError=String(error.stack??error);failure??=error;}server.closeAllConnections();await new Promise(resolve=>server.close(resolve));fs.writeFileSync(path.join(out,'manifest.json'),JSON.stringify(manifest,null,2));}
if(failure)throw failure;console.log(JSON.stringify(manifest,null,2));
