#!/usr/bin/env node
import assert from 'node:assert/strict';import fs from 'node:fs';import path from 'node:path';import {verify} from './server-probe.mjs';
const [left,right,out]=process.argv.slice(2);assert.ok(left&&right&&out&&!fs.existsSync(out));
const expected=['redirect-get','redirect-post','false-url','zero-url','nonascii-token','latin1-token'];
const read=dir=>JSON.parse(fs.readFileSync(path.join(dir,'results.json')));const a=read(left),b=read(right);
const project=rows=>{assert.deepEqual(rows.map(r=>r.name),expected);return rows.map(r=>{verify(r,r.name);assert.equal(r.valid,true);return {name:r.name,rest:r.rest,upgrades:r.upgrades.map(u=>({...u,origin:u.origin.replace(String(r.mockPort),'<mockPort>')})),stderr:r.stderr,exitCode:r.exitCode,signal:r.signal,forceKilled:r.forceKilled}})};
assert.deepEqual(project(a),project(b));const controls=[];
for(const [name,index,mutate] of [['redirect-path',0,r=>r.rest[1].path='/wrong'],['redirect-authorization',0,r=>r.rest[1].authorization='Bearer wrong'],['307-method',1,r=>r.rest[2].method='GET'],['307-body',1,r=>r.rest[2].body='{}'],['false-url-network',2,r=>r.rest.push({})],['bytestring',4,r=>r.stderr='wrong'],['latin1-query',5,r=>r.upgrades[0].path='/wrong']]) {const r=structuredClone(a[index]);mutate(r);assert.throws(()=>verify(r,r.name),undefined,name);controls.push({name,rejected:true})}
fs.writeFileSync(out,JSON.stringify({valid:true,cases:expected,selfCompare:'equal',negativeControls:controls},null,2)+'\n');console.log('6 HTTP/options scenarios equal; 7 negative controls rejected');
