import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import {createHash,randomUUID} from 'node:crypto';
import {createRequire} from 'node:module';
import {spawn,spawnSync} from 'node:child_process';
import {setTimeout as sleep} from 'node:timers/promises';
import {fileURLToPath,pathToFileURL} from 'node:url';
const here=path.dirname(fileURLToPath(import.meta.url));
const baseline=process.env.PDRS_BASELINE??'/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const rustBinary=process.env.PDRS_RUST_BINARY;
const negativeReply=process.env.PDRS_NEGATIVE_REPLY==='1';
const revision='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const entry=path.join(baseline,'dist/src/cli/pilotdeck.js');
const out=path.resolve(process.argv[2]);
fs.mkdirSync(out);
const json=(file,value)=>fs.writeFileSync(file,JSON.stringify(value,null,2)+'\n');
const hash=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const lines=file=>fs.existsSync(file)?fs.readFileSync(file,'utf8').trim().split('\n').filter(Boolean).map(s=>JSON.parse(s)):[];
const wait=async(fn,label,ms=10000)=>{const end=Date.now()+ms;while(!fn()&&Date.now()<end)await sleep(25);assert.ok(fn(),label);};
const freePort=async()=>{const s=net.createServer();await new Promise(r=>s.listen(0,'127.0.0.1',r));const p=s.address().port;await new Promise(r=>s.close(r));return p;};
assert.equal(process.version,'v22.22.0');
assert.equal(spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),revision);
assert.equal(spawnSync('git',['-C',baseline,'diff','--quiet','HEAD','--','src','packages']).status,0);
const {default:ts}=await import(pathToFileURL(path.join(baseline,'node_modules/typescript/lib/typescript.js')));
const config=JSON.parse(fs.readFileSync(path.join(baseline,'tsconfig.json'),'utf8'));
const compiler=ts.convertCompilerOptionsFromJson(config.compilerOptions,baseline).options;
const sources=['src/cli/pilotdeck.ts','src/cli/ChannelAdapterBundle.ts','src/adapters/channel/loadEnabledChannels.ts','src/adapters/channel/whatsapp/WhatsAppChannel.ts'];
const checks=[];
for(const src of sources){const built='dist/'+src.replace(/\.ts$/,'.js');const emitted=ts.transpileModule(fs.readFileSync(path.join(baseline,src),'utf8'),{fileName:path.join(baseline,src),compilerOptions:{...compiler,module:ts.ModuleKind.ESNext}}).outputText;assert.equal(emitted,fs.readFileSync(path.join(baseline,built),'utf8'));checks.push({src,sourceSha256:hash(path.join(baseline,src)),built,distSha256:hash(path.join(baseline,built))});}
const {WebSocket}=createRequire(path.join(baseline,'package.json'))('ws');
json(path.join(out,'identity.json'),{negativeReply,side:rustBinary?'rust':'ts',rustBinary:rustBinary??null,rustSha256:rustBinary?hash(rustBinary):null,revision,node:process.version,nodePath:process.execPath,nodeSha256:hash(process.execPath),driverSha256:hash(fileURLToPath(import.meta.url)),bridgeSha256:hash(path.join(here,'bridge.mjs')),compiler:ts.version,checks,matrixSdkExists:fs.existsSync(path.join(baseline,'node_modules/matrix-bot-sdk')),standaloneEsmRequireType:typeof require});
const records=[];
const CASES=['whatsapp-missing','whatsapp-basic','whatsapp-ready404','whatsapp-cron','whatsapp-active'];
const selected=process.argv[3]?process.argv[3].split(','):CASES;assert.ok(selected.length&&new Set(selected).size===selected.length&&selected.every(name=>CASES.includes(name)),'selected case set is known and unique');
for(const name of selected){
 const dir=path.join(out,name),home=path.join(dir,'home'),pilot=path.join(home,'.pilotdeck'),project=path.join(dir,'project');fs.mkdirSync(pilot,{recursive:true});fs.mkdirSync(project);
 const record={name,side:rustBinary?'rust':'ts',wire:[],provider:[],rpc:[],stdout:'',stderr:'',ready:false,forceKilled:false};
 const sockets=new Set();let heldResponse=null;
 const mock=http.createServer(async(req,res)=>{
  const chunks=[];for await(const c of req)chunks.push(c);const body=Buffer.concat(chunks).toString();record.wire.push({method:req.method,path:req.url,body});
  if(!req.url.startsWith('/v1/')){res.writeHead(500);res.end('no Matrix API expected');return;}
  const request=JSON.parse(body);record.provider.push({path:req.url,stream:request.stream,messages:request.messages.filter(m=>m.role==='user')});
  if(!request.stream){res.writeHead(200,{'content-type':'application/json'});res.end(JSON.stringify({id:'fixture-title',object:'chat.completion',choices:[{index:0,message:{role:'assistant',content:'{"title":"Fixture title"}'},finish_reason:'stop'}],usage:{prompt_tokens:1,completion_tokens:1,total_tokens:2}}));return;}
  res.writeHead(200,{'content-type':'text/event-stream'});
  res.write('data: '+JSON.stringify({id:'fixture-answer',object:'chat.completion.chunk',choices:[{index:0,delta:{role:'assistant',content:negativeReply?'intentionally wrong fixture answer':'fixture answer'},finish_reason:null}]})+'\n\n');
  if(request.messages.some(message=>message.role==='user'&&message.content==='fixture-hang')){heldResponse=res;res.on('close',()=>record.providerClosedAt=Date.now());return;}
  res.end('data: '+JSON.stringify({id:'fixture-answer',object:'chat.completion.chunk',choices:[{index:0,delta:{},finish_reason:'stop'}],usage:{prompt_tokens:1,completion_tokens:1,total_tokens:2}})+'\n\ndata: [DONE]\n\n');
 });
 mock.on('connection',s=>{sockets.add(s);s.once('close',()=>sockets.delete(s));});
 await new Promise(r=>mock.listen(0,'127.0.0.1',r));const mockPort=mock.address().port,gatewayPort=await freePort(),bridgePort=await freePort();
 Object.assign(record,{mockPort,gatewayPort,bridgePort});
 const channel='whatsapp';
 const extra=name==='whatsapp-missing'?'':`    extra:\n      bridgePath: ${JSON.stringify(path.join(here,'bridge.mjs'))}\n      bridgeUrl: http://127.0.0.1:${bridgePort}///\n`;
 const cfg=`schemaVersion: 1\nagent:\n  model: fixture/deterministic\n  maxContextTokens: 65536\n  maxOutputTokens: 8192\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:${mockPort}/v1\n      apiKey: fixture-only\n      models:\n        deterministic:\n          capabilities:\n            supportsToolUse: true\n            maxContextTokens: 65536\n            maxOutputTokens: 8192\ntelemetry:\n  enabled: false\nadapters:\n  ${channel}:\n    enabled: true\n${extra}`;
 if(name==='whatsapp-cron'){const registration=path.join(pilot,'projects','fixture-project');fs.mkdirSync(registration,{recursive:true});fs.writeFileSync(path.join(registration,'.cwd'),project+'\n');}
 const configPath=path.join(pilot,'pilotdeck.yaml');fs.writeFileSync(configPath,cfg+(name==='whatsapp-cron'?'cron:\n  enabled: true\n':''));
 if(name==='whatsapp-ready404')fs.writeFileSync(path.join(dir,'first-404'),'');
 const env={PATH:`${path.dirname(process.execPath)}:/usr/bin:/bin`,HOME:home,USERPROFILE:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:configPath,TMPDIR:dir,LANG:'C',LC_ALL:'C',TZ:'UTC',NODE_NO_WARNINGS:'1',NO_COLOR:'1'};
 const command=rustBinary??process.execPath;
 const args=[...(rustBinary?[]:[entry]),'server','--host','127.0.0.1','--port',String(gatewayPort)];json(path.join(dir,'command.json'),{command,args,env,cwd:project});
 const child=spawn(command,args,{env,cwd:project,detached:true,stdio:['ignore','pipe','pipe']});record.pid=child.pid;
 child.stdout.on('data',v=>record.stdout+=v);child.stderr.on('data',v=>record.stderr+=v);const closed=new Promise(r=>child.once('close',(exitCode,signal)=>{Object.assign(record,{exitCode,signal});r();}));
 let gateway;
 try{
  const limit=Date.now()+15000;while(Date.now()<limit&&child.exitCode===null){try{const r=await fetch(`http://127.0.0.1:${gatewayPort}/health`,{signal:AbortSignal.timeout(200)});if(r.ok){record.ready=true;break;}}catch{}await sleep(50);}assert.ok(record.ready,'formal server health');
  if(name==='whatsapp-missing'){await wait(()=>record.stderr.includes('WHATSAPP_BRIDGE_PATH is required'),'missing path error');assert.equal(lines(path.join(dir,'bridge-life.jsonl')).length,0);}
  else{
   await wait(()=>record.stdout.includes('whatsapp: connected'),'bridge connected');
   const sendPath=path.join(dir,'bridge-sends.jsonl');const inbound=value=>json(path.join(dir,'inbound.json'),value);
   inbound([{id:'new-1',chatId:'fixture-chat',text:' /new '}]);await wait(()=>lines(sendPath).length===1,'new reply');assert.deepEqual(lines(sendPath)[0].body,{chatId:'fixture-chat',message:'已创建新会话。'});
   if(['whatsapp-basic','whatsapp-cron','whatsapp-active'].includes(name)){
    if(name==='whatsapp-basic'){
    inbound({data:[{messageId:'normal-1',from:'fixture-chat',content:'fixture-message'}]});await wait(()=>lines(sendPath).length===2,'ordinary answer');assert.deepEqual(lines(sendPath)[1].body,{chatId:'fixture-chat',message:'fixture answer'});
    await sleep(2200);assert.equal(lines(sendPath).length,2,'duplicate input id suppressed');record.duplicateObserved=true;
    fs.writeFileSync(path.join(dir,'reject-send'),'');inbound({messages:[{key:'new-fail',chat_id:'fixture-other',body:'/new'}]});await wait(()=>lines(sendPath).length===3,'failed send captured');await wait(()=>record.stderr.includes('fixture send denied'),'send error logged');fs.unlinkSync(path.join(dir,'reject-send'));inbound([]);
    }else if(name==='whatsapp-cron'){inbound([{id:'cron-initial',chatId:'cron-chat',text:'fixture-message'}]);await wait(()=>lines(sendPath).length===2,'cron origin session');}
    else{inbound([{id:'active',chatId:'fixture-chat',text:'fixture-hang'}]);await wait(()=>heldResponse!==null,'active provider stream');}
    gateway=new WebSocket(`ws://127.0.0.1:${gatewayPort}/ws`);const pending=new Map();gateway.on('message',raw=>{const value=JSON.parse(raw);record.rpc.push({direction:'receive',value});if(value.type==='hello_ok')pending.get('hello')?.(value);else if(value.type==='response')pending.get(value.id)?.(value);});
    await new Promise((r,j)=>{gateway.once('open',r);gateway.once('error',j);});
    const rpc=(method,params)=>new Promise((resolve,reject)=>{const id=randomUUID(),timer=setTimeout(()=>reject(new Error('RPC timeout '+method)),10000);pending.set(id,value=>{clearTimeout(timer);value.error?reject(new Error(JSON.stringify(value.error))):resolve(value.result);});const value={type:'request',id,method,params};record.rpc.push({direction:'send',value});gateway.send(JSON.stringify(value));});
    await new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(new Error('hello timeout')),5000);pending.set('hello',()=>{clearTimeout(timer);resolve();});gateway.send(JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'h1-next-fixture',clientVersion:'0.1.0',token:fs.readFileSync(path.join(pilot,'server-token'),'utf8').trim()}));});
    if(name==='whatsapp-basic'){
    fs.writeFileSync(configPath,cfg.replace('enabled: true','enabled: false'));await rpc('reload_config',{});await wait(()=>record.stdout.includes('whatsapp=stopped'),'disable observed');const before=lines(path.join(dir,'bridge-wire.jsonl')).length;await sleep(2200);record.noPollAfterDisable=lines(path.join(dir,'bridge-wire.jsonl')).length===before;assert.ok(record.noPollAfterDisable);
    fs.writeFileSync(configPath,cfg);await rpc('reload_config',{});await wait(()=>lines(path.join(dir,'bridge-life.jsonl')).filter(r=>r.event==='start').length===2,'bridge restart');inbound([{id:'new-1',chatId:'fixture-chat',text:'/new'}]);await wait(()=>lines(sendPath).length===4,'new generation clears dedup');record.reloadSucceeded=true;
    }else if(name==='whatsapp-cron'){
     record.cronCreated=await rpc('cron_create',{message:'fixture-cron',schedule:{type:'delay',amount:1,unit:'day'},sessionKey:'whatsapp:chat=cron-chat:general',channelKey:'whatsapp',projectKey:project});
     record.cronRun=await rpc('cron_run_now',{taskId:record.cronCreated.task.taskId,projectKey:project});
     await wait(()=>lines(sendPath).length>=3,'cron result delivered');record.cronSend=lines(sendPath).at(-1);assert.equal(record.cronSend.body.chatId,'cron-chat');assert.ok(record.cronSend.body.message.includes('fixture answer'));
    }else{
     fs.writeFileSync(configPath,cfg.replace('enabled: true','enabled: false'));await rpc('reload_config',{});await wait(()=>record.stdout.includes('whatsapp=stopped'),'active disable');
     await sleep(2200);record.providerOpenAfterDisable=!heldResponse.destroyed;record.postsAfterDisable=lines(sendPath).length;
     assert.equal(record.providerOpenAfterDisable,!rustBinary,'TS retains its active turn, native must cancel and drain');
     if(!rustBinary){heldResponse.end('data: '+JSON.stringify({id:'fixture-answer',object:'chat.completion.chunk',choices:[{index:0,delta:{},finish_reason:'stop'}]})+'\n\ndata: [DONE]\n\n');await sleep(250);}
     record.postsAfterRelease=lines(sendPath).length;assert.equal(record.postsAfterRelease,1,'stopped adapter cannot send late answer');
    }
   }
  }
 }catch(e){record.scriptError=e.stack;}
 finally{
  if(gateway){const closedWs=new Promise(r=>gateway.once('close',r));gateway.close();await Promise.race([closedWs,sleep(2000)]);record.gatewayClosedBeforeShutdown=gateway.readyState===WebSocket.CLOSED;}
  const stopAt=Date.now();if(child.exitCode===null&&child.signalCode===null)child.kill('SIGTERM');
  const timer=setTimeout(()=>{if(child.exitCode===null&&child.signalCode===null){record.forceKilled=true;try{process.kill(-child.pid,'SIGKILL');}catch{}}},8000);await closed;clearTimeout(timer);record.shutdownElapsedMs=Date.now()-stopAt;
  record.bridgeLife=lines(path.join(dir,'bridge-life.jsonl'));record.bridgeWire=lines(path.join(dir,'bridge-wire.jsonl'));record.sends=lines(path.join(dir,'bridge-sends.jsonl'));
  record.bridgeRemaining=[];for(const p of record.bridgeLife.filter(r=>r.event==='start').map(r=>r.pid)){try{process.kill(p,0);record.bridgeRemaining.push(p);}catch{}}
  if(record.bridgeRemaining.length){record.cleanupKilledBridge=true;for(const p of record.bridgeRemaining){try{process.kill(p,'SIGKILL');}catch{}}}
  for(const s of sockets)s.destroy();await new Promise(r=>mock.close(r));
  fs.writeFileSync(path.join(dir,'stdout.log'),record.stdout);fs.writeFileSync(path.join(dir,'stderr.log'),record.stderr);json(path.join(dir,'result.json'),record);records.push(record);
 }
 try{assert.equal(record.scriptError,undefined);assert.equal(record.exitCode,0);assert.equal(record.signal,null);assert.equal(record.forceKilled,false);assert.deepEqual(record.bridgeRemaining,[]);record.verified=true;}catch(e){record.verifyError=e.message;record.verified=false;}
 console.log(JSON.stringify({name,verified:record.verified,error:record.scriptError??record.verifyError,provider:record.provider.length,sends:record.sends.length,shutdownMs:record.shutdownElapsedMs}));
}
json(path.join(out,'results.json'),records);process.exitCode=records.every(r=>r.verified)?0:1;
