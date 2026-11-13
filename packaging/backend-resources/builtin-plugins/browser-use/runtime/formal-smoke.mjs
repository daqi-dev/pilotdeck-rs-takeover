// Formal Rust loader/agent/MCP execution against a local HTML fixture only.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import {spawn,spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
const [binary,runtime,output]=process.argv.slice(2);
if(process.env.PILOTDECK_BROWSER_SMOKE_SLOT!=='authorized'||![binary,runtime,output].every(value=>value&&path.isAbsolute(value))||fs.existsSync(output))throw Error('authorized light slot and absolute sealed CLI/runtime/NEW output required');
const here=path.dirname(fileURLToPath(import.meta.url));
for(const dir of ['home','pilot','project','tmp','skills','screenshots','builtin-plugins'])fs.mkdirSync(path.join(output,dir),{recursive:true});
const plugin=path.join(output,'builtin-plugins/browser-use');fs.cpSync(path.join(here,'..'),plugin,{recursive:true});
const spec=JSON.parse(fs.readFileSync(path.join(plugin,'plugin.json')));const base=spec.mcpServers['browser-use'];
base.args=base.args.map(value=>value==='chromium'?'chrome':value);base.args.push('--headless','--output-mode','stdout');base.perSession=true;
fs.writeFileSync(path.join(plugin,'plugin.json'),JSON.stringify(spec,null,2)+'\n');
const records=[],frames=[],modelErrors=[],toolResults=[],owned=[],processObservations=[];const observedPids=new Set();let stdout='',stderr='',stage=0,profilePaths=[];
const html='<!doctype html><html><head><title>PilotDeck local browser fixture</title></head><body><h1>Source runtime fixture</h1><label for="fixture-name">Fixture name</label><input id="fixture-name"><button id="apply" type="button">Apply greeting</button><p role="status" id="result">Not submitted</p><script>document.getElementById("apply").addEventListener("click",()=>{document.getElementById("result").textContent="Hello, "+document.getElementById("fixture-name").value;});</script></body></html>';
function text(value){if(typeof value==='string'){try{return text(JSON.parse(value));}catch{return value;}}if(Array.isArray(value))return value.map(text).join('\n');if(value&&typeof value==='object')return Object.values(value).map(text).join('\n');return '';}
function snapshotText(content){const value=text(content);const match=/\[Snapshot\]\(([^)]+)\)/.exec(value);if(!match)return value;const file=path.resolve(output,'pilot',match[1]);const real=fs.realpathSync(file);if(!real.startsWith(output+path.sep)||!fs.statSync(real).isFile())throw Error('snapshot link escapes owned fixture');return value+'\n'+fs.readFileSync(real,'utf8');}
const findTarget=(value,role,label)=>{const escaped=label.replace(/[.*+?^${}()|[\]\\]/g,'\\$&');const match=new RegExp(`${role} "${escaped}"[^\\n]*?\\[ref=([^\\]]+)\\]`).exec(value);if(!match)throw Error(`observed snapshot has no ${role} ${label} reference`);return match[1];};
function observeOwnedProcesses(label){
 if(!server?.pid)return [];
 const listing=spawnSync('ps',['-axo','pid=,ppid=,comm='],{encoding:'utf8'});if(listing.status!==0)throw Error('owned process observation failed');
 const rows=listing.stdout.split('\n').map(line=>/^\s*(\d+)\s+(\d+)\s+(.*)$/.exec(line)).filter(Boolean).map(row=>({pid:Number(row[1]),ppid:Number(row[2]),comm:row[3]}));
 const ids=new Set([server.pid]);let changed=true;while(changed){changed=false;for(const row of rows)if(ids.has(row.ppid)&&!ids.has(row.pid)){ids.add(row.pid);changed=true;}}
 const descendants=rows.filter(row=>ids.has(row.pid));
 for(const row of descendants){observedPids.add(row.pid);const args=spawnSync('ps',['-p',String(row.pid),'-o','command='],{encoding:'utf8'});row.command=args.stdout.trim();for(const match of row.command.matchAll(/--user-data-dir(?:=|\s+)([^\s]+)/g)){const profile=match[1];if(!profile.startsWith(path.join(output,'tmp')+path.sep))throw Error('owned browser profile is not inside isolated task TMPDIR');profilePaths.push(profile);}if(/chrome/i.test(row.comm)&&row.command.includes('--no-sandbox'))throw Error('actual Chrome launched with sandbox disabled');}
 profilePaths=[...new Set(profilePaths)];processObservations.push({label,at:new Date().toISOString(),processes:descendants});return descendants;
}
let server;
const model=http.createServer(async(request,response)=>{
 if(request.method==='GET'&&request.url==='/fixture'){response.writeHead(200,{'content-type':'text/html; charset=utf-8'});response.end(html);return;}
 if(request.method!=='POST'||request.url!=='/v1/chat/completions'){response.writeHead(404);response.end();return;}
 try{
  const chunks=[];for await(const chunk of request)chunks.push(chunk);const body=JSON.parse(Buffer.concat(chunks));records.push({path:request.url,body});
  const messages=body.messages??[];const title=JSON.stringify(messages).includes('Generate a concise title');let delta,finish;
  if(title){delta={role:'assistant',content:'{"title":"Browser native smoke"}'};finish='stop';}
  else{
   const observed=observeOwnedProcesses(`provider-stage-${stage}`);if(stage===6&&observed.some(row=>/chrome/i.test(row.comm)))throw Error('browser_close returned while owned Chrome descendants are still running');
   const receipts=messages.filter(message=>message.role==='tool');const last=receipts.at(-1);if(last){const value=text(last.content);toolResults.push({stage:stage-1,toolCallId:last.tool_call_id,content:last.content,text:value});if(/### Error|isError[^\n]*true/.test(value))throw Error(`actual MCP tool failure: ${value}`);}
   const catalog=new Map((body.tools??[]).map(tool=>[tool.function?.name,tool.function]));
   const name=leaf=>{const candidates=[...catalog.keys()].filter(value=>value?.endsWith(`__${leaf}`)||value===leaf);if(candidates.length!==1)throw Error(`formal model tool catalog must contain exactly one ${leaf}: ${candidates}`);return candidates[0];};
   let leaf,args;
   switch(stage){
    case 0:leaf='browser_navigate';args={url:`http://127.0.0.1:${model.address().port}/fixture`};break;
    case 1:{const snapshot=snapshotText(last?.content);leaf='browser_type';args={target:findTarget(snapshot,'textbox','Fixture name'),text:'MCP source runtime'};break;}
    case 2:leaf='browser_snapshot';args={};break;
    case 3:{const snapshot=snapshotText(last?.content);if(!snapshot.includes('MCP source runtime'))throw Error('snapshot did not retain typed value');leaf='browser_click';args={target:findTarget(snapshot,'button','Apply greeting')};break;}
    case 4:leaf='browser_snapshot';args={};break;
    case 5:if(!snapshotText(last?.content).includes('Hello, MCP source runtime'))throw Error('actual resulting page lacks expected greeting');leaf='browser_close';args={};break;
    case 6:delta={role:'assistant',content:'BROWSER_NATIVE_LOADER_OK'};finish='stop';break;
    default:throw Error('unexpected repeated provider turn');
   }
   if(leaf){const tool=name(leaf);delta={role:'assistant',tool_calls:[{index:0,id:`browser-smoke-${stage}`,type:'function',function:{name:tool,arguments:JSON.stringify(args)}}]};finish='tool_calls';owned.push({stage,observedToolName:tool,arguments:args});stage++;}
  }
  response.writeHead(200,{'content-type':'text/event-stream'});for(const data of [{choices:[{index:0,delta,finish_reason:null}]},{choices:[{index:0,delta:{},finish_reason:finish}],usage:{prompt_tokens:8,completion_tokens:4,total_tokens:12}}])response.write('data: '+JSON.stringify(data)+'\n\n');response.end('data: [DONE]\n\n');
 }catch(error){modelErrors.push(error.stack);response.writeHead(500);response.end(error.message);}
});
await new Promise(resolve=>model.listen(0,'127.0.0.1',resolve));
const reservation=net.createServer();await new Promise(resolve=>reservation.listen(0,'127.0.0.1',resolve));const port=reservation.address().port;await new Promise(resolve=>reservation.close(resolve));if(port===18789)throw Error('fixed port forbidden');
const config=path.join(output,'pilot/pilotdeck.yaml');fs.writeFileSync(config,`schemaVersion: 1\nagent:\n  model: mock/deterministic\n  maxTurns: 12\nmodel:\n  providers:\n    mock:\n      protocol: openai\n      url: http://127.0.0.1:${model.address().port}/v1\n      apiKey: local-mock-only\n      models:\n        deterministic: {}\ntelemetry:\n  enabled: false\n`);
const env={HOME:path.join(output,'home'),PILOT_HOME:path.join(output,'pilot'),PATH:process.env.PATH??'',TMPDIR:path.join(output,'tmp'),PILOTDECK_CONFIG_PATH:config,PILOTDECK_GATEWAY_TOKEN:'browser-fixture-token',PILOTDECK_BUNDLED_PLUGINS_DIR:path.join(output,'builtin-plugins'),PILOTDECK_BUNDLED_SKILLS_DIR:path.join(output,'skills'),PILOTDECK_RUNTIME_ROOT:runtime};
server=spawn(binary,['server','--host','127.0.0.1','--port',String(port)],{cwd:path.join(output,'project'),env,stdio:['ignore','pipe','pipe'],detached:true});server.stdout.on('data',bytes=>{stdout+=String(bytes);});server.stderr.on('data',bytes=>{stderr+=String(bytes);});
const started={driverPid:process.pid,serverPid:server.pid,binary,binarySha256:createHash('sha256').update(fs.readFileSync(binary)).digest('hex'),output,profilePolicy:'Rust perSession --user-data-dir under isolated TMPDIR; no CDP attach'};fs.writeFileSync(path.join(output,'started.json'),JSON.stringify(started,null,2)+'\n');console.log(JSON.stringify({event:'started',...started}));
let socket,error,completed=false,exit;
try{
 const deadline=Date.now()+45000;for(;;){if(server.exitCode!==null||server.signalCode!==null)throw Error('formal Rust server exited before readiness');try{await new Promise((resolve,reject)=>{const probe=net.connect(port,'127.0.0.1',()=>{probe.end();resolve();});probe.once('error',reject);});break;}catch{if(Date.now()>deadline)throw Error('formal Rust server readiness timeout');await new Promise(resolve=>setTimeout(resolve,30));}}
 socket=new WebSocket(`ws://127.0.0.1:${port}/ws`);await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
 const done=new Promise((resolve,reject)=>{
  socket.addEventListener('message',message=>{const frame=JSON.parse(String(message.data));frames.push(frame);if(frame.type==='hello_ok'){socket.send(JSON.stringify({type:'request',id:'browser-turn',method:'submit_turn',params:{sessionKey:'browser-source-session',channelKey:'test',runId:'browser-source-run',message:'Use the browser-use MCP tools on the local fixture.',basePermissionMode:'bypassPermissions'}}));}else if(frame.event?.type==='permission_request'){const allowed=new Set(['browser_navigate','browser_type','browser_snapshot','browser_click','browser_close']);const leaf=frame.event.toolName?.split('__').at(-1);if(!allowed.has(leaf)){reject(Error('unexpected permission tool'));return;}socket.send(JSON.stringify({type:'request',id:'allow-'+frame.event.requestId,method:'permission_decide',params:{sessionKey:'browser-source-session',requestId:frame.event.requestId,decision:'allow'}}));}else if(frame.id==='browser-turn'&&(frame.final||frame.type==='response')){if(frame.event?.finishReason==='completed'&&stage===6&&modelErrors.length===0){completed=true;resolve();}else reject(Error('formal turn failed or did not complete all actual browser tools'));}});
  socket.addEventListener('error',()=>reject(Error('formal Gateway WebSocket error')),{once:true});
 });
 socket.send(JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'browser-resource-smoke',clientVersion:'1.0.0',token:'browser-fixture-token'}));
 let timer;try{await Promise.race([done,new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('formal browser smoke deadline')),90000);})]);}finally{clearTimeout(timer);}
 if(!frames.some(frame=>frame.event?.type==='assistant_text_delta'&&frame.event.text==='BROWSER_NATIVE_LOADER_OK'))throw Error('native final assistant event missing');
}catch(failure){error=failure;}
finally{
 if(socket)socket.close();
 if(server.exitCode===null&&server.signalCode===null)server.kill('SIGTERM');
 exit=await new Promise(resolve=>{if(server.exitCode!==null||server.signalCode!==null){resolve({code:server.exitCode,signal:server.signalCode});return;}let timer=setTimeout(()=>{error??=Error('formal server failed to release after shutdown');process.kill(-server.pid,'SIGKILL');},15000);server.once('close',(code,signal)=>{clearTimeout(timer);resolve({code,signal});});});
 const releaseDeadline=Date.now()+3000;let remaining=[];
 for(;;){remaining=[...observedPids].filter(pid=>{try{process.kill(pid,0);return true;}catch{return false;}});if(!remaining.length||Date.now()>releaseDeadline)break;await new Promise(resolve=>setTimeout(resolve,20));}
 if(remaining.length)error??=Error(`owned descendants survived formal shutdown: ${remaining}`);
 if(completed&&!profilePaths.length)error??=Error('no actual isolated user-data-dir was observed');
 for(const profile of profilePaths)if(fs.existsSync(profile))error??=Error(`temporary perSession profile survived shutdown: ${profile}`);
 await new Promise(resolve=>model.close(resolve));
 const digest=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
 const manifest={scope:'actual sealed Rust builtin loader/native model-tool loop/official MCP/isolated Chrome on local HTML; no user profile or account',binary,binarySha256:digest(binary),runtimeManifestSha256:digest(path.join(runtime,'runtime-manifest.json')),pluginSpec:spec,pluginSpecSha256:digest(path.join(plugin,'plugin.json')),home:env.HOME,pilotHome:env.PILOT_HOME,tmp:env.TMPDIR,gatewayPort:port,serverPid:server.pid,exit,stages:stage,completed,modelErrors,error:error?.stack??null,toolCalls:owned,profilePaths};
 for(const [name,value]of Object.entries({'manifest.json':manifest,'provider-requests.json':records,'gateway-frames.json':frames,'tool-results.json':toolResults,'owned-process-observations.json':processObservations}))fs.writeFileSync(path.join(output,name),JSON.stringify(value,null,2)+'\n');fs.writeFileSync(path.join(output,'server.stdout'),stdout);fs.writeFileSync(path.join(output,'server.stderr'),stderr);fs.writeFileSync(path.join(output,'fixture.html'),html);
}
if(error)throw error;
console.log(JSON.stringify({status:'PASS',output,completed,stages:stage,exit},null,2));
