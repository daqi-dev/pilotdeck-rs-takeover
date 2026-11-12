import {pathToFileURL} from 'node:url';
import {writeFileSync, mkdirSync} from 'node:fs';
import {createHash} from 'node:crypto';
const [baseline,out]=process.argv.slice(2);
if (!baseline || !out) throw new Error('baseline and fresh output file required');
const {InputProcessor}=await import(pathToFileURL(`${baseline}/dist/src/context/input/InputProcessor.js`));
const cases=[];
for (const [name,body,input] of [
 ['plain',' body ',{type:'text',text:'/local:greet arg'}],
 ['no-argument','body',{type:'text',text:'/local:greet'}],
 ['empty-body',' ',{type:'text',text:'/local:greet arg'}],
 ['unknown','body',{type:'text',text:'/unknown arg'}],
 ['blocks','body',{type:'blocks',content:[{type:'text',text:'/local:greet arg'}]}],
 ['collision','<<<PILOTDECK_COMMAND_BODY_0>>>',{type:'text',text:'/local:greet arg'}],
 ['unicode','body',{type:'text',text:'/local:greet\ufeff\ud800'}],
]) {
 const processor=new InputProcessor({extension:{listCommands:()=>[{name:'local:greet',content:process.argv.includes('--negative')?'WRONG_PRODUCT_COMMAND_BODY':body}]}});
 const result=processor.process(input);cases.push({name,input,body,result});
}
const bounded=new InputProcessor({extension:{listCommands:()=>[{name:'local:greet',content:'x'.repeat(33000)}]}}).process({type:'text',text:'/local:greet '+'\ud800'.repeat(9000)});
const text=bounded.messages[0].content[0].text;
writeFileSync(out,JSON.stringify({node:process.version,baseline,cases,bounded:{length:text.length,sha256Utf16Le:createHash('sha256').update(Buffer.from(text,'utf16le')).digest('hex'),diagnostics:bounded.diagnostics}},null,2)+'\n',{flag:'wx'});
