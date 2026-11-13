// Neutral fixture action: it writes only this scene's existing owned config.
// No product code, runtime reload, arbitrary path or command is executed.
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {performance} from 'node:perf_hooks';
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
const clone=value=>structuredClone(value);
const identity=s=>({dev:String(s.dev),ino:String(s.ino),mode:Number(s.mode),uid:Number(s.uid),nlink:Number(s.nlink),size:Number(s.size),mtimeNs:String(s.mtimeNs)});
const notification=n=>n?.type==='notification'&&n.name==='config_changed'&&Array.isArray(n.payload?.changedPaths)&&n.payload.changedPaths.includes('agent.maxOutputTokens');
function readOwned(file,expected){
 const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW|fs.constants.O_NONBLOCK);
 try{const stat=fs.fstatSync(fd,{bigint:true});assert.ok(stat.isFile());assert.equal(stat.dev,expected.dev);assert.equal(stat.ino,expected.ino);assert.equal(stat.nlink,1n);assert.ok(stat.size<=1048576n);return {bytes:fs.readFileSync(fd),stat};}finally{fs.closeSync(fd);}
}
function rewritten(source){
 const lines=source.split('\n'),agent=lines.flatMap((v,i)=>v==='agent:'?[i]:[]);assert.equal(agent.length,1,'exact agent section');let end=agent[0]+1;while(end<lines.length&&(!lines[end].trim()||/^\s/.test(lines[end])))end++;
 const matching=[];for(let i=agent[0]+1;i<end;i++)if(/^  maxOutputTokens: 4096\s*$/.test(lines[i]))matching.push(i);assert.equal(matching.length,1,'exact from value once');lines[matching[0]]=lines[matching[0]].replace('4096','2048');return lines.join('\n');
}
function snapshot(value,allowedPaths){
 assert.equal(value?.step?.ws,'resolve_settings','only read-only snapshot RPC');assert.deepEqual(value.step.params,{});
 const response=value.frames?.filter(f=>f.type==='response');assert.equal(response?.length,1);assert.equal(response[0].ok,true);assert.equal(response[0].error,undefined);
 const result=response[0].result;assert.ok(Number.isSafeInteger(result?.version)&&result.version>0);assert.match(result.contentHash,/^[a-f0-9]{64}$/);assert.ok(Array.isArray(result.sources)&&result.sources.some(s=>allowedPaths.includes(s.path)),'snapshot belongs to exact written config');return {version:result.version,contentHash:result.contentHash};
}
async function bounded(promise,remaining){
 if(remaining<=0)return null;let timer;
 try{return await Promise.race([promise,new Promise(resolve=>{timer=setTimeout(()=>resolve(null),remaining);})]);}finally{clearTimeout(timer);}
}
export async function externalConfigRewrite({scene,input,pilotHome,ownedRoot,resolveSettings,readFrames,timeoutMs=30000,pollMs=20}){
 assert.equal(scene,'settings-update','one exact fixture scene');assert.deepEqual(Object.keys(input??{}).sort(),['fromMaxOutputTokens','toMaxOutputTokens']);assert.equal(input.fromMaxOutputTokens,4096);assert.equal(input.toMaxOutputTokens,2048);
 assert.ok(Number.isSafeInteger(timeoutMs)&&timeoutMs>0&&timeoutMs<=30000);assert.ok(Number.isSafeInteger(pollMs)&&pollMs>0&&pollMs<=200);assert.equal(typeof resolveSettings,'function');assert.equal(typeof readFrames,'function');
 const root=fs.realpathSync(ownedRoot),home=fs.realpathSync(pilotHome);assert.ok(home.startsWith(root+path.sep),'config home must be inside owned root');
 const file=path.join(home,'pilotdeck.yaml'),requestedFile=path.join(path.resolve(pilotHome),'pilotdeck.yaml'),entry=fs.lstatSync(file,{bigint:true});assert.ok(entry.isFile()&&!entry.isSymbolicLink(),'owned config must be regular, not link');assert.equal(fs.realpathSync(file),file);assert.equal(entry.nlink,1n,'no hardlinked external config');if(process.getuid)assert.equal(entry.uid,BigInt(process.getuid()),'owned config UID');
 assert.ok(entry.size<=1024*1024,'bounded fixture config');const flags=fs.constants.O_RDWR|fs.constants.O_NOFOLLOW;assert.ok(fs.constants.O_NOFOLLOW,'no symlink-follow fallback');
 const start=performance.now(),deadline=start+timeoutMs;const receipt={schema:1,scene,file,requestedFile,timeoutMs,rpc:[],notification:null,written:false,observed:false};
 let fd;
 try{
  const first=await bounded(resolveSettings(),deadline-performance.now());if(first===null)return {timedOut:true,externalConfigRewrite:{written:false,observed:false},receipt};
  receipt.rpc.push(clone(first));const beforeSnapshot=snapshot(first,[file,requestedFile]);receipt.beforeSnapshot=beforeSnapshot;
  fd=fs.openSync(file,flags);const opened=fs.fstatSync(fd,{bigint:true});assert.equal(opened.dev,entry.dev);assert.equal(opened.ino,entry.ino);assert.ok(opened.isFile());assert.equal(opened.nlink,1n);assert.ok(opened.size<=1048576n);
  const before=fs.readFileSync(fd),source=before.toString('utf8');assert.equal(Buffer.from(source).compare(before),0,'UTF8 fixture config');
  const after=Buffer.from(rewritten(source));
  receipt.beforeFile={...identity(opened),sha256:hash(before),text:source};receipt.frameCursor=readFrames().length;
  assert.ok(performance.now()<deadline,'deadline before mutation');receipt.writeStartedMs=performance.now()-start;fs.writeSync(fd,after,0,after.length,0);fs.ftruncateSync(fd,after.length);fs.fsyncSync(fd);
  const observedFile=readOwned(file,opened),actual=observedFile.bytes,afterStat=observedFile.stat;assert.ok(afterStat.isFile()&&!afterStat.isSymbolicLink());assert.equal(afterStat.dev,opened.dev);assert.equal(afterStat.ino,opened.ino,'same inode external inplace write');assert.deepEqual(actual,after);assert.notEqual(hash(before),hash(actual));
  receipt.afterFile={...identity(afterStat),sha256:hash(actual),text:actual.toString('utf8')};receipt.written=true;receipt.writeCompletedMs=performance.now()-start;fs.closeSync(fd);fd=undefined;
  let nextNotification=receipt.frameCursor;
  while(performance.now()<deadline){
   const frames=readFrames();const index=frames.findIndex((f,i)=>i>=nextNotification&&notification(f));
   if(index>=0){
    nextNotification=index+1;
    const candidate=await bounded(resolveSettings(),deadline-performance.now());if(candidate===null)break;receipt.rpc.push({...clone(candidate),afterNotificationIndex:index});const observed=snapshot(candidate,[file,requestedFile]);
    if(observed.version>beforeSnapshot.version&&observed.contentHash!==beforeSnapshot.contentHash&&performance.now()<deadline){
     const current=readOwned(file,opened),now=current.stat;assert.deepEqual(current.bytes,after,'no other config writer');assert.equal(now.nlink,1n);assert.equal(now.mode,opened.mode);assert.equal(now.uid,opened.uid);
     receipt.notification={index,frame:clone(frames[index])};receipt.observedFrameCount=readFrames().length;receipt.afterSnapshot=observed;receipt.observed=true;break;
    }
   }
   await new Promise(resolve=>setTimeout(resolve,Math.min(pollMs,Math.max(1,deadline-performance.now()))));
  }
  receipt.durationMs=performance.now()-start;
  return {externalConfigRewrite:{written:true,observed:receipt.observed,fromMaxOutputTokens:4096,toMaxOutputTokens:2048},...(!receipt.observed?{timedOut:true}:{}),receipt};
 }finally{if(fd!==undefined)fs.closeSync(fd);}
}

const one=(rows,label)=>{assert.ok(Array.isArray(rows),label);assert.equal(rows.length,1,label);return rows[0];};
const messageText=m=>typeof m.content==='string'?m.content:Array.isArray(m.content)?m.content.filter(p=>p.type==='text').map(p=>p.text).join(''):'';
export function verifyConfigWatchInput(artifact){
 assert.equal(artifact?.scenario,'settings-update');assert.equal(artifact.side,'rust');assert.deepEqual(artifact.failures,[]);assert.equal(artifact.steps?.length,29);
 const setup=artifact.steps[26];assert.deepEqual(setup.step,{ws:'update_settings',params:{source:'localSettings',settings:{agent:{maxOutputTokens:4096}}}});const setupResponse=one(setup.frames.filter(f=>f.type==='response'),'actual setup response');assert.equal(setupResponse.ok,true);assert.equal(setupResponse.error,undefined);assert.ok(setupResponse.result.changedPaths.includes('agent.maxOutputTokens'));
 const action=artifact.steps[27];assert.deepEqual(action.step,{externalConfigRewrite:{fromMaxOutputTokens:4096,toMaxOutputTokens:2048}});assert.equal(action.timedOut,undefined);assert.deepEqual(action.externalConfigRewrite,{written:true,observed:true,fromMaxOutputTokens:4096,toMaxOutputTokens:2048,receiptIndex:0});
 const proof=one(artifact.evidence?.configWatchInput,'actual config write receipt');assert.equal(proof.schema,1);assert.equal(proof.scene,'settings-update');assert.equal(proof.stepIndex,27);assert.equal(proof.timeoutMs,30000);assert.equal(proof.written,true);assert.equal(proof.observed,true);assert.ok(path.isAbsolute(proof.file)&&proof.file.endsWith('/home/.pilotdeck/pilotdeck.yaml'));assert.ok(path.isAbsolute(proof.requestedFile)&&proof.requestedFile.endsWith('/home/.pilotdeck/pilotdeck.yaml'));
 const before=proof.beforeFile,after=proof.afterFile;for(const stat of [before,after]){assert.match(stat?.dev,/^\d+$/);assert.match(stat.ino,/^\d+$/);assert.equal(stat.nlink,1);assert.equal(stat.size,Buffer.byteLength(stat.text));assert.equal(stat.sha256,hash(Buffer.from(stat.text)));}
 for(const key of ['dev','ino','uid','mode'])assert.equal(after[key],before[key],'same owned inode '+key);assert.equal(after.text,rewritten(before.text));assert.notEqual(before.sha256,after.sha256);assert.equal(artifact.tree?.['/home/.pilotdeck/pilotdeck.yaml']?.text,after.text,'actual final config bytes');
 assert.ok(Number.isFinite(proof.writeStartedMs)&&proof.writeStartedMs>=0);assert.ok(proof.writeCompletedMs>=proof.writeStartedMs&&proof.durationMs>=proof.writeCompletedMs&&proof.durationMs<=30000);
 assert.ok(Number.isSafeInteger(proof.frameCursor)&&proof.frameCursor>=0);assert.ok(Number.isSafeInteger(proof.notification?.index)&&proof.notification.index>=proof.frameCursor&&Number.isSafeInteger(proof.observedFrameCount)&&proof.notification.index<proof.observedFrameCount);assert.ok(notification(proof.notification.frame));assert.ok(artifact.notifications.some(n=>JSON.stringify(n)===JSON.stringify(proof.notification.frame)),'same actual captured config notification');
 assert.ok(Array.isArray(proof.rpc)&&proof.rpc.length>=2);const first=snapshot(proof.rpc[0],[proof.file,proof.requestedFile]);assert.equal(proof.rpc[0].afterNotificationIndex,undefined);for(const rpc of proof.rpc)snapshot(rpc,[proof.file,proof.requestedFile]);const last=one(proof.rpc.filter(r=>r.afterNotificationIndex===proof.notification.index),'read-only snapshot tied to fresh notification');const next=snapshot(last,[proof.file,proof.requestedFile]);assert.deepEqual(first,proof.beforeSnapshot);assert.deepEqual(next,proof.afterSnapshot);assert.ok(next.version>first.version&&next.contentHash!==first.contentHash);
 const turn=artifact.steps[28];assert.deepEqual(turn.step,{ws:'submit_turn',params:{sessionKey:'s-watch-rewrite',channelKey:'test',message:'watch-rewrite: confirm current config'}});assert.ok(!turn.frames.some(f=>f.error||f.type?.startsWith('__')));const started=one(turn.frames.filter(f=>f.event?.type==='turn_started'),'real following turn');const terminal=one(turn.frames.filter(f=>f.final),'real following terminal');assert.equal(terminal.event?.type,'turn_completed');assert.equal(terminal.event.runId,started.event.runId);assert.equal(terminal.event.finishReason,'completed');assert.ok(terminal.seq>started.seq);
 const request=one(artifact.provider?.['watch-rewrite'],'actual following provider request');assert.equal(request.method,'POST');assert.equal(request.url,'/v1/chat/completions');assert.equal(request.body.max_tokens,2048);assert.equal(request.body.messages.filter(m=>m.role==='user'&&messageText(m)==='watch-rewrite: confirm current config').length,1);
 const [chat]=one(Object.entries(artifact.tree).filter(([file])=>file.endsWith('/chats/s-watch-rewrite.jsonl')),'actual following durable transcript');const rows=artifact.tree[chat].jsonl;assert.ok(Array.isArray(rows)&&rows.length>0);const entryIds=new Set();for(let i=0;i<rows.length;i++){const row=rows[i];assert.equal(row.sessionId,'s-watch-rewrite');assert.equal(row.sequence,i+1);assert.ok(typeof row.entryId==='string'&&row.entryId&&!entryIds.has(row.entryId));entryIds.add(row.entryId);assert.equal(row.parentEntryId,i?rows[i-1].entryId:null);}one(rows.filter(r=>r.type==='turn_started'&&r.turnId===started.event.runId),'same-run durable start');const model=one(rows.filter(r=>r.type==='model_request'&&r.turnId===started.event.runId),'same-run durable model');assert.ok(model.request?.messages?.some(m=>m.role==='user'&&messageText(m)==='watch-rewrite: confirm current config'));const assistant=one(rows.filter(r=>r.type==='assistant_message'&&r.turnId===started.event.runId),'same-run durable assistant');assert.equal(messageText(assistant.message),'Watch rewrite applied.');const accepted=one(rows.filter(r=>r.type==='accepted_input'&&r.turnId===started.event.runId),'same-run persisted input');assert.equal(accepted.sessionId,'s-watch-rewrite');assert.equal(accepted.messages.filter(m=>m.role==='user'&&messageText(m)==='watch-rewrite: confirm current config').length,1);const completed=one(rows.filter(r=>r.type==='turn_result'&&r.turnId===started.event.runId),'same-run persisted terminal');assert.equal(completed.result?.type,'success');assert.equal(completed.result.turnId,started.event.runId);assert.equal(completed.result.sessionId,'s-watch-rewrite');assert.equal(completed.result.stopReason,'completed');assert.equal(messageText(completed.result.finalMessage),'Watch rewrite applied.');
 return true;
}
