// Baseline status handler; git is an isolated executable provided by caller.
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
const [base,out]=process.argv.slice(2);
if(!path.isAbsolute(base)||!path.isAbsolute(out)||fs.existsSync(out)) throw Error('absolute baseline and new output required');
const {executeChannelCommand}=await import(pathToFileURL(path.join(base,'dist/src/adapters/channel/protocol/ChannelCommandRegistry.js')));
const replies=[]; const handled=await executeChannelCommand('/status',{gateway:{},chatId:'mock',channelKey:'mock',reply:async value=>replies.push(value)});
fs.writeFileSync(out,JSON.stringify({handled,replies,node:process.version,platform:process.platform},null,2)+'\n',{flag:'wx'});
