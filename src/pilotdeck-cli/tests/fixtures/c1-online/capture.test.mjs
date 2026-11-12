import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { startMock, requireIsolation, TOKEN } from './mock.mjs';
import { drive, verifyCase, config, snapshot } from './capture.mjs';
const here=path.dirname(fileURLToPath(import.meta.url));
const cases=JSON.parse(fs.readFileSync(path.join(here,'cases.json')));
async function capture(spec,mode='normal'){
 const root=fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(),'c1-online-driver-'))),home=path.join(root,'home'),project=path.join(root,'project');fs.mkdirSync(home);fs.mkdirSync(project);
 const mock=await startMock(spec,{port:0});
 const env={PATH:process.env.PATH,HOME:home,PILOT_HOME:path.join(home,'pilot'),NODE_NO_WARNINGS:'1',C1_SELFTEST:JSON.stringify({url:`http://127.0.0.1:${mock.port}`,token:TOKEN,mode,argv:spec.argv})};
 try{const result=await drive(process.execPath,[path.join(here,'fixture-client.mjs')],env,project,spec,mock,{startupMs:3000,observeMs:150});return{result,state:mock.state,project};}
 finally{await mock.stop();fs.rmSync(root,{recursive:true,force:true});}
}
test('fixed-port guard requires Docker and explicit private-network assertion',()=>{
 assert.throws(()=>requireIsolation({},()=>true));
 assert.throws(()=>requireIsolation({PILOTDECK_C1_ONLINE_DOCKER:'private-no-published-ports'},()=>false));
 requireIsolation({PILOTDECK_C1_ONLINE_DOCKER:'private-no-published-ports'},()=>true);
});
for(const spec of cases)test(`driver captures ${spec.id}`,async()=>{
 const {result,state,project}=await capture(spec);const verdict=verifyCase(spec,'rust',result,state,project);assert.equal(verdict.productPassed,true);
});
test('retained socket is captured as baseline hang and rejected for Rust',async()=>{
 const spec=cases.find(s=>s.id==='argv');const {result,state,project}=await capture(spec,'retain');
 assert.equal(verifyCase(spec,'ts',result,state,project).lifecycle,'observed-hang');
 assert.equal(verifyCase(spec,'ts',result,state,project).productPassed,false);
 assert.throws(()=>verifyCase(spec,'rust',result,state,project),/Rust must exit naturally/);
});
test('unexpected exit and final payload mutation are rejected',async()=>{
 const spec=cases.find(s=>s.id==='argv');
 for(const mode of ['bad-exit','bad-final']){const {result,state,project}=await capture(spec,mode);assert.throws(()=>verifyCase(spec,'rust',result,state,project));}
});
test('missing duplicate reordered RPC and missing pong mutations are rejected',async()=>{
 const spec=cases.find(s=>s.id==='idle-heartbeat');const {result,state,project}=await capture(spec);assert.equal(verifyCase(spec,'rust',result,state,project).productPassed,true);
 for(const mutate of [s=>s.requests.pop(),s=>s.requests.push(s.requests[0]),s=>s.requests.reverse(),s=>{s.pongs=0;},s=>{s.requests[1].value.id=s.requests[0].value.id;s.requests[1].raw=JSON.stringify(s.requests[1].value);}]){const copy=structuredClone(state);mutate(copy);assert.throws(()=>verifyCase(spec,'rust',result,copy,project));}
});
test('configuration and file snapshot retain actual bytes',()=>{
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'c1-online-snapshot-'));try{fs.writeFileSync(path.join(root,'config'),config(12345));const files=snapshot(root);assert.equal(files.length,1);assert.equal(Buffer.from(files[0].base64,'base64').toString(),config(12345));assert.equal(files[0].sha256.length,64);}finally{fs.rmSync(root,{recursive:true,force:true});}
});

test('WebSocket payload length 127 does not become a 64-bit length marker',async()=>{
 const mock=await startMock(cases[0],{port:0});let ws;
 try{
  ws=new WebSocket(`ws://127.0.0.1:${mock.port}/ws`);
  await new Promise((resolve,reject)=>{ws.onopen=resolve;ws.onerror=reject;});
  const value={type:'hello',padding:''};value.padding='x'.repeat(127-JSON.stringify(value).length);
  const raw=JSON.stringify(value);assert.equal(Buffer.byteLength(raw),127);
  const reply=new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('hello not decoded')),1000);ws.onmessage=e=>{clearTimeout(timer);resolve(JSON.parse(e.data));};});
  ws.send(raw);assert.equal((await reply).type,'hello_ok');assert.deepEqual(mock.state.problems,[]);assert.equal(mock.state.hellos[0].raw,raw);
 }finally{ws?.close();await mock.stop();}
});


test('failed or extra cleanup signals cannot pass as user cancellation',async()=>{
 const spec=cases.find(s=>s.id==='sigint');const {result,state,project}=await capture(spec);
 assert.equal(verifyCase(spec,'rust',result,state,project).productPassed,true);
 for(const signals of [[{...result.signals[0],sent:false}],[...result.signals,{...result.signals[0],phase:'collector-cleanup'}]])assert.throws(()=>verifyCase(spec,'rust',{...result,signals},state,project));
});
