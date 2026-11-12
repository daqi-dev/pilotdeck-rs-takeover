// Executes the fixed TypeScript sources in memory; never writes the oracle tree.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = process.env.PILOTDECK_TS_ROOT || '/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const ts = require(path.join(root, 'node_modules/typescript'));
const {EventEmitter}=require('node:events');
const calls=[], logs=[], turns=[];
class WebSocket extends EventEmitter {
 static OPEN=1;
 constructor(url){super();this.url=url;this.readyState=1;calls.push(['ws',url]);WebSocket.instance=this;}
 send(text){calls.push(['wsSend',JSON.parse(text)]);}
 close(){calls.push(['wsClose']);}
}
const fetch=async(url,options={})=>{
 calls.push(['fetch',url,options]);
 return {ok:true,status:200,json:async()=>url.includes('getAppAccessToken')?{access_token:'mock-access',expires_in:'7200'}:{url:'ws://mock.invalid'},text:async()=>''};
};
const cache = new Map();
function load(filename) {
  if(cache.has(filename))return cache.get(filename).exports;
  const module = {exports:{}}; cache.set(filename,module);
  const code=ts.transpileModule(fs.readFileSync(filename,'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
  const req = name => name==='ws'?{default:WebSocket,__esModule:true}:name.startsWith('.')?load(path.resolve(path.dirname(filename),name.replace(/\.js$/,'.ts'))):require(name);
  vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{process,console,setTimeout,clearTimeout,setInterval,clearInterval,AbortController,Map,Set,fetch})(req,module,module.exports);
  return module.exports;
}
(async()=>{
 const dir=path.join(root,'src/adapters/channel/qq');
 const {QQChannel}=load(path.join(dir,'QQChannel.ts'));
 const {QQSessionMapper}=load(path.join(dir,'QQSessionMapper.ts'));
 const mapper=new QQSessionMapper(undefined,()=> 'fixed');
 const gateway={submitTurn:async function*(req){turns.push(req);yield {type:'assistant_text_delta',text:'  reply  '};}};
 const channel=new QQChannel({appId:'fixture-app',clientSecret:'fixture-secret',mapper});
 const handle=await channel.start({gateway,logger:Object.fromEntries(['info','error','warn'].map(k=>[k,x=>logs.push([k,x])]))});
 const bot=channel.botGateway;
 bot.handleGatewayMessage({op:10,d:{heartbeat_interval:999999}});
 bot.handleGatewayMessage({op:0,s:12,t:'READY',d:{session_id:'fixture-session'}});
 bot.handleGatewayMessage({op:10,d:{heartbeat_interval:999999}});
 await channel.handleGroupMessage({id:'m1',group_openid:'g',author:{member_openid:'u'},content:'/ask /new'});
 await channel.handleGroupMessage({id:'m2',group_openid:'g',author:{member_openid:'u'},content:'/chat hello'});
 await channel.handleC2CMessage({id:'m3',author:{user_openid:'u'},content:'/new'});
 await handle.stop('fixture');
 console.log(JSON.stringify({calls,logs,turns,state:mapper.snapshot(),chunks:channel.splitText('x'.repeat(1999)+'\nrest',2000)}));
})().catch(e=>{console.error(e);process.exitCode=1;});
