import {pathToFileURL} from 'node:url';
import {writeFileSync} from 'node:fs';
const [baseline,output]=process.argv.slice(2);
const {SessionInteractionBundle}=await import(pathToFileURL(`${baseline}/dist/src/cli/SessionInteractionBundle.js`));
const requests=[];const disposers=[];let configHandler;let unregistered=0;
const bundle=new SessionInteractionBundle({
 sessionKey:'session-fixture',projectRoot:'/fixture/project',hookCwd:'/fixture/project',
 profile:{permissionProvider:'disabled',questionProvider:'disabled',policyMode:'default'},canPrompt:false,permissionRules:[],hookSettings:{},shell:{},
 sdkHooks:{url:'http://127.0.0.1/hook?retained=1',headers:{'x-fixture':'value'},events:{ConfigChange:[{}],FileChanged:[{matcher:'a.txt'}]}},
 networkFetch:async(input,init)=>{requests.push({url:String(input),headers:init.headers,body:JSON.parse(init.body)});return new Response('{}',{status:200});},
 gateway:{registerConfigChangeHandler(handler){configHandler=handler;return()=>{configHandler=undefined;unregistered++;};},registerSessionHookHandler(){return()=>{};}}
});
bundle.attach({own(dispose){disposers.push(dispose);}});
configHandler({changedPaths:['agent.model'],changeClasses:['runtime']});
await new Promise(r=>setTimeout(r,20));
await bundle.fileUpdateNotifier.didSave({relativePath:'a.txt',absolutePath:'/fixture/project/a.txt',root:'/fixture/project',previousContent:null,content:'hello'});
await bundle.fileUpdateNotifier.didSave({relativePath:'a.txt',absolutePath:'/fixture/project/a.txt',root:'/fixture/project',previousContent:'hello',content:'changed'});
await bundle.fileUpdateNotifier.didSave({relativePath:'no.md',absolutePath:'/fixture/project/no.md',root:'/fixture/project',previousContent:null,content:'ignored'});
for(const dispose of disposers.reverse())await dispose();
if(requests.length!==3 || requests.map(r=>r.body.hookEventName).join(',')!=='ConfigChange,FileChanged,FileChanged' || unregistered!==1)throw new Error('SDK event oracle did not execute all required assertions');
writeFileSync(output,JSON.stringify({requests,unregistered},null,2)+'\n');
