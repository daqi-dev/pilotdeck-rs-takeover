import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { REGISTRATION } from './https-presentation-registration.mjs';
import { auditReadset, compareProjected, WARNING } from './accepted-https-presentation-exact.mjs';
const source = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../../..');
const rawRoot = path.join(source, '_tmp/20261113-e0-resume');
const viewRoot = path.join(source, '_tmp/20261114-c1-historical-contract-completion/https-public-first/strict-parity');
const read = side => JSON.parse(fs.readFileSync(path.join(viewRoot, `feishu-success.${side}.json`)));
const rejected = (left, right) => { let invalid=false,delta;try {delta=compareProjected(left,right);}catch {invalid=true;}assert.ok(invalid||delta.length); };
const controls = [
  ['header value', (_a,b)=>{b.requests[0].rawHeaders[5]+=';canary';}],
  ['decoded header value', (_a,b)=>{b.requests[0].headers['content-type']='text/plain';}],
  ['unknown header name', (_a,b)=>{b.requests[0].rawHeaders[4]='X-Content-Type';}],
  ['extra raw pair', (_a,b)=>{b.requests[0].rawHeaders.push('x-canary','1');}],
  ['duplicate raw pair', (_a,b)=>{b.requests[0].rawHeaders.push(...b.requests[0].rawHeaders.slice(4,6));}],
  ['missing raw pair', (_a,b)=>{b.requests[0].rawHeaders.splice(4,2);}],
  ['pair order', (_a,b)=>{b.requests[0].rawHeaders.push(...b.requests[0].rawHeaders.splice(0,2));}],
  ['duplicate value order', (_a,b)=>{b.requests[0].rawHeaders.push('content-type','second','content-type','first');}],
  ['unlisted name case', (_a,b)=>{b.requests[0].rawHeaders[0]=b.requests[0].rawHeaders[0].toUpperCase();}],
  ['body bytes', (_a,b)=>{b.requests[0].bodyBase64+='YQ==';}],
  ['body field', (_a,b)=>{b.requests[0].body.app_secret='other';}],
  ['method', (_a,b)=>{b.requests[0].method='GET';}],
  ['URL', (_a,b)=>{b.requests[0].path+='/canary';}],
  ['request count', (_a,b)=>{b.requests.push(structuredClone(b.requests[0]));}],
  ['unknown request key', (_a,b)=>{b.requests[0].newField=true;}],
  ['unknown result key', (_a,b)=>{b.newField=true;}],
  ['missing key', (_a,b)=>{delete b.answered;}],
  ['business stderr', (_a,b)=>{b.stderr='business warning\n';}],
  ['Rust warning', (_a,b)=>{b.stderr=WARNING;}],
  ['nonprefix TS warning', (a,_b)=>{a.stderr='business\n'+a.stderr;}],
  ['changed TS warning', (a,_b)=>{a.stderr=a.stderr.replace('SQLite','Other');}],
  ['CRLF TS warning', (a,_b)=>{a.stderr=a.stderr.replaceAll('\n','\r\n');}],
  ['duplicate TS warning', (a,_b)=>{a.stderr=WARNING+a.stderr;}],
  ['missing TS warning', (a,_b)=>{a.stderr='';}],
  ['stdout', (_a,b)=>{b.stdout+='canary';}],
  ['exit', (_a,b)=>{b.exitCode=1;}],
  ['signal', (_a,b)=>{b.signal='SIGTERM';}],
  ['answer count', (_a,b)=>{b.answered++;}],
  ['YAML', (_a,b)=>{b.yaml+='canary: true\n';}],
  ['credentials', (_a,b)=>{b.credentials='{}';}],
  ['final tree', (_a,b)=>{b.tree.push({path:'home/extra',text:'canary'});}],
  ['initial tree', (_a,b)=>{b.initialTree.push({path:'home/extra',text:'canary'});}],
  ['argv', (_a,b)=>{b.command.argv.push('--canary');}],
  ['env', (_a,b)=>{b.command.env.NEW_CANARY='1';}],
];
test('actual validated full projections accept only registered presentation',()=>assert.deepEqual(compareProjected(read('left'),read('right')),[]));
for(const [name,mutate] of controls)test(`content rejected: ${name}`,()=>{const a=read('left'),b=read('right');mutate(a,b);rejected(a,b);});
test('missing and extra read keys fail independently of supplied file digests',()=>{
 const missing={...REGISTRATION.raw};delete missing[Object.keys(missing)[0]];assert.throws(()=>auditReadset(missing));
 assert.throws(()=>auditReadset({...REGISTRATION.raw,'unregistered/read.json':'a'.repeat(64)}));
});
test('actual public entry refuses changed raw/source/build and caller authorization',t=>{
 const root=fs.mkdtempSync(path.join(os.tmpdir(),'c1-https-exact-'));t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
 const copy=(from,to)=>{fs.mkdirSync(path.dirname(to),{recursive:true});fs.copyFileSync(from,to);};
 const localSource=path.join(root,'source'), evidence=path.join(root,'evidence');
 for(const name of Object.keys(REGISTRATION.judge))copy(path.join(source,name),path.join(localSource,name));
 const entryNames=['accepted-https-presentation-exact.mjs','https-presentation-registration.mjs'];
 for(const name of entryNames){const p='src/pilotdeck-cli/tests/fixtures/c1-entry/'+name;copy(path.join(source,p),path.join(localSource,p));}
 for(const name of Object.keys({...REGISTRATION.raw,...REGISTRATION.build}))copy(path.join(rawRoot,name),path.join(evidence,name));
 for(const name of Object.keys(REGISTRATION.raw).filter(p=>p.endsWith('/files-after.json'))){for(const row of JSON.parse(fs.readFileSync(path.join(rawRoot,name))))if(row.directory)fs.mkdirSync(path.join(evidence,path.dirname(name),row.path),{recursive:true});}
 const entry=path.join(localSource,'src/pilotdeck-cli/tests/fixtures/c1-entry/accepted-https-presentation-exact.mjs');let n=0;
 const run=extra=>spawnSync(process.execPath,[entry,'--evidence-root',evidence,'--out',path.join(root,'output-'+n++),...(extra??[])],{encoding:'utf8',timeout:10000});
 const positive=run();assert.equal(positive.status,0,positive.stderr);assert.match(positive.stdout,/ACCEPTED_EXACT_PRESENTATION: 16/,'actual CLI body must execute at a new locator');
 const edit=(file,change)=>{const before=fs.readFileSync(file);try{fs.writeFileSync(file,change(before));const q=run();assert.equal(q.status,2,q.stdout);assert.match(q.stderr,/pinned bytes/);}finally{fs.writeFileSync(file,before);}};
 edit(path.join(evidence,Object.keys(REGISTRATION.raw).find(p=>p.endsWith('/stdout.bin'))),b=>Buffer.concat([b,Buffer.from('canary')]));
 edit(path.join(evidence,REGISTRATION.capturePrefix,'ts-a/identity.json'),b=>Buffer.from(b.toString().replace('cc7c4e6','0000000')));
 edit(path.join(localSource,'src/pilotdeck-cli/tests/fixtures/c1-entry/compare-https.mjs'),b=>Buffer.concat([b,Buffer.from('\n// changed judge\n')]));
 edit(path.join(evidence,'c88650f6-sealed-release/release-record.json'),b=>Buffer.from(b.toString().replace(REGISTRATION.productCandidate,'0'.repeat(40))));
 edit(path.join(evidence,'c88650f6-release/release.exit'),_b=>Buffer.from('1\n'));
 assert.equal(run(['--manifest',path.join(root,'caller-authorizes-anything.json')]).status,2,'caller cannot supply authorization');
 const first=path.join(evidence,Object.keys(REGISTRATION.raw)[0]);fs.renameSync(first,first+'.held');try{assert.equal(run().status,2,'incomplete or wrong capture fails');}finally{fs.renameSync(first+'.held',first);}
 const restored=run();assert.equal(restored.status,0,restored.stderr);assert.match(restored.stdout,/ACCEPTED_EXACT_PRESENTATION: 16/,'restored public body actually runs');
});

test('in-process caller aliases cannot mutate trusted registration authority',()=>{
 assert.equal(Object.isFrozen(REGISTRATION),true);
 for(const name of ['raw','judge','build']) {
  const alias=REGISTRATION[name],before={...alias};assert.equal(Object.isFrozen(alias),true);
  assert.throws(()=>{alias['caller-added/authority.json']='a'.repeat(64);},TypeError);
  assert.throws(()=>{alias[Object.keys(alias)[0]]='b'.repeat(64);},TypeError);
  assert.deepEqual(alias,before);
 }
 assert.throws(()=>{REGISTRATION.raw={'caller-added/authority.json':'a'.repeat(64)};},TypeError);
 assert.throws(()=>auditReadset({...REGISTRATION.raw,'caller-added/authority.json':'a'.repeat(64)}));
});
