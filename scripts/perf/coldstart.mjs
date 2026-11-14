#!/usr/bin/env node
// 冷启动：`pilotdeck server` 从 exec 到 stdout 打出 "PilotDeck server listening:" 的耗时。
// 用法：node scripts/perf/coldstart.mjs <ts|rust> [--runs N] [--out file.json] [--reuse-home] [--port P]
//   ts   ：node <BBX_BASELINE>/dist/src/cli/pilotdeck.js server --port P（与 scripts/bbx/run.mjs 起基线的方式相同）
//   rust ：<PERF_TARGET 或 src/target/release>/pilotdeck server --port P
// 每次都在一个新建的临时目录里跑（HOME、PILOT_HOME、TMPDIR、项目目录都在里面），不碰 ~/.pilotdeck。
// --reuse-home：所有轮次共用一个 home（先跑一次热身不计入），量的是「非首次启动」。
// 环境变量与 bbx 相同：PILOTDECK_BUNDLED_SKILLS_DIR / PILOTDECK_BUNDLED_PLUGINS_DIR 都指向基线那份。
import { spawn } from 'node:child_process';
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
const runs = Number(flag('--runs', 30));
const port = Number(flag('--port', 39717));
const outFile = flag('--out', null);
const reuseHome = argv.includes('--reuse-home');
if (!['ts', 'rust'].includes(side)) {
  console.error('usage: coldstart.mjs <ts|rust> [--runs N] [--out file.json] [--reuse-home] [--port P]');
  process.exit(2);
}
if (port === 18789) { console.error('coldstart: refusing port 18789'); process.exit(2); }

const cmd = side === 'ts'
  ? ['node', path.join(BASELINE, 'dist/src/cli/pilotdeck.js'), 'server', '--port', String(port)]
  : [path.join(TARGET, 'pilotdeck'), 'server', '--port', String(port)];

// 与 bbx 的缺省配置相同（假 provider 的地址不会被连：量到监听行就停）。
const CONFIG = `schemaVersion: 1
agent:
  model: bbx/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    bbx:
      protocol: openai
      url: http://127.0.0.1:9/v1
      apiKey: bbx-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
`;
const makeRoot = () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'pdrs-perf-'));
  for (const d of ['home/.pilotdeck', 'proj', 'tmp']) fs.mkdirSync(path.join(root, d), { recursive: true });
  fs.writeFileSync(path.join(root, 'home/.pilotdeck/pilotdeck.yaml'), CONFIG);
  return fs.realpathSync(root);
};
const envFor = (root) => ({
  PATH: process.env.PATH, HOME: path.join(root, 'home'), PILOT_HOME: path.join(root, 'home/.pilotdeck'),
  TMPDIR: path.join(root, 'tmp'), TZ: 'UTC', LANG: 'en_US.UTF-8', NO_COLOR: '1',
  PILOTDECK_GATEWAY_TOKEN: 'perf-token', PILOTDECK_SKIP_BROWSER_OPEN: '1',
  PILOTDECK_BUNDLED_SKILLS_DIR: path.join(BASELINE, 'skills'),
  PILOTDECK_BUNDLED_PLUGINS_DIR: path.join(BASELINE, 'dist/src/extension/plugins/builtin'),
  ...(process.env.PERF_EXTRA_ENV ? JSON.parse(process.env.PERF_EXTRA_ENV) : {}),
});

const once = (root) => new Promise((resolve) => {
  const t0 = process.hrtime.bigint();
  const child = spawn(cmd[0], cmd.slice(1), { cwd: path.join(root, 'proj'), env: envFor(root), stdio: ['ignore', 'pipe', 'pipe'] });
  let out = ''; let err = ''; let ms = null;
  const timer = setTimeout(() => child.kill('SIGKILL'), 30000);
  child.stdout.on('data', (d) => {
    out += d;
    if (ms === null && out.includes('PilotDeck server listening:')) {
      ms = Number(process.hrtime.bigint() - t0) / 1e6;
      child.kill('SIGTERM');
    }
  });
  child.stderr.on('data', (d) => { err += d; });
  child.on('exit', () => { clearTimeout(timer); resolve({ ms, out, err }); });
});

const stats = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * (s.length - 1) + 0.5))];
  return { n: s.length, min: s[0], p25: q(0.25), median: q(0.5), p75: q(0.75), p90: q(0.9), max: s[s.length - 1] };
};

const loadavg = () => os.loadavg().map((x) => Number(x.toFixed(2)));
const results = [];
const load0 = loadavg();
let shared = null;
if (reuseHome) { shared = makeRoot(); await once(shared); }
for (let i = 0; i < runs; i++) {
  const root = shared ?? makeRoot();
  const r = await once(root);
  if (r.ms === null) { console.error(`run ${i}: no listening line\n${r.out}\n${r.err}`); process.exit(1); }
  results.push(Number(r.ms.toFixed(1)));
  if (!shared) fs.rmSync(root, { recursive: true, force: true });
}
if (shared) fs.rmSync(shared, { recursive: true, force: true });
const summary = { side, cmd: cmd.join(' '), reuseHome, runs, loadavgBefore: load0, loadavgAfter: loadavg(), stats: stats(results), samples: results };
console.log(JSON.stringify({ ...summary, samples: undefined }));
if (outFile) fs.writeFileSync(outFile, JSON.stringify(summary, null, 2) + '\n');
