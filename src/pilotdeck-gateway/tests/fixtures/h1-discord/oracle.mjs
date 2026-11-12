// Fixed compiled Discord adapter with an injected external SDK boundary.
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
import {pathToFileURL} from 'node:url';
const root=process.env.H1_BASELINE;
if(!root||!process.env.HOME||!process.env.PILOT_HOME) throw Error('isolated environment required');
const sent=[],turns=[],logs=[];
class Client {
  constructor(options){this.options=options;this.handlers={};this.channels={fetch:async id=>({send:async content=>sent.push({id,...content}),sendTyping:async()=>{}})};}
  on(event,handler){this.handlers[event]=handler;}
  async login(){this.handlers.ready({user:{id:'bot',tag:'fixture#0000'}});}
  destroy(){}
}
const realRequire=createRequire(import.meta.url);
globalThis.require=name=>name==='discord.js'?{Client,GatewayIntentBits:{Guilds:1,GuildMessages:512,DirectMessages:4096,MessageContent:32768},Partials:{Channel:1,Message:3}}:realRequire(name);
const {DiscordChannel}=await import(pathToFileURL(`${root}/dist/src/adapters/channel/discord/DiscordChannel.js`));
const channel=new DiscordChannel({token:'fixture'});
const handle=await channel.start({logger:{info:t=>logs.push(t),error:t=>logs.push(t)},gateway:{async *submitTurn(turn){turns.push(turn);yield {type:'assistant_text_delta',text:' answer '};}}});
assert.deepEqual(channel.client.options.intents,[1,512,4096,32768]);
for(const message of [{author:{id:'bot'},content:'self',channel:{id:'c'}},{author:{id:'a',bot:true},content:'other bot',channel:{id:'c'}},{author:{id:'a'},system:true,content:'system',channel:{id:'c'}},{author:{id:'a'},content:' hi ',channel:{id:'c'}},{author:{id:'a'},content:'/new',channel:{id:'c'}},{author:{id:'a'},content:'next',channel:{id:'c'}}]) await channel.handleMessageCreate(message);
assert.equal(turns.length,2);assert.equal(turns[0].sessionKey,'discord:chat=c:general');assert.match(turns[1].sessionKey,/^discord:chat=c:s_/);turns[1].sessionKey='discord:chat=c:s_<uuid>';
await channel.sendReply('c','a'.repeat(1500)+' '+'b'.repeat(1000));
if(process.argv.includes('--negative')) sent.at(-2).content='corrupted';
assert.equal(sent.at(-2).content.length,1500);assert.equal(sent.at(-1).content[0],' ');
await handle.stop('oracle');
console.log(JSON.stringify({turns,sent:sent.map(v=>v.content.length>100?{id:v.id,length:v.content.length,startsSpace:v.content[0]===' '}:v),logs},null,2));
