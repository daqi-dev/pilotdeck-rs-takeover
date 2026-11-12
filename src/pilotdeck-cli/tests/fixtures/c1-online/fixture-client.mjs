// Deliberately small driver self-test process, never a product substitute.
import fs from 'node:fs';
import { createInterface } from 'node:readline/promises';
const {url,token,mode,argv}=JSON.parse(process.env.C1_SELFTEST);
if(mode==='bad-exit')process.exit(7);
await fetch(`${url}/health`);
const ws=new WebSocket(url.replace('http:','ws:')+'/ws');
await new Promise((resolve,reject)=>{ws.onopen=resolve;ws.onerror=reject;});
ws.send(JSON.stringify({type:'hello',protocolVersion:'1.2',clientName:'cli',clientVersion:'0.1.0',token}));
const hello=await new Promise(resolve=>{ws.onmessage=e=>{if(JSON.parse(e.data).type==='hello_ok')resolve(true);};ws.onclose=()=>resolve(false);});
if(!hello){
 process.stderr.write('[pilotdeck] gateway server unavailable; using in-process mode.\n');
 const response=await fetch(`${url}/v1/chat/completions`,{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:'deterministic',stream:true,messages:[{role:'user',content:argv.join(' ').trim()}]})});
 let pending='';for await(const part of response.body){pending+=Buffer.from(part).toString();const frames=pending.split('\n\n');pending=frames.pop();for(const frame of frames){if(!frame.startsWith('data: ')||frame==='data: [DONE]')continue;const event=JSON.parse(frame.slice(6));const text=event.choices?.[0]?.delta?.content;if(text)process.stdout.write(text);}}
}else{
 let sequence=0;
 const submit=message=>new Promise((resolve,reject)=>{
  const id=`00000000-0000-4000-8000-${String(++sequence).padStart(12,'0')}`;
  ws.onclose=()=>reject(Error('Gateway WebSocket closed.'));
  ws.onmessage=e=>{const f=JSON.parse(e.data);if(f.type==='event'&&f.id===id){if(f.final){if(mode==='bad-final')process.stdout.write(f.event.text);resolve();}else if(f.event.type==='assistant_text_delta')process.stdout.write(f.event.text);}};
  ws.send(JSON.stringify({type:'request',id,method:'submit_turn',params:{sessionKey:`cli:project=${process.cwd()}:default`,channelKey:'cli',message,canPrompt:false}}));
 });
 try{
  const prompt=argv.join(' ').trim();
  if(prompt)await submit(prompt);else for await(const line of createInterface({input:process.stdin,terminal:false})){if(line.trim()){await submit(line.trim());process.stdout.write('\n');}}
 }catch(e){process.stderr.write(e.message+'\n');process.exitCode=1;}
 if(mode!=='retain')ws.close();
}
if(mode==='retain')setInterval(()=>{},1000);
