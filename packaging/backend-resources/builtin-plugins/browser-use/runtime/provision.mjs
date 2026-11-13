// Assemble public, pinned source packages offline. Never install a browser.
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawnSync} from 'node:child_process';
import {createRequire} from 'node:module';
import {createHash} from 'node:crypto';
const here=path.dirname(fileURLToPath(import.meta.url));
const [output,store,pnpm='pnpm']=process.argv.slice(2);
if(!output||!store||!path.isAbsolute(output)||!path.isAbsolute(store))throw Error('usage: node provision.mjs <NEW absolute runtime directory> <existing absolute pnpm store> [pnpm executable]');
if(fs.existsSync(output))throw Error('runtime output must be new; refusing to overwrite');
if(!fs.existsSync(store))throw Error('existing offline public pnpm store required');
fs.mkdirSync(output,{recursive:true});
for(const file of ['package.json','pnpm-lock.yaml','sources.json'])fs.copyFileSync(path.join(here,file),path.join(output,file));
const buildHome=path.join(output,'.build-home');fs.mkdirSync(buildHome);
const env={...process.env,HOME:buildHome,PNPM_HOME:path.join(buildHome,'pnpm'),PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD:'1',npm_config_ignore_scripts:'true',CI:'1'};
const args=['install','--offline','--frozen-lockfile','--ignore-scripts','--no-optional','--package-import-method=copy',`--store-dir=${store}`];
const installed=spawnSync(pnpm,args,{cwd:output,env,encoding:'utf8'});
fs.writeFileSync(path.join(output,'install.stdout'),installed.stdout??'');fs.writeFileSync(path.join(output,'install.stderr'),installed.stderr??'');fs.writeFileSync(path.join(output,'install.exit'),JSON.stringify({code:installed.status,signal:installed.signal,error:installed.error?.message??null})+'\n');
if(installed.status!==0||installed.signal||installed.error)throw Error(`offline source-package assembly failed; original logs retained at ${output}`);
const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
const require=createRequire(path.join(output,'package.json'));
const sources=JSON.parse(fs.readFileSync(path.join(output,'sources.json')));
const packages=sources.packages.map(source=>{
 const file=fs.realpathSync(require.resolve(`${source.name}/package.json`));
 if(!file.startsWith(output+path.sep))throw Error('runtime dependency escaped independent output');
 const pkg=JSON.parse(fs.readFileSync(file));if(pkg.version!==source.version||pkg.license!==source.license)throw Error(`package identity mismatch ${source.name}`);
 const root=path.dirname(file),notices={};
 for(const name of ['LICENSE','NOTICE'])if(fs.existsSync(path.join(root,name)))notices[name]=digest(fs.readFileSync(path.join(root,name)));
 if(!notices.LICENSE)throw Error(`missing public package license ${source.name}`);
 return {...source,path:root,packageSha256:digest(fs.readFileSync(file)),notices};
});
const files={};
function walk(dir){for(const entry of fs.readdirSync(dir,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))){const file=path.join(dir,entry.name),relative=path.relative(output,file);if(entry.isSymbolicLink()){const resolved=fs.realpathSync(file);if(!resolved.startsWith(output+path.sep))throw Error(`runtime symlink escapes output: ${relative}`);files[relative]={symlink:fs.readlinkSync(file)};}else if(entry.isDirectory())walk(file);else if(entry.isFile())files[relative]=digest(fs.readFileSync(file));}}
walk(path.join(output,'node_modules'));
const manifest={schema:1,scope:sources.scope,baseline:sources.baseline,node:{version:process.version,executable:process.execPath,sha256:digest(fs.readFileSync(process.execPath))},pnpm:{executable:pnpm,args},sourceManifestSha256:digest(fs.readFileSync(path.join(output,'sources.json'))),lockSha256:digest(fs.readFileSync(path.join(output,'pnpm-lock.yaml'))),packages,files,browserIncluded:false,lifecycleScriptsExecuted:false,network:'offline',entry:path.join(output,'node_modules/@playwright/mcp/cli.js')};
fs.writeFileSync(path.join(output,'runtime-manifest.json'),JSON.stringify(manifest,null,2)+'\n');
console.log(JSON.stringify({status:'assembled',output,manifest:path.join(output,'runtime-manifest.json'),entry:manifest.entry,packages:packages.map(p=>`${p.name}@${p.version}`)},null,2));
