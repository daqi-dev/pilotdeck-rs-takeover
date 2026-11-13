import test from 'node:test';
import assert from 'node:assert/strict';
import {memoryReportDerived,memoryHeaderObjectOrder,observerMemoryPaths} from './observer-memory-policy.mjs';
const parent='/home/.pilotdeck/projects/tmp-pdrs-bbx-root-home-.pilotdeck/chats/agent03-parent.jsonl';
const registry='/home/.pilotdeck/runs/registry.json';
const prefix='[worker] AG03 delegate\n\nScope: done.\nResult: AG03-CHILD-REPORT.\nKey files: none\nFiles changed: none\nIssues: none\n';
const bytes=b=>Buffer.byteLength(JSON.stringify(b),'utf8');
// Independently assembled exact-contract fixture; no product or local artifact
// paths are needed by distributable tests. The full actual pair is checked in
// a separate application receipt, not represented as synthetic wire evidence.
function fixture(right=false){
 const id=right?'93bd1e48-276c-4010-af3a-05daf122b04e':'83bd1e48-276c-4010-af3a-05daf122b04e',run='parent-run';
 const data={subagentType:'worker',description:'AG03 delegate',text:'Scope: done.\nResult: AG03-CHILD-REPORT.\nKey files: none\nFiles changed: none\nIssues: none',usage:{inputTokens:48,outputTokens:16,totalTokens:64},turns:4,durationMs:right?1436:105,parsed:{Scope:'done.',Result:'AG03-CHILD-REPORT.','Key files':'none','Files changed':'none',Issues:'none'},subagentSessionId:`/tmp/pdrs-bbx/root/home/.pilotdeck::sub::${id}`,transcriptRelativePath:`agent03-parent/subagents/${id}.jsonl`};
 const report=prefix+JSON.stringify(data),result={type:'success',toolName:'agent',toolCallId:'ag03-agent',content:[{type:'text',text:prefix.trimEnd()},{type:'json',value:data}],data};
 const finish={type:'tool_call_finished',toolName:'agent',toolCallId:'ag03-agent',ok:true,resultPreview:report,resultBytes:Buffer.byteLength(report),data,runId:run};
 const headers={host:'127.0.0.1:1234',connection:'keep-alive','content-type':'application/json',authorization:'Bearer isolated',accept:'*/*','accept-language':'*','sec-fetch-mode':'cors','user-agent':'node','accept-encoding':'gzip, deflate','content-length':'1'};
 function req(body,reorder=false){const h={...headers,'content-length':String(bytes(body))};return {method:'POST',url:'/v1/chat/completions',headers:reorder?Object.fromEntries(['content-type','authorization','accept','accept-language','sec-fetch-mode','user-agent','connection','accept-encoding','host','content-length'].map(k=>[k,h[k]])):h,body}}
 const body=()=>({model:'deterministic',messages:[{role:'tool',tool_call_id:'ag03-agent',content:report}],tools:[],stream:true});
 const provider={'parent-after':[req(body())],'parent-check':[req(body())]};
 for(const name of ['memory-gate','memory-selector'])provider[name]=[0,1,2].map(i=>req({model:'deterministic',messages:[{role:'user',content:name+i}],stream:true},right));
 return {scenario:'agent03-sdk-fork-lifecycle-probe',evidence:{scenario:{sha256:'d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a'},inputs:{ts:{commit:'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85'}}},failures:[],provider,steps:[{step:{ws:'submit_turn',params:{sessionKey:'agent03-parent',message:'AG03-PARENT-BEGIN'}},frames:[{event:{type:'turn_started',runId:run}},{event:structuredClone(finish)}]}],tree:{[parent]:{jsonl:[{type:'tool_result',sessionId:'agent03-parent',turnId:run,result:structuredClone(result)},{type:'tool_result_message',turnId:run,message:{content:[{type:'tool_result',toolCallId:'ag03-agent',content:[{type:'text',text:report}],raw:structuredClone(result)}]}}]},[registry]:{json:{events:{['\0agent03-parent\0'+run]:[{event:structuredClone(finish)}]}}}}};
}
const pair=()=>[fixture(),fixture(true)];
test('only four exact report-byte paths, original objects unchanged',()=>{const p=pair(),before=JSON.stringify(p);assert.equal(memoryReportDerived(...p,...p).size,4);assert.equal(JSON.stringify(p),before)});
test('only six captured header-key orders; no global header exception',()=>{const p=pair();const paths=observerMemoryPaths(...p,...p);assert.equal(paths.size,10);assert.equal([...paths.keys()].filter(k=>k.endsWith('.<key order>')).length,6);assert.ok(!paths.has('$.provider.parent-after[0].headers.<key order>'));assert.equal(observerMemoryPaths({scenario:'agent03-sdk-fork'},{scenario:'other'}).size,0)});
function negative(name,mutate,run=memoryHeaderObjectOrder){test(name,()=>{const p=pair();mutate(p[1]);assert.throws(()=>run(...p,...p))})}
negative('reject header value change',a=>a.provider['memory-gate'][0].headers.authorization+='x');
negative('reject missing header key',a=>delete a.provider['memory-selector'][1].headers.accept);
negative('reject extra header key',a=>a.provider['memory-selector'][2].headers.extra='x');
negative('reject changed duplicate representation',a=>a.provider['memory-gate'][1].headers.accept=[a.provider['memory-gate'][1].headers.accept]);
negative('reject body change with recomputed length',a=>{const r=a.provider['memory-gate'][0];r.body.model+='x';r.headers['content-length']=String(bytes(r.body))});
negative('reject false content-length',a=>a.provider['memory-gate'][2].headers['content-length']='1');
negative('reject extra request',a=>a.provider['memory-selector'].push(structuredClone(a.provider['memory-selector'][0])));
negative('reject wrong scene identity',a=>a.evidence.scenario.sha256='0'.repeat(64));
negative('reject false report bytes',a=>a.steps[0].frames[1].event.resultBytes++,memoryReportDerived);
negative('reject parent body change with true length',a=>{const r=a.provider['parent-after'][0];r.body.model+='x';r.headers['content-length']=String(bytes(r.body))},memoryReportDerived);
negative('reject wrong call association',a=>a.provider['parent-check'][0].body.messages[0].tool_call_id='wrong',memoryReportDerived);
negative('reject false registry bytes',a=>Object.values(a.tree[registry].json.events)[0][0].event.resultBytes++,memoryReportDerived);
