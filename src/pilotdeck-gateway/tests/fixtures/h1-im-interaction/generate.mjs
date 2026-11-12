// Run with Node --import <fixed-baseline>/node_modules/tsx/dist/loader.mjs.
// Imports unchanged source. The callbacks below observe actual pending buses.
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
const root = process.argv[2];
const output = process.argv[3];
if (!root || !output || !process.env.HOME || !process.env.PILOT_HOME) throw Error('isolated HOME/PILOT_HOME and baseline/output required');
const files = [
  'src/adapters/channel/protocol/ImElicitationHelper.ts',
  'src/adapters/channel/protocol/ImPermissionHelper.ts',
  'src/gateway/client/GatewayInteractionCoordinator.ts',
  'src/gateway/permission/GatewayPermissionBus.ts',
  'src/gateway/elicitation/GatewayElicitationBus.ts',
];
const [{ ImElicitationHelper }, { ImPermissionHelper }, { GatewayInteractionCoordinator }] = await Promise.all(files.slice(0,3).map(file => import(pathToFileURL(resolve(root,file)).href)));
const permission = (id, toolName='bash') => ({type:'permission_request',requestId:id,toolName,payload:{command:'echo hi'}});
const question = (id) => ({type:'elicitation_request',requestId:id,questions:[{question:'选择\ud800',header:'标题\udc00',options:[{label:'一\ud800',description:'说明'},{label:'二🙂',description:''}]}]});
const capture = (kind,event,extra={}) => ({op:'capture',kind,event,...extra});
const register = (kind,id,extra={}) => ({op:'register',kind,id,...extra});
const answer = (kind,text,extra={}) => ({op:'answer',kind,text,...extra});
const cases = [
 {id:'question_utf16',steps:[register('question','q\ud800'),capture('question',question('q\ud800')),answer('question','1')]},
 {id:'question_cancel',steps:[register('question','q'),capture('question',question('q')),answer('question','0')]},
 {id:'question_missing_id',steps:[register('question','undefined'),capture('question',question(undefined)),answer('question','1')]},
 ...['0','1','2'].map(text=>({id:'permission_'+text,steps:[register('permission','p'),capture('permission',permission('p')),{op:'confirm',delivered:true,id:'p'},answer('permission',text)]})),
 {id:'permission_missing_id',steps:[register('permission','undefined'),capture('permission',permission(undefined)),{op:'confirm',delivered:true},answer('permission','1')]},
 {id:'wrong_owner',steps:[register('permission','p',{session:'other'}),capture('permission',permission('p')),{op:'confirm',delivered:true,id:'p'},answer('permission','2')]},
 {id:'cancel_pending_turn',steps:[register('permission','p'),register('question','q'),capture('permission',permission('p')),capture('question',question('q')),{op:'confirm',delivered:true,id:'p'},{op:'cancel',reason:'channel_stopped'},answer('permission','1'),answer('question','1')]},
 {id:'helper_isolation',steps:[register('question','q1'),register('question','q2',{session:'other'}),capture('question',question('q1')),capture('question',question('q2'),{helper:1,session:'other'}),answer('question','1'),answer('question','0',{helper:1})]},
 {id:'permission_queue_retry',steps:[register('permission','p1'),register('permission','p2'),capture('permission',permission('p1')),capture('permission',permission('p2','shell\ud800')),{op:'confirm',delivered:true,id:'p1'},answer('permission','2'),{op:'next'},{op:'confirm_next',delivered:false},{op:'release'},answer('permission','1'),{op:'next'},{op:'confirm_next',delivered:true},answer('permission','0')]},
];
for (const item of cases) {
 const observed=[];
 const coordinator = new GatewayInteractionCoordinator({onElicitationDelivered:(session,id)=>observed.push({event:'delivered',session,id})});
 const helpers = Array.from({length:2},()=>({q:new ImElicitationHelper(),p:new ImPermissionHelper(),token:undefined}));
 const gateway = {respondElicitation:async input=>coordinator.respondElicitation(input), permissionDecide:async input=>coordinator.decidePermission(input)};
 const trace=[];
 for (const step of item.steps) {
  const session=step.session??'session'; const chat='chat\ud800'; const helper=helpers[step.helper??0]; let result;
  if (step.op==='register') {
   const bus=step.kind==='permission'?coordinator.getPermissionBus():coordinator.getElicitationBus();
   bus.register(session,{requestId:step.id,toolCallId:'tc',toolName:'fixture',resolve:value=>observed.push({event:'resolve',session,id:step.id,value}),reject:error=>observed.push({event:'reject',session,id:step.id,message:error.message})});
  } else if (step.op==='capture') result=(step.kind==='permission'?helper.p:helper.q).capture(chat,session,step.event);
  else if (step.op==='confirm') helper.p.confirmInitialPrompt(chat,step.delivered,step.id);
  else if (step.op==='answer') {
   if(step.kind==='question') result=await helper.q.answer(chat,step.text,gateway);
   else {result=await helper.p.answerWithState(chat,step.text,gateway);if(result?.answerToken!==undefined)helper.token=result.answerToken;}
  } else if(step.op==='cancel') coordinator.rejectPendingTurn(session,step.reason);
  else if(step.op==='next') result={text:helper.p.takeNextPrompt(chat,helper.token),id:helper.p.getPromptRequestId(chat,helper.token)};
  else if(step.op==='confirm_next') helper.p.confirmNextPrompt(chat,step.delivered,helper.p.getPromptRequestId(chat,helper.token),helper.token);
  else if(step.op==='release') helper.p.releaseAnswer(chat,helper.token);
  trace.push({op:step.op,result,helperQuestion:helper.q.hasPending(chat),helperPermission:helper.p.hasPending(chat),pendingQuestion:coordinator.getElicitationBus().pendingCount(session),pendingPermission:coordinator.getPermissionBus().pendingCount(session),observed:structuredClone(observed)});
 }
 item.trace=trace;
}
const hashes=Object.fromEntries(await Promise.all(files.map(async path=>[path,createHash('sha256').update(await readFile(resolve(root,path))).digest('hex')])));
await writeFile(output,JSON.stringify({baseline:'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85',hashes,cases},null,2)+'\n');
console.log(JSON.stringify({cases:cases.length,output}));
