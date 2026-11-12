// Fixed TS registry, injected in-memory gateway/reply/search; no server or account.
// Usage: isolated HOME/PILOT_HOME node capture.mjs /absolute/baseline /new/output.json
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';
const [base,out]=process.argv.slice(2);
if (!base || !out || !path.isAbsolute(base) || !path.isAbsolute(out) || fs.existsSync(out)) throw Error('absolute baseline and new output required');
const commit=execFileSync('git',['rev-parse','HEAD'],{cwd:base,encoding:'utf8'}).trim();
if(commit!=='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85') throw Error(`wrong baseline ${commit}`);
const source=path.join(base,'src/adapters/channel/protocol/ChannelCommandRegistry.ts');
const dist=path.join(base,'dist/src/adapters/channel/protocol/ChannelCommandRegistry.js');
const hash=p=>crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
const sourceHash=hash(source),distHash=hash(dist);
const {executeChannelCommand,resolveCommand}=await import(pathToFileURL(dist));
const cases=['hello','/unknown','/new','/plan test','/projects\tfoo','  /项目列表 unused  ','/switch-project','/切换项目 alpha','/switch-project ALP','/switch-project missing','/search','/search dog --all --limit 2','/帮助','/update invalid'];
const rows=[];
for(const input of cases){
 const replies=[], calls=[], projects=[{name:'Alpha',projectKey:'one',fullPath:'/one'},{name:'alpha',projectKey:'two',fullPath:'/two'}];let project='one';
 const ctx={gateway:{listProjects:async()=>{calls.push('projects');return {projects:process.env.H1_COMMAND_NEGATIVE === 'empty-projects' ? [] : projects};}},chatId:'mock-chat',channelKey:'mock',reply:async s=>{replies.push(s);},getProject:()=>project,bindProject:p=>{calls.push(`bind:${p}`);project=p;},resetSession:()=>calls.push('reset'),pilotHome:'/isolated/mock',sessionSearch:{search:async input=>{calls.push({search:input});return {query:input.query,matches:[],truncated:false,sessionsScanned:0};}}};
 const parsed=resolveCommand(input); const handled=await executeChannelCommand(input,ctx);
 rows.push({input,resolved:parsed?{name:parsed.command.name,arg:parsed.arg}:null,handled,replies,calls,project});
}
if(hash(source)!==sourceHash || hash(dist)!==distHash) throw Error('baseline changed during capture');
fs.writeFileSync(out,JSON.stringify({commit,node:process.version,sourceHash,distHash,driverHash:hash(process.argv[1]),cases:rows},null,2)+'\n',{flag:'wx'});
