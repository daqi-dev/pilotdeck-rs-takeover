// Fixed adapter boundary oracle. No real SDK or account/network is involved.
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {pathToFileURL} from 'node:url';
const root=process.env.H1_BASELINE;
if(!root||!process.env.HOME||!process.env.PILOT_HOME) throw Error('isolated environment required');
const sent=[],turns=[],logs=[];
class App {
  constructor(options){this.options=options;this.client={auth:{test:async()=>({user_id:'bot',user:'fixture'})},chat:{postMessage:async body=>{sent.push(body);return {ok:true};}}};}
  event(kind,handler){assert.equal(kind,'message');this.handler=handler;}
  error(handler){this.errorHandler=handler;}
  async start(){}
  async stop(){}
}
const require=createRequire(import.meta.url);
globalThis.require=name=>name==='@slack/bolt'?{App}:require(name);
const {SlackChannel}=await import(pathToFileURL(`${root}/dist/src/adapters/channel/slack/SlackChannel.js`));
const channel=new SlackChannel({botToken:'xoxb-fixture',appToken:'xapp-fixture'});
const handle=await channel.start({logger:{info:t=>logs.push(t),error:t=>logs.push(t)},gateway:{async *submitTurn(turn){turns.push(turn);yield {type:'assistant_text_delta',text:' answer **bold** [site](https://example.invalid) '};}}});
assert.equal(channel.app.options.socketMode,true);
for(const event of [{channel:'C1',user:'bot',text:'self'},{channel:'C1',bot_id:'B1',text:'bot'},{channel:'C1',subtype:'message_changed',text:'edit'},{channel:'C1',text:'hello <@U1>',thread_ts:'100',ts:'200'},{channel:'C1',text:'root',thread_ts:'100',ts:'100'},{channel:'C2',text:'/new'},{channel:'C2',text:'next'}]) await channel.handleSlackMessage(event);
assert.equal(turns.length,3);assert.equal(turns[0].sessionKey,'slack:chat=C1:100:general');assert.equal(turns[0].message,'hello');assert.equal(turns[1].sessionKey,'slack:chat=C1:general');
assert.match(turns[2].sessionKey,/^slack:chat=C2:s_/);turns[2].sessionKey='slack:chat=C2:s_<uuid>';
if(process.argv.includes('--negative')) sent[0].thread_ts='corrupted';
assert.equal(sent[0].thread_ts,'100');assert.equal(sent[0].text,'answer *bold* <https://example.invalid|site>');
await handle.stop('oracle');
console.log(JSON.stringify({turns,sent,logs},null,2));
