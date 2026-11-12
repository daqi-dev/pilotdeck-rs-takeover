#!/usr/bin/env node
// Formal native SOP turn under isolated HOME and dynamically allocated ports.
// Both the SOP owner and model provider are local deterministic HTTP services.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { validationIdentity } from './identity.mjs';
import assert from 'node:assert/strict';

const args=process.argv.slice(2), arg=key=>args[args.indexOf(key)+1];
if(!args.includes('--baseline')||!args.includes('--out'))throw new Error('--baseline CHECKOUT --out NEW_DIR [--rust BINARY] [--self]');
const mode=args.includes('--mode')?arg('--mode'):'inherit';if(!['inherit','new_session'].includes(mode))throw new Error('invalid mode');
const knowledgeCase=args.includes('--knowledge-case')?arg('--knowledge-case'):'success';
if(!['success','stale','untrusted','invalid_input'].includes(knowledgeCase))throw new Error('invalid knowledge case');
const hooksEnabled=args.includes('--hooks');
const receiptEnabled=args.includes('--receipt');
const behavior=args.includes('--behavior')?arg('--behavior'):'normal';
if(!['normal','correction','recovery_pending','recovery_durable'].includes(behavior))throw new Error('invalid behavior');
const recovery=behavior.startsWith('recovery_');
const out=path.resolve(arg('--out')), baseline=path.resolve(arg('--baseline')), rust=args.includes('--rust')?path.resolve(arg('--rust')):null;
if(fs.existsSync(out))throw new Error('output directory must be new');
fs.mkdirSync(out,{recursive:true});
const root=path.join(out,'fixture'), home=path.join(root,'home'), pilot=path.join(home,'.pilotdeck'), project=path.join(root,'project');
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
let observed=[],mockNode="n1",authorityReads=new Map();
const owner=http.createServer(async(req,res)=>{
  let body='';for await(const chunk of req)body+=chunk;
  let input;try{input=JSON.parse(body);}catch{input=null;}
  const route=new URL(req.url,'http://localhost').pathname;
  observed.push({method:req.method,path:route,headers:req.headers,headerOrder:Object.keys(req.headers),body:input});
  const send=payload=>{res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify(payload));};
  if(route.endsWith('/sops:route'))return send({decision:'selected',candidate_sop_ids:['sop'],selected_sop_id:'sop'});
  if(route.endsWith('/module-manifest')){const authority=route.includes('sop-capability-authority');return send({protocolVersion:'2.0',implementationId:authority?'staffdeck.sop-capability-authority':'staffdeck.knowledge',contract:authority?'staffdeck.sop-capability-authority/v1':'staffdeck.knowledge/v1',transport:'module-http-v2',methods:[authority?'resolve':'query']});}
  if(route.endsWith('/v2/module/call')){
    assert.equal(req.headers.authorization,'Bearer synthetic-account-key');
    let result={chunks:[{text:'Verified knowledge evidence.',source:'fixture'}]};
    if(input.payload.operation==='resolve'){
      const context=input.payload.input,key=JSON.stringify(context),count=(authorityReads.get(key)??0)+1;authorityReads.set(key,count);
      result={context,sopId:'sop',sopVersion:'1',nodeId:mockNode,snapshotId:knowledgeCase==='stale'&&count>1?'changed':`snapshot-${mockNode}`,registryGeneration:3,optionalCapabilities:[{operation:'knowledge.search/v1',resourceType:'knowledge_base',resourceId:'base',required:false,providerModuleId:'knowledge.local',providerVersion:'1',selectionMode:'current'}]};
    }
    return send({kind:'response',messageId:'reply',inReplyTo:input.messageId,requestId:input.requestId,ok:true,payload:{result}});
  }
  if(route==='/healthz')return send({status:'ok',protocolVersion:'2.0',moduleId:'sop.runtime',contract:'sop.lifecycle/v2',operations:['prepare','submit']});
  if(route.startsWith('/v1/sop/')){
    const state={...input.payload.state};
    if(route.endsWith('/prepare')){
      state.active_skill_id='sop';state.active_step_id??='n1';state.status='active';state.successful_tool_names??=[];
      const last=state.active_step_id==='n2';mockNode=state.active_step_id;
      return send({protocolVersion:'2.0',requestId:input.requestId,ok:true,outcome:'completed',payload:{state,step:{skillId:'sop',skillName:'Two steps',version:'1',nodeId:state.active_step_id,node:{type:'response'},instruction:last?'Return the final answer.':'Prepare evidence and advance.',expectedUserInfo:[],knownSlots:state.slots_json??{},allowedNextStepIds:last?[]:['n2'],requiredToolNames:[],...(knowledgeCase==='untrusted'?{optionalKnowledge:{snapshotId:'untrusted',optionalCapabilities:[{}]}}:{}),allowedActions:[],isTerminal:last,declaresHandoff:false,contextMode:mode}}});
    }
    const last=state.active_step_id==='n2';state.status=last?'completed':'active';if(!last)state.active_step_id='n2';
    const result={...input.payload.proposal,slotUpdates:input.payload.proposal.slotUpdates??{},events:[]};
    return send({protocolVersion:'2.0',requestId:input.requestId,ok:true,outcome:'completed',payload:{state,result}});
  }
  if(route==='/v1/chat/completions'){
    const source=JSON.stringify(input.messages), title=source.includes('Generate a concise title');
    const last=source.includes('step n2.');
    const proposal={status:'completed',replyFragment:last?'SOP final answer.':'Evidence ready.',slotUpdates:{},...(!last?{nextStepId:'n2'}:{})};
    const correction=behavior==='correction'&&!title&&!last&&observed.filter(r=>r.path==='/v1/chat/completions'&&!JSON.stringify(r.body.messages).includes('Generate a concise title')).length===1;
    const query=!title&&!last&&observed.filter(r=>r.path==='/v1/chat/completions'&&!JSON.stringify(r.body.messages).includes('Generate a concise title')).length===1;
    const calls=query?[{id:'call-knowledge',type:'function',function:{name:'knowledge_query',arguments:JSON.stringify({query:'facts',...(knowledgeCase==='invalid_input'?{actorUserId:'forged'}:{})})}}]:title||correction?[]:[{id:last?'call-n2':'call-n1',type:'function',function:{name:'submit_step_result',arguments:JSON.stringify(proposal)}}];
    const content=title?'{"title":"SOP test"}':correction?'I have prose but have not submitted the result.':null, usage={prompt_tokens:10,completion_tokens:5,total_tokens:15};
    if(!input.stream)return send({id:'sop-mock',model:input.model,choices:[{index:0,message:{role:'assistant',content,...(calls.length?{tool_calls:calls}:{})},finish_reason:calls.length?'tool_calls':'stop'}],usage});
    res.writeHead(200,{'content-type':'text/event-stream'});
    const chunks=[{role:'assistant',...(content?{content}:{}),...(calls.length?{tool_calls:calls.map((call,index)=>({index,...call}))}:{})}];
    for(const delta of chunks)res.write(`data: ${JSON.stringify({choices:[{index:0,delta,finish_reason:null}]})}\n\n`);
    res.end(`data: ${JSON.stringify({choices:[{index:0,delta:{},finish_reason:calls.length?'tool_calls':'stop'}],usage})}\n\ndata: [DONE]\n\n`);return;
  }
  res.writeHead(500);res.end(JSON.stringify({error:'unexpected mock endpoint'}));
});
owner.listen(0,'127.0.0.1');await once(owner,'listening');const endpoint=`http://127.0.0.1:${owner.address().port}`;
const ids=[];
async function run(side,command,argv){
  fs.rmSync(root,{recursive:true,force:true});fs.mkdirSync(pilot,{recursive:true});fs.mkdirSync(project,{recursive:true});
  if(hooksEnabled){
    const dir=path.join(pilot,'plugins','sop-hooks');fs.mkdirSync(dir,{recursive:true});
    const hooks=Object.fromEntries(['SessionStart','Setup','UserPromptSubmit','PreModelRequest','PostModelResponse','PreToolUse','PostToolUse','Stop','SessionEnd'].map(name=>[name,[{hooks:[{type:'command',command:'node hook.mjs'}]}]]));
    fs.writeFileSync(path.join(dir,'plugin.json'),JSON.stringify({name:'sop-hooks',version:'0.1.0',hooks}));
    fs.writeFileSync(path.join(dir,'hook.mjs'),`import fs from 'node:fs';let input='';for await(const chunk of process.stdin)input+=chunk;fs.appendFileSync(process.env.HOME+'/sop-hooks.jsonl',JSON.stringify(JSON.parse(input))+'\\n');`);
  }
  const definitions={sops:[{id:'sop',version:'1',name:'Two steps',content:{nodes:[{node_id:'n1',type:'response',contextMode:mode},{node_id:'n2',type:'response',contextMode:mode}]}}]};
  fs.writeFileSync(path.join(pilot,'definitions.json'),JSON.stringify(definitions));
  if(recovery){
    const dir=path.join(pilot,'sop','sessions');fs.mkdirSync(dir,{recursive:true});
    const timestamp='2026-10-13T00:00:00.000Z',phase=behavior.slice('recovery_'.length);
    fs.writeFileSync(path.join(dir,Buffer.from('sop-session').toString('base64url')+'.json'),JSON.stringify({schemaVersion:3,sessionId:'sop-session',bundle:definitions,state:{version:1,selected_skill_id:'sop',active_skill_id:'sop',active_step_id:'n2',status:'completed',slots_json:{},skill_stack_json:[],successful_tool_names:[]},revision:9,resumeRequests:{},updatedAt:timestamp,replyDelivery:{turnId:'committed-before-restart',phase,result:{status:'completed',replyFragment:'SOP final answer.',slotUpdates:{},events:[]},committedAt:timestamp,...(phase==='durable'?{durableAt:timestamp}:{})}}));
  }
  if(receiptEnabled){
    assert(!recovery,'receipt scenario must execute nodes');
    const dir=path.join(pilot,'sop','sessions');fs.mkdirSync(dir,{recursive:true});
    const receipt=(id,assignee)=>({accepted:true,duplicate:false,sessionId:'sop-session',requestId:id,revision:1,message:'Approved',authorization:{approval:{assigneeUserId:assignee}}});
    fs.writeFileSync(path.join(dir,Buffer.from('sop-session').toString('base64url')+'.json'),JSON.stringify({schemaVersion:3,sessionId:'sop-session',bundle:definitions,state:{version:1,selected_skill_id:'sop',active_skill_id:'sop',active_step_id:'n1',status:'active',slots_json:{},skill_stack_json:[],successful_tool_names:[]},revision:1,resumeRequests:{older:receipt('older','old-owner'),newer:receipt('newer','receipt-owner'),invalid:{authorization:{approval:{assigneeUserId:'invalid-owner'}}}},updatedAt:'2026-10-13T00:00:00.000Z'}));
  }
  fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),`schemaVersion: 1\nagent:\n  model: mock/deterministic\n  maxTurns: 6\nmodel:\n  providers:\n    mock:\n      protocol: openai\n      url: ${endpoint}/v1\n      apiKey: mock-only\n      models:\n        deterministic: {}\nmodules:\n  knowledge:\n    enabled: true\n    implementationId: staffdeck.knowledge\n    contract: staffdeck.knowledge/v1\n    transport: module-http-v2\n    endpoint: ${endpoint}\n    manifestPath: /api/v1/knowledge-module/module-manifest\n    callPath: /api/v1/agents/agent/knowledge-module/v2/module/call\n    methods: [query]\n    credentialEnv: SOP_KNOWLEDGE_TEST_KEY\n    agentId: agent\n  sop:\n    enabled: true\n    provider: staffdeck\n    endpoint: ${endpoint}\n    definitionsPath: '${path.join(pilot,'definitions.json')}'\n    defaultSopId: sop\n    discoveryEndpoint: ${endpoint}\n    discoveryApiKey: mock-route-key\n    discoveryAgentId: ${knowledgeCase==='untrusted'?'other-agent':'agent'}\n    contextMode: ${mode}\ntelemetry:\n  enabled: false\n`);
  const reserve=net.createServer();reserve.listen(0,'127.0.0.1');await once(reserve,'listening');const port=reserve.address().port;await new Promise(r=>reserve.close(r));assert.notEqual(port,18789);
  const env={PATH:process.env.PATH,HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),PILOTDECK_GATEWAY_TOKEN:'sop-test-token',DOCKER_CONTAINER:'1',COMMIT_HASH:'sop-test',SOP_KNOWLEDGE_TEST_KEY:'synthetic-account-key'};
  const child=spawn(command,[...argv,'server','--host','127.0.0.1','--port',String(port)],{cwd:project,env,stdio:['ignore','pipe','pipe']});ids.push({side,pid:child.pid,port});fs.writeFileSync(path.join(out,'processes.json'),JSON.stringify(ids,null,2));
  const logs=[];child.stdout.on('data',b=>logs.push(b));child.stderr.on('data',b=>logs.push(b));let ended;const done=new Promise(resolve=>child.once('exit',(code,signal)=>{ended={code,signal};resolve(ended);}));
  let ws;const frames=[];observed=[];mockNode='n1';authorityReads=new Map();
  try{
    const deadline=Date.now()+45000;
    for(;;){if(ended)throw new Error(`startup exited ${JSON.stringify(ended)} ${Buffer.concat(logs)}`);try{const sock=net.connect(port,'127.0.0.1');await Promise.race([once(sock,'connect'),once(sock,'error').then(([e])=>Promise.reject(e))]);sock.destroy();break;}catch{if(Date.now()>deadline)throw new Error('startup timed out');await delay(50);}}
    ws=new WebSocket(`ws://127.0.0.1:${port}/ws`);ws.onmessage=e=>frames.push(JSON.parse(e.data));await once(ws,'open');
    const wait=async predicate=>{const end=Date.now()+30000;for(;;){const found=frames.find(predicate);if(found)return found;if(Date.now()>end)throw new Error(`WS timeout ${JSON.stringify(frames)}`);await delay(20);}};
    ws.send(JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'test',clientVersion:'0.0.0',token:'sop-test-token'}));await wait(f=>f.type==='hello_ok');
    ws.send(JSON.stringify({type:'request',id:'turn',method:'submit_turn',params:{sessionKey:'sop-session',channelKey:'test',message:'Complete both SOP steps.'}}));
    await wait(f=>f.id==='turn'&&(f.type==='response'||f.final));
    const all=frames.filter(f=>f.id==='turn');
    const completion=all.find(f=>f.event?.type==='turn_completed'&&f.final);const structured=all.find(f=>f.event?.type==='structured_output');
    const statePath=path.join(pilot,'sop','sessions',Buffer.from('sop-session').toString('base64url')+'.json');
    const state=fs.existsSync(statePath)?JSON.parse(fs.readFileSync(statePath)):null;
    const transcripts={};const walk=dir=>{if(!fs.existsSync(dir))return;for(const entry of fs.readdirSync(dir,{withFileTypes:true})){const file=path.join(dir,entry.name);if(entry.isDirectory())walk(file);else if(entry.name.endsWith('.jsonl'))transcripts[path.relative(pilot,file)]=fs.readFileSync(file,'utf8').trim().split('\n').filter(Boolean).map(line=>JSON.parse(line));}};walk(path.join(pilot,'projects'));const result={frames:all,requests:observed,state,transcripts};fs.writeFileSync(path.join(out,`${side}.json`),JSON.stringify(result,null,2));
    assert(completion,`${side} lacks final turn_completed`);assert.equal(completion.event.finishReason,'completed');assert.equal(structured.event.payload.sop.replyFragment,'SOP final answer.');
    assert.equal(all.filter(f=>f.event?.type==='turn_completed').length,2);assert.equal(all.filter(f=>f.event?.type==='assistant_text_delta'&&f.event.text==='SOP final answer.').length,1);
    assert.equal(state.state.status,'completed');assert.equal(state.state.active_step_id,'n2');assert.equal(state.replyDelivery,undefined);
    assert.equal(observed.filter(r=>r.path==='/v1/sop/submit').length,recovery?0:2);assert.equal(observed.filter(r=>r.path==='/healthz').length,recovery?0:1);
    const models=observed.filter(r=>r.path==='/v1/chat/completions'&&!JSON.stringify(r.body.messages).includes('Generate a concise title'));
    assert.equal(models.length,3);
    for(const model of models)assert.deepEqual(model.body.tool_choice,knowledgeCase==='untrusted'?{type:'function',function:{name:'submit_step_result'}}:'required');
    const queries=observed.filter(r=>r.path==='/api/v1/agents/agent/knowledge-module/v2/module/call');
    assert.equal(queries.length,knowledgeCase==='success'?1:0);
    if(queries.length){const payload=queries[0].body.payload;assert.deepEqual(payload.input,{query:'facts'});assert.equal(payload.authorityContext.sessionKey,'sop-session');assert.equal(payload.authorityContext.snapshotId,'snapshot-n1');assert.equal(payload.authorityContext.registryGeneration,3);assert.equal(payload.authorityContext.expectedRevision>0,true);}
    if(knowledgeCase==='stale')assert(JSON.stringify(models[1].body.messages).includes('SOP_AUTHORITY_STALE'));
    if(knowledgeCase==='invalid_input')assert(JSON.stringify(models[1].body.messages).includes('SOP_AUTHORITY_INPUT_INVALID'));
    if(knowledgeCase==='untrusted')assert(JSON.stringify(models[1].body.messages).includes('SOP_AUTHORITY_TOOL_UNAVAILABLE'));
    if(receiptEnabled)for(const model of models){const messages=JSON.stringify(model.body.messages);assert(messages.includes('Authoritative approval assignee user ID from pinned owner approval state: receipt-owner.'));assert(!messages.includes('invalid-owner'));assert(!messages.includes('old-owner'));}
    if(behavior==='correction')assert(JSON.stringify(models[1].body.messages).includes('SOP_STEP_RESULT_REQUIRED'),'correction instruction must reach the model');
    if(mode==='new_session'||recovery){const nodes=Object.entries(transcripts).filter(([name])=>name.includes('/subagents/'));assert.equal(nodes.length,recovery?0:2);for(const [,entries] of nodes){assert(entries.some(e=>e.type==='session_metadata'&&e.turnId==='sop-node-created'&&e.metadata.parentSessionId==='sop-session'));assert(entries.some(e=>e.type==='model_request'));assert.equal(entries.filter(e=>e.type==='accepted_input').length,1,'one admission per node, including correction');}const parent=Object.entries(transcripts).find(([name])=>name.endsWith('/sop-session.jsonl'))?.[1];assert(parent);assert.equal(parent.filter(e=>e.type==='assistant_message').length,1);assert.equal(parent.filter(e=>e.type==='tool_result_message').length,0);assert.equal(parent.filter(e=>e.type==='model_request').length,0);}
    const turnId=observed.find(r=>r.path==='/v1/sop/prepare').body.turnId;
    assert.equal(typeof turnId,'string');assert(turnId.length>0);
    const domainRequests=observed.filter(r=>r.path.endsWith('/module-manifest')||r.path.endsWith('/v2/module/call')).map(r=>{
      for(const [key,value] of Object.entries({'connection':'keep-alive','accept':'*/*','accept-language':'*','sec-fetch-mode':'cors','user-agent':'node','accept-encoding':'gzip, deflate'}))assert.equal(r.headers[key],value,`${side} ${r.path} ${key}`);
      const authority=r.body?.payload.authorityContext??(r.body?.payload.operation==='resolve'?r.body.payload.input:null);
      if(authority)assert.equal(authority.requestId,`sop.authority:sop-session:${turnId}:${authority.expectedRevision}`);
      return {...r,body:r.body?JSON.parse(JSON.stringify(r.body).replaceAll(turnId,'{TURN}')):null};
    });
    const hookPath=path.join(home,'sop-hooks.jsonl'),hooks=fs.existsSync(hookPath)?fs.readFileSync(hookPath,'utf8').trim().split('\n').filter(Boolean).map(JSON.parse):[];
    if(hooksEnabled){assert(hooks.length>0,'configured hooks must execute');fs.writeFileSync(path.join(out,`${side}-hooks.json`),JSON.stringify(hooks,null,2));}
    return {domainRequests,status:state.state.status,node:state.state.active_step_id,revision:state.revision,submissions:observed.filter(r=>r.path==='/v1/sop/submit').map(r=>r.body.payload.proposal),toolChoices:models.map(r=>r.body.tool_choice),tools:models.map(r=>r.body.tools),instructions:models.map(r=>JSON.stringify(r.body.messages).match(/<staffdeck-sop>[\s\S]*?<\/staffdeck-sop>/)?.[0]),sopRevisions:observed.filter(r=>r.path.startsWith('/v1/sop/')).map(r=>({operation:r.path,expectedRevision:r.body.expectedRevision})),final:structured.event.payload.sop,...(hooksEnabled?{hooks:hooks.map(({hookEventName,sessionId})=>({hookEventName,sessionId}))}:{})};
  }finally{
    ws?.close();if(!ended)child.kill('SIGTERM');let result=await Promise.race([done,delay(10000).then(()=>null)]);if(!result){child.kill('SIGKILL');result=await done;}
    fs.writeFileSync(path.join(out,`${side}.log`),Buffer.concat(logs));assert.equal(result.signal,null,`${side} killed`);assert.equal(result.code,0,`${side} exit`);
  }
}
const identity=validationIdentity({baseline,rust,scenario:new URL(import.meta.url)});
const manifest={schema:2,scenario:`sop-knowledge-${knowledgeCase}-${mode}-${behavior}${hooksEnabled?'-hooks':''}${receiptEnabled?'-receipt':''}`,...identity.fields,node:process.version,platform:`${process.platform}-${process.arch}`};let failure;
try{const ts=await run('ts',process.execPath,[path.join(baseline,'dist/src/cli/pilotdeck.js')]);if(args.includes('--self'))assert.deepEqual(await run('ts-self',process.execPath,[path.join(baseline,'dist/src/cli/pilotdeck.js')]),ts);if(rust)assert.deepEqual(await run('rust',rust,[]),ts);manifest.passed=true;}
catch(error){manifest.passed=false;manifest.error=String(error.stack??error);failure=error;}
finally{try{Object.assign(manifest,identity.finish());}catch(error){manifest.passed=false;manifest.identityError=String(error.stack??error);failure??=error;}owner.closeAllConnections();await new Promise(r=>owner.close(r));fs.writeFileSync(path.join(out,'manifest.json'),JSON.stringify(manifest,null,2));}
if(failure)throw failure;console.log(JSON.stringify(manifest,null,2));
