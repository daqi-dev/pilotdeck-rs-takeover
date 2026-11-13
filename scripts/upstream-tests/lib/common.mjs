// run.mjs 与各子工具共用的小件：定位基线与仓库、隔离环境、Rust 二进制新鲜度、带上限的子进程。
import { existsSync, mkdirSync, readFileSync, statSync, readdirSync, symlinkSync, cpSync, lstatSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn, execFileSync } from 'node:child_process';

export const HERE = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const REPO = resolve(HERE, '../..');

/** 基线目录：PDRS_BASELINE 优先；否则本检出的 resources/PilotDeck；否则主检出（git common dir 的上一级）的。 */
export function baselineRoot() {
  const candidates = [];
  if (process.env.PDRS_BASELINE) candidates.push(resolve(process.env.PDRS_BASELINE));
  candidates.push(join(REPO, 'resources/PilotDeck'));
  try {
    const common = execFileSync('git', ['-C', REPO, 'rev-parse', '--path-format=absolute', '--git-common-dir'], { encoding: 'utf8' }).trim();
    candidates.push(join(dirname(common), 'resources/PilotDeck'));
  } catch { /* not a git checkout */ }
  for (const candidate of candidates) {
    if (existsSync(join(candidate, 'dist/src/cli/pilotdeck-agent-loop-sidecar.js'))) return candidate;
    if (process.env.PDRS_BASELINE && candidate === resolve(process.env.PDRS_BASELINE)) {
      throw new Error(`PDRS_BASELINE=${candidate} has no dist/src/cli/pilotdeck-agent-loop-sidecar.js`);
    }
  }
  throw new Error(`baseline not found in ${candidates.join(', ')}; set PDRS_BASELINE`);
}

export function rustSidecarBin() {
  return resolve(process.env.PDRS_SIDECAR_BIN || join(REPO, 'src/target/debug/pilotdeck-agent-loop-sidecar'));
}

/** 和 bbx 一样：二进制不能比 cargo 依赖清单（<bin>.d）里列的源文件旧。 */
export function assertFreshBinary(bin) {
  if (!existsSync(bin)) throw new Error(`Rust sidecar not built: ${bin} (cd src && cargo build -p pilotdeck-transport)`);
  if (process.env.PDRS_ALLOW_STALE === '1') return;
  const built = statSync(bin).mtimeMs;
  const depFile = `${bin}.d`;
  if (!existsSync(depFile)) return;
  const text = readFileSync(depFile, 'utf8').replace(/\\\n/g, ' ');
  const deps = text.slice(text.indexOf(':') + 1).split(/(?<!\\)\s+/).filter(Boolean).map((p) => p.replace(/\\ /g, ' '));
  const stale = deps.filter((p) => existsSync(p) && statSync(p).mtimeMs > built);
  if (stale.length) {
    throw new Error(`Rust sidecar ${bin} is older than ${stale.length} source file(s), e.g. ${stale[0]}; rebuild or set PDRS_ALLOW_STALE=1`);
  }
}

/**
 * 隔离环境：只带 PATH 和三个目录变量（HOME / PILOT_HOME / TMPDIR），再加调用方给的 PDRS_* 变量。
 * 不继承调用方的其他环境变量，所以 PILOTDECK_*、NODE_OPTIONS 之类一律不会漏进来。
 */
export function isolatedEnv(root, extra = {}) {
  const home = join(root, 'home');
  const tmp = join(root, 'tmp');
  mkdirSync(join(home, '.pilotdeck'), { recursive: true });
  mkdirSync(tmp, { recursive: true });
  const env = { PATH: process.env.PATH, HOME: home, PILOT_HOME: join(home, '.pilotdeck'), TMPDIR: tmp, ...extra };
  for (const key of Object.keys(env)) if (env[key] === undefined) delete env[key];
  return env;
}

/**
 * 上游用例把 process.cwd() 当基线仓库根，并会在 <cwd>/dist/node_modules 建软链接。
 * 这里在输出目录里搭一个替身根：dist/ui 是拷贝（软链接会建在拷贝旁边），其余条目软链接到基线。
 * 基线目录本身不写任何东西。
 */
export function prepareCwdRoot(baseline, root) {
  const dist = join(root, 'dist');
  if (existsSync(join(dist, 'ui'))) return root;
  mkdirSync(dist, { recursive: true });
  for (const entry of readdirSync(join(baseline, 'dist'))) {
    if (entry === 'ui' || entry === 'node_modules') continue;
    symlinkSync(join(baseline, 'dist', entry), join(dist, entry));
  }
  cpSync(join(baseline, 'dist/ui'), join(dist, 'ui'), { recursive: true });
  if (!existsSync(join(root, 'ui'))) symlinkSync(join(baseline, 'ui'), join(root, 'ui'));
  return root;
}

export function isSymlink(path) {
  try { return lstatSync(path).isSymbolicLink(); } catch { return false; }
}

/** 起一个子进程（独立进程组），收集输出；超过 timeoutMs 整组 SIGKILL。 */
export function runBounded(command, args, { cwd, env, timeoutMs, onStdout }) {
  return new Promise((resolvePromise) => {
    const child = spawn(command, args, { cwd, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    let timedOut = false;
    child.stdout.on('data', (chunk) => { stdout += chunk; onStdout?.(String(chunk)); });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    const timer = setTimeout(() => {
      timedOut = true;
      try { process.kill(-child.pid, 'SIGKILL'); } catch { /* gone */ }
    }, timeoutMs);
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      // 用例可能留下孙进程（外部 sidecar）；收尾时整组再清一次。
      try { process.kill(-child.pid, 'SIGKILL'); } catch { /* already gone */ }
      resolvePromise({ code, signal, stdout, stderr, timedOut });
    });
  });
}
