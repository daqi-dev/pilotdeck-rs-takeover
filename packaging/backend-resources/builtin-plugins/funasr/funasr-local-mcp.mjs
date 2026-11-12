#!/usr/bin/env node
// Independently authored stdio adapter. No runtime/model download is performed.
import { spawn } from 'node:child_process';
import { realpath, stat, readdir } from 'node:fs/promises';
import { resolve, relative, isAbsolute, join } from 'node:path';
import { fileURLToPath } from 'node:url';
const options = new Map();
for (let i = 2; i < process.argv.length; i += 2) options.set(process.argv[i], process.argv[i + 1]);
const tasks = new Map();
let closing = false;
const send = (id, result) => { if (!closing) process.stdout.write(`${JSON.stringify({jsonrpc:'2.0',id,result})}\n`); };
const error = (text) => ({content:[{type:'text',text}],isError:true});
const tool = {name:'transcribe_audio',description:'Convert an audio file inside the active project to text and timed segments using a separately provisioned local SenseVoice runtime.',inputSchema:{type:'object',additionalProperties:false,required:['audio_path'],properties:{audio_path:{type:'string'},language:{type:'string',enum:['auto'],default:'auto'}}}};
async function executable(directory, depth = 0) {
  if (depth > 8) throw Error('runtime tree exceeds search depth');
  let entries;
  try { entries = await readdir(directory,{withFileTypes:true}); } catch { return undefined; }
  const name = process.platform === 'win32' ? 'llama-funasr-sensevoice.exe' : 'llama-funasr-sensevoice';
  for (const entry of entries) if (entry.isFile() && entry.name === name) return join(directory,name);
  for (const entry of entries) if (entry.isDirectory()) { const found = await executable(join(directory,entry.name),depth+1); if (found) return found; }
}
function time(value) {
  const m = /^(\d{2,}):(\d{2}):(\d{2})[,.](\d{1,3})$/.exec(value);
  if (!m || +m[2] >= 60 || +m[3] >= 60) throw Error('invalid SRT timestamp');
  return +m[1]*3600 + +m[2]*60 + +m[3] + +m[4].padEnd(3,'0')/1000;
}
function segments(text) {
  const rows = [];
  for (const block of text.replaceAll('\r\n','\n').split(/\n\s*\n/)) {
    const lines = block.split('\n').map(s=>s.trim()).filter(Boolean);
    const at = lines.findIndex(s=>s.includes('-->'));
    if (at < 0) continue;
    const match = /^(\S+)\s+-->\s+(\S+)/.exec(lines[at]);
    if (!match) continue;
    const start=time(match[1]),end=time(match[2]),body=lines.slice(at+1).join(' ');
    if (end < start) throw Error('invalid SRT range');
    if (body) rows.push({start,end,text:body});
  }
  return rows;
}
function run(binary, args, signal) {
  return new Promise((done,reject) => {
    if(process.platform==='win32'){reject(Error('FunASR owned process cleanup requires POSIX and Python 3; Windows execution is unavailable.'));return;}
    const supervisor=fileURLToPath(new URL('./supervise.py',import.meta.url));
    const child=spawn('python3',['-u',supervisor,'--',binary,...args],{detached:true,stdio:['pipe','pipe','pipe','pipe']});
    let out='',err='',bytes=0,failure,nativeExit,control='',stopping=false,controlFailed=false;
    const stop=(why)=>{
      if(why) failure ??= Error(why);
      if(stopping)return;
      stopping=true;
      // Only the still-live supervisor signals its own group. Never signal a
      // PID/group here: libuv may already have reaped a failed supervisor.
      if(child.stdin && !child.stdin.destroyed)child.stdin.end('stop\n');
      else {controlFailed=true;failure ??= Error('FunASR supervisor control channel is unavailable');}
    };
    const abort=()=>stop('transcription cancelled');
    signal.addEventListener('abort',abort,{once:true});
    child.stdin.on('error',e=>{controlFailed=true;failure ??= Error(`FunASR supervisor control failed: ${e.message}`);});
    if(signal.aborted)abort();
    const timer=setTimeout(()=>stop('transcription exceeded 300 seconds'),300000);
    child.stdout.setEncoding('utf8');child.stderr.setEncoding('utf8');child.stdio[3].setEncoding('utf8');
    const consume=(chunk,stderr)=>{bytes+=Buffer.byteLength(chunk);if(bytes>8*1024*1024){stop('transcription output exceeds 8 MiB');return;}if(stderr)err+=chunk;else out+=chunk;};
    child.stdout.on('data',s=>consume(s,false));child.stderr.on('data',s=>consume(s,true));
    child.stdout.on('error',e=>stop(`FunASR stdout failed: ${e.message}`));child.stderr.on('error',e=>stop(`FunASR stderr failed: ${e.message}`));
    child.stdio[3].on('data',chunk=>{
      control+=chunk;
      if(Buffer.byteLength(control)>8192){stop('FunASR supervisor status exceeded limit');return;}
      let at;
      while((at=control.indexOf('\n'))>=0){
        const line=control.slice(0,at);control=control.slice(at+1);
        try{
          const status=JSON.parse(line);
          if(status.supervisorError){stop(`FunASR supervisor failed: ${status.supervisorError}`);}
          else if(status.nativeExit===true && !nativeExit){nativeExit=status;stop();}
          else stop('FunASR supervisor sent an invalid status');
        }catch{stop('FunASR supervisor sent malformed status');}
      }
    });
    child.stdio[3].on('error',e=>stop(`FunASR supervisor status failed: ${e.message}`));
    child.on('error',e=>{failure=e;});
    child.once('close',()=>{
      clearTimeout(timer);signal.removeEventListener('abort',abort);
      if(failure||controlFailed)reject(failure??Error('FunASR supervisor control failed'));
      else if(!nativeExit)reject(Error('FunASR supervisor did not report native exit'));
      else done({...nativeExit,out,err,sig:nativeExit.signal});
    });
  });
}
async function transcribe(input, signal) {
  if(process.platform==='win32') return error('FunASR owned process cleanup requires POSIX and Python 3; Windows execution is unavailable.');
  if(input.language !== undefined && input.language !== 'auto') return error('language only supports "auto" for the local SenseVoice runtime.');
  if(typeof input.audio_path !== 'string' || !input.audio_path.trim()) return error('audio_path must be a non-empty local file path');
  if(/^[a-z][a-z0-9+.-]*:\/\//i.test(input.audio_path)) return error('audio_path must be a local project file, not a URL');
  const rootArg=options.get('--project-root'), runtime=options.get('--runtime-root');
  if(!rootArg) return error('FunASR MCP is missing its project root configuration');
  const root=await realpath(rootArg),audio=await realpath(resolve(root,input.audio_path));
  const rel=relative(root,audio);
  if(!rel || isAbsolute(rel) || rel==='..' || rel.startsWith('../') || rel.startsWith('..\\')) return error('audio_path must resolve inside the current project root');
  if(!(await stat(audio)).isFile()) return error('audio_path must resolve to a regular file');
  if(!runtime) return error('FunASR runtime is not configured; provision the local executable and models before retrying.');
  const platform=process.platform,arch=platform==='win32'&&process.arch==='arm64'?'x64':process.arch;
  const binary=await executable(join(runtime,'runtime','v0.2.0',`${platform}-${arch}`));
  const model=join(runtime,'models','sensevoice-small-q8.gguf'),vad=join(runtime,'models','fsmn-vad.gguf');
  if(!binary || !(await stat(model).catch(()=>undefined))?.isFile() || !(await stat(vad).catch(()=>undefined))?.isFile()) return error('FunASR runtime or model files are missing; build the native runtime from source and provision licensed models before retrying.');
  const completed=await run(binary,['-m',model,'--vad',vad,'-a',audio,'--srt'],signal);
  if(completed.code!==0) return error(`FunASR transcription failed (exit ${completed.code ?? completed.sig}): ${completed.err.trim().slice(-4000)||completed.out.trim().slice(-1000)||'no CLI diagnostic'}`);
  const rows=segments(completed.out);
  if(!rows.length) return error(`FunASR completed but did not produce SRT segments. ${completed.err.trim().slice(-1000)}`.trim());
  const text=rows.map(s=>s.text).join('\n');
  return {content:[{type:'text',text:`${text}\n\nTimestamped segments:\n${rows.map(s=>`[${s.start.toFixed(3)} - ${s.end.toFixed(3)}] ${s.text}`).join('\n')}`}],structuredContent:{text,segments:rows}};
}
async function dispatch(request) {
  if(!request || request.jsonrpc!=='2.0' || typeof request.method!=='string') return;
  if(request.method==='notifications/cancelled'){tasks.get(request.params?.requestId)?.abort();return;}
  if(request.id===undefined) return;
  if(request.method==='initialize'){send(request.id,{protocolVersion:request.params?.protocolVersion??'2024-11-05',capabilities:{tools:{}},serverInfo:{name:'pilotdeck-funasr-local',version:'0.2.0'}});return;}
  if(request.method==='ping'){send(request.id,{});return;}
  if(request.method==='tools/list'){send(request.id,{tools:[tool]});return;}
  if(request.method!=='tools/call'){process.stdout.write(`${JSON.stringify({jsonrpc:'2.0',id:request.id,error:{code:-32601,message:`Method not found: ${request.method}`}})}\n`);return;}
  if(tasks.has(request.id)){process.stderr.write('duplicate in-flight request id\n');return;}
  const controller=new AbortController();tasks.set(request.id,controller);
  try{send(request.id,request.params?.name==='transcribe_audio'?await transcribe(request.params.arguments??{},controller.signal):error(`Unknown tool: ${String(request.params?.name)}`));}
  catch(e){send(request.id,error(e.message));}finally{tasks.delete(request.id);}
}
let buffer='';
process.stdin.setEncoding('utf8');
process.stdin.on('data',chunk=>{
  buffer+=chunk;
  if(Buffer.byteLength(buffer)>1024*1024){process.stderr.write('MCP input exceeds 1 MiB\n');shutdown();process.stdin.destroy();return;}
  let at;
  while((at=buffer.indexOf('\n'))>=0){const line=buffer.slice(0,at);buffer=buffer.slice(at+1);try{void dispatch(JSON.parse(line));}catch{process.stderr.write('invalid MCP JSON\n');}}
});
function shutdown(){closing=true;for(const controller of tasks.values())controller.abort();process.stdin.destroy();}
process.stdin.on('end',shutdown);process.on('SIGTERM',shutdown);process.on('SIGINT',shutdown);
