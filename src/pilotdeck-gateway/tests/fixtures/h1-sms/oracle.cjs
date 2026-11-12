// Executes the fixed TypeScript sources in memory; never writes the oracle tree.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = process.env.PILOTDECK_TS_ROOT || '/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const ts = require(path.join(root, 'node_modules/typescript'));
const calls=[], logs=[], turns=[];
let sdkAvailable=true;
function twilio(sid,token){calls.push(['twilio',sid,token]);return {messages:{create:async message=>{calls.push(['create',message]);if(message.body==='reject')throw Error('fixture rejected');return {sid:'SMfixture'};}}};}
const cache = new Map();
function load(filename) {
  if(cache.has(filename))return cache.get(filename).exports;
  const module = {exports:{}}; cache.set(filename,module);
  const code=ts.transpileModule(fs.readFileSync(filename,'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
  const req = name => name==='twilio'?(sdkAvailable?twilio:(()=>{throw Error('SDK absent')})()):name.startsWith('.')?load(path.resolve(path.dirname(filename),name.replace(/\.js$/,'.ts'))):require(name);
  vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{process,console,setTimeout,clearTimeout,AbortController,Map,Set,Buffer,URLSearchParams,URL})(req,module,module.exports);
  return module.exports;
}
(async()=>{
 const dir=path.join(root,'src/adapters/channel/sms');
 const {SmsChannel}=load(path.join(dir,'SmsChannel.ts'));
 const gateway={submitTurn:async function*(request){turns.push(request);yield {type:'assistant_text_delta',text:'中文 reply + space'};}};
 const channel=new SmsChannel({extra:{accountSid:'ACfixture',authToken:'fixture-token',phoneNumber:'+10000000000',webhookHost:'127.0.0.1',webhookPort:0}});
 const logger=Object.fromEntries(['info','warn','error'].map(level=>[level,text=>logs.push([level,text])]));
 const handle=await channel.start({gateway,logger});
 const port=channel.server.address().port;
 const response=await fetch(`http://127.0.0.1:${port}/sms/incoming`,{method:'POST',headers:{'content-type':'application/x-www-form-urlencoded'},body:new URLSearchParams({From:'+10000000001',Body:'hello'})});
 const webhook={status:response.status,body:await response.text()};
 const deadline=Date.now()+2000;while(!calls.some(c=>c[0]==='create')){if(Date.now()>deadline)throw Error('SMS send timeout');await new Promise(resolve=>setTimeout(resolve,5));}
 const failure=await channel.sendReply('+10000000001','reject');
 await handle.stop('fixture');
 sdkAvailable=false;cache.clear();
 const {SmsChannel:Missing}=load(path.join(dir,'SmsChannel.ts'));
 const absent=new Missing({extra:{accountSid:'ACfixture',authToken:'fixture-token',phoneNumber:'+10000000000',webhookPort:0}});
 const absentHandle=await absent.start({gateway,logger});await absentHandle.stop('fixture');
 console.log(JSON.stringify({calls,logs,turns,webhook,failure,missingSdkListening:absent.server!==null}));
})().catch(error=>{console.error(error);process.exitCode=1;});
