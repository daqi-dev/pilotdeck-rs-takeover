// Exact seven CLI steps; public comparator supplies its own trusted load.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
const sha=b=>createHash('sha256').update(b).digest('hex');
const BASELINE='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
const SCOPE={
 'cli-entry-default':{hash:'780dbded99fe0e6dcafff52d185d6827e8cfe32973b72edced0a9cfbdbee6e6f',indices:[0,1,2],messages:['hello','nonsense words']},
 'cli-survey-offline-nontui':{hash:'34ef21f6ff94a3c752408f0d86095225241f4cfdbfcd2e75f622aed1c910aaa4',indices:[0,4],messages:['nonsense']},
 'cli-default-turns':{hash:'f51d455ba74a638b29d9b06addf84f1e453b91de8549266d1a5fd1e27ba4d73c',indices:[0,1],messages:['case-argv: from arguments','case-text: hello there','case-read: please read','case-bash: run it','case-400: fail now']},
};
const CHAT='/home/.pilotdeck/projects/private-tmp-pdrs-bbx-root-proj/chats/cli:project=-tmp-pdrs-bbx-root-proj:default.jsonl';
const REGISTRY='/home/.pilotdeck/runs/registry.json';
const VIEWS=['agent-transcript.conversation','agent-transcript.turn-summary','agent-transcript.metadata','agent-transcript.compact-boundary','agent-transcript.subagent-references','file-history.snapshots','web-history.file-artifacts','web-history.agent-statuses','web-history.turn-errors','web-history.token-usage','web-history.incomplete-turns','plan-todo.state','goal.state'];
const errorText='[error:gateway_dialog_recovery_cleanup_failed] Unknown projectKey: /tmp/pdrs-bbx/root/proj';
function registry(a,scope){
 const value=a.tree?.[REGISTRY]?.json;assert.equal(value?.schemaVersion,1);assert.equal(value.records?.length,scope.messages.length,'exact admitted CLI work');
 const messages=[];
 for(const row of value.records){const input=JSON.parse(row.requestMaterial);messages.push(input.message);assert.equal(input.canPrompt,false);assert.equal(input.channelKey,'cli');assert.equal(input.sessionKey,row.sessionKey);assert.equal(row.result?.runId,row.runId);
  const events=value.events?.[`\0${row.sessionKey}\0${row.runId}`];assert(Array.isArray(events)&&events.length>0,'terminal registry events');assert.deepEqual(events.at(-1).event,row.result,'actual terminal result');
  if(a.scenario==='cli-default-turns'){assert.equal(row.state,'completed');assert.equal(row.result.type,'turn_completed');assert.equal(row.result.finishReason,input.message.startsWith('case-400:')?'model_error':'completed');}
  else {assert.equal(row.state,'failed');assert.equal(row.lastSeq,1);assert.equal(row.result.type,'error');assert.equal(row.result.code,'gateway_dialog_recovery_cleanup_failed');assert.equal(row.result.message,'Unknown projectKey: /tmp/pdrs-bbx/root/proj');}
 }
 assert.deepEqual(messages,scope.messages,'all actual requested turns completed before exit');
}
function side(a,kind,scope,scene){
 assert.equal(a.side,kind);assert.equal(a.evidence?.side,kind);assert.equal(a.evidence?.inputs?.ts?.commit,BASELINE);assert([scope.hash,scope.currentHash].includes(a.evidence?.scenario?.sha256),'original or exact accepted-declaration scene identity');assert.deepEqual(a.steps.map(x=>x.step),scene.steps,'unchanged fixed CLI commands/stdin/deadlines');
 for(const i of scope.indices){const s=a.steps[i];assert(s.step.expectTimeout===true);assert(s.cli.stderr.includes('[pilotdeck] gateway server unavailable; using in-process mode.\n'),'gateway probe actually finished');
  if(kind==='ts'){assert.equal(s.timedOut,true);assert.equal(s.cli.exitCode,null);assert.equal(s.cli.signal,'SIGKILL');}
  else {assert(!Object.hasOwn(s,'timedOut'),'Rust must finish before runner timeout');assert.equal(s.cli.exitCode,0);assert.equal(s.cli.signal,null);}
 }
 assert.deepEqual(a.failures,kind==='ts'?scope.indices.map(i=>({at:`steps[${i}]`,kind:'timeout',expected:true})):[],'only the precise expected hangs');
 registry(a,scope);
 if(a.scenario==='cli-entry-default'){assert.equal(a.steps[0].cli.stdout,'');assert.equal(a.steps[1].cli.stdout,'\n'+errorText+'\n\n');assert.equal(a.steps[2].cli.stdout,'\n'+errorText+'\n');}
 if(a.scenario==='cli-survey-offline-nontui'){assert.equal(a.steps[0].cli.stdout,'');assert.equal(a.steps[4].cli.stdout,'\n'+errorText+'\n');}
 if(a.scenario!=='cli-default-turns'){assert.deepEqual(a.provider,{});return;}
 const names=['title','argv','text','read','read-after-tool','bash','bash-after-tool','e400'];assert.deepEqual(Object.keys(a.provider),names,'complete actual model branches');for(const name of names){assert.equal(a.provider[name].length,1);assert.equal(a.provider[name][0].method,'POST');assert.equal(a.provider[name][0].url,'/v1/chat/completions');}
 assert.equal(a.steps[0].cli.stdout,'Argument turn answered.');const text=a.steps[1].cli.stdout;
 for(const marker of ['Hello from the mock.\nSecond line.','1|note body\n2|second line','The note was read.','BASH_RESULT[success][stdout_data]','from-bash','Bash step done.','[error:agent_model_error] bad request from mock'])assert(text.includes(marker),'completed actual CLI branch '+marker);
 for(const [name,id,marker] of [['read-after-tool','call_cli_read','note body'],['bash-after-tool','call_cli_bash','from-bash']]){const results=a.provider[name][0].body.messages.filter(m=>m.role==='tool'&&m.tool_call_id===id);assert.equal(results.length,1);assert(results[0].content.includes(marker),'actual tool result reached model');}
}
function checkpoint(a,expected){
 const entry=a.tree?.[CHAT],rows=entry?.jsonl,p=a.tree?.[CHAT+'.projections.json']?.json;assert.equal(rows?.length,130);assert.equal(entry.raw?.lines,130);assert.equal(entry.raw?.finalNewline,true);
 const bytes=Buffer.from(rows.map(r=>JSON.stringify(r)+'\n').join(''));assert.deepEqual(a.evidence?.persistedFiles?.[CHAT],{byteLength:bytes.length,sha256:sha(bytes)},'exact persisted JSONL witness');
 for(const [i,r] of rows.entries()){assert.equal(r.sequence,i+1);if(i>0)assert.equal(r.parentEntryId,rows[i-1].entryId);}
 assert.equal(rows[128].type,'turn_result');assert.equal(rows[128].result?.stopReason,'model_error');assert.equal(rows[129].type,'session_metadata');assert.equal(rows[129].metadata?.isSnapshot,true);assert.equal(rows[129].turnId,rows[128].turnId);assert.equal(rows[129].metadata.firstPrompt,'case-argv: from arguments');assert.equal(rows[129].metadata.lastPrompt,'case-400: fail now');
 assert.equal(p?.format,'pilotdeck.session-projections');assert.equal(p.sessionId,rows[0].sessionId);assert.equal(p.asOfSequence,expected);const row=rows[expected-1];
 for(const k of ['sequence','entryId','parentEntryId','turnId','type','createdAt'])assert.equal(p.anchor?.[k],row[k],'checkpoint anchor exact '+k);
 assert.deepEqual(Object.keys(p.projections),VIEWS);for(const view of Object.values(p.projections))assert.equal(view.asOfSequence,expected);
 // A stale cache can differ in timestamp only; no semantic tail metadata loss.
 const state={...p.projections['agent-transcript.metadata'].state},tail={...rows[129].metadata};delete state.updatedAt;delete tail.updatedAt;assert.deepEqual(state,tail);
}
function differences(a,b,at='$',out=[]){
 if(Object.is(a,b))return out;if(!a||!b||typeof a!=='object'||typeof b!=='object'||Array.isArray(a)!==Array.isArray(b)){out.push(at);return out;}
 if(Array.isArray(a)){if(a.length!==b.length)out.push(at+'.length');for(let i=0;i<Math.max(a.length,b.length);i++)differences(a[i],b[i],`${at}[${i}]`,out);return out;}
 const ak=Object.keys(a),bk=Object.keys(b),shared=ak.filter(k=>Object.hasOwn(b,k));if(JSON.stringify(shared)!==JSON.stringify(bk.filter(k=>Object.hasOwn(a,k))))out.push(at+'.<key order>');
 for(const k of new Set([...ak,...bk])){if(!Object.hasOwn(a,k)||!Object.hasOwn(b,k))out.push(at+'.'+k);else differences(a[k],b[k],at+'.'+k,out);}return out;
}
/** Trusted caller supplies its existing normalizer/load, independently fresh for each side.
 * This single entry derives views itself; never pass artifact-authored callbacks.
 * Returned paths apply only after all raw evidence and complete ordinary differences pass. */
export function cliShutdownAcceptedPaths(ts,rust,normalize){
 assert.equal(typeof normalize,'function','existing comparator normalizer required');assert.equal(ts.scenario,rust.scenario);cliShutdownEvidence(ts);cliShutdownEvidence(rust);const scope=SCOPE[ts.scenario];
 const allowed=new Map();for(const i of scope.indices)for(const leaf of ['timedOut','cli.exitCode','cli.signal'])allowed.set(`$.steps[${i}].${leaf}`,'CLI completed/EOF disposal bug fix, exact seven-step scope');
 if(ts.scenario==='cli-default-turns'){checkpoint(ts,129);checkpoint(rust,130);const prefix='$.tree.'+CHAT+'.projections.json.json';for(const leaf of ['asOfSequence','anchor.sequence','anchor.entryId','anchor.parentEntryId','anchor.type',...VIEWS.map(v=>'projections.'+v+'.asOfSequence')])allowed.set(prefix+'.'+leaf,'exact same-side durable anchor 129 turn_result → 130 tail metadata after shutdown');}
 const a=normalize(structuredClone(ts)),b=normalize(structuredClone(rust));const diff=differences(a,b);assert.deepEqual([...new Set(diff)].sort(),[...allowed.keys()].sort(),'every ordinary observable remains equal outside the exact shutdown leaves');return allowed;
}

/** Full single-artifact evidence; does not grant any pair comparison exemption. */
export function cliShutdownEvidence(artifact){
 const scope=SCOPE[artifact.scenario];assert(scope,'only three exact CLI scenes');const sceneBytes=readFileSync(new URL(`./scenarios/${artifact.scenario}.json`,import.meta.url));const sceneText=sceneBytes.toString();const declarationLine="  \"accepted\": [{\"id\":\"D1-CLI-SHUTDOWN-001\",\"why\":\"固定七步CLI任务/EOF完成后的关闭挂起修复及129→130尾metadata checkpoint\",\"policy\":\"cliShutdownCompleted\",\"assertions\":[\"cli-shutdown-completed-positive\"]}],\n  \"assertions\": [{\"id\":\"cli-shutdown-completed-positive\",\"path\":\"/steps\",\"cliShutdownCompleted\":true}],\n";assert(sceneText.includes(declarationLine),'exact required declaration');assert.equal(sha(Buffer.from(sceneText.replace(declarationLine,''))),scope.hash,'unchanged fixed scene except accepted declaration');const scene=JSON.parse(sceneBytes);const currentScope={...scope,currentHash:sha(sceneBytes)};assert(['ts','rust'].includes(artifact.side));side(artifact,artifact.side,currentScope,scene);if(artifact.scenario==='cli-default-turns')checkpoint(artifact,artifact.side==='ts'?129:130);return true;
}
