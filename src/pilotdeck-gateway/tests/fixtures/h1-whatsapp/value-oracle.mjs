#!/usr/bin/env node
// Fixed-source component characterization, not a formal-server success claim.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {fileURLToPath,pathToFileURL} from 'node:url';
const baseline=process.env.PDRS_BASELINE??'/Users/da/ws/pilotdeck-rs/resources/PilotDeck';
const out=path.resolve(process.argv[2]);fs.mkdirSync(out);
assert.equal(process.version,'v22.22.0');
const revision=spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim();
assert.equal(revision,'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
assert.equal(spawnSync('git',['-C',baseline,'diff','--quiet','HEAD','--','src','packages']).status,0);
const sha=file=>createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const files=['src/adapters/channel/whatsapp/WhatsAppChannel.ts','src/adapters/channel/whatsapp/WhatsAppSessionMapper.ts','src/adapters/channel/whatsapp/whatsapp-render.ts'];
const {default:ts}=await import(pathToFileURL(path.join(baseline,'node_modules/typescript/lib/typescript.js')));
const cfg=JSON.parse(fs.readFileSync(path.join(baseline,'tsconfig.json'),'utf8'));
const compiler=ts.convertCompilerOptionsFromJson(cfg.compilerOptions,baseline).options;
const identities=[];
for(const source of files){const built='dist/'+source.replace(/\.ts$/,'.js');const emit=ts.transpileModule(fs.readFileSync(path.join(baseline,source),'utf8'),{fileName:path.join(baseline,source),compilerOptions:{...compiler,module:ts.ModuleKind.ESNext}}).outputText;assert.equal(emit,fs.readFileSync(path.join(baseline,built),'utf8'));identities.push({source,sourceSha256:sha(path.join(baseline,source)),built,distSha256:sha(path.join(baseline,built))});}
for(const key of Object.keys(process.env))delete process.env[key];
Object.assign(process.env,{HOME:path.join(out,'home'),PILOT_HOME:path.join(out,'pilot'),PATH:''});fs.mkdirSync(process.env.HOME);fs.mkdirSync(process.env.PILOT_HOME);
const {WhatsAppChannel}=await import(pathToFileURL(path.join(baseline,'dist/src/adapters/channel/whatsapp/WhatsAppChannel.js')));
const {WhatsAppSessionMapper}=await import(pathToFileURL(path.join(baseline,'dist/src/adapters/channel/whatsapp/WhatsAppSessionMapper.js')));
const {renderWhatsAppEvent}=await import(pathToFileURL(path.join(baseline,'dist/src/adapters/channel/whatsapp/whatsapp-render.js')));
const adapter=new WhatsAppChannel();
const inputs=[
 null,42,'text',[],{},
 [{id:' m ',chatId:' c ',text:' x '}],
 {data:[{messageId:'m',from:'c',content:'/new'}]},
 {messages:[{key:'m',chat_id:'c',body:'text'}],data:[{id:'ignored',chatId:'c',text:'ignored'}]},
 {messages:[],data:[{id:'ignored',chatId:'c',text:'ignored'}]},
 {messages:42,data:[{id:'m',chatId:'c',text:'fallback'}]},
 [null,false,42,'x',[],{}, {id:'',key:'not-a-fallback',chatId:'c',text:'x'}, {id:'m',chatId:'',from:'not-a-fallback',text:'x'}],
 [{id:null,key:0,chatId:null,from:false,text:null,body:0}],
 [{id:{},chatId:[null,2],text:{nested:'x'}},{id:1,chatId:true,text:[null,3,false]}],
 [{id:'m',chatId:'c',text:'',body:'not-a-fallback'},{id:'n',chatId:'c',text:false}],
 [{id:'\ufeffm\ufeff',chatId:'\ud800',text:'\u2028hello\u2029'}],
 [{id:{toString:null},chatId:'c',text:'x'}],
 [{id:'m',chatId:'c',text:[{toString:'not callable'}]}],
];
const normalizations=inputs.map(input=>{try{return{input,output:adapter.normalizeMessages(input)};}catch(error){return{input,error:String(error)};}});
const mapper=new WhatsAppSessionMapper(undefined,()=> '12345678-1234-4123-8123-123456789012');
const mapped=[['a',' first '],['a',' /new '],['a',' next '],['a','/new with text'],['b','/new\ttext'],['\ud800','\ufeff/new \ud801\ufeff']].map(([chatId,text])=>({input:{chatId,text},output:mapper.resolve({chatId,text})}));
const events=[
 {type:'assistant_text_delta',text:'hello'}, {type:'assistant_text_delta',text:null}, {type:'assistant_text_delta',text:7}, {type:'assistant_text_delta'},
 {type:'assistant_thinking_delta',text:'hidden'}, {type:'tool_call_started'}, {type:'tool_call_finished',ok:true},
 {type:'tool_call_finished',ok:false,toolName:'read',toolCallId:'id'}, {type:'tool_call_finished',ok:false,toolName:null,toolCallId:'id'}, {type:'tool_call_finished',ok:false},
 {type:'error',message:'broken'}, {type:'error',message:null}, {type:'error'}, {type:'completed'},
 {type:'elicitation_request',questions:[{header:'Title',question:'Question?',options:[{label:'one'},{label:'two'}]}]},
];
const rendered=events.map(event=>{const fragment=renderWhatsAppEvent(event);return {event,fragment:fragment===undefined?{undefined:true}:fragment,accumulated:fragment==null?'':String(fragment)};});
const constructors=[{}, {bridgePath:null,bridgeUrl:null},{bridgePath:'  fixture.mjs  ',bridgeUrl:'http://localhost:1///'},{bridgePath:''},{bridgePath:0},{bridgePath:[]},{bridgeUrl:42},{bridgeUrl:{}}].map(options=>{try{const instance=new WhatsAppChannel(options);return {options,path:instance.bridgePath,url:instance.bridgeUrl};}catch(error){return {options,error:String(error)};}});
const oracle={normalizations,mapped,snapshot:mapper.snapshot(),rendered,constructors};
fs.writeFileSync(path.join(out,'oracle.json'),JSON.stringify(oracle,null,2)+'\n');
fs.writeFileSync(path.join(out,'identity.json'),JSON.stringify({layer:'real TS components; no platform SDK or formal server claim',node:process.version,revision,driverSha256:sha(fileURLToPath(import.meta.url)),identities},null,2)+'\n');
console.log(JSON.stringify({normalizations:normalizations.length,mapped:mapped.length,rendered:rendered.length,constructors:constructors.length,oracleSha256:sha(path.join(out,'oracle.json'))}));
