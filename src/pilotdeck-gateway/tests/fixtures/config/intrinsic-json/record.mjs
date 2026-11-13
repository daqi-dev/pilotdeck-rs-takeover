import {spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,rmSync,writeFileSync} from 'node:fs';
import os from 'node:os';import path from 'node:path';import {fileURLToPath} from 'node:url';
const here=path.dirname(fileURLToPath(import.meta.url));const scratch=mkdtempSync(path.join(os.tmpdir(),'pdrs-config-intrinsic-'));
const keys=Object.getOwnPropertyNames(Object.prototype);const cases=[];
try{for(const kind of ['provider','model','always','project'])for(const key of keys){const home=path.join(scratch,String(cases.length));mkdirSync(home);const p=spawnSync(process.execPath,[path.join(here,'probe.mjs'),kind,key],{encoding:'utf8',env:{...process.env,HOME:home,PILOT_HOME:home}});if(p.status!==0)throw Error(`${kind}/${key}: ${p.stderr}`);cases.push({kind,key,result:JSON.parse(p.stdout)});}}
finally{rmSync(scratch,{recursive:true,force:true});}
writeFileSync(process.argv[2],JSON.stringify({baseline:'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85',node:process.version,keys,cases},null,2)+'\n');
