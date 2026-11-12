import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
const root=process.env.TMPDIR;
const log=(name,value)=>fs.appendFileSync(path.join(root,name),JSON.stringify(value)+'\n');
log('bridge-life.jsonl',{event:'start',at:Date.now(),pid:process.pid,ppid:process.ppid,argv:process.argv,execPath:process.execPath,port:process.env.BRIDGE_PORT,home:process.env.HOME,pilot:process.env.PILOT_HOME});
let count=0;
const server=http.createServer(async(req,res)=>{
 const parts=[];for await(const p of req)parts.push(p);
 const body=Buffer.concat(parts).toString();
 log('bridge-wire.jsonl',{at:Date.now(),method:req.method,path:req.url,contentType:req.headers['content-type']??null,authorization:req.headers.authorization??null,body});
 if(req.method==='GET'&&req.url==='/messages'){
  count++;
  if(count===1&&fs.existsSync(path.join(root,'first-404'))){res.writeHead(404);res.end('fixture ready 404');return;}
  res.setHeader('content-type','application/json');res.end(fs.existsSync(path.join(root,'inbound.json'))?fs.readFileSync(path.join(root,'inbound.json')):'[]');return;
 }
 if(req.method==='POST'&&req.url==='/send'){
  const failed=fs.existsSync(path.join(root,'reject-send'));
  log('bridge-sends.jsonl',{at:Date.now(),body:JSON.parse(body),status:failed?503:200});
  res.writeHead(failed?503:200,{'content-type':'application/json'});res.end(failed?'{"error":"fixture send denied"}':'{"ok":true}');return;
 }
 res.writeHead(404);res.end();
});
server.listen(Number(process.env.BRIDGE_PORT),'127.0.0.1');
process.once('SIGTERM',()=>{log('bridge-life.jsonl',{event:'stop',signal:'SIGTERM',at:Date.now(),pid:process.pid});server.close(()=>process.exit(0));});
