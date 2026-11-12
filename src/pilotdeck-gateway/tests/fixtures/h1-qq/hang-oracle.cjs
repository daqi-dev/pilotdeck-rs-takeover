// Fixed TS splitText in an owned worker. A timeout is failure evidence, not success.
const fs = require('node:fs');
const path = require('node:path');
const {Worker} = require('node:worker_threads');
const root = process.env.PILOTDECK_TS_ROOT || '/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const preamble = fs.readFileSync(path.join(__dirname,'oracle.cjs'),'utf8').split('(async()=>{')[0];
async function probe(limit) {
  const worker = new Worker(preamble + `
    const {parentPort}=require('node:worker_threads');
    const {QQChannel}=load(path.join(root,'src/adapters/channel/qq/QQChannel.ts'));
    const channel=new QQChannel({appId:'fixture-app',clientSecret:'fixture-secret'});
    parentPort.postMessage({kind:'entered',limit:${limit}});
    const chunks=channel.splitText('abc',${limit});
    parentPort.postMessage({kind:'returned',chunks});
  `, {eval:true});
  const messages=[];
  let timer;
  return await new Promise((resolve,reject)=>{
    worker.once('error',reject);
    worker.on('message',async message=>{
      messages.push(message);
      if(message.kind==='entered') timer=setTimeout(async()=>{
        const exitCode=await worker.terminate();
        resolve({limit,outcome:'did-not-return',deadlineMs:100,termination:'parent-terminated-owned-worker',exitCode,messages});
      },100);
      if(message.kind==='returned'){
        clearTimeout(timer);
        const exitCode=await worker.terminate();
        resolve({limit,outcome:'returned',termination:'parent-terminated-owned-worker-after-result',exitCode,messages});
      }
    });
  });
}
(async()=>{
 for(const limit of [2,0,-1]){
   const result=await probe(limit);
   console.log(JSON.stringify(result));
   if((limit>0)!==(result.outcome==='returned'))process.exitCode=1;
 }
})().catch(error=>{console.error(error);process.exitCode=1;});
