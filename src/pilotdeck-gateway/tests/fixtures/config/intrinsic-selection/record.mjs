// Run with isolated HOME/PILOT_HOME and PDRS_BASELINE pointing at fixed cc7c4e6.
import {writeFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
const root=process.env.PDRS_BASELINE,home=process.env.PILOT_HOME;
if(!root||!home)throw Error('PDRS_BASELINE and isolated PILOT_HOME required');
const {loadPilotConfig}=await import(pathToFileURL(root+'/dist/src/pilot/config/loadPilotConfig.js'));
const sites={primary:(c,v)=>c.agent.model=v,memory:(c,v)=>c.memory={enabled:false,model:v},subagent:(c,v)=>c.agent.subagents={default:v},'router-default':(c,v)=>c.router={scenarios:{default:v}},'router-fallback':(c,v)=>c.router={fallback:{default:[v]}},'router-judge':(c,v)=>c.router={tokenSaver:{judge:v,tiers:{basic:{model:'parity/plain'}}}},'router-tier':(c,v)=>c.router={tokenSaver:{judge:'parity/plain',tiers:{basic:{model:v}}}},'router-baseline':(c,v)=>c.router={stats:{baselineModel:v}},'router-baseline-object':(c,v)=>c.router={stats:{baselineModel:{provider:'parity',model:v.split('/')[1]}}},'router-pricing':(c,v)=>c.router={stats:{modelPricing:{[v]:{input:1}}}}};
const cases=[];
for(const [site,set] of Object.entries(sites))for(const key of ['constructor','toString','toLocaleString','valueOf','hasOwnProperty','isPrototypeOf','propertyIsEnumerable','__defineGetter__','__defineSetter__','__lookupGetter__','__lookupSetter__','plain','missing'])for(const own of [true,false]){
 const models=JSON.parse('{"plain":{}}');if(own&&key!=='missing')models[key]={};
 const config={schemaVersion:1,agent:{model:'parity/plain'},model:{providers:{parity:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models}}}};
 set(config,`parity/${key}`);const path=home+'/config.json';writeFileSync(path,JSON.stringify(config));
 let result;try{const r=loadPilotConfig({configPath:path,env:{HOME:home,PILOT_HOME:home}});result={ok:true,diagnostics:r.diagnostics,model:r.config.model,agent:r.config.agent,memory:r.config.memory,router:r.config.router};}catch(e){result={ok:false,name:e.name,message:e.message,code:e.code};}
 cases.push({site,key,own,config,result});
}
for(const [site,set] of Object.entries(sites)){
 const config={schemaVersion:1,agent:{model:'parity/plain'},model:{providers:{parity:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models:{plain:{}}}}}};
 set(config,'parity/__proto__');const path=home+'/config.json';writeFileSync(path,JSON.stringify(config));
 let result;try{const r=loadPilotConfig({configPath:path,env:{HOME:home,PILOT_HOME:home}});result={ok:true,diagnostics:r.diagnostics,model:r.config.model,agent:r.config.agent,memory:r.config.memory,router:r.config.router};}catch(e){result={ok:false,name:e.name,message:e.message,code:e.code};}
 cases.push({site,key:'__proto__',own:false,config,result});
}
for(const [site,set] of Object.entries(sites))for(const [valueKind,value] of [['null',null],['false',false],['true',true],['array',[]],['string','x'],['number',17]]){
 const models={plain:{},constructor:value};
 const config={schemaVersion:1,agent:{model:'parity/plain'},model:{providers:{parity:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models}}}};
 set(config,'parity/constructor');const path=home+'/config.json';writeFileSync(path,JSON.stringify(config));
 let result;try{const r=loadPilotConfig({configPath:path,env:{HOME:home,PILOT_HOME:home}});result={ok:true,diagnostics:r.diagnostics,model:r.config.model,agent:r.config.agent,memory:r.config.memory,router:r.config.router};}catch(e){result={ok:false,name:e.name,message:e.message,code:e.code};}
 cases.push({site,key:'constructor',own:true,valueKind,config,result});
}
for(const [site,set] of Object.entries(sites))for(const [kind,value,own] of [['proto-null',null,false],['proto-object',{},false],['proto-object-null-constructor',{constructor:null},false],['proto-null',null,true],['proto-object',{},true],['proto-object-null-constructor',{constructor:null},true],['provider-null',null,false],['provider-false',false,false]]){
 const models={plain:{}};if(kind.startsWith('proto-'))Object.defineProperty(models,'__proto__',{value,enumerable:true,configurable:true,writable:true});if(own)models.constructor={};
 const provider=kind.startsWith('provider-')?value:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models};
 const config={schemaVersion:1,agent:{model:'parity/plain'},model:{providers:{parity:provider}}};set(config,'parity/constructor');const path=home+'/config.json';writeFileSync(path,JSON.stringify(config));
 let result;try{const r=loadPilotConfig({configPath:path,env:{HOME:home,PILOT_HOME:home}});result={ok:true,diagnostics:r.diagnostics,model:r.config.model,agent:r.config.agent,memory:r.config.memory,router:r.config.router};}catch(e){result={ok:false,name:e.name,message:e.message,code:e.code};}
 cases.push({site,key:'constructor',own,valueKind:kind,config,result});
}
for(const [site,set] of Object.entries(sites))for(const providerName of ['constructor','toString','__proto__'])for(const mode of ['inherited','own','prior-invalid']){
 const providers={parity:mode==='prior-invalid'?null:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models:{plain:{}}}};
 if(mode==='own')Object.defineProperty(providers,providerName,{value:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models:{zfirst:{},plain:{}}},enumerable:true,configurable:true,writable:true});
 const config={schemaVersion:1,agent:{model:'parity/plain'},model:{providers}};set(config,`${providerName}/plain`);const path=home+'/config.json';writeFileSync(path,JSON.stringify(config));
 let result;try{const r=loadPilotConfig({configPath:path,env:{HOME:home,PILOT_HOME:home}});result={ok:true,diagnostics:r.diagnostics,model:r.config.model,agent:r.config.agent,memory:r.config.memory,router:r.config.router};}catch(e){result={ok:false,name:e.name,message:e.message,code:e.code};}
 cases.push({site,key:providerName,own:mode==='own',valueKind:`provider-${mode}`,config,result});
}
for(const site of ['memory','subagent','router-default']){
 const config={schemaVersion:1,agent:{model:'parity/plain'},model:{providers:{parity:{protocol:'openai',url:'http://127.0.0.1:28794/v1',apiKey:'mock-only',models:{plain:{}}}}}};
 sites[site](config,' constructor/plain');const path=home+'/config.json';writeFileSync(path,JSON.stringify(config));
 let result;try{const r=loadPilotConfig({configPath:path,env:{HOME:home,PILOT_HOME:home}});result={ok:true,diagnostics:r.diagnostics,model:r.config.model,agent:r.config.agent,memory:r.config.memory,router:r.config.router};}catch(e){result={ok:false,name:e.name,message:e.message,code:e.code};}
 cases.push({site,key:'whitespace-constructor',own:false,valueKind:'provider-whitespace',config,result});
}
writeFileSync(process.argv[2],JSON.stringify({baseline:'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85',node:process.version,cases},null,2).replaceAll(home,'<PILOT_HOME>')+'\n');
