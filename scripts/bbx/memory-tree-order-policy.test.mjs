import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
import {brotliDecompressSync,brotliCompressSync} from 'node:zlib';
import {spawnSync} from 'node:child_process';
import {memoryTreeOrderPaths} from './memory-tree-order-policy.mjs';
const here = path.dirname(fileURLToPath(import.meta.url));
const fixture = path.join(here, 'fixtures/memory-tree-order');
const pins = [
 ['638fbf804b9eb2884a865c01090a6091b18d7693cee1afdd2126518c0870e7dd','cd96411ddc477083a32947bf4d28cc999675b6fd3431f1f5b39efd5fe020c75e',814000],
 ['f0cf14fab6eaca348fff1e777f2a28a1e1e72c1ef86af0b56bb4fc823c2e7424','5ca79f49465369b96c9b1586018b6927661818bbaaf06f652d6ba9739fd609fd',814036],
];
const sha = x => createHash('sha256').update(x).digest('hex');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-memory-tree-'));
const files = pins.map(([compressedSHA, rawSHA, size], i) => {
 const compressed = fs.readFileSync(path.join(fixture, `ts-${i+1}.json.br`));assert.equal(sha(compressed), compressedSHA);
 const bytes = brotliDecompressSync(compressed, {maxOutputLength:size});assert.equal(bytes.length,size);assert.equal(sha(bytes),rawSHA);
 const f=path.join(temp,`ts-${i+1}.json`);fs.writeFileSync(f,bytes);return f;
});
const raw=()=>files.map(f=>JSON.parse(fs.readFileSync(f)));
process.on('exit',()=>fs.rmSync(temp,{recursive:true,force:true}));
test.after(()=>fs.rmSync(temp,{recursive:true,force:true}));
test('sealed originals permit precisely one self leaf',()=>assert.deepEqual([...memoryTreeOrderPaths(...files,{self:true,expectedRaw:raw()}).keys()],['$.tree.<key order>']));
const variants=[
 ['content',r=>{r[1].tree[Object.keys(r[1].tree)[5]].jsonl.at(-1).type='changed';}],
 ['key name',r=>{const entries=Object.entries(r[1].tree);entries[5][0]+='-wrong';r[1].tree=Object.fromEntries(entries);}],
 ['missing key',r=>{delete r[1].tree[Object.keys(r[1].tree)[6]];}],
 ['outside scene',r=>{r[1].evidence.scenario.name='agent03-sdk-fork';}],
 ['other key order',r=>{const entries=Object.entries(r[1].tree);[entries[0],entries[1]]=[entries[1],entries[0]];r[1].tree=Object.fromEntries(entries);}],
 ['ownership',r=>{r[1].tree[Object.keys(r[1].tree)[5]].jsonl[0].sessionId='wrong-child';}],
 ['unobserved permutation',r=>{const entries=Object.entries(r[1].tree);[entries[5],entries[6]]=[entries[6],entries[5]];r[1].tree=Object.fromEntries(entries);}],
 ['Rust or future capture',r=>{r[1].side='rust';}],
];
for(const [name,mutate] of variants)test(`no permission: ${name}`,()=>{const r=raw();mutate(r);const f=path.join(temp,name.replaceAll(' ','-')+'.json');fs.writeFileSync(f,JSON.stringify(r[1]));assert.equal(memoryTreeOrderPaths(files[0],f,{self:true,expectedRaw:raw()}).size,0);});
test('pair and omitted self never obtain permission',()=>{assert.equal(memoryTreeOrderPaths(...files,{self:false}).size,0);assert.equal(memoryTreeOrderPaths(...files).size,0);});
test('reversed sealed artifacts never obtain permission',()=>assert.equal(memoryTreeOrderPaths(...[...files].reverse(),{self:true,expectedRaw:raw()}).size,0));
test('substituted manifest cannot change fixed raw or closure hashes',()=>{const source=fs.readFileSync(new URL('./memory-tree-order-policy.mjs',import.meta.url),'utf8');assert.ok(!source.includes("manifest.json"));assert.ok(source.includes('e05fd368b6b0be32b03c63ba38662b0f70ddc43d60918e72508766fb3907fe77'));assert.ok(source.includes('a3b4aa0efe32f3edcdfdea5b6b2a47ea92c95746291487b137f565ed681ae358'));const r=raw();r[1].tree[Object.keys(r[1].tree)[0]]={forged:true};const f=path.join(temp,'resigned.json');fs.writeFileSync(f,JSON.stringify(r[1]));assert.equal(memoryTreeOrderPaths(files[0],f,{self:true,expectedRaw:raw()}).size,0);});
function cli(self){return spawnSync(process.execPath,[path.join(here,'diff.mjs'),...files,'--max','10000',...(self?['--self']:[])],{encoding:'utf8',maxBuffer:8*1024*1024,timeout:30000,env:{...process.env,HOME:path.join(temp,'home'),PILOT_HOME:path.join(temp,'pilot')}});}
test('real public CLI sealed self SAME with explicit single compatibility leaf',()=>{const r=cli(true);assert.equal(r.signal,null);assert.equal(r.status,0,r.stderr);assert.equal(r.stdout.trim().split('\n').at(-1),'SAME');assert.ok(r.stdout.includes('COMPATIBILITY-VALUE sealedMemoryTreeOrderApproved $.tree.<key order>'));});
test('real public CLI same artifacts without self keep original DIFF1',()=>{const r=cli(false);assert.equal(r.signal,null);assert.equal(r.status,1,r.stderr);assert.equal(r.stdout.trim().split('\n').at(-1),'DIFF 1');assert.ok(!r.stdout.includes('COMPATIBILITY-VALUE sealedMemoryTreeOrderApproved'));});
for(const [name,change] of [['compressed bytes',bytes=>{const copy=Buffer.from(bytes);copy[0]^=1;return copy;}],['self-consistent replaced closure',bytes=>{const receipt=JSON.parse(brotliDecompressSync(bytes));receipt.harness.files['scripts/bbx/observer-lifecycle-policy.mjs']='0'.repeat(64);return brotliCompressSync(Buffer.from(JSON.stringify(receipt))); } ]])test(`reject replaced sealed closure: ${name}`,async()=>{
 const dir=fs.mkdtempSync(path.join(temp,'module-'));fs.mkdirSync(path.join(dir,'fixtures/memory-tree-order'),{recursive:true});
 for(const file of ['memory-tree-order-policy.mjs','observer-lifecycle-policy.mjs'])fs.copyFileSync(path.join(here,file),path.join(dir,file));
 const target=path.join(dir,'fixtures/memory-tree-order/closure-receipt.json.br');fs.writeFileSync(target,change(fs.readFileSync(path.join(fixture,'closure-receipt.json.br'))));
 const api=await import(new URL('file://'+path.join(dir,'memory-tree-order-policy.mjs')));assert.throws(()=>api.memoryTreeOrderPaths(...files,{self:true,expectedRaw:raw()}),/sealed compressed closure/);
});

test('reject same sealed files with different trusted comparator raw',()=>{const r=raw();r[1].tree[Object.keys(r[1].tree)[0]]={forged:true};assert.throws(()=>memoryTreeOrderPaths(...files,{self:true,expectedRaw:r}),/trusted comparator raw/);});

test('reject comparator raw with only outside-group key order changed',()=>{const r=raw();const entries=Object.entries(r[1].tree);[entries[0],entries[1]]=[entries[1],entries[0]];r[1].tree=Object.fromEntries(entries);assert.throws(()=>memoryTreeOrderPaths(...files,{self:true,expectedRaw:r}),/trusted comparator raw/);});
