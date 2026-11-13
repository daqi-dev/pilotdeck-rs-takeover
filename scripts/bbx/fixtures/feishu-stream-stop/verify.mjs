#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import {decode} from './wire.mjs';
const here=path.dirname(fileURLToPath(import.meta.url));
const scenePath=path.resolve(here,'../../scenarios/backend-feishu-stream-stop.json');
const scene=JSON.parse(fs.readFileSync(scenePath));
const hash=b=>createHash('sha256').update(b).digest('hex');
const text=v=>typeof v==='string'?v:Array.isArray(v)?v.filter(b=>b.type==='text').map(b=>b.text??'').join(''):'';
const only=(rows,label)=>{assert.equal(rows?.length,1,label);return rows[0]};
function httpBody(base64){const value=Buffer.from(base64,'base64').toString();try{return JSON.parse(value)}catch{return {raw:value}}}
function validate(a,receipt,raw){
 assert.equal(a.scenario,scene.name);assert.deepEqual(a.failures,[]);assert.ok(['ts','rust'].includes(a.side));assert.equal(a.evidence.side,a.side);
 const tail=raw.at(-1);assert.equal(tail?.kind,'mock-drain-complete','raw sealed only after actual mock drain');assert.deepEqual(tail,{kind:'mock-drain-complete',...receipt.mockDrain});assert.equal(tail.listeners,3);assert.ok(tail.acceptedSockets>0);assert.equal(tail.closedSockets,tail.acceptedSockets);assert.equal(tail.socketsRemaining,0);assert.equal(tail.handlersRemaining,0);assert.equal(tail.httpHandlersStarted,tail.httpHandlersCompleted);
 assert.equal(receipt.side,a.side);assert.equal(receipt.network,'linux-loopback-only');assert.equal(receipt.process.code,0);assert.equal(receipt.process.signal,null);
 assert.ok(['normal','wrong-nonce'].includes(receipt.variant));assert.equal(a.evidence.scenario.sha256,receipt.scenario.sha256);
 assert.equal(receipt.scenario.sha256,hash(fs.readFileSync(scenePath)),'exact independent stop scenario');
 for(const name of ['capture.mjs','mock.mjs','wire.mjs','verify.mjs','../feishu-stream/mock.mjs'])assert.equal(receipt.sources[name],hash(fs.readFileSync(path.join(here,name))),name+' exact source');
 for(const expected of scene.assertions){const row=only(a.evidence.assertions.filter(r=>r.id===expected.id),expected.id);assert.equal(row.passed,true);assert.ok(!row.skipped)}
 assert.equal(a.evidence.assertions.length,scene.assertions.length);
 const channel=only(a.provider.channel,'one actual channel request'),initial=only(a.provider['observer-initial'],'observer initial'),final=only(a.provider['observer-final'],'observer final');
 for(const r of [channel,initial,final]){assert.equal(r.method,'POST');assert.equal(r.url,'/v1/chat/completions');assert.equal(r.body.model,'deterministic');assert.equal(r.body.stream,true)}
 assert.equal(a.provider.unexpected?.length??0,0);assert.equal(a.provider.default?.length??0,0);
 assert.equal(text(channel.body.messages.filter(m=>m.role==='user').at(-1).content),'FEISHU-STOP-BEGIN','stream text actually reached model');
 const tool=only(final.body.messages.filter(m=>m.role==='tool'&&m.tool_call_id==='call_feishu_receipt'),'real builtin receipt');assert.ok(text(tool.content).includes('FEISHU-STOP-PROBE'),'actual probe receipt enters model');
 assert.equal(a.steps.length,1);const serve=a.steps[0];assert.equal(serve.serve.reached,true);assert.equal(serve.serve.exitCode,0);assert.ok(!serve.timedOut);
 const barrier='[pilotdeck] Adapter hot-reload complete: feishu=stopped';assert.equal(serve.step.until,barrier);assert.ok(serve.serve.stdout.includes(barrier),'actual awaited adapter reload completion');
 assert.equal(serve.then.length,3);const observer=serve.then[0];assert.equal(observer.step.ws,'submit_turn');assert.ok(observer.answers.length>0);
 for(const answer of observer.answers){assert.equal(answer.ws,'permission_decide');assert.equal(answer.params.decision,'allow');assert.ok(answer.frames.some(f=>f.type==='response'&&!f.error))}
 assert.ok(JSON.stringify(observer.frames).includes('FEISHU-STOP-OBSERVER-DONE'));
 const history=only(serve.then[1].frames.filter(f=>f.type==='response'),'formal read history');assert.ok(!history.error);assert.ok(JSON.stringify(history.result).includes('FEISHU-STOP-BEGIN'));
 const facts=only(Object.entries(a.tree).filter(([name])=>name.endsWith('/feishu-stop-witness/facts.json')),'actual mock facts')[1].json;
 assert.equal(facts.fixture,'feishu-stream-stop-v1');assert.equal(facts.variant,receipt.variant);
 for(const name of ['mock.mjs','wire.mjs','../feishu-stream/mock.mjs'])assert.equal(facts.sourceHashes[name],receipt.sources[name]);
 const endpoint=only(facts.endpoint,'actual endpoint');assert.deepEqual(endpoint,{method:'POST',path:'/callback/ws/endpoint',locale:'zh',userAgent:'oapi-node-sdk/1.65.0',body:{AppID:'cli_0123456789abcdef',AppSecret:'feishu-local-secret'}});
 assert.deepEqual(only(facts.upgrade,'real WS handshake'),{path:'/lark-stream?device_id=bbx-local&service_id=42',identity:'feishu-original-socket-1'});
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
 const replies=facts.outbound.filter(r=>(r.path.startsWith('/open-apis/im/v1/messages?')||r.method==='PATCH')&&String(r.body.content??'').includes('FEISHU-STOP-DONE'));
 assert.ok(replies.length>=1,'actual outbound reply must contain FEISHU-STOP-DONE');assert.ok(JSON.stringify(history.result).includes('FEISHU-STOP-DONE'),'actual formal history contains the same final reply');assert.ok(replies.every(r=>r.authorization==='Bearer feishu-local-token'));
 assert.ok(!JSON.stringify(facts.outbound).includes('FEISHU-STREAM-ACTUAL-NEGATIVE'),'wrong actual reply is rejected');
 const durable=Object.values(a.tree).flatMap(v=>Array.isArray(v.jsonl)?v.jsonl:[]).filter(r=>r.type==='accepted_input'&&r.sessionId==='feishu:chat=feishu-local-chat:general');
 assert.equal(durable.length,1,'wrong and duplicate stream events do not persist input');assert.equal(text(durable[0].messages[0].content),'FEISHU-STOP-BEGIN');
 const disableIndex=raw.findIndex(r=>r.kind==='config-disable');assert.ok(disableIndex>=0,'actual config edit receipt');
 assert.deepEqual(raw[disableIndex],{kind:'config-disable',...facts.disable});assert.equal(facts.disable.enabled,false);assert.equal(facts.disable.afterActualFinalReply,true);assert.equal(facts.disable.afterReactionDelete,true);
 const config=only(Object.entries(a.tree).filter(([name])=>name.endsWith('/.pilotdeck/pilotdeck.yaml')),'actual disabled config')[1].text;
 assert.equal(hash(config),facts.disable.afterSha256);const enabled='  feishu:\n    enabled: true\n',disabled='  feishu:\n    enabled: false\n';assert.equal(config.split(disabled).length,2);assert.equal(hash(config.replace(disabled,enabled)),facts.disable.beforeSha256);
 const finalSend=raw.findIndex(r=>r.kind==='http'&&Buffer.from(r.bodyBase64,'base64').toString().includes('FEISHU-STOP-DONE'));
 const reactionDelete=raw.findIndex(r=>r.kind==='http'&&r.method==='DELETE'&&r.url.includes('/reactions/'));
 const control=raw.findIndex(r=>r.kind==='control'&&r.path==='/probe-stopped'),probeIndex=raw.findIndex(r=>r.kind==='probe-result');
 assert.ok(finalSend>=0&&finalSend<reactionDelete&&reactionDelete<disableIndex&&disableIndex<control&&control<probeIndex,'actual final send → reaction delete → config edit → formal post-reload probe');
 const expectedAlive=a.side==='ts';assert.deepEqual(facts.probe,{marker:'FEISHU-STOP-PROBE',identity:'feishu-original-socket-1',alive:expectedAlive,evidence:expectedAlive?'exact-nonce-pong':'socket-close'},'explicit narrow ownership difference, never ignored');
 assert.deepEqual(raw[probeIndex],{kind:'probe-result',...facts.probe});assert.ok(text(tool.content).includes(JSON.stringify(facts.probe)),'same exact socket receipt in actual model tool result');
 const ping=raw.findIndex(r=>r.kind==='ws-probe-ping'),close=raw.findIndex(r=>r.kind==='ws-close'&&r.identity==='feishu-original-socket-1');
 if(expectedAlive){
  assert.ok(ping>control&&ping<probeIndex);const nonce=raw[ping].base64;assert.equal(Buffer.from(nonce,'base64').length,24);
  const pong=raw.findIndex((r,i)=>i>ping&&r.kind==='ws-receive'&&r.opcode===10&&r.base64===nonce);assert.ok(pong>ping&&pong<probeIndex,'actual exact nonce Pong after completed stop proves leak');assert.ok(close>probeIndex,'eventual whole-process close cannot stand in for adapter stop');
 }else{assert.ok(close>disableIndex&&close<probeIndex,'old exact socket closed before stop probe succeeds');assert.ok(!raw.slice(control,probeIndex).some(r=>r.kind==='ws-receive'&&r.opcode===10),'no leaked live socket Pong');}
 assert.equal(raw.filter(r=>r.kind==='probe-timeout').length,0,'timeout is failure, not a dead-socket witness');
 const providerKeys=Object.keys(a.provider);assert.ok(providerKeys.indexOf('channel')<providerKeys.indexOf('observer-initial')&&providerKeys.indexOf('observer-initial')<providerKeys.indexOf('observer-final'),'normal turn completes before post-stop observer');
 assert.equal(raw.filter(r=>r.kind==='mockError').length,0,'no hidden fixture protocol errors');
}
try{
 const artifactPath=process.argv[2];const bytes=fs.readFileSync(artifactPath),a=JSON.parse(bytes),receipt=JSON.parse(fs.readFileSync(artifactPath+'.receipt.json')),rawBytes=fs.readFileSync(artifactPath+'.mock-raw.jsonl'),raw=rawBytes.toString().split('\n').filter(Boolean).map(JSON.parse);
 assert.equal(receipt.artifact.sha256,hash(bytes),'capture bytes unchanged');assert.equal(receipt.raw.sha256,hash(rawBytes),'raw mock trace unchanged');validate(a,receipt,raw);
 if(process.argv.includes('--self-test')){
  const faults=[v=>delete v.provider.channel,v=>{v.steps[0].serve.stdout='missing reload completion'},v=>{const f=Object.entries(v.tree).find(([name])=>name.endsWith('/feishu-stop-witness/facts.json'))[1].json;f.probe.alive=!f.probe.alive},v=>{const f=Object.entries(v.tree).find(([name])=>name.endsWith('/feishu-stop-witness/facts.json'))[1].json;f.probe.identity='different-socket'}];
  for(const mutate of faults){const copy=structuredClone(a);mutate(copy);assert.throws(()=>validate(copy,receipt,raw))}
  const noProbe=raw.filter(r=>r.kind!=='probe-result');assert.throws(()=>validate(a,receipt,noProbe));console.log('5 stop corruption controls rejected');
 }
 console.log('Feishu formal stop strict PASS');
}catch(error){console.error(error.stack);process.exitCode=1}
