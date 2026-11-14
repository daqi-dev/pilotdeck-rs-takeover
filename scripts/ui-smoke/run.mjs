#!/usr/bin/env node
// ui-smoke · 把原版 ui/server 接到一侧 gateway 上跑一遍固定操作序列，记录全部流量。
// 用法：node scripts/ui-smoke/run.mjs <ts|rust> <输出目录> [产物前缀，缺省同侧名]
// 产物（输出目录下）：
//   <侧>.ui.json        ui/server 返回给「前端」（drive.mjs）的每个 HTTP 响应与每条 WS 消息，{steps:[...]}
//   <侧>.gateway.jsonl  转发器记下的 ui/server ↔ gateway 全部 HTTP 与 WS 帧（compare.mjs 整理成 <侧>.calls.json）
//   <侧>.provider.jsonl 假 provider 收到的请求
//   <侧>.*.log          各进程自己的输出
// 两侧先后在同一个根目录（UI_SMOKE_ROOT，默认 /private/tmp/pdrs-ui-smoke/root）、同样的端口里跑，路径和端口不用归一化。
// 隔离：HOME / PILOT_HOME / TMPDIR 都在根目录下；macOS 上 gateway 与 ui/server 用 sandbox-exec 跑，
// 只许连 localhost、只许写根目录与 /dev，读真实 ~/.pilotdeck 被拒（Linux 见下面 UI_SMOKE_AUDIT）。所有进程只监听 127.0.0.1。
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '../..');
const [side, outDirArg, labelArg] = process.argv.slice(2);
const label = labelArg ?? side; // 产物文件名前缀；基线自比时用 ts2 之类区分两次
if (!['ts', 'rust'].includes(side) || !outDirArg) { console.error('usage: run.mjs <ts|rust> <outdir>'); process.exit(2); }
const OUT = path.resolve(outDirArg);
fs.mkdirSync(OUT, { recursive: true });

const BASELINE = path.resolve(process.env.UI_SMOKE_BASELINE ?? path.join(REPO, 'resources/PilotDeck'));
const TARGET = path.resolve(process.env.UI_SMOKE_TARGET ?? path.join(REPO, 'src/target/debug'));
const IS_MAC = process.platform === 'darwin';
const ROOT = path.resolve(process.env.UI_SMOKE_ROOT ?? (IS_MAC ? '/private/tmp/pdrs-ui-smoke/root' : path.join(os.tmpdir(), 'pdrs-ui-smoke/root')));
// Linux 上没有 sandbox-exec（cb8 这类机器也不许非特权用户建 user namespace，bwrap / unshare 都起不来）。
// 那里改用 UI_SMOKE_AUDIT=strace：gateway 与 ui/server 套在 strace -f 下，跑完由 audit.mjs 检查
// 有没有连 localhost 以外的地址、有没有写根目录与 /dev 以外的路径、有没有读真实 ~/.pilotdeck（事后检查，不是拦截）。
const AUDIT = process.env.UI_SMOKE_AUDIT ?? '';
if (AUDIT && AUDIT !== 'strace') { console.error('UI_SMOKE_AUDIT must be empty or "strace"'); process.exit(2); }
const BASE_PORT = Number(process.env.UI_SMOKE_PORT_BASE ?? 39100);
const PORTS = { provider: BASE_PORT, gateway: BASE_PORT + 1, proxy: BASE_PORT + 2, ui: BASE_PORT + 3 };
const TOKEN = 'ui-smoke-gateway-token-0000000000000000000';
const home = path.join(ROOT, 'home');
const pilotHome = path.join(home, '.pilotdeck');
const project = path.join(ROOT, 'proj');
const tmp = path.join(ROOT, 'tmp');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const portFree = (port) => new Promise((resolve) => {
  const s = net.createServer().once('error', () => resolve(false)).once('listening', () => s.close(() => resolve(true))).listen(port, '127.0.0.1');
});
// lsof 在 Linux 上只看得到本用户的进程，另外直接连一次 127.0.0.1:18789（连得上就是有监听者）。
const connects = (port) => new Promise((resolve) => {
  const s = net.connect(port, '127.0.0.1');
  s.once('connect', () => { s.destroy(); resolve(true); }).once('error', () => resolve(false));
  s.setTimeout(1000, () => { s.destroy(); resolve(false); });
});
if (spawnSync('lsof', ['-nP', '-iTCP:18789', '-sTCP:LISTEN'], { encoding: 'utf8' }).stdout?.trim() || await connects(18789)) {
  console.error('ui-smoke: something is listening on 18789; refusing to run'); process.exit(5);
}
for (const p of Object.values(PORTS)) if (!(await portFree(p))) { console.error(`ui-smoke: port ${p} in use`); process.exit(3); }

// ── 初始状态 ──
fs.rmSync(ROOT, { recursive: true, force: true });
for (const d of [pilotHome, project, tmp]) fs.mkdirSync(d, { recursive: true });
fs.writeFileSync(path.join(pilotHome, 'pilotdeck.yaml'), `schemaVersion: 1
agent:
  model: smoke/deterministic
  maxContextTokens: 65536
  maxOutputTokens: 8192
model:
  providers:
    smoke:
      protocol: openai
      url: http://127.0.0.1:${PORTS.provider}/v1
      apiKey: ui-smoke-mock-only
      models:
        deterministic:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
        alternate:
          capabilities:
            supportsToolUse: true
            maxContextTokens: 65536
            maxOutputTokens: 8192
telemetry:
  enabled: false
`);
fs.writeFileSync(path.join(pilotHome, 'server-token'), `${TOKEN}\n`, { mode: 0o600 });
fs.writeFileSync(path.join(project, 'note.txt'), 'ui-smoke note content\n');
for (const f of fs.readdirSync(OUT)) if (f.startsWith(`${label}.`)) fs.rmSync(path.join(OUT, f));

// ── 沙箱配置：只许连 localhost，只许写根目录与 /dev ──
const realPilotDeck = path.join(os.userInfo().homedir, '.pilotdeck');
const sbPath = path.join(tmp, 'ui-smoke.sb');
if (IS_MAC) fs.writeFileSync(sbPath, `(version 1)
(allow default)
(deny network-outbound)
(allow network-outbound (remote ip "localhost:*"))
(allow network-outbound (remote unix-socket))
(deny file-read* (subpath ${JSON.stringify(realPilotDeck)}))
(deny file-write*)
(allow file-write* (subpath ${JSON.stringify(fs.realpathSync(ROOT))}) (subpath ${JSON.stringify(ROOT)}) (subpath "/dev"))
`);

// 起 gateway / ui/server 的外壳：macOS 套 sandbox-exec；其他平台直接起，设了 UI_SMOKE_AUDIT=strace 时套 strace -f
// （系统调用记到 <前缀>.<名>.strace，跑完由 audit.mjs 检查）。不套壳时 child.pid 就是 gateway 本身。
const boxed = (name, argv) => {
  if (IS_MAC) return ['sandbox-exec', ['-f', sbPath, ...argv]];
  if (AUDIT === 'strace') return ['strace', ['-f', '-qq', '-y', '-s', '4096', '-e', 'trace=connect,open,openat,creat,mkdir,mkdirat,rename,renameat,renameat2,unlink,unlinkat,rmdir,symlink,symlinkat,link,linkat,truncate,chmod,fchmodat', '-o', path.join(OUT, `${label}.${name}.strace`), ...argv]];
  return [argv[0], argv.slice(1)];
};
if (!IS_MAC && !AUDIT) console.log('[ui-smoke] no sandbox on this platform; set UI_SMOKE_AUDIT=strace to audit network / file access afterwards');

const env = {
  PATH: process.env.PATH, HOME: home, PILOT_HOME: pilotHome, TMPDIR: tmp,
  TZ: 'UTC', LANG: 'en_US.UTF-8', NO_COLOR: '1', PILOTDECK_SKIP_BROWSER_OPEN: '1',
  PILOTDECK_BUNDLED_SKILLS_DIR: path.join(BASELINE, 'skills'),
  PILOTDECK_BUNDLED_PLUGINS_DIR: path.join(BASELINE, 'dist/src/extension/plugins/builtin'),
};
const children = [];
const start = (name, cmd, args, opts = {}) => {
  const logFile = fs.openSync(path.join(OUT, `${label}.${name}.log`), 'w');
  const child = spawn(cmd, args, { stdio: ['ignore', logFile, logFile], detached: true, ...opts });
  children.push({ name, child });
  console.log(`[ui-smoke] started ${name} pid=${child.pid}`);
  return child;
};
const waitPort = async (port, name, child, ms = 90000) => {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    if (child.exitCode !== null) throw new Error(`${name} exited early (${child.exitCode})`);
    if (!(await portFree(port))) return Date.now() - t0;
    await sleep(100);
  }
  throw new Error(`${name} did not listen on ${port}`);
};
// 逐个按进程号停（倒序：ui/server → 转发器 → gateway → provider），SIGTERM 等 15 秒，不退再 SIGKILL 整个进程组。
// 停机结果写进 <前缀>.stop.json（gateway 收到 SIGTERM 后多久退出、退出码）。
const stops = [];
const stopAll = async () => {
  for (const { name, child } of [...children].reverse()) {
    if (child.exitCode !== null || child.signalCode) { console.log(`[ui-smoke] ${name} pid=${child.pid} already exited (${child.exitCode ?? child.signalCode})`); stops.push({ name, already: child.exitCode ?? child.signalCode }); continue; }
    const done = new Promise((r) => child.once('exit', (code, sig) => r(code ?? sig)));
    const t0 = Date.now();
    // strace 外壳下 SIGTERM 发给整个进程组（strace 自己收到只会脱离，被跟踪的 gateway 也要收到）。
    try { process.kill(AUDIT && ['gateway', 'ui'].includes(name) ? -child.pid : child.pid, 'SIGTERM'); } catch {}
    const res = await Promise.race([done, sleep(15000).then(() => 'timeout')]);
    if (res === 'timeout') { try { process.kill(-child.pid, 'SIGKILL'); } catch {} await done; console.log(`[ui-smoke] ${name} pid=${child.pid} SIGKILL`); stops.push({ name, result: 'SIGKILL after 15s SIGTERM' }); }
    else { console.log(`[ui-smoke] ${name} pid=${child.pid} stopped (${res})`); stops.push({ name, result: res, ms: Date.now() - t0 }); }
    try { process.kill(-child.pid, 0); try { process.kill(-child.pid, 'SIGKILL'); } catch {} console.log(`[ui-smoke] ${name} process group ${child.pid} had leftovers; killed`); } catch {}
  }
  fs.writeFileSync(path.join(OUT, `${label}.stop.json`), JSON.stringify(stops, null, 2));
};
process.on('SIGINT', async () => { await stopAll(); process.exit(130); });

let failure = null;
try {
  const provider = start('provider', process.execPath, [path.join(HERE, 'provider.mjs'), String(PORTS.provider), path.join(OUT, `${label}.provider.jsonl`)]);
  await waitPort(PORTS.provider, 'provider', provider);
  const gwCmd = side === 'ts'
    ? [process.execPath, path.join(BASELINE, 'dist/src/cli/pilotdeck.js'), 'server', '--port', String(PORTS.gateway)]
    : [path.join(TARGET, 'pilotdeck'), 'server', '--port', String(PORTS.gateway)];
  const gateway = start('gateway', ...boxed('gateway', gwCmd), { cwd: project, env });
  const gwMs = await waitPort(PORTS.gateway, 'gateway', gateway);
  console.log(`[ui-smoke] gateway listening after ${gwMs}ms`);
  const proxy = start('proxy', process.execPath, [path.join(HERE, 'proxy.mjs'), String(PORTS.proxy), String(PORTS.gateway), path.join(OUT, `${label}.gateway.jsonl`)]);
  await waitPort(PORTS.proxy, 'proxy', proxy);
  const uiEnv = {
    ...env, SERVER_PORT: String(PORTS.ui), HOST: '127.0.0.1', VITE_PORT: String(PORTS.ui + 50),
    PILOTDECK_GATEWAY_URL: `ws://127.0.0.1:${PORTS.proxy}/ws`, PILOTDECK_BRIDGE_TIMEOUT: '20000',
  };
  const ui = start('ui', ...boxed('ui', [process.execPath, '--import', 'tsx', path.join(BASELINE, 'ui/server/index.js')]), { cwd: path.join(BASELINE, 'ui'), env: uiEnv });
  const uiMs = await waitPort(PORTS.ui, 'ui/server', ui);
  console.log(`[ui-smoke] ui/server listening after ${uiMs}ms`);
  const drive = spawn(process.execPath, [path.join(HERE, 'drive.mjs'), `http://127.0.0.1:${PORTS.ui}`, path.join(OUT, `${label}.ui.json`)], {
    env: { ...process.env, UI_SMOKE_PROJECT: project, UI_SMOKE_PILOT_HOME: pilotHome }, stdio: 'inherit',
  });
  const killer = setTimeout(() => drive.kill('SIGKILL'), 600000);
  const status = await new Promise((r) => drive.once('exit', (code, sig) => r(code ?? sig)));
  clearTimeout(killer);
  if (status !== 0) failure = `drive exited ${status}`;
} catch (e) {
  failure = String(e?.stack ?? e);
} finally {
  await stopAll();
}

if (failure) { console.error(`[ui-smoke] FAILED: ${failure}`); process.exit(1); }
console.log('[ui-smoke] done');
