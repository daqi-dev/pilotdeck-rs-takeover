#!/usr/bin/env node
// Module seam: actual adapter/helper + loopback HTTP; scripted Gateway events.
import fs from 'node:fs';import path from 'node:path';import http from 'node:http';import assert from 'node:assert/strict';import {pathToFileURL} from 'node:url';import {setTimeout as sleep} from 'node:timers/promises';import {spawnSync} from 'node:child_process';
const [baseline,out]=process.argv.slice(2);assert.ok(baseline&&out);assert.equal(process.version,'v22.22.0');assert.equal(spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');assert.ok(!fs.existsSync(out));
const isolated=path.resolve(out)+'.home';fs.mkdirSync(isolated);fs.mkdirSync(path.join(isolated,'.pilotdeck'));
process.env={PATH:process.env.PATH,HOME:isolated,USERPROFILE:isolated,PILOT_HOME:path.join(isolated,'.pilotdeck'),LANG:'C',LC_ALL:'C',TZ:'UTC'};process.chdir(isolated);
const {MattermostChannel}=await import(pathToFileURL(path.join(baseline,'dist/src/adapters/channel/mattermost/MattermostChannel.js')));
const records=[];let current;let failNext=false;
const server=http.createServer(async(req,res)=>{const chunks=[];for await(const chunk of req)chunks.push(chunk);const body=JSON.parse(Buffer.concat(chunks).toString());const status=failNext?500:200;failNext=false;current.posts.push({body,status});res.writeHead(status);res.end(status===500?'fixture rejected':'{}');});await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const wait=async predicate=>{const deadline=Date.now()+3000;while(!predicate()&&Date.now()<deadline)await sleep(5);assert.ok(predicate(),'script reached stage')};
const frame=text=>JSON.stringify({event:'posted',data:{post:JSON.stringify({user_id:'u',channel_id:'c',message:text})}});
try {for(const mode of ['elicitation','permission','permission-initial-failure']) {
 current={mode,posts:[],submits:[],answers:[],logs:[]};records.push(current);const record=current;let release;const gate=new Promise(resolve=>release=resolve);let decided=0;
 const adapter=new MattermostChannel({serverUrl:`http://127.0.0.1:${server.address().port}`,token:'fixture'});adapter.apiBase=`http://127.0.0.1:${server.address().port}`;
 adapter.logger={error:text=>record.logs.push(text),info:text=>record.logs.push(text)};
 adapter.gateway={
  async *submitTurn(input){record.submits.push(input);if(mode==='elicitation')yield {type:'elicitation_request',requestId:'e1',questions:[{id:'q',header:'h',question:'Choose',options:[{label:'A'},{label:'B'}]}]};else {yield {type:'permission_request',requestId:'p1',toolName:'write',payload:{path:'fixture.txt'}};yield {type:'permission_request',requestId:'p2',toolName:'read',payload:{path:'fixture2.txt'}};}await gate;yield {type:'assistant_text_delta',text:'finished'};},
  async respondElicitation(input){record.answers.push(input);release();return {delivered:true}},
  async permissionDecide(input){record.answers.push(input);if(++decided===2)release();return {delivered:true}},
 };
 if(mode==='permission-initial-failure')failNext=true;
 const turn=adapter.onWsMessage(frame('start'));await wait(()=>record.posts.length===1);
 if(mode==='permission-initial-failure') {await adapter.onWsMessage(frame('1'));assert.equal(record.answers.length,0);}
 await adapter.onWsMessage(frame('1'));
 if(mode!=='elicitation')await adapter.onWsMessage(frame('2'));
 await turn;assert.equal(record.submits.length,1);assert.equal(record.answers.length,mode==='elicitation'?1:2);
 }}finally{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));}
fs.writeFileSync(out,JSON.stringify({boundary:'module seam with real helper and HTTP; not production Gateway registration',records},null,2)+'\n');console.log('3 interaction traces captured');
