// Controlled fault at the real baseline's second restore append. This preload
// modifies no reference file and only matches its own isolated control path.
import fs from 'node:fs';
import {syncBuiltinESMExports} from 'node:module';
import path from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
const root=process.env.PDRS_RESTORE_FAULT_DIR;
if(!root)throw new Error('S1 controlled preload needs an isolated fault directory');
const original=fs.promises.appendFile;
const consumed=new Set();
fs.promises.appendFile=async function(file,data,...options){
 const name=String(file);
 if(name.startsWith(process.env.PILOT_HOME+'/')&&name.endsWith('.restore.tmp')){
  const entry=JSON.parse(String(data));
  let control={};try{control=JSON.parse(fs.readFileSync(path.join(root,'control.json'),'utf8'));}catch(error){if(error.code!=='ENOENT')throw error;}
  if(entry.type==='accepted_input'&&control.id&&!consumed.has(control.id)){
   consumed.add(control.id);
   const previous=fs.readFileSync(name,'utf8');
   fs.writeFileSync(path.join(root,`${control.id}.json`),JSON.stringify({phase:'before-accepted-input',path:name,previous,entry},null,2)+'\n',{flag:'wx'});
   if(control.mode==='fault')throw Object.assign(new Error(`EIO: s1 controlled append failure, write '${name}'`),{code:'EIO',errno:-5,syscall:'write',path:name});
   if(control.mode==='hold'){
    let released=false;
    for(let i=0;i<3000;i++){
     try{if(JSON.parse(fs.readFileSync(path.join(root,'release.json'),'utf8')).id===control.id){released=true;break;}}
     catch(error){if(error.code!=='ENOENT')throw error;}
     await delay(10);
    }
    if(!released)throw new Error('S1 controlled hold exceeded 30s');
   }
  }
 }
 return original.call(this,file,data,...options);
};
syncBuiltinESMExports();
