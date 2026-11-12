// Read-only check that the executed channel and composition entry were emitted
// from the fixed source. This does not claim a clean rebuild of every dependency.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL} from 'node:url';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
export async function sourceIdentity(root) {
  const {default:ts}=await import(pathToFileURL(path.join(root,'node_modules/typescript/lib/typescript.js')));
  const files=['src/cli/pilotdeck.ts','src/cli/ChannelAdapterBundle.ts','src/adapters/channel/loadEnabledChannels.ts','src/adapters/channel/signal/SignalChannel.ts','src/adapters/channel/signal/SignalSessionMapper.ts','src/adapters/channel/signal/signal-render.ts','src/adapters/channel/protocol/ImPermissionHelper.ts','src/adapters/channel/protocol/ImElicitationHelper.ts','src/adapters/channel/protocol/ImCronDelivery.ts'];
  const head=spawnSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'});assert.equal(head.status,0);assert.equal(head.stdout.trim(),'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
  assert.equal(spawnSync('git',['diff','--quiet','HEAD','--',...files],{cwd:root}).status,0,'fixed source inputs must be clean');
  const hash=s=>createHash('sha256').update(s).digest('hex'), strip=s=>s.replace(/\/\/# sourceMappingURL=.*$/m,'').trim();
  return files.map(file=>{
    const source=fs.readFileSync(path.join(root,file),'utf8'),compiled=file.replace(/\.ts$/,'.js'),dist=fs.readFileSync(path.join(root,'dist',compiled),'utf8');
    const emitted=ts.transpileModule(source,{fileName:file,compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext,sourceMap:true,esModuleInterop:true}}).outputText;
    assert.equal(strip(dist),strip(emitted),`${file}: executed dist differs from fixed source emission`);
    return {file,sourceSha256:hash(source),distSha256:hash(dist),matched:true,compiler:ts.version,normalization:'only sourceMappingURL footer and outer whitespace'};
  });
}
