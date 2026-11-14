#!/usr/bin/env node
// 空闲 CPU：起 `pilotdeck server`，等它监听后再静置 settle 秒，然后量 window 秒内整个进程组的累计 CPU 时间差（ps 的 TIME 列）。
// 用法：node scripts/perf/idle-cpu.mjs <ts|rust> [--window 60] [--settle 5] [--rounds 1] [--out file.json] [--port P]
// 起法、隔离目录、环境变量与 coldstart.mjs 相同（不碰 ~/.pilotdeck，端口默认 39719，拒绝 18789）。
// 结果里 cpuPct = CPU 秒 / 墙钟秒 × 100（单核百分比），perPid 是各进程（含子进程，比如内置插件起的 MCP 服务）的份额。
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const BASELINE = process.env.BBX_BASELINE || path.join(REPO, 'resources/PilotDeck');
const TARGET = process.env.PERF_TARGET || path.join(REPO, 'src/target/release');
const argv = process.argv.slice(2);
const side = argv[0];
const flag = (name, dflt) => { const i = argv.indexOf(name); return i >= 0 ? argv[i + 1] : dflt; };
const windowSec = Number(flag('--window', 60));
const settleSec = Number(flag('--settle', 5));
const rounds = Number(flag('--rounds', 1));
const port = Number(flag('--port', 39719));
const outFile = flag('--out', null);
if (!['ts', 'rust'].includes(side)) { console.error('usage: idle-cpu.mjs <ts|rust> [--window S] [--settle S] [--rounds N] [--out F] [--port P]'); process.exit(2); }
if (port === 18789) { console.error('idle-cpu: refusing port 18789'); process.exit(2); }

const CONFIG = fs.readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), 'coldstart.mjs'), 'utf8').match(/const CONFIG = `([\s\S]*?)`;/)[1];
const cmd = side === 'ts'
  ? ['node', path.join(BASELINE, 'dist/src/cli/pilotdeck.js'), 'server', '--port', String(port)]
  : [path.join(TARGET, 'pilotdeck'), 'server', '--port', String(port)];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const secs = (t) => t.split(':').reduce((a, x) => a * 60 + Number(x), 0);
const groupCpu = (pgid) => Object.fromEntries(spawnSync('ps', ['-A', '-o', 'pgid=,pid=,time=,comm='], { encoding: 'utf8' }).stdout
  .split('\n').map((l) => l.trim().split(/\s+/)).filter((x) => x[0] === String(pgid))
  .map((x) => [`${x[1]} ${path.basename(x.slice(3).join(' '))}`, secs(x[2])]));

const results = [];
for (let r = 0; r < rounds; r++) {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'pdrs-perf-')));
  for (const d of ['home/.pilotdeck', 'proj', 'tmp']) fs.mkdirSync(path.join(root, d), { recursive: true });
  fs.writeFileSync(path.join(root, 'home/.pilotdeck/pilotdeck.yaml'), CONFIG);
  const env = {
    PATH: process.env.PATH, HOME: path.join(root, 'home'), PILOT_HOME: path.join(root, 'home/.pilotdeck'),
    TMPDIR: path.join(root, 'tmp'), TZ: 'UTC', LANG: 'en_US.UTF-8', NO_COLOR: '1',
    PILOTDECK_GATEWAY_TOKEN: 'perf-token', PILOTDECK_SKIP_BROWSER_OPEN: '1',
    PILOTDECK_BUNDLED_SKILLS_DIR: path.join(BASELINE, 'skills'),
    PILOTDECK_BUNDLED_PLUGINS_DIR: path.join(BASELINE, 'dist/src/extension/plugins/builtin'),
  };
  const child = spawn(cmd[0], cmd.slice(1), { cwd: path.join(root, 'proj'), env, stdio: ['ignore', 'pipe', 'pipe'], detached: true });
  let out = '';
  child.stdout.on('data', (d) => { out += d; });
  child.stderr.on('data', () => {});
  const t0 = Date.now();
  while (!out.includes('PilotDeck server listening:')) {
    if (child.exitCode !== null || Date.now() - t0 > 30000) { console.error(`server did not start:\n${out}`); process.exit(1); }
    await sleep(20);
  }
  await sleep(settleSec * 1000);
  const before = groupCpu(child.pid); const w0 = Date.now();
  await sleep(windowSec * 1000);
  const after = groupCpu(child.pid); const wall = (Date.now() - w0) / 1000;
  try { process.kill(-child.pid, 'SIGKILL'); } catch {}
  await new Promise((r) => (child.exitCode !== null ? r() : child.on('exit', r)));
  fs.rmSync(root, { recursive: true, force: true });
  const perPid = Object.fromEntries(Object.keys(after).map((k) => [k, +(after[k] - (before[k] ?? 0)).toFixed(2)]));
  const cpuSec = +Object.values(perPid).reduce((a, x) => a + x, 0).toFixed(2);
  results.push({ cpuSec, wallSec: +wall.toFixed(1), cpuPct: +((cpuSec / wall) * 100).toFixed(2), perPid, loadavg: os.loadavg().map((x) => +x.toFixed(2)) });
  console.log(JSON.stringify(results.at(-1)));
}
if (outFile) fs.writeFileSync(outFile, JSON.stringify({ side, cmd: cmd.join(' '), windowSec, settleSec, results }, null, 2) + '\n');
