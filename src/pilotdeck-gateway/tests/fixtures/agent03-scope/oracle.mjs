import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';
const root=process.env.PILOTDECK_TS_BASELINE;assert(root);
const {createNativeOneShotSubagentPort}=await import(pathToFileURL(`${root}/dist/src/agent/sub/OneShotSubagentPort.js`));
const {buildForkedMessages}=await import(pathToFileURL(`${root}/dist/src/agent/sub/buildForkedMessages.js`));
const results=[];
for(const failure of [false,true]){
 const calls=[],events=[],launches=[];let sequence=0;
 const worker={id:'worker',description:'worker',systemPromptSuffix:'work',allowedTools:['*'],omitProjectInstructions:false,omitGitStatus:false,isReadOnly:false,initialPrompt:' prefix ',memory:'inherit',mcpServers:{private:{type:'stdio',command:'fixture'}},observer:'review',observerMessage:' postamble '};
 const review={id:'review',description:'review',systemPromptSuffix:'review',allowedTools:['*'],omitProjectInstructions:false,omitGitStatus:false,isReadOnly:false,initialPrompt:'do not inherit',memory:'inherit',skills:['hidden'],mcpServers:{other:{type:'stdio',command:'fixture'}},background:true,observer:'worker'};
 const dependencies={uuid:()=>`fixed-${++sequence}`,now:()=>new Date(1000),eventEmitter:e=>events.push(e),subagentProvider:{name:'fixture',async run(request){calls.push({definition:request.definition,directive:request.directive});if(request.definition.id==='worker'){request.onActivity?.({type:'model_request_started',provider:'mock',model:'fixture'});request.onActivity?.({type:'assistant_message',message:{role:'assistant',content:[]}});if(failure)throw new Error('fixture failure');}return{markdown:request.definition.id==='worker'?'final':'review report',parsed:{},usage:{inputTokens:0,outputTokens:0},turns:1,durationMs:0};}},observerSubagents:{async launch(input){launches.push({sessionId:input.sessionId,turnId:input.turnId,observedSubagentId:input.observedSubagentId,observerSubagentId:input.observerSubagentId,observerSubagentType:input.observerSubagentType});await input.run(new AbortController().signal);}}};
 const api=createNativeOneShotSubagentPort({config:{cwd:'/fixture/project',permissionMode:'default',subagentDefinitions:{worker,review}},dependencies}).createForkApi({sessionId:'parent',turnId:'turn'});
 let outcome;try{const result=await api.fork({definitionId:'worker',directive:' task ',subagentId:'child'});outcome={success:true,markdown:result.markdown};}catch(error){outcome={success:false,error:error.message};}
 if(process.env.PDRS_ORACLE_FAULT)calls[1].definition={...calls[1].definition,allowedTools:["*"]};
 assert.equal(calls.length,2);const override=calls[1].definition;assert.deepEqual(override.allowedTools,[]);assert.deepEqual(override.disallowedTools,[]);assert.equal(override.isReadOnly,true);assert.equal(override.permissionMode,'plan');assert.equal(override.memory,'disabled');for(const key of ['mcpServers','skills','initialPrompt','background','observer','observerMessage'])assert.equal(override[key],undefined,key);
 assert(calls[1].directive.includes(`Outcome: ${failure?'failed':'completed'}`));assert(calls[1].directive.endsWith('postamble'));assert.equal(events.filter(e=>e.type==='observer_report').length,1);
 results.push({failure,outcome,calls,launches,events});
}
const messages=buildForkedMessages('directive',' \ud800 ');assert.equal(messages.length,2);assert.equal(messages[0].content[0].text,'\ud800');assert.equal(buildForkedMessages('directive','  ').length,1);
console.log(JSON.stringify({messages,results},null,2));
