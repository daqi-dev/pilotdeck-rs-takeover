#!/usr/bin/env node
// Real TS adapter + helpers + local HTTP. Scripted Gateway events and an
// explicitly post-readiness module seam; never a formal-server claim.
import fs from 'node:fs';import path from 'node:path';import http from 'node:http';import assert from 'node:assert/strict';import {createHash} from 'node:crypto';import {fileURLToPath,pathToFileURL} from 'node:url';import {setTimeout as sleep} from 'node:timers/promises';import {spawnSync} from 'node:child_process';
const baseline=process.env.PDRS_BASELINE??'/Users/da/ws/pilotdeck-rs/resources/PilotDeck',out=path.resolve(process.argv[2]);fs.mkdirSync(out);assert.equal(process.version,'v22.22.0');assert.equal(spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
for(const key of Object.keys(process.env))delete process.env[key];Object.assign(process.env,{HOME:path.join(out,'home'),PILOT_HOME:path.join(out,'pilot'),PATH:''});fs.mkdirSync(process.env.HOME);fs.mkdirSync(process.env.PILOT_HOME);
const {WhatsAppChannel}=await import(pathToFileURL(path.join(baseline,'dist/src/adapters/channel/whatsapp/WhatsAppChannel.js')));
const records=[];let current,failNext=false;
const server=http.createServer(async(req,res)=>{const chunks=[];for await(const chunk of req)chunks.push(chunk);const body=JSON.parse(Buffer.concat(chunks).toString());const status=failNext?503:200;failNext=false;current.posts.push({method:req.method,path:req.url,body,status});res.writeHead(status,{'content-type':'application/json'});res.end(status===503?'{"error":"fixture denied"}':'{}');});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const wait=async predicate=>{const end=Date.now()+3000;while(!predicate()&&Date.now()<end)await sleep(5);assert.ok(predicate(),'script stage');};
try{for(const mode of ['elicitation','permission','permission-initial-failure']){
 current={mode,posts:[],submits:[],answers:[],logs:[]};const record=current;records.push(record);let release,decided=0;const gate=new Promise(resolve=>release=resolve);
 const adapter=new WhatsAppChannel({bridgeUrl:`http://127.0.0.1:${server.address().port}`});adapter.running=true;adapter.logger={error:text=>record.logs.push(text),info:text=>record.logs.push(text)};
 adapter.gateway={async *submitTurn(input){record.submits.push(input);if(mode==='elicitation')yield{type:'elicitation_request',requestId:'e1',questions:[{id:'q',header:'h',question:'Choose',options:[{label:'A'},{label:'B'}]}]};else{yield{type:'permission_request',requestId:'p1',toolName:'write',payload:{path:'fixture.txt'}};yield{type:'permission_request',requestId:'p2',toolName:'read',payload:{path:'fixture2.txt'}};}await gate;yield{type:'assistant_text_delta',text:'finished'};},async respondElicitation(input){record.answers.push(input);release();return{delivered:true};},async permissionDecide(input){record.answers.push(input);if(++decided===2)release();return{delivered:true};}};
 let id=0;const dispatch=text=>adapter.dispatch({id:String(++id),chatId:'chat',text});
 if(mode==='permission-initial-failure')failNext=true;
 const turn=dispatch('start');await wait(()=>record.posts.length===1);
 if(mode==='permission-initial-failure'){await dispatch('1');assert.equal(record.answers.length,0);}
 await dispatch('1');if(mode!=='elicitation')await dispatch('2');await turn;
 assert.equal(record.submits.length,1);assert.equal(record.answers.length,mode==='elicitation'?1:2);
}}
finally{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
fs.writeFileSync(path.join(out,'oracle.json'),JSON.stringify({boundary:'module seam: actual WhatsApp adapter/helpers/local HTTP, scripted Gateway; no formal startup claim',records},null,2)+'\n');
const sha=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');fs.writeFileSync(path.join(out,'identity.json'),JSON.stringify({node:process.version,driverSha256:sha(fileURLToPath(import.meta.url)),sourceSha256:sha(path.join(baseline,'src/adapters/channel/whatsapp/WhatsAppChannel.ts')),distSha256:sha(path.join(baseline,'dist/src/adapters/channel/whatsapp/WhatsAppChannel.js'))},null,2)+'\n');console.log(JSON.stringify({traces:records.length,sha256:sha(path.join(out,'oracle.json'))}));
