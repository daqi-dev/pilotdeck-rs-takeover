import test from'node:test';import assert from'node:assert/strict';import{durationFileSizeContract,durationFileSizePaths}from'./duration-file-size-policy.mjs';import{actualManifest,readActual}from'./fixtures/observer-approved/actual.mjs';
test('unrelated scenes do not obtain file size paths',()=>assert.equal(durationFileSizePaths({scenario:'other'},{scenario:'other'}).size,0));
test('named scene without real identity fails closed',()=>assert.throws(()=>durationFileSizePaths({scenario:'gap-loop-long-tool-chain',side:'ts'},{scenario:'gap-loop-long-tool-chain',side:'rust'})));
if(actualManifest){
 const definitions=[['gap-loop-long-tool-chain',3,2],['gap-permissions-json-rules',10,11]],read=(name,side)=>readActual(actualManifest.tail[name][side]);
 for(const[name,index,delta]of definitions){
  for(const side of['ts-2','rust'])test(name+' actual '+side+' complete byte witness',()=>{const p=durationFileSizeContract(read(name,'ts-1'),read(name,side));assert.equal(p.decimalDelta,side==='rust'?delta:0);assert.ok([...p.paths.keys()].every(x=>x===`$.steps[${index}].frames[0].result.session.fileSize`));});
  for(const[label,change]of[
   ['false stat',a=>a.steps[index].frames[0].result.session.fileSize++],
   ['wrong byte hash',a=>{Object.values(a.evidence.persistedFiles)[0].sha256='0'.repeat(64);}],
   ['new provider business',a=>a.provider[Object.keys(a.provider)[0]][0].body.foreign=true],
  ])test(name+' rejects '+label,()=>{const a=read(name,'rust');change(a);assert.throws(()=>durationFileSizeContract(read(name,'ts-1'),a));});
  for(const[label,change]of[
   ['relocated size',a=>{const value=a.steps[index].frames[0].result.session.fileSize;delete a.steps[index].frames[0].result.session.fileSize;a.steps[0].frames??=[{type:'response',ok:true,result:{}}];a.steps[0].frames[0].result??={};a.steps[0].frames[0].result.session={sessionId:'s-gap',fileSize:value};}],
   ['foreign method',a=>a.steps[index].step.ws='foreign'],
   ['foreign session',a=>a.steps[index].step.params.sessionKey='foreign'],
  ])test(name+' rejects both sides '+label,()=>{const sides=['ts-1','rust'].map(s=>read(name,s));sides.forEach(change);assert.throws(()=>durationFileSizeContract(...sides));});
 }
}
