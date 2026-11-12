#!/usr/bin/env node
// Module-seam characterization only. Formal registration is server-probe.mjs.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import {pathToFileURL} from 'node:url';
import {spawnSync} from 'node:child_process';
const [baseline,out]=process.argv.slice(2); assert.ok(baseline&&out); assert.equal(process.version,'v22.22.0');
assert.equal(spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
assert.ok(!fs.existsSync(out));
const isolated=path.resolve(out)+'.home';fs.mkdirSync(isolated);fs.mkdirSync(path.join(isolated,'.pilotdeck'));
process.env={PATH:process.env.PATH,HOME:isolated,USERPROFILE:isolated,PILOT_HOME:path.join(isolated,'.pilotdeck'),LANG:'C',LC_ALL:'C',TZ:'UTC'};process.chdir(isolated);
const base=path.join(baseline,'dist/src/adapters/channel/mattermost');
const {MattermostChannel}=await import(pathToFileURL(path.join(base,'MattermostChannel.js')));
const {MattermostSessionMapper}=await import(pathToFileURL(path.join(base,'MattermostSessionMapper.js')));
const {renderMattermostEvent}=await import(pathToFileURL(path.join(base,'mattermost-render.js')));
const mapper=new MattermostSessionMapper(undefined,()=> 'fixture-uuid');
const mapping=[{chatId:'c',text:' hi '},{chatId:'c',text:'/new'},{chatId:'c',text:'next'},{chatId:'c:t',text:'/new follow'},{chatId:'c:t',text:'/new\tx'},{chatId:'\ud800',text:' \udfff '}].map(input=>({input,output:mapper.resolve(input)}));
const rendering=[{type:'assistant_text_delta',text:'a\ud800b'},{type:'assistant_thinking_delta',text:'hidden'},{type:'tool_call_started'},{type:'tool_call_finished',ok:false,toolName:'tool'},{type:'tool_call_finished',ok:false,toolCallId:'id'},{type:'tool_call_finished',ok:true},{type:'error',message:'bad'},{type:'error',message:null},{type:'tool_call_finished',ok:false,toolCallId:null},{type:'turn_completed'},{type:'elicitation_request',questions:[{header:'h',question:'q',options:[{label:'A'},{label:'B'}]}]}].map(event=>({event,output:renderMattermostEvent(event)??null}));
const posts=[]; const server=http.createServer(async(req,res)=>{const b=[];for await(const c of req)b.push(c);posts.push(JSON.parse(Buffer.concat(b).toString()));res.end('{}')});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const channel=new MattermostChannel({token:'fixture',serverUrl:`http://127.0.0.1:${server.address().port}`});
channel.apiBase=`http://127.0.0.1:${server.address().port}`;
const chunking=[];
try {for(const [name,text] of [['empty',''],['surrogate','x'.repeat(16382)+'😀z'],['newline','x'.repeat(8192)+'\n'+'y'.repeat(8192)],['space','x'.repeat(8192)+' '+'y'.repeat(8192)],['at-max','x'.repeat(16383)+'\n\nz']]){const begin=posts.length; const delivered=await channel.sendReply({channelId:'c',rootId:'r'},text);chunking.push({name,text,delivered,posts:posts.slice(begin)});}}
finally {server.closeAllConnections(); await new Promise(resolve=>server.close(resolve));}
fs.writeFileSync(out,JSON.stringify({boundary:'compiled baseline module seam, real HTTP posts; not formal server coverage',mapping,rendering,chunking},null,2)+'\n');
console.log(`${mapping.length} mappings, ${rendering.length} events, ${chunking.length} UTF16 chunk cases captured`);
