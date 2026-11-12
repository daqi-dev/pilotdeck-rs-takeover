// Invoke the actual fixed-baseline private resolver without changing reference files.
import {readFileSync,writeFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
import vm from 'node:vm';
const [baseline,output]=process.argv.slice(2);
const source=readFileSync(`${baseline}/dist/src/cli/ProjectRuntimeRegistry.js`,'utf8');
function extract(name){const start=source.indexOf(`function ${name}(`);if(start<0)throw new Error(name);const tail=source.slice(start+1);const end=tail.search(/\n(?:async )?function /);return source.slice(start,end<0?undefined:start+1+end);}
const {DialogGatewayError}=await import(pathToFileURL(`${baseline}/dist/src/gateway/dialog/errors.js`));
const resolve=vm.runInNewContext(`${extract('resolveSdkSessionSkills')}\n${extract('resolveSdkSubagentScopes')}\nresolveSdkSubagentScopes`,{DialogGatewayError});
const def=(skills)=>({description:'worker',prompt:'do work',...(skills===undefined?{}:{skills})});
const cases=[
 {id:'inherit-and-all',parent:['pkg:alpha'],available:['pkg:alpha','pkg:beta'],agents:{a:def(),b:def('all'),c:def(['alpha','alpha'])}},
 {id:'empty-parent',parent:[],available:['pkg:alpha'],agents:{a:def(),b:def('all')}},
 {id:'all-without-parent',parent:'all',available:['pkg:alpha'],agents:{a:def('all'),b:def(['alpha'])}},
 {id:'escape',parent:['pkg:alpha'],available:['pkg:alpha','pkg:beta'],agents:{a:def(['beta'])}},
 {id:'missing',parent:['pkg:alpha'],available:['pkg:alpha'],agents:{a:def(['missing'])}},
 {id:'ambiguous',parent:'all',available:['one:alpha','two:alpha'],agents:{a:def(['alpha'])}}
];
const results=cases.map(input=>{const agents=structuredClone(input.agents);try{resolve(agents,input.parent,input.available.map(name=>({name})),undefined,{},undefined);return{input,agents};}catch(error){return{input,error:{code:error.code,message:error.message}};}});
if(results.length!==6 || results.slice(0,3).some(r=>!r.agents) || results.slice(3).map(r=>r.error?.code).join(',')!=='SDK_AGENT_SKILL_OUT_OF_SCOPE,SDK_SKILL_NOT_FOUND,SDK_SKILL_AMBIGUOUS')throw new Error('SDK scope oracle did not execute all required assertions');
writeFileSync(output,JSON.stringify(results,null,2)+'\n');
