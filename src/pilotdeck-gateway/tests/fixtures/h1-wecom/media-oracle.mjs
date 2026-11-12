import {EventEmitter} from 'node:events';
import {createCipheriv,createHash} from 'node:crypto';
import {pathToFileURL} from 'node:url';
import {readFile,rm} from 'node:fs/promises';
import assert from 'node:assert/strict';
const root=process.env.PILOTDECK_TS_BASELINE;assert(root);assert(process.env.TMPDIR);
let sequence=0;const sent=[],turns=[],logs=[];
class Socket extends EventEmitter {
  constructor(){super();this.readyState=0;queueMicrotask(()=>{this.readyState=1;this.emit('open');});}
  send(raw){const frame=JSON.parse(raw);sent.push(frame);let body={errcode:0};if(frame.cmd==='aibot_upload_media_init')body.upload_id='upload';if(frame.cmd==='aibot_upload_media_finish')body.media_id='media';queueMicrotask(()=>this.emit('message',Buffer.from(JSON.stringify({headers:frame.headers,body}))));}
  close(){this.readyState=3;}
}
const {WeComChannel}=await import(pathToFileURL(`${root}/dist/src/adapters/channel/wecom/WeComChannel.js`));
const channel=new WeComChannel({botKey:'bot',extra:{secret:'secret',dm_policy:'open',text_batch_delay_ms:0},webSocketCtor:Socket,uuid:()=>`media-${++sequence}`});
const handle=await channel.start({gateway:{async *submitTurn(turn){turns.push(turn);yield {type:'tool_call_finished',ok:true,toolCallId:'tool',images:[{data:Buffer.from(process.env.PDRS_ORACLE_FAULT?'y':'x').toString('base64'),mimeType:'image/png'}]};}},logger:{info:s=>logs.push(['info',s]),warn:s=>logs.push(['warn',s]),error:s=>logs.push(['error',s])}});
const key=Buffer.alloc(32,7),plain=Buffer.from('hello'),pad=32-plain.length%32;
const cipher=createCipheriv('aes-256-cbc',key,key.subarray(0,16));cipher.setAutoPadding(false);const encrypted=Buffer.concat([cipher.update(Buffer.concat([plain,Buffer.alloc(pad,pad)])),cipher.final()]);
await channel.onSocketData(JSON.stringify({cmd:'aibot_msg_callback',headers:{req_id:'incoming-media'},body:{msgid:'media-in',from:{userid:'user'},msgtype:'image',image:{base64:encrypted.toString('base64'),aeskey:key.toString('base64').replace(/=+$/,''),filename:'a b.png',content_type:'image/png'}}}));
for(let i=0;i<20;i++)await new Promise(r=>setImmediate(r));
assert.equal(turns.length,1);assert.equal(turns[0].message,'用户发送了企业微信附件。');const attachment=turns[0].attachments[0];assert.equal(attachment.type,'image');assert.equal(attachment.name,'a_b.png');assert.equal(attachment.bytes,5);assert.equal((await readFile(attachment.path)).toString(),'hello');
const upload=sent.find(f=>f.cmd==='aibot_upload_media_init');assert(upload);assert.equal(upload.body.md5,createHash('md5').update('x').digest('hex'));assert.equal(upload.body.filename,'tool-tool-1.png');assert.equal(upload.body.total_size,1);assert.equal(upload.body.total_chunks,1);
const chunk=sent.find(f=>f.cmd==='aibot_upload_media_chunk');assert.equal(chunk.body.base64_data,'eA==');const response=sent.find(f=>f.cmd==='aibot_respond_msg');assert.deepEqual(response,{cmd:'aibot_respond_msg',headers:{req_id:'incoming-media'},body:{msgtype:'image',image:{media_id:'media'}}});
await rm(attachment.path);attachment.path='<TMPDIR>/wecom-media';await handle.stop('fixture');console.log(JSON.stringify({sent,turns,logs},null,2));
