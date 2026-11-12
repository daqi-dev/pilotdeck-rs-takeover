import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import net from 'node:net';
import {createRequire} from 'node:module';
import {spawn,spawnSync} from 'node:child_process';
import {createHash,randomUUID} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {setTimeout as delay} from 'node:timers/promises';
const baseline='/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const metadata=JSON.parse(fs.readFileSync(new URL('./metadata.json',import.meta.url),'utf8'));
const revision='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
assert.equal(process.version,'v22.22.0');assert.equal(spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),revision);
const [side,output,binary,expectation='safe']=process.argv.slice(2);assert.ok(side==='ts'||side==='rust');assert.ok(output);if(side==='rust')assert.ok(binary);
const out=path.resolve(output);fs.mkdirSync(out);
const hash=data=>createHash('sha256').update(data).digest('hex');
assert.equal(metadata.revision,revision);assert.equal(metadata.node,process.version);
for(const [file,expected] of Object.entries(metadata.sha256))assert.equal(hash(fs.readFileSync(path.join(baseline,file))),expected,`baseline file identity: ${file}`);
assert.equal(spawnSync('git',['-C',baseline,'diff','--quiet','HEAD','--','src','packages']).status,0,'read-only baseline tracked sources');
const save=(name,value)=>fs.writeFileSync(path.join(out,name),JSON.stringify(value,null,2)+'\n',{flag:'wx'});
const home=path.join(out,'home'),pilot=path.join(out,'pilot'),project=path.join(out,'project'),control=path.join(out,'control');
for(const dir of [home,pilot,project,control])fs.mkdirSync(dir);
const calls=[];const mock=http.createServer((req,res)=>{calls.push(req.url);res.writeHead(503);res.end('unexpected model request');});await new Promise(resolve=>mock.listen(0,'127.0.0.1',resolve));
fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),`schemaVersion: 1\nagent: {model: fixture/model}\nmodel:\n  providers:\n    fixture:\n      protocol: openai\n      url: http://127.0.0.1:${mock.address().port}/v1\n      apiKey: mock-only\n      models:\n        model: {}\ntelemetry: {enabled: false}\n`);
const env={PATH:'',HOME:home,USERPROFILE:home,CFFIXED_USER_HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),TMPDIR:out,LANG:'C',LC_ALL:'C',TZ:'UTC',NODE_NO_WARNINGS:'1',NO_COLOR:'1',...(side==='ts'?{PDRS_RESTORE_FAULT_DIR:control}:{PILOTDECK_NATIVE_STORAGE_STIMULUS:control})};
const {WebSocket}=createRequire(path.join(baseline,'package.json'))('ws');
const bounded=async(promise,ms,label)=>{let timer;try{return await Promise.race([promise,new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(label)),ms);})]);}finally{clearTimeout(timer);}};
const tree=(dir)=>fs.readdirSync(dir,{withFileTypes:true}).flatMap(item=>item.isDirectory()?tree(path.join(dir,item.name)):[path.join(dir,item.name)]);
const target=key=>tree(pilot).find(file=>path.basename(file)===`${key}.jsonl`);
const temps=()=>tree(pilot).filter(file=>file.endsWith('.restore.tmp')||/\.ledger-[^/]+\.tmp$/.test(file));
const archive={schemaVersion:1,format:'portable_text_messages',title:'Imported safely',messages:[{role:'user',text:'question'},{role:'assistant',text:'answer'}]};
const runs=[],operations=[];let active;let activeControl=control;
function arm(id,mode){if(side==='ts')fs.writeFileSync(path.join(activeControl,'control.json'),JSON.stringify({id,mode}));else fs.writeFileSync(path.join(activeControl,'stimulus.json'),JSON.stringify({writer:{[mode==='fault'?'fault':'barrier']:{id,key:'accepted_input',boundMs:30000}}}));}
function disarm(){for(const name of ['control.json','stimulus.json'])fs.writeFileSync(path.join(activeControl,name),'{}');}
async function boot(){
 const reserve=net.createServer();await new Promise(resolve=>reserve.listen(0,'127.0.0.1',resolve));const port=reserve.address().port;await new Promise(resolve=>reserve.close(resolve));
 const command=side==='ts'?process.execPath:path.resolve(binary);
 const args=[...(side==='ts'?['--import',fileURLToPath(new URL('./ts-append-fault.mjs',import.meta.url)),path.join(baseline,'dist/src/cli/pilotdeck.js')]:[]),'server','--host','127.0.0.1','--port',String(port)];
 activeControl=path.join(control,`run-${runs.length+1}`);fs.mkdirSync(activeControl);
 const childEnv={...env,...(side==='ts'?{PDRS_RESTORE_FAULT_DIR:activeControl}:{PILOTDECK_NATIVE_STORAGE_STIMULUS:activeControl})};
 const row={command,args,env:childEnv,cwd:project,stdout:'',stderr:'',rpc:[],forceKilled:false};runs.push(row);
 const child=spawn(command,args,{cwd:project,env:childEnv,stdio:['ignore','pipe','pipe']});row.pid=child.pid;child.on('error',error=>{row.spawnError=error.message;});child.stdout.on('data',d=>row.stdout+=d);child.stderr.on('data',d=>row.stderr+=d);
 const closed=new Promise(resolve=>child.once('close',(code,signal)=>{row.exitCode=code;row.signal=signal;resolve();}));
 active={row,child,closed,socket:null};
 for(let i=0;i<300;i++){try{const r=await fetch(`http://127.0.0.1:${port}/health`,{headers:{connection:'close'},signal:AbortSignal.timeout(150)});if(r.ok){row.ready=true;break;}}catch{}assert.equal(row.spawnError,undefined);assert.equal(child.exitCode,null,'server exited before health');await delay(30);}assert.equal(row.ready,true);
 const socket=new WebSocket(`ws://127.0.0.1:${port}/ws`);active.socket=socket;const pending=new Map();
 socket.on('message',bytes=>{const frame=JSON.parse(String(bytes));row.rpc.push({direction:'receive',frame});pending.get(frame.type==='hello_ok'?'hello':frame.id)?.(frame);});
 await bounded(new Promise((resolve,reject)=>{socket.once('open',resolve);socket.once('error',reject);}),3000,'WS open timeout');
 const send=(frame,key)=>bounded(new Promise(resolve=>{pending.set(key,resolve);row.rpc.push({direction:'send',frame});socket.send(JSON.stringify(frame));}),5000,'RPC timeout');
 await send({type:'hello',protocolVersion:'1.2',clientName:'s1-restore-fixture',clientVersion:'0.1.0',token:fs.readFileSync(path.join(pilot,'server-token'),'utf8').trim()},'hello');
 active.rpc=(method,params)=>{const id=randomUUID();return send({type:'request',id,method,params},id);};
}
async function shutdown(signal='SIGTERM'){
 const current=active;if(!current)return;active=null;
 if(current.socket?.readyState!==3){const closed=new Promise(resolve=>current.socket.once('close',resolve));current.socket.close();try{await bounded(closed,1000,'WS close timeout');}catch{current.socket.terminate();}}
 if(current.child.exitCode===null&&current.child.signalCode===null)current.child.kill(signal);
 try{await bounded(current.closed,5000,'server shutdown timeout');}catch(error){current.row.forceKilled=true;current.child.kill('SIGKILL');await bounded(current.closed,2000,'server kill timeout');throw error;}
}
async function witness(id){
 for(let i=0;i<300;i++){
  if(side==='ts'){const file=path.join(activeControl,`${id}.json`);if(fs.existsSync(file))return JSON.parse(fs.readFileSync(file,'utf8'));}
  else {const dir=path.join(activeControl,'witness');if(fs.existsSync(dir)){const found=fs.readdirSync(dir).filter(name=>name.endsWith('.json')).map(name=>JSON.parse(fs.readFileSync(path.join(dir,name),'utf8'))).filter(row=>row.phase==='block'&&row.key==='accepted_input').sort((a,b)=>b.seq-a.seq)[0];if(found)return found;}}
  await delay(10);
 }
 throw new Error(`hold receipt absent: ${id}`);
}
function release(id,marker){fs.writeFileSync(path.join(activeControl,'release.json'),JSON.stringify(side==='ts'?{id}:{seq:marker.seq}));}
async function restore(key,value=archive){return active.rpc('restore_session_transcript',{sessionKey:key,archive:value});}
async function read(key){return active.rpc('export_session_transcript',{sessionKey:key});}
let failure;
try{
 await boot();arm('middle-fault','fault');
 const failed=await restore('s1-fault');assert.ok(failed.error,'controlled append must reject');
 const left=target('s1-fault');operations.push({name:'middle-fault',response:failed,finalPresent:!!left,finalBytes:left?fs.readFileSync(left,'utf8'):null,temps:temps().map(file=>path.relative(pilot,file))});
 const retry=await restore('s1-fault');operations.push({name:'retry-after-fault',response:retry});
 if(expectation==='unsafe'){assert.ok(left);assert.ok(retry.error);}else{assert.equal(left,undefined);assert.equal(temps().length,0);assert.equal(retry.error,undefined);assert.equal(failed.error.code,'EIO');}
 disarm();const success=await restore('s1-success');assert.equal(success.error,undefined);
 const successPath=target('s1-success');assert.ok(successPath);const successBytes=fs.readFileSync(successPath,'utf8');
 const entries=successBytes.trim().split('\n').map(line=>JSON.parse(line));
 assert.deepEqual(entries.map(row=>Object.keys(row)),[
  ['type','metadata','sessionId','turnId','sequence','createdAt','entryId','parentEntryId'],
  ['type','messages','metadata','sessionId','turnId','sequence','createdAt','entryId','parentEntryId'],
 ]);
 assert.deepEqual(entries.map(row=>row.type),['session_metadata','accepted_input']);
 assert.deepEqual(entries.map(row=>row.sequence),[1,2]);assert.equal(entries[0].parentEntryId,null);assert.equal(entries[1].parentEntryId,entries[0].entryId);
 assert.equal(entries[1].turnId,entries[0].turnId);assert.match(entries[0].turnId,/^sdk:restore:[0-9a-f-]{36}$/);
 assert.deepEqual(Object.keys(entries[0].metadata),['title','updatedAt']);assert.deepEqual(Object.keys(entries[1].metadata),['restoredFromGatewayArchive','archiveSchemaVersion']);
 const conflict=await restore('s1-success',{...archive,messages:[{role:'user',text:'must not replace'}]});assert.ok(conflict.error);assert.equal(fs.readFileSync(successPath,'utf8'),successBytes);
 const untitled=await restore('s1-untitled',{schemaVersion:1,format:'portable_text_messages',messages:[{role:'user',text:'untitled'}]});assert.equal(untitled.error,undefined);assert.equal(fs.readFileSync(target('s1-untitled'),'utf8').trim().split('\n').length,1);
 const before=await read('s1-success');assert.equal(before.error,undefined);assert.deepEqual(before.result,archive);
 operations.push({name:'success-and-existing-target',success,conflict,untitled,exported:before,bytes:successBytes});
 if(expectation!=='unsafe'){
  const chat=path.dirname(successPath);const boundaries=[];
  const directory=path.join(chat,'s1-directory.jsonl');fs.mkdirSync(directory);
  const dirResult=await restore('s1-directory');boundaries.push({name:'directory',response:dirResult,stillDirectory:fs.statSync(directory).isDirectory()});
  const liveReferent=path.join(out,'live-referent');fs.writeFileSync(liveReferent,'original-live-file');const live=path.join(chat,'s1-live-link.jsonl');fs.symlinkSync(liveReferent,live);
  const liveResult=await restore('s1-live-link');boundaries.push({name:'live-file-symlink',response:liveResult,stillSymlink:fs.lstatSync(live).isSymbolicLink(),referentBytes:fs.readFileSync(liveReferent,'utf8')});
  const missingReferent=path.join(out,'missing-referent');const dangling=path.join(chat,'s1-dangling.jsonl');fs.symlinkSync(missingReferent,dangling);
  const danglingResult=await restore('s1-dangling');boundaries.push({name:'dangling-symlink',response:danglingResult,stillSymlink:fs.lstatSync(dangling).isSymbolicLink(),referentExists:fs.existsSync(missingReferent),targetBytes:fs.lstatSync(dangling).isFile()?fs.readFileSync(dangling,'utf8'):null});
  const liveDirReferent=path.join(out,'live-directory');fs.mkdirSync(liveDirReferent);fs.writeFileSync(path.join(liveDirReferent,'marker'),'untouched-dir');const liveDir=path.join(chat,'s1-dir-link.jsonl');fs.symlinkSync(liveDirReferent,liveDir);
  const liveDirResult=await restore('s1-dir-link');boundaries.push({name:'live-directory-symlink',response:liveDirResult,stillSymlink:fs.lstatSync(liveDir).isSymbolicLink(),referentMarker:fs.readFileSync(path.join(liveDirReferent,'marker'),'utf8'),targetBytes:fs.lstatSync(liveDir).isFile()?fs.readFileSync(liveDir,'utf8'):null});
  arm('external-race','hold');const racing=restore('s1-race');racing.catch(()=>{});const raceMarker=await witness('external-race');
  const raceTarget=path.join(chat,'s1-race.jsonl');fs.writeFileSync(raceTarget,'external-owner-bytes');release('external-race',raceMarker);const raceResult=await racing;
  boundaries.push({name:'target-created-during-stage',response:raceResult,marker:raceMarker,targetBytes:fs.readFileSync(raceTarget,'utf8')});disarm();
  operations.push({name:'occupied-target-boundaries',boundaries});
  assert.equal(dirResult.error?.code,side==='ts'?'EISDIR':'SESSION_RESTORE_CONFLICT');assert.equal(boundaries[0].stillDirectory,true);
  assert.equal(liveResult.error?.code,'SESSION_RESTORE_CONFLICT');assert.equal(boundaries[1].stillSymlink,true);assert.equal(boundaries[1].referentBytes,'original-live-file');
  assert.equal(boundaries[2].referentExists,false);assert.equal(boundaries[3].referentMarker,'untouched-dir');
  if(side==='ts'){
   assert.equal(danglingResult.error,undefined);assert.equal(liveDirResult.error,undefined);assert.equal(raceResult.error,undefined);
   assert.equal(boundaries[2].stillSymlink,false);assert.equal(boundaries[3].stillSymlink,false);
   assert.deepEqual(JSON.parse(boundaries[4].targetBytes.trim().split('\n')[1]).messages,entries[1].messages);
  }else{
   for(const result of [danglingResult,liveDirResult,raceResult])assert.equal(result.error?.code,'SESSION_RESTORE_CONFLICT');
   assert.equal(boundaries[2].stillSymlink,true);assert.equal(boundaries[3].stillSymlink,true);assert.equal(boundaries[4].targetBytes,'external-owner-bytes');
  }
  assert.equal(temps().length,0);
 }

 await shutdown();await boot();const restarted=await read('s1-success');assert.deepEqual(restarted.result,archive);operations.push({name:'restart-read',response:restarted});
 arm('middle-hold','hold');const held=restore('s1-cancel');held.catch(()=>{});
 const marker=await witness('middle-hold');const heldPath=target('s1-cancel');operations.push({name:'middle-hold',marker,finalPresent:!!heldPath,finalBytes:heldPath?fs.readFileSync(heldPath,'utf8'):null,temps:temps().map(file=>({path:path.relative(pilot,file),bytes:fs.readFileSync(file,'utf8')}))});
 await shutdown('SIGKILL');disarm();await boot();
 const afterKill=await read('s1-cancel');const cancelRetry=await restore('s1-cancel');operations.push({name:'retry-after-process-cancel',exportedBeforeRetry:afterKill,response:cancelRetry});
 if(expectation==='unsafe'){assert.ok(heldPath);assert.ok(cancelRetry.error);}else{assert.equal(heldPath,undefined);assert.equal(afterKill.error?.code,'SESSION_NOT_FOUND');assert.equal(cancelRetry.error,undefined);}
 assert.equal(calls.length,0);await shutdown();
 assert.deepEqual(runs.map(row=>({exitCode:row.exitCode,signal:row.signal,forceKilled:row.forceKilled})),[{exitCode:0,signal:null,forceKilled:false},{exitCode:null,signal:'SIGKILL',forceKilled:false},{exitCode:0,signal:null,forceKilled:false}]);
}catch(error){failure={message:error.message,stack:error.stack};}
finally{try{await shutdown();}catch(error){failure??={message:error.message,stack:error.stack};}mock.closeAllConnections();await new Promise(resolve=>mock.close(resolve));
 for(let i=0;i<runs.length;i++){fs.writeFileSync(path.join(out,`run-${i+1}.stdout`),runs[i].stdout,{flag:'wx'});fs.writeFileSync(path.join(out,`run-${i+1}.stderr`),runs[i].stderr,{flag:'wx'});}
 save('observed.json',{side,expectation,revision,node:process.version,baselineSha256:metadata.sha256,driverSha256:hash(fs.readFileSync(fileURLToPath(import.meta.url))),binarySha256:side==='rust'?hash(fs.readFileSync(binary)):null,runs,operations,calls});if(failure)save('failure.json',failure);
}
assert.equal(failure,undefined);console.log(JSON.stringify({side,expectation,operations:operations.length,processes:runs.length,providerRequests:calls.length,passed:true}));
