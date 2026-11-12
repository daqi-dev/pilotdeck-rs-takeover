#!/usr/bin/env node
// Actual frozen TS adapter, isolated synchronous spawn-failure inputs.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL,fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import childProcess, {spawnSync} from 'node:child_process';
import {syncBuiltinESMExports} from 'node:module';
const baseline=process.env.PDRS_BASELINE??'/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const out=path.resolve(process.argv[2]);fs.mkdirSync(out);
assert.equal(process.version,'v22.22.0');
assert.equal(spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
assert.equal(spawnSync('git',['-C',baseline,'diff','--quiet','HEAD','--','src','packages']).status,0);
const sha=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const source=path.join(baseline,'src/adapters/channel/whatsapp/WhatsAppChannel.ts');
const built=path.join(baseline,'dist/src/adapters/channel/whatsapp/WhatsAppChannel.js');
const {default:ts}=await import(pathToFileURL(path.join(baseline,'node_modules/typescript/lib/typescript.js')));
const cfg=JSON.parse(fs.readFileSync(path.join(baseline,'tsconfig.json'),'utf8'));
const compiler=ts.convertCompilerOptionsFromJson(cfg.compilerOptions,baseline).options;
assert.equal(ts.transpileModule(fs.readFileSync(source,'utf8'),{fileName:source,compilerOptions:{...compiler,module:ts.ModuleKind.ESNext}}).outputText,fs.readFileSync(built,'utf8'));
for(const key of Object.keys(process.env))delete process.env[key];
Object.assign(process.env,{HOME:path.join(out,'home'),PILOT_HOME:path.join(out,'pilot'),PATH:''});fs.mkdirSync(process.env.HOME);fs.mkdirSync(process.env.PILOT_HOME);
const {WhatsAppChannel}=await import(pathToFileURL(built));
const records=[];
for(const bridgePath of ['bad\u0000bridge.mjs','prefix\u0000suffix','plain\nline\u0000.mjs']){
 const logs=[];const adapter=new WhatsAppChannel({bridgePath,bridgeUrl:'http://127.0.0.1:18081'});
 let readinessCalls=0;adapter.waitForBridgeReady=async()=>{readinessCalls++;throw Error('readiness must not run after synchronous spawn failure');};
 const handle=await adapter.start({logger:{error:message=>logs.push(['error',message]),warn:message=>logs.push(['warn',message]),info:message=>logs.push(['info',message])}});
 assert.equal(readinessCalls,0);assert.equal(adapter.child,null);assert.equal(adapter.running,false);assert.equal(typeof handle.stop,'function');await handle.stop('noop');
 assert.equal(logs.length,1);assert.match(logs[0][1],/^whatsapp: start failed: TypeError/);
 records.push({bridgePath,logs,readinessCalls,child:adapter.child,running:adapter.running});
}
// The frozen adapter always selects process.execPath. The seam changes only
// that executable to a nonexistent path; the real Node spawn emits its actual
// asynchronous error. Readiness is separately injected to demonstrate order.
const originalSpawn=childProcess.spawn;
const missingExecutable=path.join(out,'missing-node');
let selectedExecutable;
childProcess.spawn=(executable,args,options)=>{selectedExecutable=executable;return originalSpawn(missingExecutable,args,options);};
syncBuiltinESMExports();
const asyncLogs=[];let asyncReadinessCalls=0;let asyncRecord;
try {
 const adapter=new WhatsAppChannel({bridgePath:'fixture.mjs',bridgeUrl:'http://127.0.0.1:18081'});
 adapter.waitForBridgeReady=async()=>{asyncReadinessCalls++;await new Promise(resolve=>setImmediate(resolve));return true;};
 const handle=await adapter.start({logger:{error:message=>asyncLogs.push(['error',message]),info:message=>asyncLogs.push(['info',message])}});
 assert.equal(selectedExecutable,process.execPath);assert.equal(asyncReadinessCalls,1);assert.equal(adapter.running,true);
 assert.equal(asyncLogs[0][1],`whatsapp: bridge spawn error: Error: spawn ${missingExecutable} ENOENT`);
 // Failed child has no OS PID and there is no actual bridge to stop. Avoid
 // the baseline's five-second cleanup timer without concealing any process.
 assert.equal(adapter.child.pid,undefined);adapter.child=null;await handle.stop('fixture');
 asyncRecord={mode:'actual OS spawn failure with injected executable and successful readiness',logs:asyncLogs.map(([level,message])=>[level,message.replaceAll(missingExecutable,'<missing-executable>')]),readinessCalls:asyncReadinessCalls,runningBeforeStop:true};
} finally {childProcess.spawn=originalSpawn;syncBuiltinESMExports();}
fs.writeFileSync(path.join(out,'oracle.json'),JSON.stringify({records,asyncRecord},null,2)+'\n');
fs.writeFileSync(path.join(out,'identity.json'),JSON.stringify({node:process.version,revision:'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85',driverSha256:sha(fileURLToPath(import.meta.url)),sourceSha256:sha(source),distSha256:sha(built),boundary:'actual TS start; embedded NUL makes native spawn throw before any child/HTTP'},null,2)+'\n');
console.log(JSON.stringify({cases:records.length,oracleSha256:sha(path.join(out,'oracle.json'))}));
