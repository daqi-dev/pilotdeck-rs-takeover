// Executes the fixed TypeScript sources in memory; never writes the oracle tree.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = process.env.PILOTDECK_TS_ROOT || '/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const ts = require(path.join(root, 'node_modules/typescript'));
const calls = [], logs = [], turns = [];
let fail = false;
class Bot {
  constructor(token) { calls.push(['token',token]); this.api = {
    deleteWebhook: async()=>calls.push(['deleteWebhook']),
    setWebhook: async(url)=>calls.push(['setWebhook',url]),
    getMe: async()=>({username:'mockbot'}),
    sendMessage: async(chat,text)=>{calls.push(['sendMessage',chat,text]);if(fail)throw Error('mock rejected');},
    sendChatAction:async(chat,action)=>calls.push(['typing',chat,action]),
  }; }
  on(){} catch(){} start(options){calls.push(['start',options]);} async stop(){calls.push(['stop']);}
}
const cache = new Map();
function load(filename) {
  if(cache.has(filename))return cache.get(filename).exports;
  const module = {exports:{}}; cache.set(filename,module);
  const code=ts.transpileModule(fs.readFileSync(filename,'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
  const req = name => name==='grammy'?{Bot}:name.startsWith('.')?load(path.resolve(path.dirname(filename),name.replace(/\.js$/,'.ts'))):require(name);
  vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{process,console,setTimeout,clearTimeout,AbortController,Map,Set})(req,module,module.exports);
  return module.exports;
}
(async()=>{
  const dir=path.join(root,'src/adapters/channel/telegram');
  const {TelegramChannel}=load(path.join(dir,'TelegramChannel.ts'));
  const {TelegramSessionMapper}=load(path.join(dir,'TelegramSessionMapper.ts'));
  const mapper=new TelegramSessionMapper(undefined,()=> 'fixed');
  const gateway={submitTurn:async function*(req){turns.push(req);yield {type:'assistant_text_delta',text:'  reply  '};}};
  const channel=new TelegramChannel({token:'fixture-token',mapper});
  const handle=await channel.start({gateway,logger:Object.fromEntries(['info','error','warn'].map(k=>[k,x=>logs.push([k,x])]))});
  await channel.handleTextMessage({message:{chat:{id:42},text:'/new'}});
  await channel.handleTextMessage({message:{chat:{id:42},text:'hello'}});
  await channel.sendReply('42','x'.repeat(4095)+'\n'+'tail');
  fail=true; const failure=await channel.sendReply('42','reject'); fail=false;
  await handle.stop('fixture');
  console.log(JSON.stringify({calls,logs,turns,failure,state:mapper.snapshot()}));
})().catch(e=>{console.error(e);process.exitCode=1;});
