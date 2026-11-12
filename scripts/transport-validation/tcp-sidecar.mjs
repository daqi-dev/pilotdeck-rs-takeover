#!/usr/bin/env node
// Formal sidecar CLI over TCP, with a deterministic in-process model host.
// No provider account, user HOME, or JS factory replacement is involved.
import net from 'node:net';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import assert from 'node:assert/strict';
import { artifact, assertArtifactsUnchanged, validationIdentity } from '../staffdeck-validation/identity.mjs';

const args=process.argv.slice(2), arg=key=>args[args.indexOf(key)+1];
if(!args.includes('--baseline')||!args.includes('--out'))throw Error('--baseline CHECKOUT --out NEW_DIR [--rust SIDECAR_BINARY] [--self]');
const baseline=path.resolve(arg('--baseline')), out=path.resolve(arg('--out')), rust=args.includes('--rust')?path.resolve(arg('--rust')):null;
assert(!fs.existsSync(out),'output directory must be new');fs.mkdirSync(out,{recursive:true});
const root=path.join(out,'fixture'), home=path.join(root,'home');
const delay=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const tsEntry=path.join(baseline,'dist/src/cli/pilotdeck-agent-loop-sidecar.js');
const sidecarArtifact=artifact(tsEntry);
const identity=validationIdentity({baseline,rust,scenario:new URL(import.meta.url)});
const negativeControl=args.includes('--mutate-answer');
const orphanedCall=args.includes('--orphaned-call');
const activeRepeat=args.includes('--active-repeat'), completedRepeat=args.includes('--completed-repeat');
assert([negativeControl,orphanedCall,activeRepeat,completedRepeat].filter(Boolean).length<=1,'select one scenario variant');
const manifest={schema:2,scenario:orphanedCall?'tcp-sidecar-nonresumable-eof':activeRepeat?'tcp-sidecar-active-repeat':completedRepeat?'tcp-sidecar-completed-repeat':'tcp-sidecar-default',negativeControl,...(orphanedCall?{acceptedDifference:'C-TCP-NONRESUMABLE-EOF-001'}:completedRepeat?{acceptedDifference:'C-TCP-COMPLETED-REPEAT-001'}:{}),...identity.fields,tsSidecar:sidecarArtifact,node:process.version,platform:`${process.platform}-${process.arch}`};

async function run(side,command,argv){
  fs.mkdirSync(home,{recursive:true});
  const reserve=net.createServer();reserve.listen(0,'127.0.0.1');await once(reserve,'listening');
  const port=reserve.address().port;await new Promise(resolve=>reserve.close(resolve));
  const child=spawn(command,argv,{cwd:root,env:{PATH:process.env.PATH,HOME:home,PILOT_HOME:path.join(home,'.pilotdeck'),PILOTDECK_AGENT_LOOP_TCP_HOST:'127.0.0.1',PILOTDECK_AGENT_LOOP_TCP_PORT:String(port)},stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='',ended;child.stdout.on('data',data=>stdout+=data);child.stderr.on('data',data=>stderr+=data);
  const done=new Promise(resolve=>child.once('exit',(code,signal)=>{ended={code,signal};resolve(ended);}));
  const peers=[],raw=[],calls=[],events=[];
  async function connect(){
    for(let n=0;n<200;n++){
      if(ended)throw Error(`${side} exited before listening: ${JSON.stringify(ended)} ${stderr}`);
      const socket=net.createConnection({host:'127.0.0.1',port});
      try{await once(socket,'connect');
        let buffer='',queue=[],waiter,closed=false;
        socket.setEncoding('utf8');socket.on('error',error=>{closed=true;waiter?.reject(error);waiter=null;});
        socket.on('close',()=>{closed=true;waiter?.reject(Error('socket closed'));waiter=null;});
        socket.on('data',chunk=>{buffer+=chunk;let end;while((end=buffer.indexOf('\n'))>=0){const message=JSON.parse(buffer.slice(0,end));buffer=buffer.slice(end+1);raw.push(message);if(waiter){const current=waiter;waiter=null;current.resolve(message);}else queue.push(message);}});
        peers.push(socket);return{send:message=>socket.write(JSON.stringify(message)+'\n'),read:()=>queue.length?Promise.resolve(queue.shift()):closed?Promise.reject(Error('socket closed')):new Promise((resolve,reject)=>{const timer=setTimeout(()=>{waiter=null;reject(Error('module frame timeout'));},5000);waiter={resolve:message=>{clearTimeout(timer);resolve(message);},reject:error=>{clearTimeout(timer);reject(error);}};})};
      }catch(error){socket.destroy();if(n===199)throw error;await delay(20);}
    }
  }
  async function request(peer,method,fields={}){peer.send({kind:'request',messageId:method,method,payload:{},...fields});const response=await peer.read();assert.equal(response.inReplyTo,method);return response;}
  function answerModel(peer,message,count){
    assert.equal(message.method,'module_call');assert.equal(message.module,'model');
    const payload=message.payload;calls.push(payload);
    let response;
    if(payload.operation==='prepare')response={prepared:{request:payload.request,provider:'host-provider',model:'host-model'}};
    else if(payload.operation==='stream')response={events:[{type:'text_delta',text:negativeControl?'Wrong answer injected by negative control.':'Native TCP answer.'},{type:'message_end',finishReason:'stop'}]};
    else throw Error(`unexpected model operation ${payload.operation}`);
    // This durable counter records actual provider dispatch, independently of
    // any execute acceptance or cached terminal assertions.
    if(payload.operation==='stream')fs.appendFileSync(path.join(out,`${side}-provider-dispatch.jsonl`),JSON.stringify({requestId:message.requestId,operationId:message.operationId})+'\n');
    peer.send({kind:'response',messageId:`host-${count}`,inReplyTo:message.messageId,requestId:message.requestId,ok:true,payload:response});
  }
  try{
    const first=await connect(), one=await request(first,'hello'), caps=await request(first,'capabilities');
    const second=await connect(), two=await request(second,'hello');
    const resume=await request(second,'resume',{streamId:'missing',previousBinding:{moduleInstanceId:one.moduleInstanceId,connectionGeneration:one.connectionGeneration},lastAppliedSequence:-1});
    const ack=await request(second,'ack',{streamId:'missing',lastAppliedSequence:0});
    const execute={kind:'request',messageId:'execute',method:'execute',runId:'tcp-run',operationId:'tcp-operation',requestId:'tcp-request',sessionId:'tcp-session',turnId:'tcp-turn',payload:{agent:{provider:'mock',model:'test'},hostModules:{model:{methods:['prepare','stream']}},messages:[{role:'user',content:[{type:'text',text:'Answer through the TCP sidecar.'}]}]}};
    first.send(execute);
    const accepted=await first.read();assert.equal(accepted.inReplyTo,'execute');assert.equal(accepted.ok,true);assert.equal(typeof accepted.streamId,'string');
    let activeDuplicate;
    if(activeRepeat){
      second.send(execute);activeDuplicate=await second.read();
      assert.equal(activeDuplicate.inReplyTo,'execute');assert.equal(activeDuplicate.ok,false);assert.equal(activeDuplicate.code,'OPERATION_ALREADY_ACTIVE');
      assert.equal(activeDuplicate.streamId,undefined);
    }
    if(orphanedCall){
      let callback;
      for(let n=0;n<20;n++){const message=await first.read();if(message.kind==='request'){callback=message;break;}assert.equal(message.kind,'event');assert.notEqual(message.final,true);}
      assert(callback,'real default factory must reach a host callback');assert.equal(callback.method,'module_call');assert.equal(callback.payload.operation,'prepare');
      peers[0].destroy(); // The only peer authorized to answer this callback disappears.
      const statuses=[];
      for(let n=0;n<3;n++){await delay(150);const status=await request(second,'status',{requestId:'tcp-request'});assert.equal(status.ok,true);statuses.push(status.payload);}
      const native=side==='rust';
      for(const status of statuses){assert.equal(status.state,native?'resolving':'running');if(native)assert.equal(status.outcome,'result_unknown');}
      second.send(execute);const duplicate=await second.read();
      assert.equal(duplicate.inReplyTo,'execute');assert.equal(duplicate.ok,false);
      assert.equal(duplicate.code,native?'RESULT_UNKNOWN':'OPERATION_ALREADY_ACTIVE');
      if(native){assert.equal(duplicate.final,true);assert.equal(duplicate.outcome,'result_unknown');}
      assert.equal(raw.filter(message=>message.kind==='request'&&message.method==='module_call').length,1,'a retry cannot start another model callback');
      fs.writeFileSync(path.join(out,`${side}.json`),JSON.stringify({acceptedDifference:manifest.acceptedDifference,statuses,duplicate,raw,assertions:{onlyOneCallback:true,explicitReconciliation:true,defaultResumeDisabled:true}},null,2));
      // Different states are required by the named hang fix, not ignored by
      // normalization. Each side's precise state and duplicate result were
      // asserted above and remain in its raw artifact.
      return {acceptedDifference:manifest.acceptedDifference,onlyOneCallback:true,explicitReconciliation:true,capabilities:caps.payload};
    }
    let terminal;
    for(let count=0;count<100;count++){
      const message=await first.read();
      if(message.kind==='request'){
        answerModel(first,message,count);
      }else{
        assert.equal(message.kind,'event');assert.equal(message.streamId,accepted.streamId);assert.equal(message.sequence,events.length);events.push(message);
        if(message.final){terminal=message;break;}
      }
    }
    assert(terminal,'exactly one terminal must arrive');assert.equal(terminal.outcome,'completed');
    assert.deepEqual(calls.map(call=>call.operation),['prepare','stream']);
    assert.equal(calls[0].preparationId,calls[1].preparationId);
    assert.equal(calls[1].request.provider,'host-provider');assert.equal(calls[1].request.model,'host-model');
    assert.deepEqual(calls[1].request.messages,calls[0].request.messages);
    assert.equal(terminal.payload.result.type,'success');assert.equal(terminal.payload.result.stopReason,'completed');
    assert(terminal.payload.result.finalMessage.content.some(block=>block.type==='text'&&block.text==='Native TCP answer.'));
    const status=await request(second,'status',{requestId:'tcp-request'});
    assert.equal(status.ok,true);assert.equal(status.payload.state,'completed');
    if(completedRepeat){
      second.send(execute);const duplicate=await second.read();
      let repeatedTerminal, repeatedEvents=[];
      if(side==='rust'){
        assert.deepEqual(duplicate,terminal,'the identical completed request reuses its exact terminal');
        repeatedTerminal=duplicate;
      }else{
        assert.equal(duplicate.inReplyTo,'execute');assert.equal(duplicate.ok,true);assert.notEqual(duplicate.streamId,accepted.streamId);
        for(let count=0;count<100;count++){
          const message=await second.read();
          if(message.kind==='request')answerModel(second,message,`repeat-${count}`);
          else{assert.equal(message.kind,'event');assert.equal(message.streamId,duplicate.streamId);assert.equal(message.sequence,repeatedEvents.length);repeatedEvents.push(message);if(message.final){repeatedTerminal=message;break;}}
        }
      }
      assert(repeatedTerminal);assert.equal(repeatedTerminal.outcome,'completed');
      assert.equal(repeatedTerminal.payload.result.finalMessage.content[0].text,'Native TCP answer.');
      const dispatches=fs.readFileSync(path.join(out,`${side}-provider-dispatch.jsonl`),'utf8').trim().split('\n');
      assert.equal(dispatches.length,side==='rust'?1:2);assert.equal(calls.length,side==='rust'?2:4);
      fs.writeFileSync(path.join(out,`${side}.json`),JSON.stringify({acceptedDifference:manifest.acceptedDifference,providerDispatches:dispatches.length,duplicate,repeatedEvents,raw,assertions:{completedReplyPreserved:true,dispatchCountMatchesDeclaredDifference:true}},null,2));
      return{acceptedDifference:manifest.acceptedDifference,completedReplyPreserved:true,dispatchCountMatchesDeclaredDifference:true};
    }
    const observation={sameInstance:one.moduleInstanceId===two.moduleInstanceId,newGeneration:one.connectionGeneration!==two.connectionGeneration,capabilities:caps.payload,resume:{ok:resume.ok,code:resume.code},ack:{ok:ack.ok,code:ack.code},modelCalls:calls.map(call=>({operation:call.operation,provider:call.request.provider,model:call.request.model,messages:call.request.messages})),events:events.map(event=>({kind:event.payload?.type??null,final:event.final,outcome:event.outcome??null})),result:{type:terminal.payload.result.type,stopReason:terminal.payload.result.stopReason,text:terminal.payload.result.finalMessage.content.filter(block=>block.type==='text').map(block=>block.text),turns:terminal.payload.result.turns},status:status.payload.state};
    assert.equal(observation.sameInstance,true);assert.equal(observation.newGeneration,true);assert.equal(resume.code,'RESUME_UNSUPPORTED');assert.equal(ack.code,'ACK_UNSUPPORTED');
    if(activeRepeat){observation.activeRepeat={ok:activeDuplicate.ok,code:activeDuplicate.code,providerDispatches:fs.readFileSync(path.join(out,`${side}-provider-dispatch.jsonl`),'utf8').trim().split('\n').length};assert.equal(observation.activeRepeat.providerDispatches,1);}
    fs.writeFileSync(path.join(out,`${side}.json`),JSON.stringify({observation,raw},null,2));return observation;
  }finally{
    for(const peer of peers)peer.destroy();
    const wasRunning=!ended;if(wasRunning)child.kill('SIGTERM');
    let exit=await Promise.race([done,delay(5000).then(()=>null)]);
    if(!exit){child.kill('SIGKILL');exit=await done;}
    fs.writeFileSync(path.join(out,`${side}-process.json`),JSON.stringify({pid:child.pid,port,exit,stdout,stderr,raw},null,2));
    assert(wasRunning,`${side} exited unexpectedly`);assert.equal(exit.signal,'SIGTERM',`${side} listener did not remain live until owned shutdown`);assert.equal(stdout,'','TCP listener must not put protocol data on stdout');
  }
}
let failure;
try{const expected=await run('ts',process.execPath,[tsEntry]);if(args.includes('--self'))assert.deepEqual(await run('ts-self',process.execPath,[tsEntry]),expected);if(rust)assert.deepEqual(await run('rust',rust,[]),expected);manifest.passed=true;}
catch(error){manifest.passed=false;manifest.error=String(error.stack??error);failure=error;}
finally{try{assertArtifactsUnchanged([sidecarArtifact]);Object.assign(manifest,identity.finish());}catch(error){manifest.passed=false;manifest.identityError=String(error.stack??error);failure??=error;}fs.writeFileSync(path.join(out,'manifest.json'),JSON.stringify(manifest,null,2));}
if(failure)throw failure;console.log(JSON.stringify(manifest,null,2));
