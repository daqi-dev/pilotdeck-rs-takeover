#!/usr/bin/env node
// Formal server entry only: no product imports, patches or injected gateways.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import os from 'node:os';
import {spawn, spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {sourceIdentity} from './source-identity.mjs';

const BASELINE='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const ACCOUNT='+10000000000', SOURCE='+10000000001';
const json=value=>JSON.stringify(value,null,2)+'\n';
const hash=value=>createHash('sha256').update(value).digest('hex');
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(fn,label,ms=20000){const end=Date.now()+ms;while(!fn()){assert.ok(Date.now()<end,`deadline: ${label}`);await sleep(10);}}
function snapshot(root){const files=[];function walk(dir){for(const e of fs.readdirSync(dir,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))){const target=path.join(dir,e.name);if(e.isSymbolicLink())files.push({path:path.relative(root,target),symlink:fs.readlinkSync(target)});else if(e.isDirectory())walk(target);else if(e.isFile()){const bytes=fs.readFileSync(target);files.push({path:path.relative(root,target),sha256:hash(bytes),base64:bytes.toString('base64')});}}}walk(root);return files;}
function config(port){return `schemaVersion: 1
agent:
  model: h1/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    h1:
      protocol: openai
      url: http://127.0.0.1:${port}/v1
      apiKey: h1-fake-local-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
adapters:
  signal:
    enabled: true
    extra:
      restUrl: http://127.0.0.1:${port}
      account: '${ACCOUNT}'
telemetry:
  enabled: false
`;}
function envelope(message,extra={}){return {envelope:{source:SOURCE,dataMessage:{message},...extra}};}
function lastUser(body){const messages=body.messages??[];return [...messages].reverse().find(m=>m.role==='user')?.content;}
export function sessionContract(registry,models){
  const rows=registry.records;assert.equal(rows.length,5,'one durable run per accepted turn');
  const inputs=['中文 hello','synced text','long-answer','hold-turn','after-reconnect'];
  const next=rows[2].sessionKey;assert.match(next,/^signal:chat=dm:\+10000000001:s_[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.deepEqual(rows.map(r=>r.sessionKey),[`signal:chat=dm:${SOURCE}:general`,'signal:chat=group:group-one:general',next,next,next],'new session replaces only DM mapping and survives reconnect');
  assert.equal(new Set(rows.map(r=>r.runId)).size,5);for(const row of rows)assert.match(row.runId,/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  const contract=rows.map((row,i)=>{const request=JSON.parse(row.requestMaterial);assert.deepEqual(request,{channelKey:'signal',message:inputs[i],sessionKey:row.sessionKey});assert.equal(row.state,'completed');assert.equal(row.result.type,'turn_completed');return {message:request.message,sessionKey:row.sessionKey===next?`signal:chat=dm:${SOURCE}:s_<generated-uuid>`:row.sessionKey,state:row.state};});
  const histories=models.filter(m=>m.stream).map(m=>m.messages.filter(x=>x.role==='user').map(x=>x.content));
  assert.deepEqual(histories,[['中文 hello'],['synced text'],['long-answer'],['long-answer','hold-turn'],['long-answer','hold-turn','after-reconnect']],'model history reflects actual session boundaries');
  return {runs:contract,userHistories:histories};
}
export async function run({baseline,out,command,args,mutate=false,scenario='traffic',side='ts',candidate=null}){
  assert.equal(process.version,'v22.22.0');
  baseline=fs.realpathSync(baseline);
  const git=spawnSync('git',['rev-parse','HEAD'],{cwd:baseline,encoding:'utf8'});assert.equal(git.status,0);assert.equal(git.stdout.trim(),BASELINE);
  fs.mkdirSync(out);out=fs.realpathSync(out);
  fs.writeFileSync(path.join(out,'source-identity.json'),json(await sourceIdentity(baseline)));
  const home=path.join(out,'fixture/home'),pilot=path.join(home,'.pilotdeck'),project=path.join(out,'fixture/project'),tmp=path.join(out,'fixture/tmp');
  for(const dir of [pilot,project,tmp,path.join(out,'fixture/skills'),path.join(out,'fixture/plugins')])fs.mkdirSync(dir,{recursive:true});
  fs.writeFileSync(path.join(pilot,'.legacy-bundled-skills-migrated-v1'),'completed\n');
  const records=[],receives=[],sends=[],models=[],problems=[],sockets=new Set(),ownership=[];let heldModel=null;
  const log=(kind,details={})=>{const row={kind,at:new Date().toISOString(),...details};records.push(row);fs.appendFileSync(path.join(out,'network.jsonl'),JSON.stringify(row)+'\n');};
  const reply=(res,text)=>{res.writeHead(200,{'content-type':'text/event-stream'});for(const delta of [{role:'assistant'},{content:text}])res.write(`data: ${JSON.stringify({id:'h1-local-model',object:'chat.completion.chunk',model:'deterministic',choices:[{index:0,delta,finish_reason:null}]})}\n\n`);res.write(`data: ${JSON.stringify({id:'h1-local-model',object:'chat.completion.chunk',model:'deterministic',choices:[{index:0,delta:{},finish_reason:'stop'}],usage:{prompt_tokens:10,completion_tokens:3,total_tokens:13}})}\n\n`);res.end('data: [DONE]\n\n');};
  const server=http.createServer((req,res)=>{
    if(req.method==='GET'&&req.url===`/v1/receive/${encodeURIComponent(ACCOUNT)}`){log('receive',{method:req.method,url:req.url,headers:req.headers});receives.push(res);res.writeHead(200,{'content-type':'text/event-stream'});res.write(':ok\n');res.on('close',()=>log('receive-closed',{index:receives.indexOf(res)}));return;}
    const chunks=[];req.on('data',b=>chunks.push(b));req.on('end',()=>{const raw=Buffer.concat(chunks).toString();log('request',{method:req.method,url:req.url,headers:req.headers,raw});if(req.method==='POST'&&req.url==='/v2/send'){sends.push(JSON.parse(raw));res.end('{}');}else if(req.method==='POST'&&req.url==='/v1/chat/completions'){const body=JSON.parse(raw);models.push(body);const index=models.length-1;res.on('close',()=>log('model-response-closed',{index}));const user=lastUser(body);if(!body.stream){res.setHeader('content-type','application/json');res.end(JSON.stringify({choices:[{message:{role:'assistant',content:'fixture-title'},finish_reason:'stop'}]}));}else if(JSON.stringify(user).includes('hold-turn'))heldModel=res;else reply(res,mutate?'injected-wrong-answer':JSON.stringify(user).includes('long-answer')?'a'.repeat(1999)+'😀z':'signal-answer');}else{problems.push(`${req.method} ${req.url}`);res.writeHead(404);res.end('{}');}});
  });
  server.on('connection',socket=>{sockets.add(socket);socket.on('close',()=>sockets.delete(socket));});
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
  const port=server.address().port;assert.notEqual(port,18789);
  fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),config(port));
  const env={PATH:process.env.PATH,HOME:home,USERPROFILE:home,CFFIXED_USER_HOME:home,PILOT_HOME:pilot,PILOTDECK_CONFIG_PATH:path.join(pilot,'pilotdeck.yaml'),TMPDIR:tmp,LANG:'C',LC_ALL:'C',TZ:'UTC',TERM:'dumb',NO_COLOR:'1',NODE_NO_WARNINGS:'1',DOCKER_CONTAINER:'1',COMMIT_HASH:'h1-local-fixture',PILOTDECK_BUNDLED_SKILLS_DIR:path.join(out,'fixture/skills'),PILOTDECK_BUNDLED_PLUGINS_DIR:path.join(out,'fixture/plugins')};
  command??=process.execPath;args??=[path.join(baseline,'dist/src/cli/pilotdeck.js'),'server','--port','0','--host','127.0.0.1'];
  fs.writeFileSync(path.join(out,'identity.json'),json({baseline:BASELINE,side,candidate,driverSha256:hash(fs.readFileSync(fileURLToPath(import.meta.url))),platform:`${os.platform()}-${os.arch()}`,node:process.version,command,args,commandHash:hash(fs.readFileSync(command)),entryHash:fs.existsSync(args[0])?hash(fs.readFileSync(args[0])):null,env,cwd:project,port,mutate}));fs.writeFileSync(path.join(out,'files-before.json'),json(snapshot(path.join(out,'fixture'))));
  const child=spawn(command,args,{cwd:project,env,stdio:['ignore','pipe','pipe']});const stdout=[],stderr=[];let status=null;const signals=[];
  child.stdout.on('data',b=>stdout.push(b));child.stderr.on('data',b=>stderr.push(b));child.on('error',e=>problems.push(e.message));const exit=new Promise(resolve=>child.on('close',(code,signal)=>{status={code,signal};resolve(status);}));
  const signal=(name,reason)=>{if(status)return;const row={pid:child.pid,signal:name,reason,sent:false};try{row.sent=child.kill(name);}catch(e){row.error=e.message;}signals.push(row);};
  let failure=null;
  try{
    await until(()=>receives.length===1&&Buffer.concat(stdout).includes('PilotDeck server listening:'),'formal server ready');
    if(scenario==='reload-held'){
      receives[0].write(JSON.stringify(envelope('hold-turn'))+'\n');await until(()=>heldModel!==null,'held provider connection');
      const state=phase=>{let ownedPidAlive=false;try{process.kill(child.pid,0);ownedPidAlive=true;}catch{}const registry=path.join(pilot,'runs/registry.json'),row={phase,pid:child.pid,ownedPidAlive,processExited:status!==null,receiveClosed:records.filter(r=>r.kind==='receive-closed').length,modelConnectionOpen:!heldModel.destroyed&&!heldModel.writableEnded,modelSocketDestroyed:heldModel.socket?.destroyed??null,runs:fs.existsSync(registry)?JSON.parse(fs.readFileSync(registry)).records:[]};ownership.push(row);fs.writeFileSync(path.join(out,'ownership.json'),json(ownership));};
      state('before-disable');fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),config(port).replace('    enabled: true','    enabled: false'));
      await until(()=>Buffer.concat(stdout).includes('signal=stopped'),'config disable stops channel');await sleep(500);state('after-disable');
      const serverUrl=Buffer.concat(stdout).toString().match(/PilotDeck server listening: (http:\/\/[^\s]+)/)[1];const health=await fetch(serverUrl+'/health');log('server-health-after-disable',{status:health.status,body:await health.text()});assert.equal(health.status,200);assert.equal(status,null,'server remains alive');assert.equal(ownership[1].receiveClosed,1,'disabled adapter closes receive stream');assert.equal(ownership[1].ownedPidAlive,true);assert.equal(ownership[1].modelConnectionOpen,side==='ts',side==='ts'?'baseline leaves active provider connection after channel stop':'Rust releases its owned provider connection before stop completes');
      fs.writeFileSync(path.join(pilot,'pilotdeck.yaml'),config(port));await until(()=>receives.length===2,'config reenable creates new adapter');state('after-reenable');
      if(!heldModel.destroyed)reply(heldModel,'released-answer');await until(()=>heldModel.destroyed||heldModel.writableEnded,'release held provider');await sleep(150);assert.equal(sends.length,0,'stopped adapter does not send late result');
      receives[1].write(JSON.stringify(envelope('after-reload'))+'\n');await until(()=>sends.length===1,'new adapter answers');assert.equal(sends[0].message,'signal-answer');
      signal('SIGINT','user-cancel');await Promise.race([exit,sleep(10000).then(()=>{throw Error('SIGINT exit deadline');})]);assert.deepEqual(status,{code:0,signal:null});assert.deepEqual(problems,[]);
    }else{
    // Ignored lines and UTF-8 split across writes; no transport shortcuts.
    receives[0].write('data: [DONE]\r\ninvalid\n\n');
    const data=Buffer.from('data: '+JSON.stringify(envelope('中文 hello'))+'\r\n');const split=data.indexOf(Buffer.from('中'))+1;receives[0].write(data.subarray(0,split));await sleep(20);receives[0].write(data.subarray(split));
    await until(()=>sends.length===1,'first model answer');
    assert.equal(sends[0].message,'signal-answer','first reply preserves actual model output');
    receives[0].write(JSON.stringify(envelope('/new'))+'\n');await until(()=>sends.length===2,'new session acknowledgement');
    receives[0].write('data: '+JSON.stringify(envelope('ignored ordinary',{syncMessage:{sentMessage:{message:'synced text'}},dataMessage:{message:'ordinary',groupInfo:{groupId:'group-one'}}}))+'\n');await until(()=>sends.length===3,'group/source reply');
    receives[0].write(JSON.stringify(envelope('long-answer'))+'\n');await until(()=>sends.length===5,'UTF16 split reply');
    receives[0].write(JSON.stringify(envelope('hold-turn'))+'\n');await until(()=>heldModel!==null,'held model request');receives[0].write(JSON.stringify(envelope('must-skip'))+'\n');await until(()=>Buffer.concat(stdout).includes('already active, skipping')||Buffer.concat(stderr).includes('already active, skipping'),'busy skip');reply(heldModel,'released-answer');await until(()=>sends.length===6,'released turn');
    receives[0].end();await until(()=>receives.length===2,'clean EOF reconnect');
    receives[1].write(JSON.stringify(envelope('after-reconnect'))+'\n');await until(()=>sends.length===7,'reconnected turn');
    signal('SIGINT','user-cancel');await Promise.race([exit,sleep(10000).then(()=>{throw Error('SIGINT exit deadline');})]);
    assert.deepEqual(status,{code:0,signal:null});assert.deepEqual(problems,[]);assert.equal(signals.length,1);assert.equal(signals[0].sent,true);
    assert.equal(models.filter(m=>m.stream).length,5);assert.equal(records.filter(r=>r.kind==='receive-closed').length,2,'product closes both receive streams before collector cleanup');assert.deepEqual(sends.map(s=>s.recipients),Array.from({length:7},()=>[SOURCE]));assert.ok(sends.every(s=>s.number===ACCOUNT));assert.deepEqual(sends.map(s=>s.message),['signal-answer','已创建新会话。','signal-answer','a'.repeat(1999)+'\ud83d','\ude00z','released-answer','signal-answer']);
    fs.writeFileSync(path.join(out,'session-contract.json'),json(sessionContract(JSON.parse(fs.readFileSync(path.join(pilot,'runs/registry.json'))),models)));
    }
  }catch(e){failure=e.stack??String(e);}finally{
    if(!status){signal('SIGTERM','failure-cleanup');await Promise.race([exit,sleep(1000)]);if(!status){signal('SIGKILL','failure-cleanup');await exit;}}
    for(const s of sockets)s.destroy();await new Promise(resolve=>server.close(resolve));
    fs.writeFileSync(path.join(out,'stdout.bin'),Buffer.concat(stdout));fs.writeFileSync(path.join(out,'stderr.bin'),Buffer.concat(stderr));fs.writeFileSync(path.join(out,'files-after.json'),json(snapshot(path.join(out,'fixture'))));
  }
  const contract={sends,modelInputs:models.filter(m=>m.stream).map(lastUser),receiveCount:receives.length,status,signal:signals.map(({signal,reason,sent})=>({signal,reason,sent}))};
  fs.writeFileSync(path.join(out,'contract.json'),json(contract));fs.writeFileSync(path.join(out,'result.json'),json({pid:child.pid,status,signals,problems,failure,contract}));
  if(failure)throw Error(failure);return contract;
}
export function compare(a,b){assert.deepEqual(a,b,'formal server contract differs');}
async function main(){
  const args=process.argv.slice(2),options={};for(let i=0;i<args.length;i+=2)options[args[i].replace(/^--/,'')]=args[i+1];assert.ok(options.baseline&&options.out);
  let command,commandArgs,candidate=null,side='ts';
  if(options['rust-bin']){assert.match(options['rust-sha']??'',/^[0-9a-f]{40}$/);assert.ok(options['rust-root']);const root=fs.realpathSync(options['rust-root']),head=spawnSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'});assert.equal(head.status,0);assert.equal(head.stdout.trim(),options['rust-sha']);candidate={root,sha:options['rust-sha']};command=fs.realpathSync(options['rust-bin']);commandArgs=['server','--port','0','--host','127.0.0.1'];side='rust';}
  const contract=await run({baseline:options.baseline,out:path.resolve(options.out),mutate:options.mutate==='model-output',scenario:options.scenario??'traffic',command,args:commandArgs,side,candidate});if(options.compare){const prior=JSON.parse(fs.readFileSync(path.join(options.compare,'contract.json')));compare(prior,contract);}console.log(json({passed:true,side,out:options.out}));
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url))main().catch(e=>{console.error(e.stack);process.exitCode=1;});
