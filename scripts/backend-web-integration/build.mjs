// Build the fixed baseline through the reviewed source overlay.
import { readFile, writeFile, mkdir, access, realpath } from 'node:fs/promises';
import { basename, dirname, resolve, isAbsolute, relative, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { integrationOverlay, verifyOverlay, sha256 } from './overlay.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const usage = 'Usage: node scripts/backend-web-integration/build.mjs --baseline /absolute/fixed-baseline --out /absolute/NEW-output';
function argumentsOf(argv) {
  if (argv.length === 1 && argv[0] === '--help') { console.log(usage); process.exit(0); }
  const values = {};
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    if (!['--baseline', '--out'].includes(name) || values[name] || !argv[index + 1]) throw new Error(usage);
    values[name] = argv[index + 1];
  }
  if (!values['--baseline'] || !values['--out'] || Object.values(values).some(value => !isAbsolute(value))) throw new Error(usage);
  return values;
}
const args = argumentsOf(process.argv.slice(2));
const baseline = await realpath(args['--baseline']);
const ui = resolve(baseline, 'ui');
// Require an existing real parent; an output symlink can never redirect writes.
const requested = resolve(args['--out']);
const output = resolve(await realpath(dirname(requested)), basename(requested));
const repository = await realpath(resolve(here, '../..'));
const inside = (parent, child) => child === parent || child.startsWith(parent + sep);
if (inside(baseline, output) || inside(output, baseline)) throw new Error('Output must not overlap the read-only baseline');
if (inside(output, repository)) throw new Error('Output must not contain the product repository');
if (inside(repository, output) && !['_tmp', '_build'].includes(relative(repository, output).split(sep)[0])) {
  throw new Error('Inside the product repository, output is allowed only under _tmp or _build');
}
const [nodeMajor, nodeMinor] = process.versions.node.split('.').map(Number);
if (nodeMajor !== 22 || nodeMinor < 13) throw new Error('Baseline requires Node >=22.13.0 <23');
const lock = JSON.parse(await readFile(resolve(here, 'baseline.json'), 'utf8'));
const patchBytes = await readFile(resolve(here, 'patches.json'));
if (sha256(patchBytes) !== lock.patchesSha256 || JSON.parse(patchBytes).length !== lock.overlayTargetCount) {
  throw new Error('Reviewed overlay manifest identity mismatch');
}
function git(argv) {
  return execFileSync('git', ['-C', baseline, ...argv], {
    encoding: 'utf8',
    env: { PATH: process.env.PATH, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1', GIT_OPTIONAL_LOCKS: '0' },
  }).trim();
}
async function verifyCheckout() {
  if (baseline === resolve(repository, 'vendor/pilotdeck-ts')) {
    const manifest = JSON.parse(await readFile(resolve(here, 'vendored-baseline.json'), 'utf8'));
    if (manifest.commit !== lock.head) throw new Error('Vendored baseline identity mismatch');
    for (const [path, expected] of Object.entries(manifest.files)) {
      if (isAbsolute(path) || path.split('/').includes('..')) throw new Error('Invalid vendored file path');
      if (sha256(await readFile(resolve(baseline, path))) !== expected) throw new Error(`Vendored baseline file hash mismatch: ${path}`);
    }
    return;
  }
  if (git(['rev-parse', 'HEAD']) !== lock.head) throw new Error('Baseline HEAD mismatch');
  if (git(['rev-parse', '--show-toplevel']) !== baseline) throw new Error('--baseline must name the checkout root');
  if (git(['status', '--porcelain', '--untracked-files=no'])) throw new Error('Baseline tracked files are dirty');
}
await verifyCheckout();
for (const [path, expected] of Object.entries(lock.files)) {
  if (sha256(await readFile(resolve(baseline, path))) !== expected) throw new Error(`Baseline file hash mismatch: ${path}`);
}
const files = await verifyOverlay(baseline);
const inputNames = ['build.mjs', 'overlay.mjs', 'patches.json', 'baseline.json', 'vendored-baseline.json'];
async function scriptIdentity() {
  return Object.fromEntries(await Promise.all(inputNames.map(async name => [name, sha256(await readFile(resolve(here, name)))])));
}
const scriptHashes = await scriptIdentity();
// Exclusive output creation preserves the first failed attempt as well as success.
await mkdir(output, { recursive: false });
for (const name of ['cache', 'home', 'pilot-home', 'tmp']) await mkdir(resolve(output, name));
const environment = {
  PATH: process.env.PATH || '/usr/bin:/bin',
  HOME: resolve(output, 'home'), PILOT_HOME: resolve(output, 'pilot-home'), TMPDIR: resolve(output, 'tmp'),
  NODE_ENV: 'production', PILOTDECK_DISABLE_LOCAL_AUTH: '1', VITE_IS_PLATFORM: 'false',
  TSX_DISABLE_CACHE: '1', JITI_FS_CACHE: '0', LANG: 'C', LC_ALL: 'C', TZ: 'UTC',
};
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, environment);
const requireUI = createRequire(resolve(ui, 'package.json'));
const requireRoot = createRequire(resolve(baseline, 'package.json'));
const resolvePackage = name => { try { return requireUI.resolve(name); } catch { return requireRoot.resolve(name); } };
let unregister;
try {
  let config = await readFile(resolve(ui, 'vite.config.js'), 'utf8');
  function replaceOnce(before, after) {
    if (config.split(before).length !== 2) throw new Error(`Vite config anchor mismatch: ${before}`);
    config = config.replace(before, after);
  }
  replaceOnce("    ...loadEnv(mode, repoRoot, ''),", '    // Deliberately exclude all baseline .env files.');
  replaceOnce('const __dirname = path.dirname(__filename)', `const __dirname = ${JSON.stringify(ui)}`);
  config = config.replace(/from '([^']+)'/g, (whole, specifier) => {
    if (specifier === 'path' || specifier === 'url') return whole;
    const target = specifier.startsWith('.')
      ? resolve(ui, specifier === '../apps/desktop/src/lightAppearance' ? specifier + '.ts' : specifier)
      : resolvePackage(specifier);
    return `from ${JSON.stringify(pathToFileURL(target).href)}`;
  });
  const configPath = resolve(output, 'baseline-config.mjs');
  await writeFile(configPath, config);
  const { register } = await import(pathToFileURL(resolvePackage('tsx/esm/api')).href);
  unregister = register();
  const { default: configFactory } = await import(pathToFileURL(configPath).href);
  process.chdir(ui);
  const original = await configFactory({ mode: 'production', command: 'build' });
  const { build } = await import(pathToFileURL(resolvePackage('vite')).href);
  const buildOptions = {
    ...original, configFile: false, root: ui, envDir: false, cacheDir: resolve(output, 'cache'),
    plugins: [integrationOverlay(baseline), ...original.plugins],
    build: { ...original.build, outDir: resolve(output, 'dist'), emptyOutDir: false },
  };
  await writeFile(resolve(output, 'source-manifest.json'), JSON.stringify({
    schemaVersion: 1, baseline, expectedHead: lock.head, files,
    baselineFiles: lock.files, scriptHashes, patchesSha256: lock.patchesSha256,
    node: { version: process.version, executable: process.execPath },
    configEnvFilesRead: false, output, command: process.argv,
  }, null, 2) + '\n');
  await build(buildOptions);
  await access(resolve(output, 'dist/index.html'));
  await verifyCheckout();
  await verifyOverlay(baseline);
  for (const [path, expected] of Object.entries(lock.files)) {
    if (sha256(await readFile(resolve(baseline, path))) !== expected) throw new Error(`Baseline input changed during build: ${path}`);
  }
  if (JSON.stringify(await scriptIdentity()) !== JSON.stringify(scriptHashes)) throw new Error('Overlay build inputs changed during build');
  const result = { result: 'built', dist: resolve(output, 'dist'), indexSha256: sha256(await readFile(resolve(output, 'dist/index.html'))), patched: files.map(file => file.path) };
  await writeFile(resolve(output, 'build-result.json'), JSON.stringify(result, null, 2) + '\n');
  console.log(JSON.stringify(result));
} catch (error) {
  await writeFile(resolve(output, 'build-result.json'), JSON.stringify({ result: 'failed', error: error.message }, null, 2) + '\n');
  throw error;
} finally {
  await unregister?.();
}
