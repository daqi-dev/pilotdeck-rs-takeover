#!/usr/bin/env node
// Private orchestration only. Product execution remains the unmodified BBX run.mjs.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {spawnSync,spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {fileURLToPath} from 'node:url';
import {startMock} from './mock.mjs';
const here=path.dirname(fileURLToPath(import.meta.url));
const repo=path.resolve(here,'../../../..');
const [side,output,...args]=process.argv.slice(2);
if(!['ts','rust'].includes(side)||!output||args.some(arg=>!['--wrong-nonce'].includes(arg))){console.error('usage: capture.mjs ts|rust OUTPUT.json [--wrong-nonce]');process.exit(2)}
// No override: this fixture is executable only in a Linux network-none namespace.
if(process.platform!=='linux'||Object.values(os.networkInterfaces()).flat().some(i=>i&&!i.internal))throw Error('requires Linux Docker --network none; refuses any nonloopback interface');
const variant=args.includes('--wrong-nonce')?'wrong-nonce':'normal';
const slot=Number(process.env.BBX_SLOT??0);if(!Number.isInteger(slot)||slot<0||slot>8)throw Error('invalid BBX_SLOT');
const base=path.resolve(process.env.BBX_TMP_ROOT||os.tmpdir());const parent=path.join(base,slot?`pdrs-bb${slot}`:'pdrs-bbx');const root=path.join(parent,'root');
fs.mkdirSync(parent,{recursive:true});fs.mkdirSync(path.dirname(path.resolve(output)),{recursive:true});
const lock=path.join(parent,'feishu-stop-wrapper.lock');const lockFd=fs.openSync(lock,'wx',0o600);fs.writeFileSync(lockFd,String(process.pid));
const caDir=path.join(parent,'feishu-stop-ca');
const outputPath=path.resolve(output);const rawPath=outputPath+'.mock-raw.jsonl';const logPath=outputPath+'.run.log';
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const sources=Object.fromEntries(['capture.mjs','mock.mjs','wire.mjs','verify.mjs','../feishu-stream/mock.mjs'].map(name=>[name,sha(fs.readFileSync(path.join(here,name)))]));
let mock;let child;
try{
 fs.mkdirSync(caDir,{recursive:true,mode:0o700});fs.writeFileSync(rawPath,'',{mode:0o600});
 const isolatedEnv={PATH:process.env.PATH,HOME:caDir,PILOT_HOME:path.join(caDir,'pilot'),TMPDIR:caDir};
 const openssl=argv=>{const r=spawnSync('openssl',argv,{cwd:caDir,env:isolatedEnv,encoding:'utf8'});if(r.status!==0)throw Error(`openssl failed: ${r.stderr??r.error}`)};
 openssl(['req','-x509','-newkey','rsa:2048','-nodes','-keyout','ca.key','-out','ca.pem','-days','1','-subj','/CN=Feishu BBX isolated test CA','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign']);
 openssl(['req','-new','-newkey','rsa:2048','-nodes','-keyout','server.key','-out','server.csr','-subj','/CN=open.feishu.cn']);
 fs.writeFileSync(path.join(caDir,'server.ext'),'subjectAltName=DNS:open.feishu.cn\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n');
 openssl(['x509','-req','-in','server.csr','-CA','ca.pem','-CAkey','ca.key','-CAcreateserial','-out','server.pem','-days','1','-extfile','server.ext']);
 mock=await startMock({root,caDir,proxyPort:38413+slot*10,wsPort:38414+slot*10,tlsPort:38415+slot*10,rawPath,variant,onFinalReply:()=>{
  const configPath=path.join(root,'home/.pilotdeck/pilotdeck.yaml');
  const before=fs.readFileSync(configPath,'utf8');const enabled='  feishu:\n    enabled: true\n';
  if(before.split(enabled).length!==2)throw Error('expected exactly one enabled Feishu stanza');
  const after=before.replace(enabled,'  feishu:\n    enabled: false\n');fs.writeFileSync(configPath,after);
  return {operation:'disable-feishu-in-isolated-config',beforeSha256:sha(before),afterSha256:sha(after),enabled:false};
 }});
 let scenarioPath=path.join(repo,'scripts/bbx/scenarios/backend-feishu-stream-stop.json');
 const log=fs.openSync(logPath,'w',0o600);
 child=spawn(process.execPath,[path.join(repo,'scripts/bbx/run.mjs'),side,scenarioPath,outputPath],{cwd:repo,env:process.env,stdio:['ignore',log,log]});
 const onSignal=()=>child.kill('SIGTERM');process.once('SIGTERM',onSignal);process.once('SIGINT',onSignal);
 const result=await new Promise(resolve=>{child.once('error',error=>resolve({error:error.message}));child.once('close',(code,signal)=>resolve({code,signal}))});
 process.removeListener('SIGTERM',onSignal);process.removeListener('SIGINT',onSignal);fs.closeSync(log);
 const mockDrain=await mock.close();mock=undefined;
 const receipt={schema:1,side,variant,network:'linux-loopback-only',mockDrain,sources,scenario:{path:scenarioPath,sha256:sha(fs.readFileSync(scenarioPath))},artifact:fs.existsSync(outputPath)?{sha256:sha(fs.readFileSync(outputPath))}:null,raw:{sha256:sha(fs.readFileSync(rawPath))},process:result};
 fs.writeFileSync(outputPath+'.receipt.json',JSON.stringify(receipt,null,2)+'\n');
 process.exitCode=result.code??1;
}finally{
 if(mock)await mock.close();fs.rmSync(caDir,{recursive:true,force:true});fs.closeSync(lockFd);fs.unlinkSync(lock);
}
