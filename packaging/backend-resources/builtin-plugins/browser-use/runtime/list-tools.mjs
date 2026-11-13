// Static MCP handshake/catalog probe only. It never calls a browser tool.
import fs from 'node:fs';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
const [runtime,output]=process.argv.slice(2);
if(!runtime||!output||!path.isAbsolute(runtime)||!path.isAbsolute(output)||fs.existsSync(output))throw Error('usage: node list-tools.mjs <absolute independent runtime> <NEW absolute output>');
fs.mkdirSync(output,{recursive:true});const home=path.join(output,'home'),pilot=path.join(output,'pilot'),profile=path.join(output,'profile');for(const dir of [home,pilot,profile])fs.mkdirSync(dir);
const entry=path.join(runtime,'node_modules/@playwright/mcp/cli.js');
const args=[entry,'--browser','chrome','--headless','--user-data-dir',profile];
const child=spawn(process.execPath,args,{env:{HOME:home,PILOT_HOME:pilot,PATH:process.env.PATH??'',PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD:'1'},cwd:output,stdio:['pipe','pipe','pipe']});
let stdout='',stderr='',pending='',catalog,initialized=false,closeResult;
const frames=[],timers=[];let finish;
const done=new Promise((resolve,reject)=>{finish={resolve,reject};});
const send=frame=>{frames.push({direction:'out',frame});child.stdin.write(JSON.stringify(frame)+'\n');};
child.once('error',error=>finish.reject(error));
child.stderr.on('data',bytes=>{stderr+=String(bytes);});
child.stdout.on('data',bytes=>{const text=String(bytes);stdout+=text;pending+=text;for(;;){const end=pending.indexOf('\n');if(end<0)break;const line=pending.slice(0,end);pending=pending.slice(end+1);if(!line.trim())continue;let frame;try{frame=JSON.parse(line);}catch{finish.reject(Error('MCP stdout contains a non-JSON protocol line'));return;}frames.push({direction:'in',frame});if(frame.error){finish.reject(Error(JSON.stringify(frame.error)));return;}if(frame.id===1){initialized=true;send({jsonrpc:'2.0',method:'notifications/initialized'});send({jsonrpc:'2.0',id:2,method:'tools/list',params:{}});}else if(frame.id===2){catalog=frame.result;child.stdin.end();}}});
child.once('close',(code,signal)=>{closeResult={code,signal};if(catalog&&code===0&&!signal)finish.resolve();else finish.reject(Error(`MCP probe exited code=${code} signal=${signal}`));});
const timeout=setTimeout(()=>{child.kill('SIGTERM');finish.reject(Error('MCP catalog probe timed out'));},15000);timers.push(timeout);
send({jsonrpc:'2.0',id:1,method:'initialize',params:{protocolVersion:'2024-11-05',capabilities:{},clientInfo:{name:'pilotdeck-offline-source-catalog-probe',version:'1.0.0'}}});
let error;
try{await done;}catch(failure){error=failure;child.stdin.end();if(child.exitCode===null&&child.signalCode===null)child.kill('SIGTERM');await new Promise(resolve=>child.exitCode!==null||child.signalCode!==null?resolve():child.once('close',resolve));}
finally{timers.forEach(clearTimeout);fs.writeFileSync(path.join(output,'stdout.jsonl'),stdout);fs.writeFileSync(path.join(output,'stderr.txt'),stderr);fs.writeFileSync(path.join(output,'frames.json'),JSON.stringify(frames,null,2)+'\n');if(catalog)fs.writeFileSync(path.join(output,'tools-list.json'),JSON.stringify(catalog,null,2)+'\n');}
const manifest={scope:'official MCP initialize/tools/list only; no browser tool called',entry,entrySha256:createHash('sha256').update(fs.readFileSync(entry)).digest('hex'),node:process.version,args,home,pilot,profile,pid:child.pid,initialized,...closeResult,toolNames:catalog?.tools?.map(tool=>tool.name)??[],error:error?.message??null};fs.writeFileSync(path.join(output,'manifest.json'),JSON.stringify(manifest,null,2)+'\n');
if(error)throw error;
const required=['browser_navigate','browser_snapshot','browser_click','browser_type','browser_close'];for(const name of required)if(!manifest.toolNames.includes(name))throw Error(`official required tool missing: ${name}`);
console.log(JSON.stringify({status:'PASS',output,count:manifest.toolNames.length,required,scope:manifest.scope},null,2));
