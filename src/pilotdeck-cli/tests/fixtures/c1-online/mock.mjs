// Fixture peer only. No product import or fixed port outside explicit Docker.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { createHash } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { frame } from '../c1-cron/capture.mjs';
export const TOKEN = 'c1-online-isolated-fixture-token';
export function requireIsolation(env=process.env, exists=fs.existsSync) {
  assert.equal(env.PILOTDECK_C1_ONLINE_DOCKER,'private-no-published-ports','explicit private Docker declaration required');
  assert.ok(exists('/.dockerenv'),'refusing fixed port 18789 outside Docker');
}
export async function startMock(spec,{port=18789,record=()=>{}}={}) {
  if(port===18789)requireIsolation();
  const events=new EventEmitter(), sockets=new Set(), timers=new Set();
  const state={health:[],hellos:[],requests:[],modelRequests:[],problems:[],closeReceived:false,pongs:0,openSockets:0};
  const log=(kind,details={})=>record({kind,at:new Date().toISOString(),...details});
  const server=http.createServer((req,res)=>{
    if(req.method==='GET'&&req.url==='/health'){
      state.health.push({method:req.method,url:req.url});log('health');res.end('{"ok":true}');return;
    }
    if(req.method==='POST'&&req.url==='/v1/chat/completions'){
      const parts=[];req.on('data',c=>parts.push(c));req.on('end',()=>{
        const raw=Buffer.concat(parts).toString('utf8');state.modelRequests.push({method:req.method,url:req.url,headers:req.headers,raw});log('model-request',{raw});
        res.writeHead(200,{'content-type':'text/event-stream'});
        for(const delta of [{role:'assistant'},{content:spec.id==='local-sigint'?'local-pending':'local-fallback'}])res.write(`data: ${JSON.stringify({id:'c1-model',object:'chat.completion.chunk',model:'deterministic',choices:[{index:0,delta,finish_reason:null}]})}\n\n`);
        if(spec.id==='local-sigint'){events.emit('model-pending');return;}
        res.write(`data: ${JSON.stringify({id:'c1-model',object:'chat.completion.chunk',model:'deterministic',choices:[{index:0,delta:{},finish_reason:'stop'}],usage:{prompt_tokens:10,completion_tokens:3,total_tokens:13}})}\n\n`);
        res.end('data: [DONE]\n\n');events.emit('model-complete');
      });return;
    }
    state.problems.push(`unexpected HTTP ${req.method} ${req.url}`);res.writeHead(404);res.end('{}');
  });
  server.on('connection',s=>{sockets.add(s);s.on('close',()=>sockets.delete(s));});
  server.on('upgrade',(req,socket,head)=>{
    if(req.url!=='/ws'||typeof req.headers['sec-websocket-key']!=='string'){state.problems.push('invalid upgrade');socket.destroy();return;}
    const accept=createHash('sha1').update(req.headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    state.openSockets++;log('ws-open');socket.on('close',()=>{state.openSockets--;log('ws-closed');});socket.on('error',e=>log('ws-error',{message:e.message}));
    const send=value=>{const raw=JSON.stringify(value);log('ws-send',{raw});socket.write(frame(1,raw));};
    const close=(code,reason)=>{const data=Buffer.alloc(2+Buffer.byteLength(reason));data.writeUInt16BE(code);data.write(reason,2);log('ws-server-close',{code,reason});socket.write(frame(8,data));};
    const text=payload=>{
      const raw=payload.toString('utf8'), value=JSON.parse(raw);log('ws-receive',{raw});
      if(value.type==='hello'){
        state.hellos.push({raw,value});
        if(['hello-fallback','local-sigint'].includes(spec.id))close(1008,'denied');else send({type:'hello_ok',protocolVersion:'1.2',serverInfo:{name:'c1-online-owned-mock',version:'0.1.0'}});
        events.emit('hello');return;
      }
      assert.equal(value.type,'request');assert.equal(value.method,'submit_turn');state.requests.push({raw,value});
      const text=spec.id==='disconnect'?'partial':spec.id==='sigint'?'pending':`reply:${value.params.message}`;
      send({type:'event',id:value.id,seq:7,event:{type:'assistant_text_delta',text}});
      if(spec.id==='disconnect')close(1011,'specific reason');
      else if(spec.id!=='sigint'){
        send({type:'response',id:value.id,ok:true,result:{ignored:true}});
        send({type:'event',id:value.id,seq:7,final:true,event:{type:'assistant_text_delta',text:'must-not-render'}});
      }
      if(spec.id==='idle-heartbeat'&&state.requests.length===1){
        const timer=setTimeout(()=>{timers.delete(timer);log('ws-ping');socket.write(frame(9,'idle'));},25);timers.add(timer);
      }
      events.emit('turn',value);
    };
    let buffered=Buffer.alloc(0), fragments=[], fragmentBytes=0;
    const consume=chunk=>{
      buffered=Buffer.concat([buffered,chunk]);
      try{
        while(buffered.length>=2){
          const opcode=buffered[0]&15, final=!!(buffered[0]&128);assert.equal(buffered[0]&0x70,0);assert.ok(buffered[1]&128,'masked client frame');
          let size=buffered[1]&127,offset=2;
          if(size===126){if(buffered.length<4)return;size=buffered.readUInt16BE(2);offset=4;}
          else if(size===127){if(buffered.length<10)return;const n=buffered.readBigUInt64BE(2);assert.ok(n<=1048576n);size=Number(n);offset=10;}
          assert.ok(size+fragmentBytes<=1048576);if(buffered.length<offset+4+size)return;
          const mask=buffered.subarray(offset,offset+4);offset+=4;const payload=Buffer.from(buffered.subarray(offset,offset+size));
          for(let i=0;i<payload.length;i++)payload[i]^=mask[i%4];buffered=buffered.subarray(offset+size);
          if(opcode===8){state.closeReceived=true;log('ws-client-close',{payloadBase64:payload.toString('base64')});socket.end(frame(8,payload));continue;}
          if(opcode===9){socket.write(frame(10,payload));continue;}
          if(opcode===10){assert.equal(payload.toString(),'idle');state.pongs++;log('ws-pong');events.emit('pong');continue;}
          assert.ok(opcode===1||(opcode===0&&fragments.length));if(opcode===1)assert.equal(fragments.length,0);
          fragments.push(payload);fragmentBytes+=payload.length;
          if(final){text(Buffer.concat(fragments));fragments=[];fragmentBytes=0;}
        }
      }catch(e){state.problems.push(e.message);log('protocol-error',{message:e.message});socket.destroy();}
    };
    socket.on('data',consume);if(head.length)consume(head);
  });
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(port,'127.0.0.1',resolve);});
  return {state,events,port:server.address().port,async stop(){for(const timer of timers)clearTimeout(timer);for(const s of sockets)s.destroy();await new Promise(r=>server.close(r));}};
}
