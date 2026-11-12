#!/usr/bin/env node
// Supplemental channel-level capture. Imports the fixed real channel and
// exercises real HTTP, but supplies a scripted Gateway: not a formal server.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import {pathToFileURL} from 'node:url';
import {sourceIdentity} from './source-identity.mjs';
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
const until=async(fn,label)=>{const deadline=Date.now()+10000;while(!fn()){assert.ok(Date.now()<deadline,label);await sleep(5);}};
const json=x=>JSON.stringify(x,null,2)+'\n';
function deferred(){let resolve;const promise=new Promise(r=>resolve=r);return {promise,resolve};}
const args=process.argv.slice(2),options={};for(let i=0;i<args.length;i+=2)options[args[i].slice(2)]=args[i+1];
assert.ok(options.baseline&&options.out);assert.equal(process.version,'v22.22.0');
const out=path.resolve(options.out);fs.mkdirSync(out);const home=path.join(out,'home');fs.mkdirSync(home);process.env.HOME=home;process.env.PILOT_HOME=path.join(home,'.pilotdeck');
// Clear every externally selected endpoint/proxy/credential before product import.
for(const key of Object.keys(process.env))if(/PROXY|TOKEN|API_KEY|SIGNAL_|PILOTDECK_/i.test(key))delete process.env[key];
fs.writeFileSync(path.join(out,'source-identity.json'),json(await sourceIdentity(options.baseline)));
const {SignalChannel}=await import(pathToFileURL(path.join(options.baseline,'dist/src/adapters/channel/signal/SignalChannel.js')));
const rows=[];
async function scene(id,run){
  const requests=[],sends=[],logs=[],turns=[],decisions=[],answers=[],receive=[],statuses=[],heldReplies=[],sockets=new Set();const steps=[];let get503=false,decisionFails=false,script=[];
  const server=http.createServer((req,res)=>{if(req.method==='GET'){requests.push({method:req.method,url:req.url,at:performance.now()});if(get503){get503=false;res.writeHead(503);res.end('retry-me');return;}receive.push(res);res.writeHead(200,{'content-type':'text/event-stream'});res.flushHeaders();return;}const parts=[];req.on('data',b=>parts.push(b));req.on('end',()=>{const raw=Buffer.concat(parts).toString();sends.push(JSON.parse(raw));steps.push({kind:'send',value:JSON.parse(raw)});const status=statuses.shift()??200;if(status==='hold'){heldReplies.push(res);return;}res.writeHead(status);res.end(status===200?'{}':'x'.repeat(520));});});
  server.on('connection',s=>{sockets.add(s);s.on('close',()=>sockets.delete(s));});await new Promise(r=>server.listen(0,'127.0.0.1',r));const port=server.address().port;
  const channel=new SignalChannel({restUrl:`http://127.0.0.1:${port}///`,account:'+10000000000'});
  const gateway={async *submitTurn(input){turns.push(input);steps.push({kind:'turn',value:input});const next=script.shift();if(next){for(const event of next.events??[])yield event;if(next.gate)await next.gate.promise;if(next.empty)return;}yield {type:'assistant_text_delta',text:next?.reply??'done'};},async permissionDecide(input){decisions.push(input);steps.push({kind:'decision',value:input});if(decisionFails){decisionFails=false;throw Error('fixture decision failed');}return {delivered:true};},async respondElicitation(input){answers.push(input);steps.push({kind:'answer',value:input});}};
  const start=async()=>channel.start({gateway,logger:Object.fromEntries(['info','warn','error'].map(level=>[level,message=>logs.push({level,message:message.replaceAll(`:${port}`,':<port>')})]))});
  const inbound=text=>receive.at(-1).write(JSON.stringify({envelope:{source:'+10000000001',dataMessage:{message:text}}})+'\n');
  let handle;try{await run({channel,start,setHandle:h=>handle=h,inbound,requests,sends,logs,turns,decisions,answers,receive,statuses,heldReplies,script,steps,set503:()=>get503=true,failDecision:()=>decisionFails=true});}catch(error){fs.writeFileSync(path.join(out,id+'-failure.json'),json({failure:error.stack,requests,sends,logs,turns,decisions,answers,steps}));throw error;}finally{if(handle)await handle.stop('fixture-cleanup');for(const s of sockets)s.destroy();await new Promise(r=>server.close(r));}
  const row={id,requests:requests.map(({method,url})=>({method,url})),sends,logs,turns,decisions,answers,steps};rows.push(row);fs.writeFileSync(path.join(out,id+'.json'),json(row));
}
await scene('permission-elicitation',async c=>{
  const gate=deferred();c.script.push({events:[{type:'permission_request',requestId:'p1',toolName:'write_file',payload:{path:'fixture'}}],gate});c.statuses.push(503);c.setHandle(await c.start());await until(()=>c.receive.length===1,'receive');c.inbound('permission');await until(()=>c.logs.some(l=>l.message.startsWith('signal: send HTTP 503:')),'failed prompt');
  c.inbound('1');await until(()=>c.sends.length===3,'status and retry');assert.equal(c.decisions.length,0);c.failDecision();c.inbound('2');await until(()=>c.logs.some(l=>l.message.includes('permission answer error:')),'RPC failure');assert.equal(c.decisions.length,1);c.inbound('2');await until(()=>c.decisions.length===2&&c.sends.length===4,'decision retry');gate.resolve();await until(()=>c.sends.length===5,'permission final');assert.equal(c.decisions[1].remember,true);
  const question=deferred();c.script.push({events:[{type:'elicitation_request',requestId:'e1',questions:[{header:'选择',question:'which\ud800',options:[{label:'one\udfff',description:'desc'},{label:'two',description:''}]}]}],gate:question});c.inbound('question');await until(()=>c.sends.length===6,'question');c.inbound('1');await until(()=>c.answers.length===1,'answer');assert.equal(c.sends.length,6,'answered elicitation has no acknowledgement');question.resolve();await until(()=>c.sends.length===7,'question final');assert.equal(c.turns.length,2);assert.equal(c.answers[0].answer.answers['which\ud800'],'one\udfff');
});
await scene('cron-chunks',async c=>{
  c.setHandle(await c.start());await until(()=>c.receive.length===1,'receive');c.statuses.push(500,200);
  const delivery={taskId:'t',runId:'r',sessionKey:'signal:chat=group:fixture:general',channelKey:'signal',text:'x'.repeat(2001)};
  const delivered=await c.channel.deliverCronResult(delivery);assert.equal(delivered,false);assert.equal(c.sends.length,2);assert.equal(c.sends[1].message,'x');assert.deepEqual(c.sends[0].recipients,['fixture']);
  assert.equal(await c.channel.deliverCronResult({...delivery,originChannelKey:'other'}),false);assert.equal(c.sends.length,2);c.steps.push({kind:'delivery-result',value:delivered});
});
await scene('receive-backoff-carry',async c=>{
  c.set503();c.setHandle(await c.start());await until(()=>c.receive.length===1,'retry receive');assert.ok(c.requests[1].at-c.requests[0].at>=2900);c.steps.push({kind:'backoff-at-least-2900ms',value:true});
  const raw=JSON.stringify({envelope:{source:'+10000000001',dataMessage:{message:'carried'}}});c.receive[0].end(raw.slice(0,30));await until(()=>c.receive.length===2,'EOF reconnect');c.receive[1].write(raw.slice(30)+'\n');await until(()=>c.sends.length===1,'carry reply');assert.equal(c.turns[0].message,'carried');
});
await scene('stop-owned-turn-baseline',async c=>{
  const gate=deferred();c.script.push({gate});const handle=await c.start();c.setHandle(handle);await until(()=>c.receive.length===1,'receive');c.inbound('held');await until(()=>c.turns.length===1,'held turn');await handle.stop('test-stop');
  // Observable source-state probe is explicitly supplemental. The baseline
  // returns stop while its async inbound handler still owns this chat.
  c.steps.push({kind:'active-after-stop',value:channelActive(c.channel)});assert.equal(channelActive(c.channel),1);gate.resolve();await until(()=>channelActive(c.channel)===0,'late turn release');assert.equal(c.sends.length,0);
});
await scene('cron-held-send-after-stop-baseline',async c=>{
  const handle=await c.start();c.setHandle(handle);await until(()=>c.receive.length===1,'receive');c.statuses.push('hold');
  const delivery=c.channel.deliverCronResult({taskId:'t',runId:'r',sessionKey:'signal:chat=dm:+10000000001:general',channelKey:'signal',text:'x'.repeat(2001)});
  let settled=false;delivery.finally(()=>settled=true);
  await until(()=>c.heldReplies.length===1,'actual POST pending without response headers');
  await handle.stop('stop-pending-send');await sleep(30);
  assert.equal(settled,false);assert.equal(c.heldReplies[0].destroyed,false);
  c.steps.push({kind:'stop-returned-with-provider-post-open',value:true});
  c.heldReplies[0].end('{}');assert.equal(await delivery,true);assert.equal(c.sends.length,2);assert.equal(c.sends[1].message,'x');
  c.steps.push({kind:'late-chunk-after-stop',value:true});
});
await scene('ready-empty-busy-batch',async c=>{
  const original=globalThis.fetch,reads=[];
  // Observe actual undici reader results without replacing bytes or boundaries.
  globalThis.fetch=async(...args)=>{const response=await original(...args);if(String(args[0]).includes('/v1/receive/')){const get=response.body.getReader.bind(response.body);response.body.getReader=(...options)=>{const reader=get(...options),read=reader.read.bind(reader);reader.read=()=>read().then(value=>{if(value.value)reads.push(Buffer.from(value.value).toString());return value;});return reader;};}return response;};
  try{
    c.script.push({empty:true},{empty:true});c.setHandle(await c.start());await until(()=>c.receive.length===1,'receive');
    const encode=message=>JSON.stringify({envelope:{source:'+10000000001',dataMessage:{message}}})+'\n';c.receive[0].write(encode('ready-first')+encode('ready-second'));
    await until(()=>c.logs.some(l=>l.message.includes('already active')),'ready empty turn keeps busy through batch');await sleep(30);assert.equal(c.turns.length,1);assert.equal(c.sends.length,0);assert.ok(reads.some(raw=>raw.includes('ready-first')&&raw.includes('ready-second')),'both messages in one observed reader result');c.steps.push({kind:'read-chunks',value:reads});
  }finally{globalThis.fetch=original;}
});
function channelActive(channel){return channel.activeChats.size;}
fs.writeFileSync(path.join(out,'oracle.json'),json({layer:'channel-with-scripted-gateway-real-http',rows}));
if(options.compare)assert.deepEqual(JSON.parse(fs.readFileSync(path.join(options.compare,'oracle.json'))),{layer:'channel-with-scripted-gateway-real-http',rows});
console.log(json({passed:true,cases:rows.length,out}));
