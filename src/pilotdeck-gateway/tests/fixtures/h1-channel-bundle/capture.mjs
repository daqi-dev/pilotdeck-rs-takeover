// H1 module-composition characterization: actual TS bundle/loader/lifecycle,
// mocked channel start/stop and Gateway. This is not a formal CLI/server run.
import assert from 'node:assert/strict';
import { mkdirSync, readFileSync, writeFileSync, realpathSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
const metadata=JSON.parse(readFileSync(new URL('./metadata.json',import.meta.url),'utf8'));
const CONTROL_NAMES=['loader-order','forget-history','stale-options','wrong-stop-reason','fake-success'];
const REVISION='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
assert.equal(metadata.revision,REVISION);
assert.equal(metadata.node,'v22.22.0');
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const save=(path,value)=>writeFileSync(path,JSON.stringify(value,null,2)+'\n',{flag:'wx'});
assert.equal(process.version,'v22.22.0');
if(process.argv[2]==='--compare'){
 assert.equal(process.argv.length,5,'capture.mjs --compare FIRST/summary.json SECOND/summary.json');
 const first=JSON.parse(readFileSync(process.argv[3],'utf8'));
 const second=JSON.parse(readFileSync(process.argv[4],'utf8'));
 for(const result of [first,second]){
  assert.equal(result.revision,REVISION);assert.equal(result.node,metadata.node);
  assert.equal(result.kind,'module-composition-mock');
  assert.deepEqual(result.sourceSha256,metadata.sourceSha256);
  assert.deepEqual(result.compiler,metadata.compiler);
  assert.deepEqual(result.controls,CONTROL_NAMES.map(name=>({name,rejected:true})));
  assert.equal(result.negativeControlKind,'observed-result-comparator-mutations');
  assert.equal(result.driverSha256,sha(readFileSync(fileURLToPath(import.meta.url))));
  verify(result.observed);
 }
 assert.deepEqual(first,second);
 console.log(JSON.stringify({selfCompare:'passed',operations:first.observed.results.length,negativeControls:first.controls.length}));
 process.exit(0);
}
const runtimeMutant=process.argv[2]==='--mutant-drop-history';
assert.equal(process.argv.length,runtimeMutant?4:3,'capture.mjs [--mutant-drop-history] OUTPUT');
const output=resolve(process.argv[runtimeMutant?3:2]); mkdirSync(output,{mode:0o700});
for(const name of ['home','pilot','tmp'])mkdirSync(join(output,name),{mode:0o700});
let phase='identity';
let stateSnapshot=()=>undefined;
function recordFailure(error, filename='failure.json'){
 try{save(join(output,filename),{phase,runtimeMutant,revision:REVISION,node:process.version,name:error.name,message:error.message,stack:error.stack,observed:stateSnapshot()});}
 catch(writeError){if(writeError.code!=='EEXIST')process.stderr.write(`failure recording rejected: ${writeError.message}\n`);}
}
// The driver owns this process and its one random listener. A stuck import,
// lifecycle operation, or close is bounded even when finally cannot finish.
const deadline=setTimeout(()=>{
 recordFailure(new Error('H1 module capture exceeded 15 seconds'),'deadline.json');
 process.exit(124);
},15_000);
deadline.unref();
process.on('uncaughtExceptionMonitor',error=>recordFailure(error));
const baseline=realpathSync(process.env.PDRS_BASELINE??'/Users/da/ws/pilotdeck-rs/resources/PilotDeck');
const identity=spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'});
assert.equal(identity.status,0);assert.equal(identity.stdout.trim(),REVISION);
const clean=spawnSync('git',['-C',baseline,'diff','--quiet','HEAD','--','src','packages'],{encoding:'utf8'});
assert.equal(clean.status,0,'baseline tracked source must be unchanged');
for(const key of Object.keys(process.env))delete process.env[key];
Object.assign(process.env,{HOME:join(output,'home'),PILOT_HOME:join(output,'pilot'),TMPDIR:join(output,'tmp'),PATH:'',ANALYTICS_ENABLED:'0',DOCKER_CONTAINER:'1'});
const sourceSha256=Object.fromEntries(Object.keys(metadata.sourceSha256).map(file=>[file,sha(readFileSync(join(baseline,file)))]));
assert.deepEqual(sourceSha256,metadata.sourceSha256,'fixed baseline source/build identity');
assert.equal(sha(readFileSync(join(baseline,'tsconfig.json'))),metadata.compiler.tsconfigSha256);
assert.equal(sha(readFileSync(join(baseline,'node_modules/typescript/lib/typescript.js'))),metadata.compiler.typescriptSha256);
phase='source-to-dist';
const {default:ts}=await import(pathToFileURL(join(baseline,'node_modules/typescript/lib/typescript.js')));
assert.equal(ts.version,metadata.compiler.version);
const tsconfig=JSON.parse(readFileSync(join(baseline,'tsconfig.json'),'utf8'));
const compilerOptions=ts.convertCompilerOptionsFromJson(tsconfig.compilerOptions,baseline).options;
// The reference package is ESM. transpileModule has no package lookup, so
// select ESNext emit explicitly for the reference's NodeNext ESM source files.
assert.equal(JSON.parse(readFileSync(join(baseline,'package.json'),'utf8')).type,'module');
const transpileMatches=[];
for(const file of Object.keys(sourceSha256).filter(file=>file.startsWith('src/'))){
 const emitted=ts.transpileModule(readFileSync(join(baseline,file),'utf8'),{
  fileName:join(baseline,file),compilerOptions:{...compilerOptions,module:ts.ModuleKind.ESNext},
 }).outputText;
 const built=`dist/${file.replace(/\.ts$/,'.js')}`;
 assert.equal(emitted,readFileSync(join(baseline,built),'utf8'),`source-to-dist byte equality: ${file}`);
 transpileMatches.push({source:file,built,sha256:sha(emitted)});
}
phase='imports';
const {ChannelAdapterBundle}=await import(pathToFileURL(join(baseline,'dist/src/cli/ChannelAdapterBundle.js')));
const {loadEnabledChannels}=await import(pathToFileURL(join(baseline,'dist/src/adapters/channel/loadEnabledChannels.js')));
const {startPilotDeckServer}=await import(pathToFileURL(join(baseline,'dist/src/cli/pilotdeckServer.js')));
const config=(generation,adapters)=>({generation,adapters});
const initial=config('initial',{
 homeassistant:{enabled:true,token:'ha-one',extra:{url:'http://127.0.0.1:1/ha-one'}},
 signal:{enabled:true,extra:{restUrl:'http://127.0.0.1:1/signal-one',account:'signal-one'}},
});
const updated=config('updated',{
 signal:{enabled:true,extra:{restUrl:'http://127.0.0.1:1/signal-two',account:'signal-two'}},
 mattermost:{enabled:true,token:'mm-two',extra:{serverUrl:'http://127.0.0.1:1/mm-two',teamId:'team-two'}},
 homeassistant:{enabled:false},
});
const disabled=config('disabled',{});
const trace=[],constructions=[],reconciles=[],results=[];let server;
stateSnapshot=()=>({results,constructions,reconciles,trace});
let failLoad=false;
function controlled(key,label,flags={}){
 let stops=0;
 return {channelKey:key,async start(deps){
  trace.push({event:'start',key,label,configGeneration:deps.config.generation});
  if(flags.failStart)throw new Error(`${key} start rejected: ${label}`);
  return {async stop(reason){trace.push({event:'stop',key,label,reason});if(flags.failStop&&stops++===0)throw new Error(`${key} stop rejected: ${label}`);}};
 }};
}
const bundle=new ChannelAdapterBundle({pilotHome:join(output,'pilot'),statePersistence:{async load(){return undefined},save(){},async flush(){}},
 async loadEnabledChannels(adapters){
  if(failLoad)throw new Error('configured loader rejected');
  const loaded=await loadEnabledChannels(adapters);
  return loaded.map(channel=>{
   const key=channel.channelKey;
   const options=key==='signal'?{restUrl:channel.restUrl,account:channel.account}
    :key==='mattermost'?{token:channel.token,serverUrl:channel.serverUrl,teamId:channel.teamId}
    :{url:channel.url,token:channel.token};
   constructions.push({key,options});
   const label=options.account??options.token;
   return controlled(key,label,adapters[key].extra);
  });
 }});
try{
 phase='lifecycle';
 server=await startPilotDeckServer({gateway:{},host:'127.0.0.1',port:0,token:'h1-fixture',pilotHome:join(output,'pilot'),config:initial});
 const lifecycle={async reconcileChannels(input){
  // A real boundary mutation: send the third reconcile to the actual server
  // without a historical key. This must leave the manually started handle
  // running and make the ordinary behavior assertions fail.
  if(runtimeMutant&&reconciles.length===2)input={...input,managedChannelKeys:input.managedChannelKeys.filter(key=>key!=='signal')};
  reconciles.push({channels:input.channels.map(channel=>channel.channelKey),managedChannelKeys:[...input.managedChannelKeys]});
  return server.reconcileChannels(input);
 }};
 const startup=await bundle.createStartupAdapters(initial);
 results.push({operation:'startup',channels:startup.channels.map(channel=>channel.channelKey)});
 for(const channel of startup.channels)await server.hotStartChannel(channel);
 results.push({operation:'replace-add-disable',changes:await bundle.reload(lifecycle,updated)});
 results.push({operation:'disable',changes:await bundle.reload(lifecycle,disabled)});
 await server.hotStartChannel(controlled('signal','manual-history'));
 results.push({operation:'disable-again-after-manual-start',changes:await bundle.reload(lifecycle,disabled)});
 const base=config('restore-base',{signal:{enabled:true,extra:{restUrl:'http://127.0.0.1:1/base',account:'restore-base'}}});
 results.push({operation:'restore-base',changes:await bundle.reload(lifecycle,base)});
 const fail=config('start-failure',{signal:{enabled:true,extra:{restUrl:'http://127.0.0.1:1/fail',account:'candidate-fail',failStart:true}}});
 try{await bundle.reload(lifecycle,fail);results.push({operation:'start-failure',unexpectedSuccess:true});}
 catch(error){results.push({operation:'start-failure',error:error.message});}
 results.push({operation:'disable-restored',changes:await bundle.reload(lifecycle,disabled)});
 const stopFail=config('stop-failure',{signal:{enabled:true,extra:{restUrl:'http://127.0.0.1:1/stop',account:'stop-fail',failStop:true}}});
 results.push({operation:'start-stop-failure',changes:await bundle.reload(lifecycle,stopFail)});
 try{await bundle.reload(lifecycle,disabled);results.push({operation:'stop-failure',unexpectedSuccess:true});}
 catch(error){results.push({operation:'stop-failure',error:error.message});}
 results.push({operation:'retry-disable',changes:await bundle.reload(lifecycle,disabled)});
 failLoad=true;
 try{await bundle.reload(lifecycle,disabled);results.push({operation:'loader-failure',unexpectedSuccess:true});}
 catch(error){results.push({operation:'loader-failure',error:error.message});}
 await bundle.flush();
}catch(error){recordFailure(error);throw error;}
finally{phase='close';if(server)await server.close();}
const observed={results,constructions,reconciles,trace};
save(join(output,'observed.json'),observed);
function verify(data){
 assert.deepEqual(data.results,[
  {operation:'startup',channels:['signal','homeassistant']},
  {operation:'replace-add-disable',changes:['mattermost=started','signal=started','homeassistant=stopped']},
  {operation:'disable',changes:['signal=stopped','mattermost=stopped']},
  {operation:'disable-again-after-manual-start',changes:['signal=stopped']},
  {operation:'restore-base',changes:['signal=started']},
  {operation:'start-failure',error:'signal start rejected: candidate-fail'},
  {operation:'disable-restored',changes:['signal=stopped']},
  {operation:'start-stop-failure',changes:['signal=started']},
  {operation:'stop-failure',error:'signal stop rejected: stop-fail'},
  {operation:'retry-disable',changes:['signal=stopped']},
  {operation:'loader-failure',error:'configured loader rejected'},
 ]);
 assert.equal(data.reconciles.length,9);
 const managed=['feishu','weixin','qq','wecom','signal','homeassistant','mattermost'];
 assert.deepEqual(data.reconciles[0],{channels:['mattermost','signal'],managedChannelKeys:managed});
 for(const row of data.reconciles)assert.deepEqual(row.managedChannelKeys,managed);
 assert.deepEqual(data.constructions.slice(0,4),[
  {key:'signal',options:{restUrl:'http://127.0.0.1:1/signal-one',account:'signal-one'}},
  {key:'homeassistant',options:{url:'http://127.0.0.1:1/ha-one',token:'ha-one'}},
  {key:'mattermost',options:{token:'mm-two',serverUrl:'http://127.0.0.1:1/mm-two',teamId:'team-two'}},
  {key:'signal',options:{restUrl:'http://127.0.0.1:1/signal-two',account:'signal-two'}},
 ]);
 assert.ok(data.trace.filter(row=>row.event==='start').every(row=>row.configGeneration==='initial'));
 assert.deepEqual(data.trace.slice(2,6),[
  {event:'stop',key:'homeassistant',label:'ha-one',reason:'config-disabled'},
  {event:'start',key:'mattermost',label:'mm-two',configGeneration:'initial'},
  {event:'stop',key:'signal',label:'signal-one',reason:'hot-reload'},
  {event:'start',key:'signal',label:'signal-two',configGeneration:'initial'},
 ]);
 const failed=data.trace.findIndex(row=>row.event==='start'&&row.label==='candidate-fail');
 assert.deepEqual(data.trace[failed+1],{event:'start',key:'signal',label:'restore-base',configGeneration:'initial'});
 assert.equal(data.trace.filter(row=>row.event==='stop'&&row.label==='stop-fail'&&row.reason==='config-disabled').length,2);
}
phase='verify';
verify(observed);
const controls=[];
for(const name of CONTROL_NAMES){
 const bad=structuredClone(observed);
 if(name==='loader-order')bad.results[0].channels.reverse();
 if(name==='forget-history')bad.reconciles.at(-1).managedChannelKeys.pop();
 if(name==='stale-options')bad.constructions[3].options.account='signal-one';
 if(name==='wrong-stop-reason')bad.trace[2].reason='server-shutdown';
 if(name==='fake-success')bad.results[5]={operation:'start-failure',changes:['signal=started']};
 assert.throws(()=>verify(bad));controls.push({name,rejected:true});
}
save(join(output,'summary.json'),{revision:REVISION,node:process.version,kind:'module-composition-mock',sourceSha256,compiler:metadata.compiler,transpileMatches,negativeControlKind:'observed-result-comparator-mutations',driverSha256:sha(readFileSync(fileURLToPath(import.meta.url))),observed,controls});
clearTimeout(deadline);
console.log(JSON.stringify({operations:results.length,reconciles:reconciles.length,negativeControls:controls.length}));
