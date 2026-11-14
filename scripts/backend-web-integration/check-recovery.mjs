// Run only the eleven regression checks for stale interaction recovery.
import { readFile, mkdir, realpath } from 'node:fs/promises';
import { basename, dirname, resolve, relative, isAbsolute, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { applyOverlay, verifyOverlay, sha256 } from './overlay.mjs';
const here = dirname(fileURLToPath(import.meta.url));
const usage = 'Usage: node scripts/backend-web-integration/check-recovery.mjs --baseline /absolute/fixed-baseline --out /absolute/NEW-check-output';
if (process.argv.length === 3 && process.argv[2] === '--help') { console.log(usage); process.exit(0); }
const args = {};
for (let index = 2; index < process.argv.length; index += 2) {
  const name = process.argv[index], value = process.argv[index + 1];
  if (!['--baseline', '--out'].includes(name) || args[name] || !value || !isAbsolute(value)) throw new Error(usage);
  args[name] = value;
}
if (!args['--baseline'] || !args['--out']) throw new Error(usage);
const baseline = await realpath(args['--baseline']);
const requested = resolve(args['--out']);
const output = resolve(await realpath(dirname(requested)), basename(requested));
const repository = await realpath(resolve(here, '../..'));
const inside = (parent, child) => child === parent || child.startsWith(parent + sep);
if (inside(baseline, output) || inside(output, baseline) || inside(output, repository)) throw new Error('Output overlaps source');
if (inside(repository, output) && !['_tmp', '_build'].includes(relative(repository, output).split(sep)[0])) throw new Error('Use _tmp or _build for repository-local output');
const lock = JSON.parse(await readFile(resolve(here, 'baseline.json'), 'utf8'));
const bytes = await readFile(resolve(here, 'patches.json'));
const specs = JSON.parse(bytes);
if (sha256(bytes) !== lock.patchesSha256 || specs.length !== lock.overlayTargetCount) throw new Error('Reviewed patch identity mismatch');
function git(argv) {
  return execFileSync('git', ['-C', baseline, ...argv], { encoding: 'utf8', env: {
    PATH: process.env.PATH, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1', GIT_OPTIONAL_LOCKS: '0',
  } }).trim();
}
if (git(['rev-parse', 'HEAD']) !== lock.head || git(['status', '--porcelain', '--untracked-files=no'])) throw new Error('Fixed clean baseline required');
await verifyOverlay(baseline);
for (const [path, expected] of Object.entries(lock.files)) {
  if (sha256(await readFile(resolve(baseline, path))) !== expected) throw new Error(`Baseline identity mismatch: ${path}`);
}
await mkdir(output, { recursive: false });
for (const name of ['home', 'pilot-home', 'tmp', 'cache']) await mkdir(resolve(output, name));
const taskPath = process.env.PATH;
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, {
  PATH: taskPath, HOME: resolve(output, 'home'), PILOT_HOME: resolve(output, 'pilot-home'), TMPDIR: resolve(output, 'tmp'),
  TSX_DISABLE_CACHE: '1', JITI_FS_CACHE: '0', LANG: 'C', LC_ALL: 'C', TZ: 'UTC',
});
const requireUI = createRequire(resolve(baseline, 'ui/package.json'));
const requireRoot = createRequire(resolve(baseline, 'package.json'));
const locate = name => { try { return requireUI.resolve(name); } catch { return requireRoot.resolve(name); } };
const targets = new Map(specs.map(spec => [resolve(baseline, spec.path), spec]));
const { startVitest } = await import(pathToFileURL(locate('vitest/node')).href);
const testFile = resolve(here, 'recovery.test.ts');
const ctx = await startVitest('test', [testFile], {
  config: false, root: here, run: true, watch: false,
  environment: 'jsdom', maxWorkers: 1, cache: false, include: [testFile],
  reporters: ['default', 'json'], outputFile: resolve(output, 'results.json'),
}, {
  configFile: false, envDir: false, server: { fs: { allow: [here, baseline] } }, cacheDir: resolve(output, 'cache'),
  resolve: { alias: {
    '@pilotdeck-baseline': baseline,
    react: locate('react'),
    '@testing-library/react': locate('@testing-library/react'),
    vitest: resolve(dirname(locate('vitest/package.json')), 'dist/index.js'),
  } },
  plugins: [{
    name: 'stale-interaction-regression-overlay', enforce: 'pre',
    transform(source, id) {
      const spec = targets.get(id.split('?')[0]);
      return spec ? { code: applyOverlay(source, spec), map: null } : null;
    },
  }],
});
await ctx.close();
