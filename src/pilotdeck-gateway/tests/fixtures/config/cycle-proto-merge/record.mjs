import {mkdirSync,writeFileSync,readFileSync} from 'node:fs';
import {resolve,join} from 'node:path';
import {pathToFileURL} from 'node:url';
import {createHash} from 'node:crypto';
import assert from 'node:assert/strict';
import {assertBaseline,baselineSourceHashes} from '../../_shared/baseline.mts';
const baseline=resolve(process.env.PDRS_BASELINE),root=resolve(process.argv[2]);
const commit=assertBaseline(baseline);
const {loadPilotConfig}=await import(pathToFileURL(join(baseline,'src/pilot/config/loadPilotConfig.ts')));
const {getPilotProjectConfigFilePath}=await import(pathToFileURL(join(baseline,'src/pilot/paths.ts')));
const base='schemaVersion: 1\nagent: {model: parity/deterministic}\nmodel:\n  providers:\n    parity:\n      protocol: openai\n      url: http://127.0.0.1:28794/v1\n      apiKey: mock-only\n      models: {deterministic: {}}\nunused: &a {self: *a}\n';
const cases=[];
for(const [id,projectYaml] of [['unrelated-cycle-and-proto','unusedProto: {__proto__: null}\n'],['actual-recursive-merge','unused: &b {self: *b}\nunusedProto: {__proto__: null}\n'],['cycle-overlaps-proto-path','unused: {__proto__: &b {self: *b}}\n']]){
 const dir=join(root,id),pilot=join(dir,'pilot'),project=join(dir,'project');mkdirSync(pilot,{recursive:true});mkdirSync(project,{recursive:true});const projectPath=getPilotProjectConfigFilePath(project);mkdirSync(resolve(projectPath,'..'),{recursive:true});writeFileSync(join(pilot,'pilotdeck.yaml'),base);writeFileSync(projectPath,projectYaml);
 let result;try{const s=loadPilotConfig({env:{HOME:join(root,'home'),PILOT_HOME:pilot},projectRoot:project});result={ok:true,diagnostics:s.diagnostics,agent:s.config.agent,sourceKinds:s.sources.map(x=>x.kind)};}catch(e){result={ok:false,name:e.name,code:e.code??null,message:e.message};}
 if(id!=='actual-recursive-merge')assert.equal(result.ok,true);else{assert.equal(result.ok,false);assert.equal(result.name,'RangeError');assert.equal(result.message,'Maximum call stack size exceeded');}
 cases.push({id,globalYaml:base,projectYaml,yamlDataOnly:true,containsJavaScriptFunctions:false,containsYamlAliasCycle:true,result});
}
const sources=await baselineSourceHashes(baseline,['src/pilot/config/loadPilotConfig.ts','src/pilot/config/merge.ts','src/pilot/paths.ts']);
const h=p=>createHash('sha256').update(readFileSync(p)).digest('hex');const out={sourceCommit:commit,node:process.version,nodeSHA256:h(process.execPath),recordSHA256:h(new URL(import.meta.url)),sourceSha256:sources,cases};const text=JSON.stringify(out,null,2).replaceAll(root,'<capture>')+'\n';writeFileSync(join(root,'oracle.json'),text);console.log(JSON.stringify(cases.map(x=>({id:x.id,result:x.result}))));
