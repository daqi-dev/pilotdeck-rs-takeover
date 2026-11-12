// Compare only the four TS inputs consumed by the command oracle.
import fs from 'node:fs'; import crypto from 'node:crypto'; import {createRequire} from 'node:module'; import path from 'node:path';
const base=process.argv[2]; if(!base||!path.isAbsolute(base))throw Error('absolute baseline required');
const ts=createRequire(path.join(base,'package.json'))('typescript');
const norm=s=>s.replace(/^\/\/# sourceMappingURL=.*$/gm,'').trim();
const hash=s=>crypto.createHash('sha256').update(s).digest('hex');
for(const file of ['adapters/channel/protocol/ChannelCommandRegistry','session/search/ChatSearchArgs','session/search/formatChatHistorySearch','runtime/updateCommand']) {
 const source=fs.readFileSync(path.join(base,'src',`${file}.ts`),'utf8'); const dist=fs.readFileSync(path.join(base,'dist/src',`${file}.js`),'utf8');
 const generated=ts.transpileModule(source,{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext}}).outputText;
 if(norm(generated)!==norm(dist))throw Error(`transpilation mismatch: ${file}`);
 console.log(JSON.stringify({file,sourceSha256:hash(source),distSha256:hash(dist),typescript:ts.version}));
}
