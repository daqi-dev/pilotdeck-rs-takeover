// The oracle is the fixed product's read_file tool, not a second implementation
// of its resize policy. Images are deterministic, generated test inputs.
import fs from 'node:fs';
import path from 'node:path';
import {pathToFileURL, fileURLToPath} from 'node:url';
import {createRequire} from 'node:module';
import {spawnSync} from 'node:child_process';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';
const [helperArg, baselineArg, outputArg, buildIdentity] = process.argv.slice(2);
if (!helperArg || !baselineArg || !outputArg || (helperArg !== '-' && !buildIdentity)) throw new Error('usage: check-image.mjs HELPER_OR_DASH BASELINE NEW_OUTPUT [BUILD_IDENTITY]');
const baseline=path.resolve(baselineArg), out=path.resolve(outputArg), helper=helperArg==='-'?null:path.resolve(helperArg);
fs.mkdirSync(out);const home=path.join(out,'home');fs.mkdirSync(path.join(home,'.pilotdeck'),{recursive:true});
process.env.HOME=home;process.env.PILOT_HOME=path.join(home,'.pilotdeck');
const git=spawnSync('git',['-C',baseline,'rev-parse','HEAD'],{encoding:'utf8'});
assert.equal(git.status,0);assert.equal(git.stdout.trim(),'cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85');
const require=createRequire(path.join(baseline,'package.json'));
const sharp=require('sharp');assert.equal(sharp.versions.sharp,'0.34.5');assert.equal(sharp.versions.vips,'8.17.3');
const toolFile=path.join(baseline,'dist/src/tool/builtin/readFile.js');
const {createReadFileTool}=await import(pathToFileURL(toolFile).href);
const sha=value=>crypto.createHash('sha256').update(value).digest('hex');
const artifacts=[fileURLToPath(import.meta.url),toolFile,path.join(baseline,'pnpm-lock.yaml'),...(helper?[helper,buildIdentity]:[])].map(file=>({file:path.resolve(file),sha256:sha(fs.readFileSync(file))}));
// Both source-built and package layouts are accepted, and the actually loaded
// addon must be identified. require.cache records Sharp's native require.
const addons=Object.keys(require.cache).filter(file=>file.endsWith('.node')&&file.includes('sharp'));
assert.equal(addons.length,1,'one actually loaded Sharp addon is required');
artifacts.push({file:addons[0],sha256:sha(fs.readFileSync(addons[0]))});
if(helper){const commit=fs.readFileSync(buildIdentity,'utf8').split('\n')[0];assert.match(commit,/^[a-f0-9]{40}$/);}

function noise(width,height,channels=3){let seed=0x21335487;const buffer=Buffer.alloc(width*height*channels);for(let i=0;i<buffer.length;i++){seed^=seed<<13;seed^=seed>>>17;seed^=seed<<5;buffer[i]=seed&255;}return sharp(buffer,{raw:{width,height,channels}});}
const fixtures=[];
async function fixture(name,mime,bytes,budgets){const file=path.join(out,name);fs.writeFileSync(file,await bytes);fixtures.push({name,mime,file,budgets,sha256:sha(fs.readFileSync(file))});}
const huge=20*1024*1024;
await fixture('small.png','image/png',noise(8,8).png().toBuffer(),[huge,1]);
await fixture('small.jpg','image/jpeg',noise(8,8).jpeg().toBuffer(),[huge]);
await fixture('small.gif','image/gif',noise(8,8).gif().toBuffer(),[huge]);
await fixture('small.webp','image/webp',noise(8,8).webp().toBuffer(),[huge]);
await fixture('small-gray.png','image/png',noise(8,8,1).png().toBuffer(),[huge]);
await fixture('small-alpha.png','image/png',noise(8,8,4).png().toBuffer(),[huge]);
await fixture('small-palette.png','image/png',noise(8,8).png({palette:true}).toBuffer(),[huge]);
await fixture('gray16.png','image/png',noise(101,77).toColourspace('grey16').png().toBuffer(),[huge,2000]);
await fixture('large.jpg','image/jpeg',noise(2048,1536).jpeg({quality:95}).toBuffer(),[huge,900000,350000,160000,1]);
await fixture('large.png','image/png',noise(2048,1536).png().toBuffer(),[huge,6500000,350000,160000,1]);
await fixture('large.webp','image/webp',noise(2048,1536).webp({quality:95}).toBuffer(),[huge,1100000,350000,160000,1]);
await fixture('large.gif','image/gif',noise(256,256).gif().toBuffer(),[huge,180000,30000]);
await fixture('alpha.png','image/png',noise(701,509,4).png().toBuffer(),[huge,100000]);
await fixture('rotate.jpg','image/jpeg',noise(2048,1536).jpeg({quality:95}).withMetadata({orientation:6}).toBuffer(),[900000,160000]);
await fixture('profile.jpg','image/jpeg',noise(501,303).withIccProfile('p3').jpeg().toBuffer(),[20000]);
await fixture('thin.png','image/png',noise(1,4096).png().toBuffer(),[5000]);
const jpeg=fs.readFileSync(path.join(out,'large.jpg'));
await fixture('truncated.jpg','image/jpeg',jpeg.subarray(0,jpeg.length-30),[huge]);
await fixture('corrupt.webp','image/webp',Buffer.from('RIFF0000WEBP'+'?'.repeat(1800)),[huge]);
await fixture('corrupt.png','image/png',Buffer.from('not an image'),[huge]);
const checks=[];
for(const fixture of fixtures)for(const budget of fixture.budgets){
  const name=`${fixture.name}-${budget}`, context={cwd:out,permissionMode:'default',permissionContext:{additionalWorkingDirectories:[]},abortSignal:new AbortController().signal,modelMultimodal:{input:['image'],maxImageBytes:budget},env:{PILOT_HOME:process.env.PILOT_HOME}};
  const expected=await createReadFileTool().execute({file_path:fixture.name},context);
  fs.writeFileSync(path.join(out,`${name}.expected.json`),JSON.stringify(expected,null,2));
  const image=expected.content.find(block=>block.type==='image');
  let passed=true, actual=null;
  if(helper){
    const result=spawnSync(helper,['prepare',fixture.file,fixture.mime,String(budget)],{timeout:30000,maxBuffer:32*1024*1024+32});
    fs.writeFileSync(path.join(out,`${name}.stdout`),result.stdout??Buffer.alloc(0));fs.writeFileSync(path.join(out,`${name}.stderr`),result.stderr??Buffer.alloc(0));
    actual={status:result.status,signal:result.signal,error:result.error?.message};
    const split=result.stdout?.indexOf(10)??-1;
    if(image){const bytes=Buffer.from(image.data,'base64');passed=!result.error&&!result.signal&&result.status===0&&split>=0&&result.stdout.subarray(0,split).toString()===image.mimeType&&result.stdout.subarray(split+1).equals(bytes);actual.outputSha256=sha(result.stdout??Buffer.alloc(0));}
    else passed=!result.error&&!result.signal&&result.status===1&&result.stdout.length===0&&result.stderr.toString().trim()===expected.data.error;
  }
  checks.push({name,budget,passed,expected:image?{mime:image.mimeType,bytes:image.bytes,sha256:sha(Buffer.from(image.data,'base64'))}:{error:expected.data?.error},...(actual?{actual}:{})});
}
for(const artifact of artifacts)assert.equal(sha(fs.readFileSync(artifact.file)),artifact.sha256,`artifact changed: ${artifact.file}`);
const manifest={kind:helper?'component comparison':'TypeScript oracle only',baseline:git.stdout.trim(),node:process.version,platform:process.platform,arch:process.arch,sharp:sharp.versions,artifacts,fixtures:fixtures.map(({name,mime,budgets,sha256})=>({name,mime,budgets,sha256})),checks,passed:checks.every(c=>c.passed)};
fs.writeFileSync(path.join(out,'manifest.json'),JSON.stringify(manifest,null,2));console.log(JSON.stringify({kind:manifest.kind,passed:manifest.passed,checks:checks.length,failures:checks.filter(c=>!c.passed).map(c=>c.name)}));if(!manifest.passed)process.exitCode=1;
