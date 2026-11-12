import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {pathToFileURL} from 'node:url';
export const baselineSha='cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85';
export const hash=value=>createHash('sha256').update(value).digest('hex');
export async function sourceIdentity(root) {
  const files=['src/telemetry/collector.ts','src/telemetry/sender.ts','src/telemetry/context.ts','src/telemetry/types.ts','src/telemetry/TelemetryObserverRegistry.ts','src/telemetry/index.ts','src/gateway/client/GatewayAgentEventTelemetryObserver.ts','src/gateway/client/GatewayTurnTelemetryContextResolver.ts','src/model/normalizeProviderBaseUrl.ts','src/gateway/server/authToken.ts','src/cli/GatewayTelemetryBundle.ts','src/cli/pilotdeck.ts'];
  assert.equal(spawnSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'}).stdout.trim(),baselineSha);
  assert.equal(spawnSync('git',['diff','--quiet','HEAD','--',...files],{cwd:root}).status,0,'fixed source must be clean');
  const {default:ts}=await import(pathToFileURL(path.join(root,'node_modules/typescript/lib/typescript.js')));
  const strip=s=>s.replace(/\/\/# sourceMappingURL=.*$/m,'').trim();
  return {baselineSha,compiler:ts.version,scope:'listed source modules only; not a complete dist/dependency rebuild',files:files.map(file=>{
    const source=fs.readFileSync(path.join(root,file),'utf8'),dist=fs.readFileSync(path.join(root,'dist',file.replace(/\.ts$/,'.js')),'utf8');
    const emitted=ts.transpileModule(source,{fileName:file,compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext,sourceMap:true,esModuleInterop:true}}).outputText;
    assert.equal(strip(dist),strip(emitted),file+' differs from fixed source emission');
    return {file,sourceSha256:hash(source),distSha256:hash(dist)};
  })};
}
