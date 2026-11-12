#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
const names=['whatsapp-missing','whatsapp-basic','whatsapp-ready404','whatsapp-cron','whatsapp-active'];
export function verify(rows,side){
 assert.equal(rows.length,names.length,'complete scenario denominator');assert.equal(new Set(rows.map(row=>row.name)).size,names.length,'duplicate scenario');assert.deepEqual([...rows.map(row=>row.name)].sort(),[...names].sort());
 for(const row of rows){
  assert.equal(row.side,side);assert.equal(row.ready,true);assert.equal(row.scriptError,undefined);assert.equal(row.verified,true);assert.equal(row.exitCode,0);assert.equal(row.signal,null);assert.equal(row.forceKilled,false);assert.deepEqual(row.bridgeRemaining,[]);
  for(const wire of row.bridgeWire){assert.equal(wire.authorization,null);if(wire.method==='GET'){assert.equal(wire.path,'/messages');assert.equal(wire.body,'');}else{assert.equal(wire.method,'POST');assert.equal(wire.path,'/send');assert.equal(wire.contentType,'application/json');}}
  const starts=row.bridgeLife.filter(life=>life.event==='start'),stops=row.bridgeLife.filter(life=>life.event==='stop');
  if(row.name==='whatsapp-missing'){assert.equal(starts.length,0);assert.equal(row.sends.length,0);assert.equal(row.provider.length,0);assert.match(row.stderr,/whatsapp: bridgePath \/ WHATSAPP_BRIDGE_PATH is required/);continue;}
  assert.equal(starts.length,row.name==='whatsapp-basic'?2:1);assert.equal(stops.length,starts.length);
  for(const life of starts){assert.equal(life.ppid,row.pid);assert.equal(life.port,String(row.bridgePort));assert.match(life.argv[0],/node$/);assert.match(life.argv[1],/bridge\.mjs$/);assert.equal(stops.filter(stop=>stop.pid===life.pid&&stop.signal==='SIGTERM').length,1);}
  assert.deepEqual(row.sends[0].body,{chatId:'fixture-chat',message:'已创建新会话。'});
  if(row.name==='whatsapp-basic'){
   assert.deepEqual(row.sends.map(send=>send.body),[{chatId:'fixture-chat',message:'已创建新会话。'},{chatId:'fixture-chat',message:'fixture answer'},{chatId:'fixture-other',message:'已创建新会话。'},{chatId:'fixture-chat',message:'已创建新会话。'}]);assert.deepEqual(row.sends.map(send=>send.status),[200,200,503,200]);
   assert.equal(row.duplicateObserved,true);assert.equal(row.noPollAfterDisable,true);assert.equal(row.reloadSucceeded,true);assert.equal(row.provider.length,2);assert.match(row.stderr,/whatsapp: send HTTP 503: fixture send denied/);
  }else if(row.name==='whatsapp-ready404'){assert.equal(row.sends.length,1);assert.equal(row.provider.length,0);assert.equal(row.stderr,'');}
  else if(row.name==='whatsapp-cron'){assert.equal(row.sends.length,3);assert.equal(row.cronSend.body.chatId,'cron-chat');assert.equal(row.cronSend.body.message,'fixture answer');assert.equal(row.provider.length,4);assert.equal(row.stderr,'');assert.ok(row.rpc.some(frame=>frame.direction==='send'&&frame.value.method==='cron_create'));assert.ok(row.rpc.some(frame=>frame.direction==='send'&&frame.value.method==='cron_run_now'));}
  else{assert.equal(row.sends.length,1);assert.equal(row.provider.length,2);assert.equal(row.providerOpenAfterDisable,side==='ts');assert.equal(row.postsAfterDisable,1);assert.equal(row.postsAfterRelease,1);assert.equal(row.stderr,'');}
 }
}
function projection(rows){return [...rows].sort((a,b)=>a.name.localeCompare(b.name)).map(row=>({name:row.name,sends:row.sends.map(({body,status})=>({body,status})),provider:row.provider,bridgeLifecycle:row.bridgeLife.map(({event,signal})=>({event,signal:signal??null}))}));}
export function compare(ts,rust){verify(ts,'ts');verify(rust,'rust');assert.deepEqual(projection(rust),projection(ts));}
const args=process.argv.slice(2);
if(args[0]==='--self-test'){
 const ts=JSON.parse(fs.readFileSync(args[1],'utf8'));verify(ts,'ts');const candidate=structuredClone(ts);for(const row of candidate){row.side='rust';if(row.name==='whatsapp-active')row.providerOpenAfterDisable=false;}compare(ts,candidate);
 const mutations={wrong_reply:rows=>rows.find(row=>row.name==='whatsapp-basic').sends[1].body.message='wrong',missing_case:rows=>rows.pop(),duplicate_case:rows=>rows[1]=structuredClone(rows[0]),signal_exit:rows=>rows[0].signal='SIGTERM',forced_cleanup:rows=>rows[1].forceKilled=true,late_owned_turn:rows=>rows.find(row=>row.name==='whatsapp-active').providerOpenAfterDisable=true,wrong_cron_chat:rows=>rows.find(row=>row.name==='whatsapp-cron').cronSend.body.chatId='wrong'};
 const detected=[];for(const [name,mutate]of Object.entries(mutations)){const bad=structuredClone(candidate);mutate(bad);assert.throws(()=>compare(ts,bad),undefined,name);detected.push(name);}console.log(JSON.stringify({scope:'comparator self-test using captured TS and synthetic candidate; no native product pass claimed',negativeControls:detected}));
}else if(args.length===2){compare(JSON.parse(fs.readFileSync(args[0],'utf8')),JSON.parse(fs.readFileSync(args[1],'utf8')));console.log('5 WhatsApp formal scenarios match; native owned-cancel expectation checked separately');}
else throw new Error('usage: compare.mjs TS-results.json Rust-results.json | --self-test TS-results.json');
