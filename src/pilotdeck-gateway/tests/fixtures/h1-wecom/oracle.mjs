import {EventEmitter} from 'node:events';
import {pathToFileURL} from 'node:url';
import assert from 'node:assert/strict';
const root=process.env.PILOTDECK_TS_BASELINE;
assert(root,'PILOTDECK_TS_BASELINE is required');
const sent=[],turns=[],logs=[];let sequence=0;let socket;
class MockSocket extends EventEmitter {
  constructor(url){super();this.url=url;this.readyState=0;socket=this;queueMicrotask(()=>{this.readyState=1;this.emit('open');});}
  send(raw){const frame=JSON.parse(raw);sent.push(frame);queueMicrotask(()=>this.emit('message',Buffer.from(JSON.stringify({headers:frame.headers,body:{errcode:0}}))));}
  close(){this.readyState=3;}
}
const {WeComChannel}=await import(pathToFileURL(`${root}/dist/src/adapters/channel/wecom/WeComChannel.js`));
const {WeComSessionMapper}=await import(pathToFileURL(`${root}/dist/src/adapters/channel/wecom/WeComSessionMapper.js`));
const channel=new WeComChannel({botKey:' fixture ',extra:{secret:' secret ',websocket_url:'ws://mock.invalid',dm_policy:'open',group_policy:'allowlist',group_allow_from:['wecom:group:ROOM'],text_batch_delay_ms:0},webSocketCtor:MockSocket,uuid:()=>`fixed-${++sequence}`,mapper:new WeComSessionMapper(undefined,()=> 'session')});
const handle=await channel.start({gateway:{async *submitTurn(input){turns.push(input);yield {type:'assistant_text_delta',text:process.env.PDRS_ORACLE_FAULT?'wrong':'答复'};yield {type:'tool_call_finished',toolName:'mock',ok:false};}},logger:{info:s=>logs.push(['info',s]),warn:s=>logs.push(['warn',s]),error:s=>logs.push(['error',s])}});
async function deliver(id,body){await channel.onSocketData(JSON.stringify({cmd:'aibot_msg_callback',headers:{req_id:`r-${id}`},body:{msgid:id,from:{userid:'user'},...body}}));for(let i=0;i<12;i++)await new Promise(r=>setImmediate(r));}
await deliver('1',{msgtype:'text',text:{content:' hello '}});
await deliver('1',{msgtype:'text',text:{content:'duplicate'}});
await deliver('2',{chatid:'room',chattype:'group',msgtype:'text',text:{content:'@bot hello group'}});
await deliver('3',{chatid:'denied',chattype:'group',msgtype:'text',text:{content:'denied'}});
await deliver('4',{msgtype:'text',text:{content:'/new'}});
await deliver('5',{msgtype:'text',text:{content:'follow'}});
await deliver('6',{msgtype:'text',text:{content:''},quote:{msgtype:'voice',voice:{content:'quoted'}}});
await handle.stop('fixture');
assert.equal(turns.length,4);assert.equal(turns[1].sessionKey,'wecom:group=room:user=user:general');assert.equal(turns[2].sessionKey,'wecom:dm=user:s_session');assert.equal(sent[1].cmd,'aibot_respond_msg');assert.equal(sent[1].headers.req_id,'r-1');assert.equal(sent[1].body.markdown.content,'答复\n⚠️ mock failed');
process.stdout.write(JSON.stringify({sent,turns,logs},null,2)+'\n');
