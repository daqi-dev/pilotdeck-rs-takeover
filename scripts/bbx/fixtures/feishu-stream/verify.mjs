#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import {decode} from './wire.mjs';
const here=path.dirname(fileURLToPath(import.meta.url));
const scenePath=path.resolve(here,'../../scenarios/backend-feishu-stream.json');
const scene=JSON.parse(fs.readFileSync(scenePath));
const hash=b=>createHash('sha256').update(b).digest('hex');
const text=v=>typeof v==='string'?v:Array.isArray(v)?v.filter(b=>b.type==='text').map(b=>b.text??'').join(''):'';
const only=(rows,label)=>{assert.equal(rows?.length,1,label);return rows[0]};
function httpBody(base64){const value=Buffer.from(base64,'base64').toString();try{return JSON.parse(value)}catch{return {raw:value}}}
function validate(a,receipt,raw){
 assert.equal(a.scenario,scene.name);assert.deepEqual(a.failures,[]);assert.ok(['ts','rust'].includes(a.side));assert.equal(a.evidence.side,a.side);
 const tail=raw.at(-1);assert.equal(tail?.kind,'mock-drain-complete','raw sealed only after actual mock drain');assert.deepEqual(tail,{kind:'mock-drain-complete',...receipt.mockDrain});assert.equal(tail.listeners,3);assert.ok(tail.acceptedSockets>0);assert.equal(tail.closedSockets,tail.acceptedSockets);assert.equal(tail.socketsRemaining,0);assert.equal(tail.handlersRemaining,0);assert.equal(tail.httpHandlersStarted,tail.httpHandlersCompleted);
 assert.equal(receipt.side,a.side);assert.equal(receipt.network,'linux-loopback-only');assert.equal(receipt.process.code,0);assert.equal(receipt.process.signal,null);
 assert.ok(['normal','wrongreply'].includes(receipt.variant));assert.equal(a.evidence.scenario.sha256,receipt.scenario.sha256);
 const declared=structuredClone(scene);if(receipt.variant==='wrongreply')declared.provider.find(r=>r.name==='channel').reply.text='FEISHU-STREAM-ACTUAL-NEGATIVE';
 assert.equal(receipt.scenario.sha256,hash(receipt.variant==='normal'?fs.readFileSync(scenePath):JSON.stringify(declared,null,2)+'\n'),'only the declared input variant is allowed');
 for(const name of ['capture.mjs','mock.mjs','wire.mjs','verify.mjs'])assert.equal(receipt.sources[name],hash(fs.readFileSync(path.join(here,name))),name+' exact source');
 for(const expected of scene.assertions){const row=only(a.evidence.assertions.filter(r=>r.id===expected.id),expected.id);assert.equal(row.passed,true);assert.ok(!row.skipped)}
 assert.equal(a.evidence.assertions.length,scene.assertions.length);
 const channel=only(a.provider.channel,'one actual channel request'),initial=only(a.provider['observer-initial'],'observer initial'),final=only(a.provider['observer-final'],'observer final');
 for(const r of [channel,initial,final]){assert.equal(r.method,'POST');assert.equal(r.url,'/v1/chat/completions');assert.equal(r.body.model,'deterministic');assert.equal(r.body.stream,true)}
 assert.equal(a.provider.unexpected?.length??0,0);assert.equal(a.provider.default?.length??0,0);
 assert.equal(text(channel.body.messages.filter(m=>m.role==='user').at(-1).content),'FEISHU-STREAM-BEGIN','stream text actually reached model');
 const tool=only(final.body.messages.filter(m=>m.role==='tool'&&m.tool_call_id==='call_feishu_receipt'),'real builtin receipt');assert.ok(text(tool.content).includes('FEISHU-STREAM-REPLY-OBSERVED'));
 assert.equal(a.steps.length,2);assert.equal(a.steps[0].step.ws,'submit_turn');assert.ok(a.steps[0].answers.length>0);
 for(const answer of a.steps[0].answers){assert.equal(answer.ws,'permission_decide');assert.equal(answer.params.decision,'allow');assert.ok(answer.frames.some(f=>f.type==='response'&&!f.error))}
 assert.ok(JSON.stringify(a.steps[0].frames).includes('FEISHU-STREAM-OBSERVER-DONE'));
 const history=only(a.steps[1].frames.filter(f=>f.type==='response'),'formal read history');assert.ok(!history.error);assert.ok(JSON.stringify(history.result).includes('FEISHU-STREAM-BEGIN'));
 const facts=only(Object.entries(a.tree).filter(([name])=>name.endsWith('/feishu-stream-witness/facts.json')),'actual mock facts')[1].json;
 assert.equal(facts.fixture,'feishu-stream-v1');assert.equal(facts.variant,receipt.variant);
 for(const name of ['mock.mjs','wire.mjs'])assert.equal(facts.sourceHashes[name],receipt.sources[name]);
 const endpoint=only(facts.endpoint,'actual endpoint');assert.deepEqual(endpoint,{method:'POST',path:'/callback/ws/endpoint',locale:'zh',userAgent:'oapi-node-sdk/1.65.0',body:{AppID:'cli_0123456789abcdef',AppSecret:'feishu-local-secret'}});
 assert.deepEqual(only(facts.upgrade,'real WS handshake'),{path:'/lark-stream?device_id=bbx-local&service_id=42'});
 assert.equal(facts.sent.length,4);const sent=raw.filter(r=>r.kind==='ws-send').map(r=>decode(Buffer.from(r.base64,'base64')));assert.deepEqual(sent,facts.sent.map(r=>({...r,seq:String(r.seq),log:String(r.log)})),'actual injected wire bytes');
 const incoming=raw.filter(r=>r.kind==='ws-receive'&&r.opcode===2).map(r=>decode(Buffer.from(r.base64,'base64')));
 assert.deepEqual(incoming.filter(r=>r.method===0),facts.ping);assert.equal(facts.ping.length,1);assert.equal(facts.ping[0].service,42);assert.deepEqual(facts.ping[0].headers,[['type','ping']]);
 const acknowledgements=incoming.filter(r=>r.method===1).map(frame=>{
  const duration=only(frame.headers.filter(([key])=>key==='biz_rt'),'actual ACK timing');assert.match(duration[1],/^-?\d+$/);return {...frame,headers:frame.headers.filter(([key])=>key!=='biz_rt'),durationMs:Number(duration[1])};
 });assert.deepEqual(acknowledgements,facts.ack,'semantic facts remain tied to unmodified raw ACKs');assert.equal(facts.ack.length,3);
 for(const key of ['wire-wrong','wire-real','wire-duplicate']){
  const ack=only(facts.ack.filter(r=>r.headers.some(([k,v])=>k==='message_id'&&v===key)),key+' ACK');assert.equal(ack.seq,'9');assert.equal(ack.log,'17');assert.equal(ack.service,42);assert.equal(JSON.parse(ack.payload).code,200);
 }
 const wrong=JSON.parse(facts.ack.find(r=>r.headers.some(([k,v])=>k==='message_id'&&v==='wire-wrong')).payload);assert.equal(Buffer.from(wrong.data,'base64').toString(),JSON.stringify('no not.registered event handle'));
 const actualHttp=raw.filter(r=>r.kind==='http');assert.equal(actualHttp.filter(r=>new URL(r.url,'https://open.feishu.cn').pathname==='/callback/ws/endpoint').length,1);
 const actualOutbound=actualHttp.filter(r=>new URL(r.url,'https://open.feishu.cn').pathname.startsWith('/open-apis/')).map(r=>({method:r.method,path:new URL(r.url,'https://open.feishu.cn').pathname+new URL(r.url,'https://open.feishu.cn').search,authorization:r.headers.authorization??null,body:httpBody(r.bodyBase64)}));assert.deepEqual(actualOutbound,facts.outbound,'actual outbound bytes represented without rewriting raw trace');
 const replies=facts.outbound.filter(r=>(r.path.startsWith('/open-apis/im/v1/messages?')||r.method==='PATCH')&&String(r.body.content??'').includes('FEISHU-STREAM-DONE'));
 assert.ok(replies.length>=1,'actual outbound reply must contain FEISHU-STREAM-DONE');assert.ok(JSON.stringify(history.result).includes('FEISHU-STREAM-DONE'),'actual formal history contains the same final reply');assert.ok(replies.every(r=>r.authorization==='Bearer feishu-local-token'));
 assert.ok(!JSON.stringify(facts.outbound).includes('FEISHU-STREAM-ACTUAL-NEGATIVE'),'wrong actual reply is rejected');
 const durable=Object.values(a.tree).flatMap(v=>Array.isArray(v.jsonl)?v.jsonl:[]).filter(r=>r.type==='accepted_input'&&r.sessionId==='feishu:chat=feishu-local-chat:general');
 assert.equal(durable.length,1,'wrong and duplicate stream events do not persist input');assert.equal(text(durable[0].messages[0].content),'FEISHU-STREAM-BEGIN');
 assert.equal(raw.filter(r=>r.kind==='mockError').length,0,'no hidden fixture protocol errors');
}
try{
 const artifactPath=process.argv[2];const bytes=fs.readFileSync(artifactPath),a=JSON.parse(bytes),receipt=JSON.parse(fs.readFileSync(artifactPath+'.receipt.json')),rawBytes=fs.readFileSync(artifactPath+'.mock-raw.jsonl'),raw=rawBytes.toString().split('\n').filter(Boolean).map(JSON.parse);
 assert.equal(receipt.artifact.sha256,hash(bytes),'capture bytes unchanged');assert.equal(receipt.raw.sha256,hash(rawBytes),'raw mock trace unchanged');validate(a,receipt,raw);
 if(process.argv.includes('--self-test')){
  const faults=[v=>delete v.provider.channel,v=>v.provider.channel[0].body.messages.filter(m=>m.role==='user').at(-1).content='WRONG',v=>{const f=Object.entries(v.tree).find(([name])=>name.endsWith('/feishu-stream-witness/facts.json'))[1].json;f.ack=[]},v=>{const f=Object.entries(v.tree).find(([name])=>name.endsWith('/feishu-stream-witness/facts.json'))[1].json;f.outbound=[]}];
  for(const mutate of faults){const copy=structuredClone(a);mutate(copy);assert.throws(()=>validate(copy,receipt,raw))}console.log('4 corruption controls rejected');
 }
 console.log('Feishu formal stream strict PASS');
}catch(error){console.error(error.stack);process.exitCode=1}
