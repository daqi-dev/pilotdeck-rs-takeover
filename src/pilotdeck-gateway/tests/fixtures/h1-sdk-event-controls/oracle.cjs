// Whole fixed modules are transpiled without modifying their bodies. Imports
// outside each exercised pure closure fail closed if accessed.
const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto');
const root=process.env.PILOTDECK_TS_ROOT||'/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const ts=require(path.join(root,'node_modules/typescript'));
const sources=[];
function load(relative,imports={}){
 const source=fs.readFileSync(path.join(root,relative),'utf8');
 sources.push({path:relative,sha256:crypto.createHash('sha256').update(source).digest('hex')});
 const compiled=ts.transpileModule(source,{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
 const module={exports:{}};
 const dependency=name=>{
  if(name.startsWith('node:'))return require(name);
  if(Object.hasOwn(imports,name))return imports[name];
  return new Proxy({},{get:(_,key)=>{if(key==='__esModule')return true;throw Error(`unexercised import accessed: ${relative} -> ${name}.${String(key)}`);}});
 };
 new Function('require','module','exports',compiled)(dependency,module,module.exports);
 return module.exports;
}
const permission=load('src/permission/protocol/types.ts');
const {SessionAgentConfigBundle}=load('src/cli/SessionAgentConfigBundle.ts',{'../permission/index.js':{createDefaultPermissionContext:permission.createDefaultPermissionContext,permissionEntryToRule:()=>{throw Error('policy import outside controls probe');}}});
const rows=[];
for(const config of [{},{agentProgressSummaries:false},{agentProgressSummaries:true},{outputFormat:{type:'json_schema',schema:{type:'object',required:['status'],properties:{status:{type:'string'}}}}},{agentProgressSummaries:true,outputFormat:{type:'json_schema',schema:{type:'string'}}}]){
 const result=new SessionAgentConfigBundle({runtime:{projectRoot:'/fixture',snapshot:{config:{agent:{model:{provider:'fixture',model:'mock'}}}},profile:{runtimeContextSurface:'cli'},model:{getMultimodal:()=>undefined,getCapabilities:()=>({})}},sdkSessionConfig:config,permissionRules:{allow:[],deny:[],ask:[]},interaction:{canPrompt:false},permissionMode:'default',env:{}}).compose();
 rows.push({config,includeToolProgress:result.includeToolProgress??null,stopOnStructuredOutput:result.stopOnStructuredOutput??null,providerOutputFormat:Object.hasOwn(result,'outputFormat')});
}
const model=load('src/model/protocol/toolResultContent.ts');
const tool=load('src/tool/protocol/result.ts');
const store=load('src/gateway/client/GatewayToolResultArtifactStore.ts');
const {mapAgentEvent}=load('src/gateway/client/GatewayAgentEventProjector.ts',{'../../model/index.js':{flattenToolResultBlockText:model.flattenToolResultBlockText},'../../tool/index.js':{contentToText:tool.contentToText},'./GatewayToolResultArtifactStore.js':store});
const events=[];
for(const forward of [false,true])for(const kind of ['text_delta','thinking_delta']){
 const input={type:'subagent_model_event',sessionId:'session',turnId:'turn',subagentId:'child-1',subagentType:'general',event:{type:kind,text:'child fragment'}};
 events.push({forward,kind,events:mapAgentEvent(input,'run',{forwardSubagentText:forward})});
}
const structured=mapAgentEvent({type:'turn_completed',sessionId:'session',turnId:'turn',result:{type:'success',stopReason:'completed',usage:{},structuredOutput:{status:'ok'}}},'run');
const progress=mapAgentEvent({type:'tool_progress',sessionId:'session',turnId:'turn',toolCallId:'call',toolName:'progress_probe',message:'working',createdAt:'2026-11-13T00:00:00.000Z'},'run');
console.log(JSON.stringify({node:process.version,sources,rows,events,structured,progress}));
