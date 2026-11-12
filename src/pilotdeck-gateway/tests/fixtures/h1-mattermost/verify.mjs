#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {verify} from './server-probe.mjs';
const [left,right,out] = process.argv.slice(2);
assert.ok(left && right && out, 'verify.mjs LEFT RIGHT NEW_REPORT');
assert.ok(!fs.existsSync(out), 'report must be new');
const read = dir => JSON.parse(fs.readFileSync(path.join(dir,'results.json')));
const expected = ['root-url','api-prefix','encoded-token','rejected','missing','lifecycle','cron-unsupported','lifecycle-active'];
function project(rows) {
 assert.deepEqual(rows.map(row=>row.name), expected);
 return rows.map(row=> {
  verify(row,row.name); assert.equal(row.valid,true);
  const provider = row.providerRequests.map(request=>JSON.parse(request.body)).map(body=>({model:body.model,stream:body.stream,user:body.messages.filter(m=>m.role==='user')})).sort((a,b)=>Number(a.stream)-Number(b.stream));
  return {name:row.name,rest:row.rest,upgrades:row.upgrades.map(u=>({...u,origin:u.origin.replace(String(row.mockPort),'<mockPort>')})),provider,exitCode:row.exitCode,signal:row.signal,forceKilled:row.forceKilled,disabledUpgrades:row.disabledUpgrades,disabledClients:row.disabledClients,providerOpenAfterDisable:row.providerOpenAfterDisable,aliveAfterDisable:row.aliveAfterDisable,postsAfterDisable:row.postsAfterDisable,latePostsAfterRelease:row.latePostsAfterRelease};
 });
}
const a=read(left), b=read(right);
assert.deepEqual(project(a),project(b));
const controls=[];
for(const [name,mutate] of [
 ['ws-path',r=>r.upgrades[0].path='/api/v4/websocket?wrong=1'],
 ['origin',r=>r.upgrades[0].origin+='/api/v4'],
 ['rest-bearer',r=>r.rest[0].authorization='Bearer wrong'],
 ['reply',r=>r.rest[2].body='{"channel_id":"fixture-channel","message":"wrong"}'],
 ['provider-missing',r=>r.providerRequests.pop()],
 ['forced-exit',r=>r.forceKilled=true],
]) {const row=structuredClone(a[0]); mutate(row); assert.throws(()=>verify(row,row.name),undefined,name); controls.push({name,rejected:true});}
for(const [name,index,mutate] of [['stop-reconnect',5,r=>r.disabledUpgrades=3],['stop-resource-observation',7,r=>r.providerOpenAfterDisable=false],['cron-provider-missing',6,r=>r.providerRequests=[]],['cron-warning-missing',6,r=>r.stderr='']]) {const row=structuredClone(a[index]);mutate(row);assert.throws(()=>verify(row,row.name),undefined,name);controls.push({name,rejected:true});}
fs.writeFileSync(out,JSON.stringify({valid:true,cases:expected,selfCompare:'equal',negativeControls:controls,normalization:'随机mock端口仅Origin；provider仅比较模型/stream/用户输入，其余原始字段全部保留在capture，未声称全Gateway provider对拍。'},null,2)+'\n');
console.log('8 formal scenarios self-compare equal; 10 negative controls rejected');
