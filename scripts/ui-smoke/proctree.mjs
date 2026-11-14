#!/usr/bin/env node
// ui-smoke · 进程树采样：证明 Rust 一侧的进程树里没有 Node 的 gateway 进程（交付标准第 6 条）。
// 用法：node scripts/ui-smoke/proctree.mjs --prefix <Rust 二进制目录> --out <结果.json> [--interval 50] -- <命令…>
//   起 <命令…>，在它运行期间每 interval 毫秒（默认 50）扫一遍进程表，直到它退出；退出码照传。
//   「根」= 可执行文件在 --prefix 目录下的进程（Rust 的 pilotdeck、sidecar 等，不管是谁起的）；
//   跟踪集合 = 根 + 根的全部后代（按父进程号闭包）+ 与某个根同一会话的进程（被收养到 init 的孙进程也算进来）。
//   记下跟踪集合里出现过的每一个 (进程号, 命令行)，最后按命令行汇总：几个进程、被采到几次、可执行文件、与根的关系。
// 输出：<结果.json>（汇总）与同名 .txt（人读的清单，含结论行 node-gateway=<个数>）。
// 判 Node gateway 的规则：命令行里出现基线 gateway 的入口（dist/src/cli/pilotdeck.js、src/cli/pilotdeck.ts、
//   dist/src/gateway/、src/gateway/server/），或可执行文件是 node 且参数里有 " server"。
// Linux 读 /proc；其他平台退到 ps（只能看到 ps 给的 comm / args）。命令行里形如 key=… / token=… / sk-… 的值抹掉。
import fs from 'node:fs';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';

const argv = process.argv.slice(2);
const dash = argv.indexOf('--');
if (dash < 0) { console.error('usage: proctree.mjs --prefix DIR --out FILE.json [--interval MS] -- CMD...'); process.exit(2); }
const opts = {}; for (let i = 0; i < dash; i += 2) opts[argv[i].replace(/^--/, '')] = argv[i + 1];
const cmd = argv.slice(dash + 1);
if (!opts.prefix || !opts.out || !cmd.length) { console.error('proctree: --prefix, --out and a command are required'); process.exit(2); }
const PREFIX = fs.realpathSync(path.resolve(opts.prefix)) + path.sep;
const INTERVAL = Number(opts.interval ?? 50);
const OUT = path.resolve(opts.out);
const LINUX = process.platform === 'linux';

const redact = (s) => s
  .replace(/((?:api[_-]?key|token|secret|password|authorization)[=:]\s*)\S+/gi, '$1<redacted>')
  .replace(/\bsk-[A-Za-z0-9_-]{8,}/g, 'sk-<redacted>');

function snapshotLinux() {
  const procs = new Map();
  for (const name of fs.readdirSync('/proc')) {
    if (!/^\d+$/.test(name)) continue;
    const pid = Number(name);
    try {
      const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
      const rest = stat.slice(stat.lastIndexOf(')') + 2).split(' ');
      const ppid = Number(rest[1]); const sid = Number(rest[3]); const start = rest[19];
      let exe = null; try { exe = fs.readlinkSync(`/proc/${pid}/exe`); } catch {}
      procs.set(pid, { pid, ppid, sid, start, exe });
    } catch {}
  }
  return procs;
}
const cmdlineLinux = (pid) => { try { return fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').replace(/\0+$/, '').split('\0').join(' '); } catch { return null; } };

function snapshotPs() {
  const r = spawnSync('ps', ['-axo', 'pid=,ppid=,sess=,comm=,args='], { encoding: 'utf8' });
  const procs = new Map();
  for (const line of r.stdout.split('\n')) {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(\S+)\s+(\S+)\s+(.*)$/); if (!m) continue;
    procs.set(Number(m[1]), { pid: Number(m[1]), ppid: Number(m[2]), sid: m[3], start: '', exe: m[4], args: m[5] });
  }
  return procs;
}

const seen = new Map(); // `${pid}/${start}/${cmdline}` → 记录
let ticks = 0; let maxTickMs = 0; let maxGapMs = 0; let lastTick = 0; let maxTracked = 0;
const t0 = Date.now();
function tick() {
  const ts = Date.now();
  if (lastTick) maxGapMs = Math.max(maxGapMs, ts - lastTick);
  lastTick = ts;
  const procs = LINUX ? snapshotLinux() : snapshotPs();
  const roots = [...procs.values()].filter((p) => p.exe && (p.exe.startsWith(PREFIX)));
  const rootPids = new Set(roots.map((p) => p.pid));
  const children = new Map();
  for (const p of procs.values()) { if (!children.has(p.ppid)) children.set(p.ppid, []); children.get(p.ppid).push(p); }
  const tracked = new Map(); // pid → 关系
  const walk = (p, depth) => {
    if (tracked.has(p.pid) && tracked.get(p.pid).depth <= depth) return;
    tracked.set(p.pid, { rel: depth === 0 ? 'root' : 'descendant', depth });
    for (const c of children.get(p.pid) ?? []) walk(c, depth + 1);
  };
  for (const r of roots) walk(r, 0);
  const rootSids = new Set(roots.filter((r) => String(r.sid) === String(r.pid)).map((r) => String(r.sid)));
  for (const p of procs.values()) if (!tracked.has(p.pid) && rootSids.has(String(p.sid))) tracked.set(p.pid, { rel: 'same-session', depth: -1 });
  maxTracked = Math.max(maxTracked, tracked.size);
  for (const [pid, info] of tracked) {
    const p = procs.get(pid);
    const cl = LINUX ? cmdlineLinux(pid) : p.args;
    if (cl === null || cl === '') continue; // 已退出（僵尸）或内核线程：/proc/<pid>/cmdline 为空，不另记一种
    const k = `${pid}/${p.start}/${cl}`;
    const rec = seen.get(k) ?? { pid, cmdline: redact(cl), exe: p.exe, rel: info.rel, depth: info.depth, samples: 0, firstMs: ts - t0, rootOf: rootPids.has(pid) };
    rec.samples++; rec.lastMs = ts - t0; seen.set(k, rec);
  }
  ticks++;
  maxTickMs = Math.max(maxTickMs, Date.now() - ts);
}

const NODE_GATEWAY = /(dist\/src\/cli\/pilotdeck\.js|src\/cli\/pilotdeck\.ts|dist\/src\/gateway\/|src\/gateway\/server\/)/;
const isNode = (r) => /(^|\/)node(\d+)?$/.test(r.exe ?? '') || /^(\S*\/)?node\s/.test(r.cmdline);
function summarize(exitCode) {
  const kinds = new Map();
  for (const r of seen.values()) {
    const k = kinds.get(r.cmdline) ?? { cmdline: r.cmdline, exe: r.exe, rel: new Set(), processes: 0, samples: 0, node: isNode(r), nodeGateway: NODE_GATEWAY.test(r.cmdline) || (isNode(r) && / server(\s|$)/.test(r.cmdline)) };
    k.processes++; k.samples += r.samples; k.rel.add(r.rel); kinds.set(r.cmdline, k);
  }
  const list = [...kinds.values()].map((k) => ({ ...k, rel: [...k.rel].sort() })).sort((a, b) => b.processes - a.processes || a.cmdline.localeCompare(b.cmdline));
  const durationMs = Date.now() - t0;
  const summary = {
    command: cmd.join(' '), prefix: PREFIX, platform: process.platform, intervalMs: INTERVAL,
    startedAt: new Date(t0).toISOString(), durationMs, ticks, maxTickMs, maxGapMs, maxTracked, exitCode,
    processes: seen.size, kinds: list.length,
    nodeProcesses: list.filter((k) => k.node).reduce((n, k) => n + k.processes, 0),
    nodeGatewayProcesses: list.filter((k) => k.nodeGateway).reduce((n, k) => n + k.processes, 0),
    list,
  };
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(summary, null, 2));
  const lines = [
    `# proctree: ${summary.command}`,
    `# prefix=${PREFIX} interval=${INTERVAL}ms duration=${durationMs}ms ticks=${ticks} maxTick=${maxTickMs}ms maxGap=${maxGapMs}ms exit=${exitCode}`,
    `# processes=${seen.size} kinds=${list.length} node=${summary.nodeProcesses} node-gateway=${summary.nodeGatewayProcesses}`,
    '# 进程数\t采样次数\t关系\tnode\t命令行',
    ...list.map((k) => `${k.processes}\t${k.samples}\t${k.rel.join(',')}\t${k.nodeGateway ? 'NODE-GATEWAY' : k.node ? 'node' : '-'}\t${k.cmdline}`),
  ];
  fs.writeFileSync(OUT.replace(/\.json$/, '') + '.txt', lines.join('\n') + '\n');
  console.error(`[proctree] ${ticks} ticks over ${durationMs}ms (max gap ${maxGapMs}ms), ${seen.size} processes, ${list.length} kinds, node=${summary.nodeProcesses}, node-gateway=${summary.nodeGatewayProcesses} → ${OUT}`);
}

const child = spawn(cmd[0], cmd.slice(1), { stdio: 'inherit' });
let busy = false;
const timer = setInterval(() => { if (busy) return; busy = true; try { tick(); } finally { busy = false; } }, INTERVAL);
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => child.kill(sig));
child.on('exit', (code, sig) => {
  clearInterval(timer); tick();
  summarize(code ?? sig);
  process.exit(code ?? 1);
});
