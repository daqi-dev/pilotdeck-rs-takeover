// Real fixed-budget methods and JSONL IO; runtime/model resolution is the
// provider seam. No budget methods or fs calls are replaced.
const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto');
const root=process.env.PILOTDECK_TS_ROOT||'/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const ts=require(path.join(root,'node_modules/typescript'));
const sandbox=fs.mkdtempSync(path.join(process.env.PILOT_HOME,'task-budget-oracle-'));
const sources=[];
function load(relative,imports={}){
 const source=fs.readFileSync(path.join(root,relative),'utf8');sources.push({path:relative,sha256:crypto.createHash('sha256').update(source).digest('hex')});
 const code=ts.transpileModule(source,{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022}}).outputText;
 const module={exports:{}};
 const req=name=>name.startsWith('node:')?require(name):Object.hasOwn(imports,name)?imports[name]:new Proxy({},{get:(_,key)=>{if(key==='__esModule')return true;throw Error(`unexercised import accessed: ${relative} -> ${name}.${String(key)}`);}});
 new Function('require','module','exports',code)(req,module,module.exports);return module.exports;
}
const errors=load('src/gateway/dialog/errors.ts');
const {ProjectRuntimeRegistry}=load('src/cli/ProjectRuntimeRegistry.ts',{'../gateway/dialog/errors.js':errors});
let clock=1000;
function registry(name){
 const home=path.join(sandbox,name);fs.mkdirSync(home,{recursive:true});
 const value=Object.create(ProjectRuntimeRegistry.prototype);
 Object.assign(value,{options:{pilotHome:home,fallbackProjectRoot:'/fixture/a',now:()=>new Date(clock)},sdkSessionConfigs:new Map(),sdkSessionProjects:new Map(),sdkTaskBudgetLedgers:new Map(),taskBudgetLedgerLoad:undefined,taskBudgetLedgerRecordCount:0,taskBudgetLedgerCompactAfterRecords:512,resolve:key=>({projectRoot:path.resolve(key??'/fixture/a')})});
 return value;
}
async function publish(r,session,budget,project='/fixture/a',signal){
 await r.configureProjectTaskBudget(project,session,budget,signal);
 r.sdkSessionConfigs.set(session,{taskBudget:budget});r.sdkSessionProjects.set(session,project);
}
function captureError(error){return {name:error.name,code:error.code??null,message:String(error.message).replaceAll(sandbox,'<ROOT>')};}
const snapshot=(r,session,project)=>r.taskBudgetSnapshotForSdk({sessionKey:session,projectKey:project});
const settle=(r,session,run,spend,project)=>r.recordTaskBudgetSpendForSdk({sessionKey:session,runId:run,turnSpentUsd:spend,projectKey:project});
(async()=>{
 const session=registry('session');await publish(session,'a',{total:10});await publish(session,'b',{total:10});await Promise.all(Array.from({length:8},()=>settle(session,'a','same-run',0.5)));
 const sessions={a:await snapshot(session,'a'),b:await snapshot(session,'b'),settledRows:fs.readFileSync(session.taskBudgetLedgerPath(),'utf8').trim().split('\n').map(JSON.parse).filter(r=>r.kind==='settled').length};
 const project=registry('project');await publish(project,'a',{total:10,scope:'project',projectRetentionMs:100});await publish(project,'b',{total:10,scope:'project',projectRetentionMs:100});await settle(project,'a','project-run',0.75);
 const projects={a:await snapshot(project,'a'),b:await snapshot(project,'b')};await publish(project,'other',{total:20,scope:'project'},'/fixture/b');projects.other=await snapshot(project,'other','/fixture/b');
 clock=1050;const reboot=registry('project');await publish(reboot,'after-restart',{total:10,scope:'project',projectRetentionMs:100});const restart={loaded:await snapshot(reboot,'after-restart')};clock=1200;restart.beforeReconfigure=await snapshot(reboot,'after-restart');await publish(reboot,'expired',{total:20,scope:'project',projectRetentionMs:100});restart.expired=await snapshot(reboot,'expired');
 const failed=registry('failed');await publish(failed,'a',{total:10,scope:'project'});fs.unlinkSync(failed.taskBudgetLedgerPath());fs.mkdirSync(failed.taskBudgetLedgerPath());let firstError;try{await settle(failed,'a','failed-run',0.5);}catch(error){firstError=captureError(error);}let repeated='resolved';try{await settle(failed,'a','failed-run',0.5);}catch(error){repeated=captureError(error);}const failedWrite={firstError,repeated,snapshot:await snapshot(failed,'a')};
 const cancelled=registry('cancelled'),abort=new AbortController();abort.abort();let cancellation;try{await publish(cancelled,'a',{total:10,scope:'project'},'/fixture/a',abort.signal);}catch(error){cancellation=captureError(error);}const cancel={error:cancellation,ledgerExists:fs.existsSync(cancelled.taskBudgetLedgerPath()),configPublished:cancelled.sdkSessionConfigs.has('a')};
 console.log(JSON.stringify({sources,sessions,projects,restart,failedWrite,cancel}));
})().catch(error=>{console.error(error);process.exitCode=1;}).finally(()=>fs.rmSync(sandbox,{recursive:true,force:true}));
