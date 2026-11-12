import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { createHash } from 'node:crypto';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';

export function nativeFileIdentity(file) {
  if (!file) return null;
  if (!fs.existsSync(file)) return { path: file, sha256: null };
  const actual = fs.realpathSync(file);
  return { path: file, realpath: actual, sha256: createHash('sha256').update(fs.readFileSync(actual)).digest('hex') };
}

// Probe in a fresh process: require.cache in this supervisor must never conceal
// an addon replacement between the beginning and end of a run.
function loadedSharp(baseline, entry) {
  if (!entry) return { loaded: false, files: [], libraries: [] };
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'bbx-native-identity-'));
  try {
    const script = `
      const {createRequire}=require('node:module'),path=require('node:path');
      const req=createRequire(path.join(process.argv[1],'package.json'));
      try {
        const sharp=req('sharp'),root=path.dirname(req.resolve('sharp/package.json'));
        const files=Object.keys(require.cache).filter(f=>f.startsWith(root+path.sep)).sort();
        const libraries=process.report.getReport().sharedObjects.filter(f=>/libvips|sharp.*\\.node$/.test(f)).sort();
        process.stdout.write(JSON.stringify({loaded:true,versions:sharp.versions,files,libraries}));
      } catch(e) { process.stdout.write(JSON.stringify({loaded:false,error:e.code||e.name,files:[],libraries:[]})); }
    `;
    const result = spawnSync(process.execPath, ['-e', script, baseline], { encoding: 'utf8', timeout: 10000, maxBuffer: 1024 * 1024,
      env: { ...process.env, HOME: home, PILOT_HOME: path.join(home, '.pilotdeck') } });
    if (result.status !== 0 || result.signal || result.error) throw Error('native dependency probe failed');
    const loaded = JSON.parse(result.stdout);
    return { ...loaded, files: loaded.files.map(nativeFileIdentity), libraries: loaded.libraries.map(nativeFileIdentity) };
  } finally { fs.rmSync(home, { recursive: true, force: true }); }
}

export function nativeDependencyIdentity(baseline, imageTool) {
  const require = createRequire(path.join(path.resolve(baseline), 'package.json'));
  const resolve = (name) => { try { return require.resolve(name); } catch { return null; } };
  const mupdf = resolve('mupdf');
  const tokenizer = resolve('js-tiktoken/lite');
  let base64 = null;
  try { if (tokenizer) base64 = createRequire(tokenizer).resolve('base64-js'); } catch { /* missing dependency is recorded */ }
  const image = nativeFileIdentity(imageTool);
  const libDir = imageTool ? path.resolve(path.dirname(imageTool), '../lib') : null;
  const libraries = libDir && fs.existsSync(libDir) ? fs.readdirSync(libDir).filter(name => /^libvips.*\.(so(?:\..*)?|dylib)$/.test(name)).sort()
    .map(name => nativeFileIdentity(path.join(libDir, name))) : [];
  let resolvedLibraries = [];
  if (process.platform === 'linux' && image?.sha256) {
    const links = spawnSync('ldd', [imageTool], { encoding: 'utf8', timeout: 10000 });
    if (links.error || links.signal) throw Error('image library identity probe failed');
    resolvedLibraries = (links.stdout ?? '').split('\n').filter(line => line.includes('libvips')).map(line => {
      const resolved = /=>\s+(\/\S+)/.exec(line)?.[1];
      return resolved ? nativeFileIdentity(resolved) : { unresolved: line.trim() };
    });
  }
  return {
    manifests: Object.fromEntries(['sharp', 'mupdf', 'js-tiktoken'].map(name => [name,
      nativeFileIdentity(path.join(baseline, 'node_modules', name, 'package.json'))])),
    sharp: loadedSharp(baseline, resolve('sharp')),
    mupdf: { entry: nativeFileIdentity(mupdf), loader: nativeFileIdentity(mupdf && path.join(path.dirname(mupdf), 'mupdf-wasm.js')),
      wasm: nativeFileIdentity(mupdf && path.join(path.dirname(mupdf), 'mupdf-wasm.wasm')) },
    tokenizer: { entry: nativeFileIdentity(tokenizer), o200k: nativeFileIdentity(resolve('js-tiktoken/ranks/o200k_base')), base64: nativeFileIdentity(base64) },
    image: { executable: image, libraries, resolvedLibraries },
  };
}
