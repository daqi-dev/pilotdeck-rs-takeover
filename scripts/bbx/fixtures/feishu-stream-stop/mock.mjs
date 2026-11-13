import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import {createHash,randomBytes} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {encode,decode,websocket,websocketReader} from './wire.mjs';
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const origin='https://open.feishu.cn';
import {closeBarrier} from '../feishu-stream/mock.mjs';
export async function startMock({root,caDir,proxyPort,wsPort,tlsPort,rawPath,onFinalReply,variant='normal'}){
 if(!['normal','wrong-nonce'].includes(variant))throw Error('unknown fixture variant');
 const sourceHashes=Object.fromEntries(['mock.mjs','wire.mjs','../feishu-stream/mock.mjs'].map(name=>[name,hash(fs.readFileSync(new URL(name,import.meta.url)))]));
 const state={fixture:'feishu-stream-stop-v1',sourceHashes,variant,endpoint:[],upgrade:[],sent:[],ping:[],ack:[],outbound:[]};
 const drain=closeBarrier(),waiters=new Set();let injected=false,replyObserved=false,disableRequested=false,originalSocket,originalClosed=false,probe;
 let sealed=false;
 const raw=record=>{if(sealed)throw Error('mock raw append after drain seal');fs.appendFileSync(rawPath,JSON.stringify(record)+'\n')};
 const save=()=>{const dir=path.join(root,'home/.pilotdeck/feishu-stop-witness');fs.mkdirSync(dir,{recursive:true});fs.writeFileSync(path.join(dir,'facts.json'),JSON.stringify(state,null,2)+'\n',{mode:0o600})};
 const track=socket=>drain.track(socket);
 const sendJson=(res,body,status=200)=>{res.writeHead(status,{'content-type':'application/json'});res.end(JSON.stringify(body))};
 function notify(){if(!replyObserved)return;for(const res of waiters){sendJson(res,{marker:'FEISHU-STREAM-REPLY-OBSERVED',replyCount:state.outbound.filter(r=>r.path.startsWith('/open-apis/im/v1/messages?')).length});}waiters.clear()}
 function handle(req,res){return drain.handle(respond(req,res));}
 async function respond(req,res){
  try{
   const parts=[];for await(const chunk of req)parts.push(chunk);const bodyBytes=Buffer.concat(parts),bodyText=bodyBytes.toString();let body;try{body=JSON.parse(bodyText)}catch{body={raw:bodyText}}
   const url=req.url.startsWith('https://')?new URL(req.url):new URL(req.url,origin);
   raw({kind:'http',method:req.method,url:req.url,headers:req.headers,bodyBase64:bodyBytes.toString('base64')});
   if(url.origin!==origin||req.headers.host?.split(':')[0]!=='open.feishu.cn')return sendJson(res,{error:'mock denies nonlocal origin'},403);
   const target=url.pathname+url.search;
   if(req.method==='POST'&&target==='/callback/ws/endpoint'){
    state.endpoint.push({method:req.method,path:target,locale:req.headers.locale,userAgent:req.headers['user-agent'],body});save();
    return sendJson(res,{code:0,data:{URL:`ws://127.0.0.1:${wsPort}/lark-stream?device_id=bbx-local&service_id=42`,ClientConfig:{PingInterval:120,ReconnectCount:0,ReconnectInterval:0,ReconnectNonce:0}}});
   }
   if(!url.pathname.startsWith('/open-apis/'))return sendJson(res,{error:'unregistered mock path'},404);
   const record={method:req.method,path:target,authorization:req.headers.authorization??null,body};state.outbound.push(record);save();
   if(target==='/open-apis/auth/v3/tenant_access_token/internal')return sendJson(res,{code:0,tenant_access_token:'feishu-local-token',expire:7200});
   if(target==='/open-apis/im/v1/messages?receive_id_type=chat_id'){
    if(req.headers.authorization!=='Bearer feishu-local-token')return sendJson(res,{code:99991663,msg:'invalid token'});
    replyObserved=replyObserved||String(body.content??'').includes('FEISHU-STOP-DONE');sendJson(res,{code:0,data:{message_id:'feishu-local-reply'}});notify();return;
   }
   if(target.includes('/reactions')){
    if(req.method==='DELETE'&&replyObserved&&!disableRequested){
     disableRequested=true;res.once('finish',()=>{try{state.disable=onFinalReply();state.disable.afterActualFinalReply=true;state.disable.afterReactionDelete=true;raw({kind:'config-disable',...state.disable});save()}catch(error){raw({kind:'mockError',message:error.message})}});
    }
    return sendJson(res,{code:0,data:{reaction_id:'feishu-local-reaction'}});
   }
   if(req.method==='PATCH'&&target==='/open-apis/im/v1/messages/feishu-local-reply'){replyObserved=replyObserved||String(body.content??'').includes('FEISHU-STOP-DONE');sendJson(res,{code:0,data:{message_id:'feishu-local-reply'}});notify();return;}
   return sendJson(res,{code:404,msg:'unregistered mock operation'},404);
  }catch(error){raw({kind:'mockError',message:error.message});sendJson(res,{error:error.message},500)}
 }
 const tls=https.createServer({key:fs.readFileSync(path.join(caDir,'server.key')),cert:fs.readFileSync(path.join(caDir,'server.pem'))},handle);
 const proxy=http.createServer(handle);
 proxy.on('connect',(req,client,head)=>{
  track(client);raw({kind:'connect',authority:req.url,headers:req.headers});
  if(req.url!=='open.feishu.cn:443'){client.end('HTTP/1.1 403 Forbidden\r\n\r\n');return}
  const upstream=track(net.connect({host:'127.0.0.1',port:tlsPort},()=>{client.write('HTTP/1.1 200 Connection Established\r\n\r\n');if(head.length)upstream.write(head);client.pipe(upstream);upstream.pipe(client)}));
  upstream.on('error',()=>client.destroy());client.on('error',()=>upstream.destroy());client.once('close',()=>upstream.destroy());upstream.once('close',()=>client.destroy());
 });
 const ws=http.createServer((req,res)=>{
  raw({kind:'control',method:req.method,path:req.url});
  if(req.method!=='GET'||req.url!=='/probe-stopped')return sendJson(res,{error:'unregistered control'},404);
  if(!disableRequested||!state.disable)return sendJson(res,{error:'config disable receipt missing'},409);
  if(probe)return sendJson(res,{error:'only one exact-socket probe allowed'},409);
  const nonce=randomBytes(24);
  let timer;const finish=alive=>{
   if(probe.finished)return;probe.finished=true;clearTimeout(timer);
   state.probe={marker:'FEISHU-STOP-PROBE',identity:'feishu-original-socket-1',alive,evidence:alive?'exact-nonce-pong':'socket-close'};
   raw({kind:'probe-result',...state.probe});save();sendJson(res,state.probe);
  };
  probe={nonce:variant==='wrong-nonce'?Buffer.concat([nonce,Buffer.from([0])]):nonce,finish,finished:false};
  if(originalClosed){finish(false);return}
  if(!originalSocket)return sendJson(res,{error:'original socket missing'},409);
  timer=setTimeout(()=>{if(probe.finished)return;probe.finished=true;raw({kind:'probe-timeout'});sendJson(res,{error:'exact socket probe timed out'},504)},3000);
  raw({kind:'ws-probe-ping',identity:'feishu-original-socket-1',base64:nonce.toString('base64')});
  originalSocket.write(websocket(9,nonce));
 });
 ws.on('upgrade',(req,socket,head)=>{
  track(socket);raw({kind:'upgrade',path:req.url,headers:req.headers});
  if(req.url!=='/lark-stream?device_id=bbx-local&service_id=42'||req.headers.upgrade?.toLowerCase()!=='websocket'){socket.end('HTTP/1.1 403 Forbidden\r\n\r\n');return}
  if(originalSocket){socket.destroy();raw({kind:'mockError',message:'unexpected second socket'});return}
  originalSocket=socket;state.upgrade.push({path:req.url,identity:'feishu-original-socket-1'});save();
  const accept=createHash('sha1').update(req.headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  function send(frame){raw({kind:'ws-send',base64:encode(frame).toString('base64')});socket.write(websocket(2,encode(frame)))}
  function inject(){
   if(injected)return;injected=true;
   const event=(type,id,text)=>Buffer.from(JSON.stringify({schema:'2.0',header:{event_type:type,event_id:'feishu-header-shared'},event:{message:{message_id:id,chat_id:'feishu-local-chat',message_type:'text',content:JSON.stringify({text})}}}));
   const frame=(payload,id,sum,seq)=>({seq:9,log:17,service:42,method:1,headers:[['type','event'],['message_id',id],['sum',String(sum)],['seq',String(seq)]],payload});
   const correct=event('im.message.receive_v1','feishu-local-message','FEISHU-STOP-BEGIN');const middle=Math.floor(correct.length/2);
   const frames=[frame(event('not.registered','feishu-wrong-message','MUST-NOT-ENTER-MODEL'),'wire-wrong',1,0),frame(correct.subarray(middle),'wire-real',2,1),frame(correct.subarray(0,middle),'wire-real',2,0),frame(correct,'wire-duplicate',1,0)];
   state.sent=frames.map(f=>({...f,payload:f.payload.toString()}));save();for(const frame of frames)send(frame);
  }
  const read=websocketReader((opcode,body)=>{
   try{
    raw({kind:'ws-receive',opcode,base64:body.toString('base64')});
    if(opcode===8){socket.end(websocket(8,body));return}if(opcode===9){socket.write(websocket(10,body));return}if(opcode===10){if(probe&&!probe.finished&&body.equals(probe.nonce))probe.finish(true);else raw({kind:'unmatched-pong',base64:body.toString('base64')});return}if(opcode!==2)throw Error('nonbinary Lark frame');
    const frame=decode(body);
    if(frame.method===0){state.ping.push(frame);save();inject();return}
    const duration=frame.headers.find(([key])=>key==='biz_rt');if(!duration||!/^-[0-9]+$|^[0-9]+$/.test(duration[1]))throw Error('missing numeric biz_rt');
    state.ack.push({...frame,headers:frame.headers.filter(([key])=>key!=='biz_rt'),durationMs:Number(duration[1])});save();
   }catch(error){raw({kind:'mockError',message:error.message});socket.destroy()}
  });
  socket.on('data',read);socket.on('error',()=>{});socket.on('close',()=>{originalClosed=true;raw({kind:'ws-close',identity:'feishu-original-socket-1'});if(probe&&!probe.finished)probe.finish(false)});if(head.length)read(head);
 });
 for(const server of [tls,proxy,ws])server.on('connection',track);
 await Promise.all([[tls,tlsPort],[proxy,proxyPort],[ws,wsPort]].map(([server,port])=>new Promise((resolve,reject)=>{server.once('error',reject);server.listen(port,'127.0.0.1',resolve)})));
 let closeReceipt;return {close(){if(!closeReceipt)closeReceipt=(async()=>{for(const res of waiters)res.destroy();const receipt=await drain.close([tls,proxy,ws]);raw({kind:'mock-drain-complete',...receipt});sealed=true;return receipt})();return closeReceipt;}};
}

