// Exact positive proof for legacy accepted differences. No normalization or
// accepted-path construction happens here; callers keep the existing guards.
import assert from 'node:assert/strict';
const allowed = new Map([
 ['baseline-bug-rename-reload','rename-then-reload-bricks-session'],
 ['hook-cancelled','aborted-hook-does-not-run-tool'],
 ['project-keys','rename-record-chain'],
 ['read-messages-after-rename','rename-record-chain'],
 ['robust-restart-registry-lock-leftover','registry-dead-owner-lock-blocks-startup'],
 ['robust-restart-crash-leftovers','dialog-recovery-keeps-history'],
]);
const only=(rows,label)=>{assert.ok(Array.isArray(rows),label+' array required');assert.equal(rows.length,1,label+' exactly once');return rows[0];};
const step=(a,index,method)=>{const s=a.steps?.[index];assert.equal(s?.step?.ws,method,`step${index} ${method}`);assert.ok(Array.isArray(s.frames),'actual frames required');assert.ok(!s.frames.some(f=>f.error||f.type?.startsWith('__')),'no failed frames');return s;};
const result=(a,index,method)=>{const frame=only(step(a,index,method).frames.filter(f=>f.type==='response'),'response');assert.equal(frame.ok,true,'successful actual response');return frame.result;};
const eventRows=s=>s.frames.filter(f=>f.type==='event').map(f=>f.event);
const text=c=>typeof c==='string'?c:Array.isArray(c)?c.filter(p=>p.type==='text').map(p=>p.text).join(''):'';
const plainText=m=>typeof m.text==='string'?m.text:text(m.content);
function transcript(a,session){
 const [file,value]=only(Object.entries(a.tree??{}).filter(([p])=>p.endsWith(`/chats/${session}.jsonl`)),session+' transcript');
 const rows=value.jsonl;assert.ok(Array.isArray(rows)&&rows.length>0,'nonempty actual transcript');const ids=new Set();
 for(let i=0;i<rows.length;i++){const r=rows[i];assert.equal(r.sessionId,session,'transcript session');assert.ok(Number.isSafeInteger(r.sequence)&&r.sequence>0,'positive sequence');assert.ok(typeof r.entryId==='string'&&r.entryId&&!ids.has(r.entryId),'unique entry id');ids.add(r.entryId);if(i){assert.equal(r.sequence,rows[i-1].sequence+1,'continuous sequence');assert.equal(r.parentEntryId,rows[i-1].entryId,'parent follows prior actual entry');}else{assert.equal(r.sequence,1,'first sequence');assert.equal(r.parentEntryId,null,'first parent');}}
 return {file,rows};
}
function completed(a,index,message,expectedSession){const s=step(a,index,'submit_turn');assert.ok(typeof expectedSession==='string'&&expectedSession);assert.equal(s.step.params.sessionKey,expectedSession,'actual submit session');assert.equal(s.step.params.message,message);const events=eventRows(s);const start=only(events.filter(e=>e?.type==='turn_started'),'turn start');const terminal=only(s.frames.filter(f=>f.final===true),'final frame');assert.equal(terminal.event?.type,'turn_completed');assert.equal(terminal.event.finishReason,'completed');assert.equal(terminal.event.runId,start.runId);assert.ok(typeof start.runId==='string'&&start.runId);return start.runId;}
function messages(a,index,session){const r=result(a,index,'read_session_messages');assert.ok(Array.isArray(r?.messages),'actual readback messages');assert.equal(a.steps[index].step.params.sessionKey,session);return r.messages;}
function hasExactMessage(rows,role,value,count=1){assert.equal(rows.filter(m=>m.role===role&&plainText(m)===value).length,count,`exact ${role} ${value}`);}
function provider(a,name,count){const rows=a.provider?.[name];assert.ok(Array.isArray(rows),`actual provider ${name}`);if(count!==undefined)assert.equal(rows.length,count,`provider ${name} count`);return rows;}
function savedInputs(rows){return rows.filter(r=>r.type==='accepted_input').flatMap(r=>r.messages??[]);}
function savedAssistant(rows){return rows.filter(r=>r.type==='assistant_message').map(r=>r.message);}
function metadata(rows,key,value){assert.ok(rows.some(r=>r.type==='session_metadata'&&r.metadata?.[key]===value),`persisted ${key}=${value}`);}
function providerInput(request,value){assert.ok(Array.isArray(request.body?.messages),'actual provider messages');assert.ok(request.body.messages.some(m=>m.role==='user'&&plainText(m)===value),`provider received ${value}`);}
function durableTurn(rows,run,session,input,reply,aborted=false){
 const start=only(rows.filter(r=>r.type==='turn_started'&&r.turnId===run),'same-run durable start');assert.equal(start.sessionId,session);
 const accepted=only(rows.filter(r=>r.type==='accepted_input'&&r.turnId===run),'same-run durable input');hasExactMessage(accepted.messages,'user',input);assert.ok(start.sequence<accepted.sequence);
 const models=rows.filter(r=>r.type==='model_request'&&r.turnId===run);assert.ok(models.length>0,'same-run actual model request');assert.ok(models.some(r=>r.request?.messages?.some(m=>m.role==='user'&&plainText(m)===input)),'same-run model input');
 const final=only(rows.filter(r=>r.type==='turn_result'&&r.turnId===run),'same-run durable result');assert.equal(final.result?.type,aborted?'aborted':'success');assert.equal(final.result.stopReason,aborted?'aborted_streaming':'completed');assert.equal(final.result.turnId,run);assert.equal(final.result.sessionId,session);assert.ok(accepted.sequence<final.sequence);
 if(reply!==undefined){const answer=only(rows.filter(r=>r.type==='assistant_message'&&r.turnId===run&&r.message?.role==='assistant'&&plainText(r.message)===reply),'same-run actual assistant');assert.ok(accepted.sequence<answer.sequence&&answer.sequence<final.sequence);assert.equal(plainText(final.result.finalMessage),reply);}
 return final;
}
function renameReload(a){
 assert.equal(result(a,2,'rename_session')?.updated,true);assert.equal(result(a,3,'close_session')?.ok,true);
 const firstRun=completed(a,0,'hello','s-meta'),secondRun=completed(a,4,'hello after reload','s-meta');assert.notEqual(firstRun,secondRun);const requests=provider(a,'main',2);providerInput(requests[0],'hello');providerInput(requests[1],'hello after reload');hasExactMessage(requests[1].body.messages,'user','hello');hasExactMessage(requests[1].body.messages,'assistant','Hello from the mock.');
 const history=messages(a,6,'s-meta');for(const value of ['hello','hello after reload'])hasExactMessage(history,'user',value);hasExactMessage(history,'assistant','Hello from the mock.',2);
 const t=transcript(a,'s-meta');durableTurn(t.rows,firstRun,'s-meta','hello','Hello from the mock.');durableTurn(t.rows,secondRun,'s-meta','hello after reload','Hello from the mock.');metadata(t.rows,'title','Renamed once');for(const value of ['hello','hello after reload'])hasExactMessage(savedInputs(t.rows),'user',value);hasExactMessage(savedAssistant(t.rows),'assistant','Hello from the mock.',2);
 return ['actual rename/close/reload completed','provider and readback two turns','persisted continuous metadata chain'];
}
function renameRead(a){
 for(const [i,method]of [[2,'rename_session'],[4,'tag_session'],[7,'rename_session'],[8,'tag_session'],[14,'rename_session'],[15,'tag_session'],[17,'rename_session']])assert.equal(result(a,i,method)?.updated,true);
 const warm1=completed(a,0,'hello','s-meta'),warm2=completed(a,10,'hello again','s-meta'),coldRun=completed(a,20,'first words','s-cold');assert.equal(new Set([warm1,warm2,coldRun]).size,3);const actualRequests=provider(a,'main',3);for(const [i,value]of ['hello','hello again','first words'].entries())providerInput(actualRequests[i],value);
 for(const value of ['hello','hello again'])hasExactMessage(messages(a,12,'s-meta'),'user',value);hasExactMessage(messages(a,22,'s-cold'),'user','first words');
 const main=transcript(a,'s-meta');durableTurn(main.rows,warm1,'s-meta','hello','Hello from the mock.');durableTurn(main.rows,warm2,'s-meta','hello again','Hello from the mock.');metadata(main.rows,'title','Renamed once');metadata(main.rows,'title','Renamed twice');metadata(main.rows,'tag','important');metadata(main.rows,'tag','42');
 for(const value of ['hello','hello again'])hasExactMessage(savedInputs(main.rows),'user',value);const cold=transcript(a,'s-cold');durableTurn(cold.rows,coldRun,'s-cold','first words','Hello from the mock.');metadata(cold.rows,'title','Never talked');hasExactMessage(savedInputs(cold.rows),'user','first words');
 return ['all metadata requests executed','warm and cold actual turns survived','readback and persisted chain'];
}

function rejected(a,index,method,code){assert.equal(a.steps?.[index]?.step?.ws,method);const frame=only(a.steps[index].frames.filter(f=>f.type==='response'),'rejected response');assert.equal(frame.ok,false);assert.equal(frame.error?.code,code);}
function projectKeys(a){
 assert.equal(result(a,29,'rename_session')?.updated,true);rejected(a,30,'rename_session','PROJECT_NOT_FOUND');rejected(a,31,'delete_session','PROJECT_NOT_FOUND');rejected(a,32,'delete_session','SESSION_NOT_FOUND');assert.equal(result(a,33,'delete_session')?.ok,true);
 const alpha=only(result(a,34,'list_sessions')?.sessions,'alpha session'),beta=only(result(a,35,'list_sessions')?.sessions,'beta session');assert.equal(alpha.sessionId,'s-alpha');assert.equal(alpha.customTitle,'Renamed');assert.ok(alpha.cwd.endsWith('/home/work/alpha'));assert.equal(beta.sessionId,'s-beta');assert.ok(beta.cwd.endsWith('/home/work/beta'));assert.notEqual(beta.customTitle,'Renamed');
 const alphaRun=completed(a,1,'hello alpha','s-alpha'),betaRun=completed(a,3,'hello beta','s-beta');completed(a,5,'hello beta again','s-beta-2');const aHistory=transcript(a,'s-alpha');durableTurn(aHistory.rows,alphaRun,'s-alpha','hello alpha','Hello from the mock.');metadata(aHistory.rows,'title','Renamed');hasExactMessage(savedInputs(aHistory.rows),'user','hello alpha');const bHistory=transcript(a,'s-beta');durableTurn(bHistory.rows,betaRun,'s-beta','hello beta','Hello from the mock.');hasExactMessage(savedInputs(bHistory.rows),'user','hello beta');assert.ok(!bHistory.rows.some(r=>r.type==='session_metadata'&&r.metadata?.title==='Renamed'));
 assert.ok(!Object.keys(a.tree).some(p=>p.endsWith('/chats/s-beta-2.jsonl')),'deleted beta2 absent');const requests=provider(a,'main',3);for(const [i,value]of ['hello alpha','hello beta','hello beta again'].entries())providerInput(requests[i],value);
 return ['actual project-specific rename and rejected wrong-project mutations','alpha/beta transcript chains and isolation','actual three provider turns and requested delete'];
}
function cancelledHook(a){
 const s=step(a,1,'submit_turn');assert.equal(s.step.params.sessionKey,'s-cancel');const frames=s.frames;
 const started=only(frames.filter(f=>f.event?.type==='turn_started'),'actual turn start');const starts=frames.filter(f=>f.event?.type==='agent_status'&&f.event.event==='hook_execution_started');const begun=only(starts,'actual hook start');assert.equal(begun.event.runId,started.event.runId);assert.equal(begun.event.detail?.hookEvent,'PreToolUse');assert.equal(begun.event.detail?.hookName,'bbx-cancel:command');
 const finished=only(frames.filter(f=>f.event?.type==='agent_status'&&f.event.event==='hook_execution_completed'),'hook finish');assert.equal(finished.event.detail?.outcome,'cancelled');assert.equal(finished.event.runId,begun.event.runId);assert.ok(finished.seq>begun.seq);
 const call=only(frames.filter(f=>f.event?.type==='tool_call_started'&&f.event.toolCallId==='call_cancel_case'),'actual requested tool');assert.equal(call.event.name,'bash');assert.equal(call.event.runId,begun.event.runId);assert.equal(JSON.parse(call.event.argsPreview).command,'printf ran > ran.txt');
 const answer=only(s.answers?.filter(v=>v.ws==='abort_turn'),'actual abort answer');assert.equal(answer.params.sessionKey,'s-cancel');const abortResponse=only(answer.frames.filter(f=>f.type==='response'),'abort response');assert.equal(abortResponse.ok,true,'successful actual abort response');assert.equal(abortResponse.error,undefined);assert.equal(abortResponse.result?.ok,true);
 const terminal=only(frames.filter(f=>f.final===true),'cancel terminal');assert.equal(terminal.event?.type,'turn_completed');assert.equal(terminal.event.finishReason,'aborted_streaming');assert.equal(terminal.event.runId,begun.event.runId);assert.ok([started.seq,call.seq,begun.seq,finished.seq,terminal.seq].every(Number.isSafeInteger),'actual stream sequence values');assert.ok(started.seq<call.seq&&call.seq<begun.seq&&begun.seq<finished.seq&&finished.seq<terminal.seq,'start → requested tool → hook start → cancelled → final sequence');
 assert.ok(!Object.keys(a.tree).some(p=>p.endsWith('/ran.txt')),'no actual tool sideeffect');assert.ok(!frames.some(f=>f.event?.type==='tool_call_finished'&&f.event.toolCallId==='call_cancel_case'),'tool did not finish');assert.equal(a.provider?.['cancel-case-after'],undefined,'no post-tool model request');providerInput(only(provider(a,'cancel-case',1),'cancel provider'),'cancel-case: run the tool');
 const t=transcript(a,'s-cancel');durableTurn(t.rows,begun.event.runId,'s-cancel','cancel-case: run the tool',undefined,true);const savedCall=only(t.rows.filter(r=>r.type==='assistant_message'&&r.turnId===begun.event.runId&&r.message?.content?.some(p=>p.type==='tool_call'&&p.id==='call_cancel_case')),'same-run requested tool persisted');assert.equal(savedCall.sessionId,'s-cancel');assert.ok(!t.rows.some(r=>r.type==='tool_result'||r.type==='permission_started'||r.type==='permission_completed'),'no post-cancel execution or permission');const abort=only(t.rows.filter(r=>r.type==='turn_result'),'durable aborted result');assert.equal(abort.result?.type,'aborted');assert.equal(abort.result.stopReason,'aborted_streaming');assert.equal(abort.turnId,begun.event.runId);
 // Immediate cancellation may beat the hook child's first log write. A missing
 // log is not itself the proof; actual matched start/cancel/abort above is.
 for(const [file,value]of Object.entries(a.tree))if(file.endsWith('/hooklog/pre-slow.jsonl'))assert.ok(Array.isArray(value.jsonl)&&!value.jsonl.some(r=>r.done===true),'no completed hook log');
 return ['same-run requested tool + PreToolUse start/cancel + successful abort','no executed tool/permission/model continuation/physical sideeffect','durable aborted turn'];
}
function registryLock(a){
 const before=result(a,0,'run_get'),old=result(a,3,'run_get');assert.equal(before.runId,'run-bbx-1');assert.equal(before.sessionKey,'s-bbx-1');assert.equal(before.state,'interrupted');assert.deepEqual(old,before,'old record preserved after new turn');assert.equal(completed(a,1,'turn-2: hello','s-bbx-1'),'run-bbx-2');
 const now=result(a,2,'run_get');assert.equal(now.runId,'run-bbx-2');assert.equal(now.sessionKey,'s-bbx-1');assert.equal(now.state,'completed');assert.equal(now.result?.finishReason,'completed');assert.equal(now.result.runId,'run-bbx-2');providerInput(only(provider(a,'turn-2',1),'new provider'),'turn-2: hello');
 const t=transcript(a,'s-bbx-1');durableTurn(t.rows,'run-bbx-2','s-bbx-1','turn-2: hello','Done 2.');const registry=a.tree?.['/home/.pilotdeck/runs/registry.json']?.json;assert.equal(registry?.schemaVersion,1);assert.equal(registry.records?.length,2);const prior=only(registry.records.filter(r=>r.runId==='run-bbx-1'),'old durable record');assert.equal(prior.state,'interrupted');assert.equal(JSON.parse(prior.requestMaterial).message,'first question');assert.equal(prior.ownerPid,99999999);const next=only(registry.records.filter(r=>r.runId==='run-bbx-2'),'new durable record');assert.equal(next.state,'completed');assert.equal(next.sessionKey,'s-bbx-1');assert.equal(JSON.parse(next.requestMaterial).message,'turn-2: hello');assert.deepEqual(next.result,now.result);
 const events=registry.events?.['\0s-bbx-1\0run-bbx-2'];assert.ok(Array.isArray(events)&&events.length>0);for(let i=0;i<events.length;i++){assert.equal(events[i].seq,i+1);assert.equal(events[i].event.runId,'run-bbx-2');}assert.equal(events.at(-1).event.type,'turn_completed');assert.equal(events.at(-1).event.finishReason,'completed');assert.ok(events.some(r=>r.event.type==='assistant_text_delta'&&r.event.text==='Done 2.'));assert.equal(a.tree['/home/.pilotdeck/runs/registry.json.lock'],undefined);assert.deepEqual(a.shutdown,{code:0,signal:null});
 return ['dead-owner seeded record actually recovered and preserved','exact new run/provider/durable completed result','stale lock released and clean shutdown'];
}
export function recoveredDialog(a){
 const before=messages(a,6,'s-bbx-1');for(const [role,value]of [['user','first question'],['assistant','first answer'],['user','second question']])hasExactMessage(before,role,value);
 const request='dddddddd-dddd-4ddd-8ddd-dddddddddddd';
 const recoveredRequests=index=>{const dialogs=result(a,index,'user_dialog_list')?.dialogs;assert.ok(Array.isArray(dialogs),'actual recovered dialog list');return dialogs.map(d=>{assert.equal(d?.type,'user_dialog_terminated','recovered dialog wrapper');assert.equal(d.reason,'gateway_restarted','recovered dialog reason');assert.equal(d.recovery,'next_turn_context','recovered dialog continuation');assert.equal(d.request?.type,'user_dialog_request','nested recovered request');assert.ok(typeof d.request.requestId==='string'&&d.request.requestId,'nested recovered request ID');return d.request;});};
 const pending=only(recoveredRequests(5).filter(d=>d.requestId===request),'actual recovered pending dialog');assert.equal(pending.toolCallId,'call_bbx_2');assert.equal(a.steps[7].step.params.requestId,request);assert.deepEqual(a.steps[7].step.params.result,{behavior:'answered',value:true});assert.equal(result(a,7,'user_dialog_respond')?.delivered,true,'seed dialog must actually be recovered');for(const i of [8,11])assert.ok(!recoveredRequests(i).some(d=>d.requestId===request),'answered recovered dialog no longer pending');
 const recoveredRun=completed(a,10,'turn-3: list tasks','s-bbx-1');const requestBody=only(provider(a,'turn-3',1),'recovery model request').body;const actual=requestBody?.messages;assert.ok(Array.isArray(actual));let cursor=-1;for(const [role,value]of [['user','first question'],['assistant','first answer'],['user','second question'],['user','turn-3: list tasks']]){const found=actual.findIndex((m,i)=>i>cursor&&m.role===role&&plainText(m)===value);assert.ok(found>cursor,'ordered old history survives in actual provider');cursor=found;}
 const afterTool=only(provider(a,'turn-3-after-tool',1),'after actual task_list');assert.ok(afterTool.body.messages.some(m=>m.role==='tool'&&m.tool_call_id==='call_bbx_3'),'actual task_list result');
 const after=messages(a,12,'s-bbx-1');for(const [role,value]of [['user','first question'],['assistant','first answer'],['user','second question'],['user','turn-3: list tasks'],['assistant','Done 3.']])hasExactMessage(after,role,value);
 const t=transcript(a,'s-bbx-1');durableTurn(t.rows,recoveredRun,'s-bbx-1','turn-3: list tasks','Done 3.');for(const value of ['first question','second question','turn-3: list tasks'])hasExactMessage(savedInputs(t.rows),'user',value);hasExactMessage(savedAssistant(t.rows),'assistant','first answer');hasExactMessage(savedAssistant(t.rows),'assistant','Done 3.');
 return ['seed history loaded before actual dialog answer','same request delivered and real task_list continuation','old ordered history retained in actual model and persisted readback'];
}
const handlers = { 'baseline-bug-rename-reload':renameReload, 'read-messages-after-rename':renameRead, 'project-keys':projectKeys, 'hook-cancelled':cancelledHook, 'robust-restart-registry-lock-leftover':registryLock, 'robust-restart-crash-leftovers':recoveredDialog };

export function acceptedPositiveContract(artifact, config){
 assert.deepEqual(Object.keys(config??{}).sort(),['id','scene'],'exact positive contract config');assert.ok(allowed.has(config.scene),'unsupported accepted scene');assert.equal(allowed.get(config.scene),config.id,'exact accepted ID/scene');assert.equal(artifact?.scenario,config.scene,'actual scene');assert.equal(artifact.side,'rust','Rust benefit only');assert.deepEqual(artifact.failures,[],'no capture failures');
 const handler=handlers[config.scene];assert.equal(typeof handler,'function','supported exact positive contract');
 return {id:config.id,scene:config.scene,passed:true,checks:handler(artifact)};
}
