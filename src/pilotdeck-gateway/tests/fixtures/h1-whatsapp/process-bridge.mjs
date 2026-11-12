import fs from 'node:fs';
import path from 'node:path';
import {spawn} from 'node:child_process';
const dir=process.env.TMPDIR;
fs.writeFileSync(path.join(dir,'bridge-pid'),String(process.pid));
if(process.env.PDRS_BRIDGE_MODE==='stdout')await new Promise(resolve=>process.stdout.write(Buffer.alloc(4*1024*1024,120),resolve));
if(process.env.PDRS_BRIDGE_MODE==='descendant'){
 const worker=spawn(process.execPath,['-e',"require('node:fs').writeFileSync(process.env.TMPDIR+'/descendant-pid',String(process.pid));process.on('SIGTERM',()=>{});setInterval(()=>{},1000)"],{stdio:['ignore','inherit','inherit']});
 fs.writeFileSync(path.join(dir,'spawned-descendant-pid'),String(worker.pid));
}
if(process.env.PDRS_BRIDGE_MODE==='ignore-term')process.on('SIGTERM',()=>{});
process.stderr.write('fixture stderr\n');
fs.writeFileSync(path.join(dir,'ready'),'ready');
setInterval(()=>{},1000);
